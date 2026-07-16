use crate::OwnerToken;
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Failure to publish a validated native module range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddressOwnershipError {
    /// Addon signatures are never zero.
    ZeroSignature,
    /// A mapped image cannot be empty.
    EmptyRange,
    /// The half-open mapped range overflowed the address space.
    RangeOverflow,
    /// Another mapped generation still owns the signature.
    SignatureInUse,
    /// The same generation was republished with different bounds.
    RangeChanged,
    /// The mapped range overlaps another live module.
    Overlap,
}

impl fmt::Display for AddressOwnershipError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ZeroSignature => "addon signature is zero",
            Self::EmptyRange => "module address range is empty",
            Self::RangeOverflow => "module address range overflowed",
            Self::SignatureInUse => "addon signature already has a mapped generation",
            Self::RangeChanged => "addon generation was republished with different bounds",
            Self::Overlap => "module address range overlaps another live module",
        })
    }
}

impl std::error::Error for AddressOwnershipError {}

/// Result of publishing one validated range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddressPublish {
    /// A new mapped generation was published.
    Inserted,
    /// The exact same generation and bounds were already published.
    AlreadyPresent,
}

#[derive(Clone, Copy)]
struct Entry {
    owner: OwnerToken,
    start: usize,
    end: usize,
    accepting: bool,
}

#[derive(Default)]
struct State {
    entries: Vec<Entry>,
}

/// Concurrent, redaction-safe ownership index for mapped addon images.
///
/// A range remains mapped while an unload is draining, but [`Self::close`]
/// immediately rejects new caller attribution. [`Self::retire`] removes the
/// range only after the platform has successfully released the DLL.
#[derive(Default)]
pub struct AddressOwnershipIndex {
    state: RwLock<State>,
}

