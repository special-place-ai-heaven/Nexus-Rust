//! Synchronous, bounded Windows WinHTTP transport for `nexus-network`.
//!
//! The transport follows the operating system's automatic proxy and TLS
//! policy, applies explicit finite timeouts, and never includes request URLs,
//! query strings, or header values in diagnostics. WinHTTP is deliberately
//! opened in synchronous mode so every request owns a simple stack of RAII
//! handles with deterministic teardown.

mod policy;

#[cfg(windows)]
mod platform;

use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};

use nexus_network::{HttpRequest, Transport, TransportError, TransportResponse};
use thiserror::Error;

pub use policy::{TimeoutStage, WinHttpTimeouts, WinHttpTimeoutsError};

fn contain_transport_boundary<T>(
    operation: impl FnOnce() -> Result<T, TransportError>,
) -> Result<T, TransportError> {
    catch_unwind(AssertUnwindSafe(operation)).unwrap_or(Err(TransportError::RequestFailed))
}

/// Initialization operation that failed before a transport became usable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitializationStage {
    /// Opening the synchronous WinHTTP session.
    OpenSession,
    /// Applying the finite resolve/connect/send/receive timeouts.
    ConfigureTimeouts,
}

/// Redaction-safe failure returned while constructing a WinHTTP transport.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum WinHttpInitializationError {
    /// WinHTTP is unavailable on the current target.
    #[error("WinHTTP transport is only available on Windows")]
    UnsupportedPlatform,
    /// A Windows API operation failed.
    #[error("WinHTTP initialization failed at {stage:?} (system error {code})")]
    Platform {
        /// Closed operation category; never request-derived.
        stage: InitializationStage,
        /// Numeric Windows error code; never request-derived.
        code: u32,
    },
}

/// Synchronous WinHTTP implementation of [`Transport`].
///
/// The session uses `WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY` and leaves secure
/// protocol selection to the system. HTTPS requests add only WinHTTP's secure
/// request flag; certificate validation and Schannel defaults remain intact.
pub struct WinHttpTransport {
    timeouts: WinHttpTimeouts,
    #[cfg(windows)]
    session: platform::Session,
}

impl WinHttpTransport {
    /// Opens a transport using finite default timeouts.
    ///
    /// # Errors
    ///
    /// Returns a redacted initialization error if WinHTTP is unavailable or
    /// the session cannot be configured.
    pub fn new() -> Result<Self, WinHttpInitializationError> {
        Self::with_timeouts(WinHttpTimeouts::default())
    }

    /// Opens a transport with explicit finite timeouts.
    ///
    /// # Errors
    ///
    /// Returns a redacted initialization error if WinHTTP is unavailable or
    /// the session cannot be configured.
    pub fn with_timeouts(timeouts: WinHttpTimeouts) -> Result<Self, WinHttpInitializationError> {
        #[cfg(windows)]
        {
            let session = platform::Session::open(timeouts).map_err(|failure| {
                WinHttpInitializationError::Platform {
                    stage: failure.stage,
                    code: failure.code,
                }
            })?;
            Ok(Self { timeouts, session })
        }

        #[cfg(not(windows))]
        {
            let _ = timeouts;
            Err(WinHttpInitializationError::UnsupportedPlatform)
        }
    }

    /// Returns the finite timeout policy applied to this session.
    #[must_use]
    pub const fn timeouts(&self) -> WinHttpTimeouts {
        self.timeouts
    }

    fn get_contained(
        &mut self,
        request: &HttpRequest,
        max_body_bytes: usize,
    ) -> Result<TransportResponse, TransportError> {
        #[cfg(windows)]
        {
            platform::execute(&self.session, request, max_body_bytes)
        }

        #[cfg(not(windows))]
        {
            let _ = (request, max_body_bytes);
            Err(TransportError::RequestFailed)
        }
    }
}

impl fmt::Debug for WinHttpTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WinHttpTransport")
            .field("timeouts", &self.timeouts)
            .field("session", &"[redacted]")
            .finish()
    }
}

impl Transport for WinHttpTransport {
    fn get(
        &mut self,
        request: &HttpRequest,
        max_body_bytes: usize,
    ) -> Result<TransportResponse, TransportError> {
        contain_transport_boundary(|| self.get_contained(request, max_body_bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_debug_never_exposes_native_handles() {
        #[cfg(windows)]
        {
            let Ok(transport) = WinHttpTransport::new() else {
                return;
            };
            let rendered = format!("{transport:?}");
            assert!(rendered.contains("[redacted]"));
            assert!(!rendered.contains("0x"));
        }
    }

    #[test]
    fn transport_boundary_contains_and_discards_rust_panics() {
        let result: Result<(), TransportError> =
            contain_transport_boundary(|| panic!("synthetic boundary panic"));
        assert_eq!(result, Err(TransportError::RequestFailed));
    }
}
