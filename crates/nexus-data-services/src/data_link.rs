use core::ffi::{c_char, c_void};
use core::num::NonZeroUsize;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use nexus_core::{DataLink, DataLinkError, ResourceHandle};
use thiserror::Error;

use crate::mapping::{MappingBackend, MappingDisposition, MappingFailure, MappingView};
use crate::name::{NameError, ValidatedName, identifier_from_c};

/// Storage kind retained for a DataLink resource.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceKind {
    /// Process-local zero-initialized allocation owned by `nexus-core`.
    Internal,
    /// Writable named mapping owned until the final lease is dropped.
    Public,
}

#[derive(Clone)]
enum ResourceStorage {
    Internal(ResourceHandle),
    Public(Arc<dyn MappingView>),
}

/// Stable owning lease for one DataLink resource.
///
/// A lease keeps either the internal allocation or the public mapping alive,
/// so its raw pointer remains stable even if the service itself is dropped.
#[derive(Clone)]
pub struct ResourceLease {
    storage: ResourceStorage,
}

impl ResourceLease {
    /// Returns the exact immutable resource size.
    #[must_use]
    pub fn len(&self) -> usize {
        match &self.storage {
            ResourceStorage::Internal(resource) => resource.len(),
            ResourceStorage::Public(resource) => resource.len(),
        }
    }

    /// Returns whether the resource is empty. DataLink never creates empty resources.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the retained storage kind.
    #[must_use]
    pub const fn kind(&self) -> ResourceKind {
        match &self.storage {
            ResourceStorage::Internal(_) => ResourceKind::Internal,
            ResourceStorage::Public(_) => ResourceKind::Public,
        }
    }

    /// Returns the public mapping acquisition disposition, when applicable.
    #[must_use]
    pub fn mapping_disposition(&self) -> Option<MappingDisposition> {
        match &self.storage {
            ResourceStorage::Internal(_) => None,
            ResourceStorage::Public(resource) => Some(resource.disposition()),
        }
    }

    /// Returns the stable pointer exposed through the native DataLink ABI.
    ///
    /// Dereferencing this pointer remains unsafe. Native readers and writers
    /// must honor the resource's synchronization and layout contract.
    #[must_use]
    pub fn as_mut_ptr(&self) -> *mut c_void {
        match &self.storage {
            ResourceStorage::Internal(resource) => resource.as_mut_ptr(),
            ResourceStorage::Public(resource) => resource.address().get() as *mut c_void,
        }
    }
}

impl fmt::Debug for ResourceLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResourceLease")
            .field("kind", &self.kind())
            .field("len", &self.len())
            .field("mapping_disposition", &self.mapping_disposition())
            .finish_non_exhaustive()
    }
}

