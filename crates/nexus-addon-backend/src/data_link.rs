use core::ffi::{c_char, c_void};
use core::fmt;
use std::sync::Arc;

use nexus_data_services::DataLinkService;

use crate::{BackendFailure, BackendOperationError, NativeCallBoundary};

/// Caller-attributed adapter for stable DataLink resources.
pub struct DataLinkApi {
    boundary: Arc<NativeCallBoundary>,
    service: Arc<DataLinkService>,
}

impl DataLinkApi {
    /// Creates an adapter around the process DataLink service.
    #[must_use]
    pub fn new(boundary: Arc<NativeCallBoundary>, service: Arc<DataLinkService>) -> Self {
        Self { boundary, service }
    }

    /// Gets one existing stable resource pointer.
    pub fn get(&self, identifier: *const c_char) -> Result<*mut c_void, BackendOperationError> {
        let _owner = self.boundary.resolve_owner(None)?;
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        let resource = self
            .service
            .get(identifier.as_str())
            .map_err(|_| self.rejected())?;
        Ok(resource.map_or(core::ptr::null_mut(), |lease| lease.as_mut_ptr()))
    }

    /// Gets or creates one exact-size public mapping.
    pub fn share(
        &self,
        identifier: *const c_char,
        size: usize,
    ) -> Result<*mut c_void, BackendOperationError> {
        let _owner = self.boundary.resolve_owner(None)?;
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        self.service
            .share_public(identifier.as_str(), size, None)
            .map(|lease| lease.as_mut_ptr())
            .map_err(|_| self.rejected())
    }

    fn rejected(&self) -> BackendOperationError {
        self.boundary
            .failures()
            .record(BackendFailure::ServiceRejected);
        BackendOperationError::ServiceRejected
    }
}

impl fmt::Debug for DataLinkApi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DataLinkApi")
            .field("boundary", &self.boundary)
            .finish_non_exhaustive()
    }
}