impl AddressOwnershipIndex {
    /// Creates an empty ownership index.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: RwLock::new(State {
                entries: Vec::new(),
            }),
        }
    }

    /// Publishes one loader-validated, non-overlapping mapped image.
    pub fn publish(
        &self,
        owner: OwnerToken,
        start: NonZeroUsize,
        size: usize,
    ) -> Result<AddressPublish, AddressOwnershipError> {
        if owner.signature == 0 {
            return Err(AddressOwnershipError::ZeroSignature);
        }
        if size == 0 {
            return Err(AddressOwnershipError::EmptyRange);
        }
        let end = start
            .get()
            .checked_add(size)
            .ok_or(AddressOwnershipError::RangeOverflow)?;
        let mut state = self.write_state();

        if let Some(existing) = state.entries.iter().find(|entry| entry.owner == owner) {
            if existing.start != start.get() || existing.end != end {
                return Err(AddressOwnershipError::RangeChanged);
            }
            return Ok(AddressPublish::AlreadyPresent);
        }
        if state
            .entries
            .iter()
            .any(|entry| entry.owner.signature == owner.signature)
        {
            return Err(AddressOwnershipError::SignatureInUse);
        }
        if state
            .entries
            .iter()
            .any(|entry| start.get() < entry.end && entry.start < end)
        {
            return Err(AddressOwnershipError::Overlap);
        }

        state.entries.push(Entry {
            owner,
            start: start.get(),
            end,
            accepting: true,
        });
        Ok(AddressPublish::Inserted)
    }

    /// Closes API caller admission while retaining the mapped address range.
    pub fn close(&self, owner: OwnerToken) -> bool {
        let mut state = self.write_state();
        let Some(entry) = state.entries.iter_mut().find(|entry| entry.owner == owner) else {
            return false;
        };
        let was_accepting = entry.accepting;
        entry.accepting = false;
        was_accepting
    }

    /// Removes one exact generation after its DLL was successfully released.
    pub fn retire(&self, owner: OwnerToken) -> bool {
        let mut state = self.write_state();
        let before = state.entries.len();
        state.entries.retain(|entry| entry.owner != owner);
        before != state.entries.len()
    }

    /// Finds the mapped generation containing one native code address.
    #[must_use]
    pub fn owner_for_address(&self, address: NonZeroUsize) -> Option<OwnerToken> {
        let address = address.get();
        self.read_state()
            .entries
            .iter()
            .find(|entry| entry.start <= address && address < entry.end)
            .map(|entry| entry.owner)
    }

    /// Returns whether the exact generation still accepts addon API calls.
    #[must_use]
    pub fn is_current_owner(&self, owner: OwnerToken) -> bool {
        self.read_state()
            .entries
            .iter()
            .any(|entry| entry.owner == owner && entry.accepting)
    }

    /// Returns the number of mapped generations without exposing addresses.
    #[must_use]
    pub fn mapped_count(&self) -> usize {
        self.read_state().entries.len()
    }

    fn read_state(&self) -> RwLockReadGuard<'_, State> {
        self.state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write_state(&self) -> RwLockWriteGuard<'_, State> {
        self.state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl fmt::Debug for AddressOwnershipIndex {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.read_state();
        let accepting = state.entries.iter().filter(|entry| entry.accepting).count();
        formatter
            .debug_struct("AddressOwnershipIndex")
            .field("mapped", &state.entries.len())
            .field("accepting", &accepting)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{AddressOwnershipError, AddressOwnershipIndex, AddressPublish};
    use crate::OwnerToken;
    use std::num::NonZeroUsize;

    fn owner(signature: u32, generation: u64) -> OwnerToken {
        OwnerToken {
            signature,
            generation,
        }
    }

    #[test]
    fn close_rejects_new_calls_but_keeps_mapping_until_retirement() {
        let index = AddressOwnershipIndex::new();
        let owner = owner(17, 1);
        let start = NonZeroUsize::new(0x1_0000).expect("fixture address is non-zero");

        assert_eq!(
            index.publish(owner, start, 0x1000),
            Ok(AddressPublish::Inserted)
        );
        assert_eq!(
            index.owner_for_address(NonZeroUsize::new(0x1_0fff).expect("non-zero")),
            Some(owner)
        );
        assert!(index.is_current_owner(owner));

        assert!(index.close(owner));
        assert_eq!(index.owner_for_address(start), Some(owner));
        assert!(!index.is_current_owner(owner));
        assert_eq!(
            index.publish(owner, start, 0x1000),
            Ok(AddressPublish::AlreadyPresent)
        );
        assert!(!index.is_current_owner(owner));
        assert!(index.retire(owner));
        assert_eq!(index.owner_for_address(start), None);
    }

    #[test]
    fn stale_generation_cannot_replace_or_retire_its_successor() {
        let index = AddressOwnershipIndex::new();
        let first = owner(17, 1);
        let second = owner(17, 2);
        let first_start = NonZeroUsize::new(0x1_0000).expect("non-zero");
        let second_start = NonZeroUsize::new(0x2_0000).expect("non-zero");

        assert_eq!(
            index.publish(first, first_start, 0x1000),
            Ok(AddressPublish::Inserted)
        );
        assert_eq!(
            index.publish(second, second_start, 0x1000),
            Err(AddressOwnershipError::SignatureInUse)
        );
        assert!(index.retire(first));
        assert_eq!(
            index.publish(second, second_start, 0x1000),
            Ok(AddressPublish::Inserted)
        );
        assert!(!index.close(first));
        assert!(!index.retire(first));
        assert!(index.is_current_owner(second));
    }

    #[test]
    fn publication_is_idempotent_but_rejects_overlap_and_range_changes() {
        let index = AddressOwnershipIndex::new();
        let first = owner(17, 1);
        let other = owner(23, 1);
        let start = NonZeroUsize::new(0x1_0000).expect("non-zero");

        assert_eq!(
            index.publish(first, start, 0x1000),
            Ok(AddressPublish::Inserted)
        );
        assert_eq!(
            index.publish(first, start, 0x1000),
            Ok(AddressPublish::AlreadyPresent)
        );
        assert_eq!(
            index.publish(first, start, 0x2000),
            Err(AddressOwnershipError::RangeChanged)
        );
        assert_eq!(
            index.publish(
                other,
                NonZeroUsize::new(0x1_0800).expect("non-zero"),
                0x1000,
            ),
            Err(AddressOwnershipError::Overlap)
        );
        assert!(!format!("{index:?}").contains("65536"));
    }
}
