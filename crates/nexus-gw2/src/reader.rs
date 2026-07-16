use nexus_abi::{MumbleData, MumbleIdentity};
use thiserror::Error;

use crate::{DerivedTelemetry, IdentityParseError, TelemetryTracker, parse_identity};

/// Redaction-safe failure to obtain a coherent shared-memory snapshot.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum SnapshotError {
    /// The producer changed its tick during every bounded copy attempt.
    #[error("the MumbleLink producer did not yield a coherent snapshot")]
    Unstable,
}

/// Source of owned Mumble snapshots.
pub trait MumbleSource {
    /// Copies one snapshot without returning a borrow into mutable shared memory.
    fn snapshot(&self) -> Result<MumbleData, SnapshotError>;
}

/// Result of comparing a parsed identity with the current owned value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum IdentityUpdate {
    /// The Mumble identity buffer was empty.
    Absent,
    /// A valid identity matched the current value.
    Unchanged,
    /// A valid identity replaced the current value.
    Updated(MumbleIdentity),
}

/// One combined telemetry poll.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MumblePoll {
    /// Owned raw Mumble snapshot.
    pub snapshot: MumbleData,
    /// Parsed identity result; failures never partially modify reader state.
    pub identity: Result<IdentityUpdate, IdentityParseError>,
    /// States derived from this snapshot and the render frame count.
    pub derived: DerivedTelemetry,
}

/// Stateful synchronous Mumble reader over an injected snapshot source.
pub struct MumbleReader<S> {
    source: S,
    identity: MumbleIdentity,
    telemetry: TelemetryTracker,
}

impl<S> MumbleReader<S>
where
    S: MumbleSource,
{
    /// Creates a reader with zeroed legacy-compatible initial state.
    pub fn new(source: S) -> Self {
        Self {
            source,
            identity: MumbleIdentity::default(),
            telemetry: TelemetryTracker::default(),
        }
    }

    /// Returns the latest completely validated identity.
    #[must_use]
    pub const fn identity(&self) -> &MumbleIdentity {
        &self.identity
    }

    /// Returns the underlying source.
    #[must_use]
    pub const fn source(&self) -> &S {
        &self.source
    }

    /// Reads and processes a single coherent snapshot.
    pub fn poll(&mut self, frame_count: u64) -> Result<MumblePoll, SnapshotError> {
        let snapshot = self.source.snapshot()?;
        let identity = if snapshot.identity.first().copied().unwrap_or_default() == 0 {
            Ok(IdentityUpdate::Absent)
        } else {
            parse_identity(&snapshot.identity).map(|next| {
                if next == self.identity {
                    IdentityUpdate::Unchanged
                } else {
                    self.identity = next;
                    IdentityUpdate::Updated(next)
                }
            })
        };
        let derived = self.telemetry.advance(&snapshot, frame_count);
        Ok(MumblePoll {
            snapshot,
            identity,
            derived,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use nexus_abi::MumbleData;

    use super::{IdentityUpdate, MumbleReader, MumbleSource, SnapshotError};

    struct CellSource {
        data: Cell<MumbleData>,
    }

    impl MumbleSource for CellSource {
        fn snapshot(&self) -> Result<MumbleData, SnapshotError> {
            Ok(self.data.get())
        }
    }

    fn data_with_identity(json: &str) -> MumbleData {
        let mut data = MumbleData::default();
        for (slot, value) in data.identity.iter_mut().zip(json.encode_utf16()) {
            *slot = value;
        }
        data
    }

    #[test]
    fn invalid_identity_never_partially_replaces_the_last_good_value() {
        let valid = r#"{"name":"Good","profession":1,"spec":2,"race":3,"map_id":4,"world_id":5,"team_color_id":6,"commander":false,"fov":1.0,"uisz":1}"#;
        let source = CellSource {
            data: Cell::new(data_with_identity(valid)),
        };
        let mut reader = MumbleReader::new(source);
        let first = reader.poll(1);
        assert!(first.is_ok());
        assert!(matches!(
            first.ok().and_then(|poll| poll.identity.ok()),
            Some(IdentityUpdate::Updated(_))
        ));
        let previous = *reader.identity();

        reader.source.data.set(data_with_identity("not-json"));
        let second = reader.poll(2);
        assert!(second.is_ok());
        assert!(second.is_ok_and(|poll| poll.identity.is_err()));
        assert_eq!(*reader.identity(), previous);
    }

    #[test]
    fn empty_identity_is_distinct_from_a_parse_failure() {
        let source = CellSource {
            data: Cell::new(MumbleData::default()),
        };
        let mut reader = MumbleReader::new(source);
        let result = reader.poll(1);
        assert!(result.is_ok_and(|poll| poll.identity == Ok(IdentityUpdate::Absent)));
    }
}
