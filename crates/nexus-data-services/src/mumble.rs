use core::mem::{MaybeUninit, offset_of, size_of};
use core::ptr;
use core::sync::atomic::{Ordering, compiler_fence};

use nexus_abi::MumbleData;
use nexus_gw2::{MumbleSource, SnapshotError};
use thiserror::Error;

use crate::{ResourceKind, ResourceLease};

const SNAPSHOT_ATTEMPTS: usize = 4;

/// Closed failures when adapting a DataLink resource to Mumble telemetry.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum MumbleResourceError {
    /// Guild Wars 2 telemetry must remain backed by a public named mapping.
    #[error("the Mumble resource is not a public shared-memory mapping")]
    NotPublic,
    /// The retained resource does not cover exactly one Mumble ABI value.
    #[error("the Mumble resource has an incompatible length")]
    LengthMismatch,
}

/// Retained DataLink resource read as coherent MumbleLink snapshots.
#[derive(Clone, Debug)]
pub struct MumbleResourceSource {
    resource: ResourceLease,
}

impl MumbleResourceSource {
    /// Validates and retains one exact public MumbleLink resource.
    pub fn new(resource: ResourceLease) -> Result<Self, MumbleResourceError> {
        if resource.kind() != ResourceKind::Public {
            return Err(MumbleResourceError::NotPublic);
        }
        if resource.len() != size_of::<MumbleData>() {
            return Err(MumbleResourceError::LengthMismatch);
        }
        Ok(Self { resource })
    }

    /// Returns the retained DataLink lease.
    #[must_use]
    pub const fn resource(&self) -> &ResourceLease {
        &self.resource
    }

    fn read_tick(&self) -> u32 {
        let address = self.resource.as_mut_ptr() as usize + offset_of!(MumbleData, ui_tick);
        let source = address as *const u8;
        let mut bytes = [0_u8; size_of::<u32>()];
        for (offset, byte) in bytes.iter_mut().enumerate() {
            // SAFETY: construction proves the retained resource covers the
            // complete ui_tick field. Byte access has no alignment requirement
            // and volatile reads observe an external writer.
            *byte = unsafe { ptr::read_volatile(source.add(offset)) };
        }
        u32::from_ne_bytes(bytes)
    }

    fn copy_volatile(&self) -> MumbleData {
        let source = self.resource.as_mut_ptr().cast_const().cast::<u8>();
        let mut snapshot = MaybeUninit::<MumbleData>::uninit();
        let destination = snapshot.as_mut_ptr().cast::<u8>();
        compiler_fence(Ordering::SeqCst);
        for offset in 0..size_of::<MumbleData>() {
            // SAFETY: construction proves the source covers the exact ABI size;
            // the destination is an equally sized uninitialized value. Every
            // Mumble ABI field accepts every copied bit pattern.
            unsafe {
                destination
                    .add(offset)
                    .write(ptr::read_volatile(source.add(offset)));
            }
        }
        compiler_fence(Ordering::SeqCst);
        // SAFETY: every destination byte was initialized above and all ABI bit
        // patterns are valid.
        unsafe { snapshot.assume_init() }
    }
}

impl MumbleSource for MumbleResourceSource {
    fn snapshot(&self) -> Result<MumbleData, SnapshotError> {
        for _ in 0..SNAPSHOT_ATTEMPTS {
            let before = self.read_tick();
            let snapshot = self.copy_volatile();
            let after = self.read_tick();
            if before == after && snapshot.ui_tick == before {
                return Ok(snapshot);
            }
        }
        Err(SnapshotError::Unstable)
    }
}

#[cfg(test)]
mod tests {
    use core::mem::size_of;
    use core::ptr;
    use std::sync::Arc;

    use nexus_abi::{DL_MUMBLE_LINK, MumbleData};
    use nexus_gw2::MumbleSource;

    use crate::test_support::MemoryMappingBackend;
    use crate::{DataLinkService, MumbleResourceError, MumbleResourceSource};

    #[test]
    fn public_data_link_resource_produces_an_owned_coherent_snapshot() {
        let data_link =
            DataLinkService::with_process_id(Arc::new(MemoryMappingBackend::default()), 42);
        let resource = data_link
            .share_public(DL_MUMBLE_LINK, size_of::<MumbleData>(), Some("mumble-test"))
            .expect("the injected mapping is valid");
        let expected = MumbleData {
            ui_tick: 73,
            ..MumbleData::default()
        };
        // SAFETY: the retained mapping is exactly one writable MumbleData and
        // no concurrent writer exists in this test.
        unsafe { ptr::write(resource.as_mut_ptr().cast::<MumbleData>(), expected) };

        let source =
            MumbleResourceSource::new(resource).expect("the exact public resource is valid");
        let snapshot = source.snapshot().expect("the test writer is stable");

        assert_eq!(snapshot.ui_tick, 73);
    }

    #[test]
    fn internal_or_wrong_length_resources_are_rejected_closed() {
        let data_link =
            DataLinkService::with_process_id(Arc::new(MemoryMappingBackend::default()), 7);
        let internal = data_link
            .share_internal("internal-mumble", size_of::<MumbleData>())
            .expect("the internal resource is valid");
        assert_eq!(
            MumbleResourceSource::new(internal).expect_err("internal storage must be rejected"),
            MumbleResourceError::NotPublic
        );

        let short = data_link
            .share_public("short-mumble", 1, Some("short-mumble-test"))
            .expect("the injected mapping is valid");
        assert_eq!(
            MumbleResourceSource::new(short).expect_err("wrong lengths must be rejected"),
            MumbleResourceError::LengthMismatch
        );
    }
}
