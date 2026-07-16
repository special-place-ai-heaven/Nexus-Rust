use thiserror::Error;

use crate::OwnerGeneration;

/// Errors returned by bounded UI host registries.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum UiRegistryError {
    /// The owner generation has already begun cleanup.
    #[error("owner generation {0:?} is retired")]
    OwnerRetired(OwnerGeneration),

    /// A native pointer was registered with a different lifecycle gate than
    /// the one it was bound to at the unsafe boundary.
    #[error("native resource bound to {bound:?} cannot register as {registration:?}")]
    NativeOwnerMismatch {
        /// Owner supplied when the native resource was constructed.
        bound: OwnerGeneration,
        /// Owner supplied to the registry.
        registration: OwnerGeneration,
    },

    /// A text field exceeds its configured byte limit.
    #[error("{field} is {actual} bytes; maximum is {maximum}")]
    TextTooLong {
        /// Name of the rejected field.
        field: &'static str,
        /// Actual UTF-8 byte length.
        actual: usize,
        /// Configured maximum UTF-8 byte length.
        maximum: usize,
    },

    /// A string contains a NUL and cannot cross the native C-string boundary.
    #[error("{field} contains an interior NUL byte")]
    InteriorNul {
        /// Name of the rejected field.
        field: &'static str,
    },

    /// A bounded registry has reached capacity.
    #[error("{registry} capacity {maximum} has been reached")]
    CapacityExceeded {
        /// Registry or queue whose capacity was reached.
        registry: &'static str,
        /// Configured maximum entry count.
        maximum: usize,
    },

    /// A numeric render phase does not match the legacy ABI.
    #[error("invalid render phase {0}")]
    InvalidRenderPhase(u32),

    /// A limit configuration cannot provide a usable registry.
    #[error("invalid UI host configuration: {0}")]
    InvalidConfiguration(&'static str),
}

pub(crate) fn validate_text(
    field: &'static str,
    value: &str,
    maximum: usize,
) -> Result<(), UiRegistryError> {
    if value.len() > maximum {
        return Err(UiRegistryError::TextTooLong {
            field,
            actual: value.len(),
            maximum,
        });
    }
    if value.as_bytes().contains(&0) {
        return Err(UiRegistryError::InteriorNul { field });
    }
    Ok(())
}
