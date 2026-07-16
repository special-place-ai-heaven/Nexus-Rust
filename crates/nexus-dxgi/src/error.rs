use std::fmt;

use nexus_hook::VtableError;

/// Failure to attach or control a concrete DXGI object.
#[derive(Debug)]
pub enum DxgiError {
    /// The caller supplied a null COM interface pointer.
    NullInterface,
    /// The supplied IID is not one of the supported factory or swap-chain interfaces.
    UnsupportedInterface,
    /// Shutdown already stopped admission of new native objects.
    ManagerClosed,
    /// A different interception manager already owns this concrete interface.
    HookConflict,
    /// Preparing, publishing, or restoring a typed shadow vtable failed.
    Vtable(VtableError),
}

impl fmt::Display for DxgiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NullInterface => formatter.write_str("null DXGI interface"),
            Self::UnsupportedInterface => formatter.write_str("unsupported DXGI interface"),
            Self::ManagerClosed => formatter.write_str("DXGI interception manager is closed"),
            Self::HookConflict => {
                formatter.write_str("DXGI interface is owned by another hook manager")
            }
            Self::Vtable(error) => write!(formatter, "DXGI vtable interception failed: {error}"),
        }
    }
}

impl std::error::Error for DxgiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Vtable(error) => Some(error),
            Self::NullInterface
            | Self::UnsupportedInterface
            | Self::ManagerClosed
            | Self::HookConflict => None,
        }
    }
}

impl From<VtableError> for DxgiError {
    fn from(error: VtableError) -> Self {
        Self::Vtable(error)
    }
}