/// Redaction-safe DataLink service failures.
#[derive(Debug, Error)]
pub enum DataServiceError {
    /// The logical DataLink identifier was invalid.
    #[error("invalid DataLink identifier: {0}")]
    InvalidIdentifier(#[source] NameError),
    /// The underlying public mapping name was invalid.
    #[error("invalid shared-memory name: {0}")]
    InvalidMappingName(#[source] NameError),
    /// DataLink rejects zero-sized resources.
    #[error("DataLink resources cannot be empty")]
    ZeroSize,
    /// An identifier was already registered with a different immutable size.
    #[error("the resource already has size {existing}, not {requested}")]
    SizeMismatch {
        /// Existing immutable size.
        existing: usize,
        /// Requested size.
        requested: usize,
    },
    /// An internal allocation could not reserve the requested capacity.
    #[error("the internal resource allocation failed")]
    AllocationFailed,
    /// A public resource exceeded the Win32-compatible size bound.
    #[error("the public resource size is unsupported")]
    PublicSizeUnsupported,
    /// The public named-mapping backend failed.
    #[error(transparent)]
    Mapping(#[from] MappingFailure),
}

/// Owned registry for internal DataLink allocations and public named mappings.
pub struct DataLinkService {
    internal: DataLink,
    mapping_backend: Arc<dyn MappingBackend>,
    process_id: u32,
    registry: RwLock<HashMap<String, ResourceLease>>,
}

impl DataLinkService {
    /// Creates an empty service using the current process ID for default public names.
    #[must_use]
    pub fn new(mapping_backend: Arc<dyn MappingBackend>) -> Self {
        Self::with_process_id(mapping_backend, std::process::id())
    }

    /// Creates an empty service with an explicit process ID.
    ///
    /// This is useful for deterministic host integration and injected-backend tests.
    #[must_use]
    pub fn with_process_id(mapping_backend: Arc<dyn MappingBackend>, process_id: u32) -> Self {
        Self {
            internal: DataLink::new(),
            mapping_backend,
            process_id,
            registry: RwLock::new(HashMap::new()),
        }
    }

    /// Gets an existing resource by logical identifier.
    pub fn get(&self, identifier: &str) -> Result<Option<ResourceLease>, DataServiceError> {
        let identifier =
            ValidatedName::identifier(identifier).map_err(DataServiceError::InvalidIdentifier)?;
        Ok(self.get_validated(&identifier))
    }

    /// Gets or creates one process-local zero-initialized allocation.
    pub fn share_internal(
        &self,
        identifier: &str,
        size: usize,
    ) -> Result<ResourceLease, DataServiceError> {
        let identifier =
            ValidatedName::identifier(identifier).map_err(DataServiceError::InvalidIdentifier)?;
        let size = NonZeroUsize::new(size).ok_or(DataServiceError::ZeroSize)?;
        let mut registry = write_lock(&self.registry);
        if let Some(existing) = registry.get(identifier.as_str()) {
            return exact_size(existing.clone(), size.get());
        }

        let resource = self
            .internal
            .share(identifier.as_str(), size.get())
            .map_err(DataServiceError::from)?;
        let lease = ResourceLease {
            storage: ResourceStorage::Internal(resource),
        };
        registry.insert(identifier.into_string(), lease.clone());
        Ok(lease)
    }

    /// Gets or creates one writable public named mapping.
    ///
    /// When `underlying_name` is absent or empty, the legacy-compatible name
    /// `<identifier>_<process id>` is used. Existing mapping bytes are never
    /// cleared.
    pub fn share_public(
        &self,
        identifier: &str,
        size: usize,
        underlying_name: Option<&str>,
    ) -> Result<ResourceLease, DataServiceError> {
        let identifier =
            ValidatedName::identifier(identifier).map_err(DataServiceError::InvalidIdentifier)?;
        let size = NonZeroUsize::new(size).ok_or(DataServiceError::ZeroSize)?;
        let mut registry = write_lock(&self.registry);
        if let Some(existing) = registry.get(identifier.as_str()) {
            return exact_size(existing.clone(), size.get());
        }

        if u32::try_from(size.get()).is_err() {
            return Err(DataServiceError::PublicSizeUnsupported);
        }
        let mapping_name = match underlying_name {
            Some(name) if !name.is_empty() => ValidatedName::mapping(name),
            _ => ValidatedName::mapping(&format!("{}_{}", identifier.as_str(), self.process_id)),
        }
        .map_err(DataServiceError::InvalidMappingName)?;

        let mapping = self
            .mapping_backend
            .open_or_create(mapping_name.as_str(), size)?;
        if mapping.len() != size.get() {
            return Err(MappingFailure::ContractLength {
                expected: size.get(),
                actual: mapping.len(),
            }
            .into());
        }
        let lease = ResourceLease {
            storage: ResourceStorage::Public(mapping),
        };
        registry.insert(identifier.into_string(), lease.clone());
        Ok(lease)
    }

    /// Returns the number of logical identifiers retained by this service.
    #[must_use]
    pub fn resource_count(&self) -> usize {
        read_lock(&self.registry).len()
    }

    /// Validates a native identifier and returns an owning resource lease.
    ///
    /// # Safety
    ///
    /// `identifier` must satisfy the readable bounded C-string contract
    /// documented by the native Nexus DataLink ABI.
    pub unsafe fn try_get_abi(
        &self,
        identifier: *const c_char,
    ) -> Result<Option<ResourceLease>, DataServiceError> {
        // SAFETY: forwarded from this method's native string contract.
        let identifier = unsafe { identifier_from_c(identifier) }
            .map_err(DataServiceError::InvalidIdentifier)?;
        Ok(self.get_validated(&identifier))
    }

    /// Legacy-compatible `GetResource` boundary that returns null on invalid input.
    ///
    /// # Safety
    ///
    /// `identifier` must satisfy the readable bounded C-string contract
    /// documented by the native Nexus DataLink ABI.
    #[must_use]
    pub unsafe fn get_abi(&self, identifier: *const c_char) -> *mut c_void {
        // SAFETY: forwarded from this method's native string contract.
        unsafe { self.try_get_abi(identifier) }
            .ok()
            .flatten()
            .map_or(core::ptr::null_mut(), |resource| resource.as_mut_ptr())
    }

    /// Validates native input and gets or creates an internal ABI resource.
    ///
    /// # Safety
    ///
    /// `identifier` must satisfy the readable bounded C-string contract
    /// documented by the native Nexus DataLink ABI.
    pub unsafe fn try_share_internal_abi(
        &self,
        identifier: *const c_char,
        size: usize,
    ) -> Result<ResourceLease, DataServiceError> {
        // SAFETY: forwarded from this method's native string contract.
        let identifier = unsafe { identifier_from_c(identifier) }
            .map_err(DataServiceError::InvalidIdentifier)?;
        self.share_internal(identifier.as_str(), size)
    }

    /// Legacy-compatible `ShareResource` boundary that returns null on failure.
    ///
    /// # Safety
    ///
    /// `identifier` must satisfy the readable bounded C-string contract
    /// documented by the native Nexus DataLink ABI.
    #[must_use]
    pub unsafe fn share_internal_abi(&self, identifier: *const c_char, size: usize) -> *mut c_void {
        // SAFETY: forwarded from this method's native string contract.
        unsafe { self.try_share_internal_abi(identifier, size) }
            .map_or(core::ptr::null_mut(), |resource| resource.as_mut_ptr())
    }

    fn get_validated(&self, identifier: &ValidatedName) -> Option<ResourceLease> {
        read_lock(&self.registry).get(identifier.as_str()).cloned()
    }
}

impl From<DataLinkError> for DataServiceError {
    fn from(value: DataLinkError) -> Self {
        match value {
            DataLinkError::ZeroSize => Self::ZeroSize,
            DataLinkError::SizeMismatch {
                existing,
                requested,
            } => Self::SizeMismatch {
                existing,
                requested,
            },
            DataLinkError::AllocationFailed => Self::AllocationFailed,
        }
    }
}

fn exact_size(
    resource: ResourceLease,
    requested: usize,
) -> Result<ResourceLease, DataServiceError> {
    if resource.len() == requested {
        Ok(resource)
    } else {
        Err(DataServiceError::SizeMismatch {
            existing: resource.len(),
            requested,
        })
    }
}

fn read_lock<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write_lock<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::mapping::{MappingBackend, MappingFailure};
    use crate::test_support::{MemoryMappingBackend, WrongLengthBackend};

    use super::{DataLinkService, DataServiceError, ResourceKind};

    #[test]
    fn internal_resources_are_zeroed_stable_and_size_checked() {
        let backend: Arc<dyn MappingBackend> = Arc::new(MemoryMappingBackend::default());
        let service = DataLinkService::with_process_id(backend, 42);
        let first = service
            .share_internal("DL_INTERNAL", 32)
            .expect("the allocation should succeed");
        assert_eq!(first.kind(), ResourceKind::Internal);
        assert_eq!(first.len(), 32);
        // SAFETY: the lease retains a writable 32-byte internal allocation.
        let bytes = unsafe { core::slice::from_raw_parts(first.as_mut_ptr().cast::<u8>(), 32) };
        assert!(bytes.iter().all(|byte| *byte == 0));

        let second = service
            .share_internal("DL_INTERNAL", 32)
            .expect("same-size sharing should return the existing allocation");
        assert_eq!(first.as_mut_ptr(), second.as_mut_ptr());
        let legacy_existing = service
            .share_public("DL_INTERNAL", 32, Some("invalid\nname"))
            .expect("an existing same-size identifier wins before public-name handling");
        assert_eq!(legacy_existing.kind(), ResourceKind::Internal);
        assert_eq!(first.as_mut_ptr(), legacy_existing.as_mut_ptr());
        assert!(matches!(
            service.share_internal("DL_INTERNAL", 31),
            Err(DataServiceError::SizeMismatch {
                existing: 32,
                requested: 31
            })
        ));
    }

    #[test]
    fn injected_backend_reopens_without_clearing_existing_bytes() {
        let backend = Arc::new(MemoryMappingBackend::default());
        let first_service = DataLinkService::with_process_id(backend.clone(), 7);
        let first = first_service
            .share_public("DL_FIRST", 16, Some("Local\\NexusDataServiceTest"))
            .expect("the first mapping should be created");
        // SAFETY: the lease retains at least one writable byte.
        unsafe { first.as_mut_ptr().cast::<u8>().write(0xA5) };

        let second_service = DataLinkService::with_process_id(backend, 8);
        let second = second_service
            .share_public("DL_SECOND", 16, Some("Local\\NexusDataServiceTest"))
            .expect("the existing mapping should reopen");
        // SAFETY: the lease retains at least one readable byte.
        assert_eq!(unsafe { second.as_mut_ptr().cast::<u8>().read() }, 0xA5);
        assert_eq!(first.as_mut_ptr(), second.as_mut_ptr());
    }

    #[test]
    fn leases_retain_allocations_after_the_service_is_dropped() {
        let backend: Arc<dyn MappingBackend> = Arc::new(MemoryMappingBackend::default());
        let (internal, public) = {
            let service = DataLinkService::with_process_id(backend, 17);
            let internal = service
                .share_internal("DL_RETAINED_INTERNAL", 8)
                .expect("the internal fixture should allocate");
            let public = service
                .share_public("DL_RETAINED_PUBLIC", 8, Some("Local\\NexusRetained"))
                .expect("the public fixture should map");
            (internal, public)
        };

        // SAFETY: both leases independently retain at least one writable byte
        // after their originating service has been dropped.
        unsafe {
            internal.as_mut_ptr().cast::<u8>().write(0x11);
            public.as_mut_ptr().cast::<u8>().write(0x22);
            assert_eq!(internal.as_mut_ptr().cast::<u8>().read(), 0x11);
            assert_eq!(public.as_mut_ptr().cast::<u8>().read(), 0x22);
        }
    }

    #[test]
    fn rejects_an_injected_backend_contract_violation() {
        let backend: Arc<dyn MappingBackend> = Arc::new(WrongLengthBackend);
        let service = DataLinkService::new(backend);
        assert!(matches!(
            service.share_public("DL_BAD", 16, Some("Local\\NexusBadLength")),
            Err(DataServiceError::Mapping(MappingFailure::ContractLength {
                expected: 16,
                actual: 15
            }))
        ));
    }

    #[test]
    fn errors_are_redaction_safe() {
        let backend: Arc<dyn MappingBackend> = Arc::new(MemoryMappingBackend::default());
        let service = DataLinkService::new(backend);
        let private_marker = "do-not-echo-this-marker";
        let error = service
            .share_public("DL_SAFE", 1, Some(&format!("{private_marker}\n")))
            .expect_err("control characters must be rejected");
        assert!(!error.to_string().contains(private_marker));
    }
}
