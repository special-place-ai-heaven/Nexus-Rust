use std::{
    cell::UnsafeCell,
    collections::HashMap,
    error::Error,
    ffi::c_void,
    fmt,
    sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard},
};

/// Failure to create or retrieve a shared DataLink resource.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataLinkError {
    /// Legacy ABI rejects zero-sized shared resources.
    ZeroSize,
    /// An identifier already exists with a different immutable size.
    SizeMismatch {
        /// Size of the existing allocation.
        existing: usize,
        /// Size requested by the caller.
        requested: usize,
    },
    /// The allocation could not reserve the requested capacity.
    AllocationFailed,
}

impl fmt::Display for DataLinkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroSize => formatter.write_str("DataLink resources cannot be empty"),
            Self::SizeMismatch {
                existing,
                requested,
            } => write!(
                formatter,
                "DataLink resource size mismatch: existing {existing}, requested {requested}"
            ),
            Self::AllocationFailed => formatter.write_str("DataLink resource allocation failed"),
        }
    }
}

impl Error for DataLinkError {}

/// Stable owning handle for one shared DataLink allocation.
#[derive(Clone)]
pub struct ResourceHandle {
    resource: Arc<SharedResource>,
}

impl ResourceHandle {
    /// Returns the immutable allocation size.
    #[must_use]
    pub fn len(&self) -> usize {
        self.resource.bytes.len()
    }

    /// Returns whether the allocation is empty. Shared resources are never empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the stable raw address exposed by the native ABI.
    ///
    /// Dereferencing the pointer remains unsafe. Native producers and consumers
    /// must coordinate access exactly as they do for a shared-memory mapping.
    #[must_use]
    pub fn as_mut_ptr(&self) -> *mut c_void {
        self.resource.bytes.as_ptr().cast::<u8>().cast_mut().cast()
    }
}

struct SharedResource {
    bytes: Box<[UnsafeCell<u8>]>,
}

// SAFETY: `SharedResource` never creates Rust references to the cells after
// construction. It exposes only a stable raw pointer, whose synchronization is
// explicitly the responsibility of native DataLink participants.
unsafe impl Send for SharedResource {}
// SAFETY: see the `Send` rationale. Moving or sharing the owner cannot relocate
// the boxed allocation and does not itself access the cell contents.
unsafe impl Sync for SharedResource {}

impl SharedResource {
    fn zeroed(size: usize) -> Result<Self, DataLinkError> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(size)
            .map_err(|_| DataLinkError::AllocationFailed)?;
        bytes.extend((0..size).map(|_| UnsafeCell::new(0)));
        Ok(Self {
            bytes: bytes.into_boxed_slice(),
        })
    }
}

/// Stable, zero-initialized resource registry for the native DataLink ABI.
#[derive(Default)]
pub struct DataLink {
    resources: RwLock<HashMap<String, Arc<SharedResource>>>,
}

impl DataLink {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a stable handle when the identifier has already been shared.
    #[must_use]
    pub fn get(&self, identifier: &str) -> Option<ResourceHandle> {
        read_lock(&self.resources)
            .get(identifier)
            .cloned()
            .map(|resource| ResourceHandle { resource })
    }

    /// Gets or creates an allocation, requiring exact size agreement.
    pub fn share(&self, identifier: &str, size: usize) -> Result<ResourceHandle, DataLinkError> {
        if size == 0 {
            return Err(DataLinkError::ZeroSize);
        }

        if let Some(existing) = self.get(identifier) {
            return size_checked(existing, size);
        }

        let candidate = Arc::new(SharedResource::zeroed(size)?);
        let mut resources = write_lock(&self.resources);
        let resource = resources
            .entry(identifier.to_owned())
            .or_insert_with(|| candidate)
            .clone();
        size_checked(ResourceHandle { resource }, size)
    }

    /// Returns the number of shared identifiers for diagnostics.
    #[must_use]
    pub fn resource_count(&self) -> usize {
        read_lock(&self.resources).len()
    }
}

fn size_checked(handle: ResourceHandle, requested: usize) -> Result<ResourceHandle, DataLinkError> {
    let existing = handle.len();
    if existing == requested {
        Ok(handle)
    } else {
        Err(DataLinkError::SizeMismatch {
            existing,
            requested,
        })
    }
}

fn read_lock<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_lock<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, thread};

    use super::{DataLink, DataLinkError};

    #[test]
    fn allocations_are_zeroed_and_stable() {
        let data_link = DataLink::new();
        let first = data_link
            .share("resource", 32)
            .unwrap_or_else(|error| panic!("share failed: {error}"));
        let address = first.as_mut_ptr();

        for index in 0..128 {
            data_link
                .share(&format!("other-{index}"), 8)
                .unwrap_or_else(|error| panic!("share failed: {error}"));
        }

        let second = data_link
            .get("resource")
            .unwrap_or_else(|| panic!("resource disappeared"));
        assert_eq!(address, second.as_mut_ptr());
        // SAFETY: the allocation is 32 bytes, remains owned by `first`, and no
        // other test participant mutates it.
        let bytes = unsafe { core::slice::from_raw_parts(address.cast::<u8>(), 32) };
        assert!(bytes.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn rejects_zero_and_mismatched_sizes() {
        let data_link = DataLink::new();
        assert!(matches!(
            data_link.share("zero", 0),
            Err(DataLinkError::ZeroSize)
        ));
        data_link
            .share("resource", 4)
            .unwrap_or_else(|error| panic!("share failed: {error}"));
        assert!(matches!(
            data_link.share("resource", 5),
            Err(DataLinkError::SizeMismatch {
                existing: 4,
                requested: 5,
            })
        ));
    }

    #[test]
    fn concurrent_creation_converges_on_one_allocation() {
        let data_link = Arc::new(DataLink::new());
        let handles = (0..8)
            .map(|_| {
                let data_link = Arc::clone(&data_link);
                thread::spawn(move || {
                    data_link
                        .share("shared", 16)
                        .unwrap_or_else(|error| panic!("share failed: {error}"))
                        .as_mut_ptr() as usize
                })
            })
            .map(|thread| {
                thread
                    .join()
                    .unwrap_or_else(|_| panic!("worker thread panicked"))
            })
            .collect::<Vec<_>>();

        assert!(handles.windows(2).all(|pair| pair[0] == pair[1]));
        assert_eq!(data_link.resource_count(), 1);
    }
}
