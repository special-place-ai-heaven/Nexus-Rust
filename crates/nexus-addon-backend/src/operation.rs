use thiserror::Error;

use crate::CallBoundaryError;

/// Redacted failure returned by a validated backend operation.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum BackendOperationError {
    /// Caller attribution or native argument copying failed.
    #[error(transparent)]
    Boundary(#[from] CallBoundaryError),
    /// A typed domain service rejected a validated request.
    #[error("typed backend service rejected the request")]
    ServiceRejected,
}
