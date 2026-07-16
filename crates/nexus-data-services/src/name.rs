use core::ffi::c_char;
use core::slice;
use core::str;

use thiserror::Error;

/// Maximum byte length accepted for DataLink and event identifiers.
pub const MAX_IDENTIFIER_BYTES: usize = 255;
const MAX_MAPPING_NAME_BYTES: usize = 1_024;

/// Closed, redaction-safe validation failures for native service names.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum NameError {
    /// A required native string pointer was null.
    #[error("the name pointer is null")]
    NullPointer,
    /// The supplied name was empty.
    #[error("the name is empty")]
    Empty,
    /// The supplied name contained an embedded nul.
    #[error("the name contains an embedded nul")]
    EmbeddedNul,
    /// The supplied name contained another control character.
    #[error("the name contains a control character")]
    ControlCharacter,
    /// The supplied identifier exceeded the accepted bound.
    #[error("the name is too long")]
    TooLong,
    /// The supplied native identifier was not UTF-8.
    #[error("the name is not valid UTF-8")]
    InvalidUtf8,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ValidatedName(String);

impl ValidatedName {
    pub(crate) fn identifier(value: &str) -> Result<Self, NameError> {
        validate(value, MAX_IDENTIFIER_BYTES)
    }

    pub(crate) fn mapping(value: &str) -> Result<Self, NameError> {
        validate(value, MAX_MAPPING_NAME_BYTES)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn into_string(self) -> String {
        self.0
    }
}

fn validate(value: &str, maximum: usize) -> Result<ValidatedName, NameError> {
    if value.is_empty() {
        return Err(NameError::Empty);
    }
    if value.len() > maximum {
        return Err(NameError::TooLong);
    }
    if value.bytes().any(|byte| byte == 0) {
        return Err(NameError::EmbeddedNul);
    }
    if value.chars().any(char::is_control) {
        return Err(NameError::ControlCharacter);
    }
    Ok(ValidatedName(value.to_owned()))
}

/// Copies and validates a bounded native identifier.
///
/// # Safety
///
/// `pointer` must be null or point to readable memory through the first nul
/// byte, with at least `MAX_IDENTIFIER_BYTES + 1` readable bytes when no
/// earlier nul exists.
pub(crate) unsafe fn identifier_from_c(pointer: *const c_char) -> Result<ValidatedName, NameError> {
    if pointer.is_null() {
        return Err(NameError::NullPointer);
    }

    for length in 0..=MAX_IDENTIFIER_BYTES {
        // SAFETY: the caller guarantees a readable bounded C string.
        let byte = unsafe { pointer.add(length).read() } as u8;
        if byte == 0 {
            if length == 0 {
                return Err(NameError::Empty);
            }
            // SAFETY: the loop established that these `length` bytes are
            // readable and precede the terminating nul.
            let bytes = unsafe { slice::from_raw_parts(pointer.cast::<u8>(), length) };
            let value = str::from_utf8(bytes).map_err(|_| NameError::InvalidUtf8)?;
            return ValidatedName::identifier(value);
        }
    }

    Err(NameError::TooLong)
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;

    use super::{MAX_IDENTIFIER_BYTES, NameError, ValidatedName, identifier_from_c};

    #[test]
    fn validates_closed_names_without_echoing_them() {
        assert_eq!(ValidatedName::identifier(""), Err(NameError::Empty));
        assert_eq!(
            ValidatedName::identifier("event\nname"),
            Err(NameError::ControlCharacter)
        );
        let private_marker = "sentinel-value";
        let overlong = format!("{private_marker}{}", "x".repeat(MAX_IDENTIFIER_BYTES + 1));
        let error = ValidatedName::identifier(&overlong)
            .expect_err("the overlong fixture must be rejected");
        assert_eq!(error, NameError::TooLong);
        assert!(!error.to_string().contains(private_marker));
    }

    #[test]
    fn copies_a_bounded_c_identifier() {
        let input = CString::new("EV_TEST").expect("the fixture contains no nul");
        // SAFETY: `input` is a live, nul-terminated C string.
        let parsed = unsafe { identifier_from_c(input.as_ptr()) }
            .expect("the fixture is a valid identifier");
        assert_eq!(parsed.as_str(), "EV_TEST");
    }
}
