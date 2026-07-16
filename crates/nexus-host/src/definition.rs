use std::{ffi::c_char, ptr::NonNull, str};

use nexus_abi::{AddonDefinitionFlags, AddonDefinitionV1, UpdateProvider, Version};
use thiserror::Error;

use crate::{ApiRevision, ApiTableError};

const MAX_CONFIGURED_FIELD_BYTES: usize = 1024 * 1024;

/// A string field borrowed from a native add-on definition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataField {
    /// Add-on display name.
    Name,
    /// Add-on author.
    Author,
    /// Add-on description.
    Description,
    /// Optional update URL.
    UpdateLink,
}

impl MetadataField {}

/// Maximum UTF-8 payload length accepted for each metadata field.
///
/// Limits exclude the terminating NUL. Validation reads at most one additional
/// byte so an exact-limit string can still be distinguished from an unbounded
/// one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MetadataLimits {
    name: usize,
    author: usize,
    description: usize,
    update_link: usize,
}

impl MetadataLimits {
    /// Builds validated per-field limits.
    pub fn new(
        name: usize,
        author: usize,
        description: usize,
        update_link: usize,
    ) -> Result<Self, MetadataLimitError> {
        let limits = Self {
            name,
            author,
            description,
            update_link,
        };
        for (field, limit) in [
            (MetadataField::Name, name),
            (MetadataField::Author, author),
            (MetadataField::Description, description),
            (MetadataField::UpdateLink, update_link),
        ] {
            if limit > MAX_CONFIGURED_FIELD_BYTES {
                return Err(MetadataLimitError {
                    field,
                    requested: limit,
                    maximum: MAX_CONFIGURED_FIELD_BYTES,
                });
            }
        }
        Ok(limits)
    }

    const fn for_field(self, field: MetadataField) -> usize {
        match field {
            MetadataField::Name => self.name,
            MetadataField::Author => self.author,
            MetadataField::Description => self.description,
            MetadataField::UpdateLink => self.update_link,
        }
    }
}

impl Default for MetadataLimits {
    fn default() -> Self {
        Self {
            name: 255,
            author: 255,
            description: 4096,
            update_link: 2048,
        }
    }
}

/// An invalid metadata-limit configuration.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("{field:?} metadata limit {requested} exceeds the hard safety maximum of {maximum} bytes")]
pub struct MetadataLimitError {
    field: MetadataField,
    requested: usize,
    maximum: usize,
}

/// A bounded native-memory read failure reported by a platform adapter.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("{message}")]
pub struct ModuleReadError {
    message: Box<str>,
}

impl ModuleReadError {
    /// Creates a platform-neutral memory-read error without exposing memory.
    #[must_use]
    pub fn new(message: impl Into<Box<str>>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Failure to acquire an add-on definition while its module is live.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("{message}")]
pub struct ModuleAccessError {
    message: Box<str>,
}

impl ModuleAccessError {
    /// Creates a platform-neutral module-access error.
    #[must_use]
    pub fn new(message: impl Into<Box<str>>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Bounded access to addresses owned by one currently live native module.
///
/// Implementations must never read more than `maximum_bytes`. The host also
/// checks the returned slice length, so a faulty adapter is rejected before any
/// metadata is accepted.
pub trait ModuleMemory {
    /// Borrows up to `maximum_bytes` starting at `address`.
    fn read_bounded(
        &self,
        address: NonNull<c_char>,
        maximum_bytes: usize,
    ) -> Result<&[u8], ModuleReadError>;
}

/// A raw definition and memory reader tied to one live module borrow.
///
/// The lease contains no ownership of the module. Its lifetime prevents the
/// host from retaining borrowed metadata after the module adapter is released.
#[derive(Clone, Copy)]
pub struct DefinitionLease<'module> {
    definition: &'module AddonDefinitionV1,
    memory: &'module dyn ModuleMemory,
}

impl<'module> DefinitionLease<'module> {
    /// Couples a raw definition with the reader for its live module.
    #[must_use]
    pub const fn new(
        definition: &'module AddonDefinitionV1,
        memory: &'module dyn ModuleMemory,
    ) -> Self {
        Self { definition, memory }
    }

    const fn definition(self) -> &'module AddonDefinitionV1 {
        self.definition
    }

    const fn memory(self) -> &'module dyn ModuleMemory {
        self.memory
    }
}

/// Injected provider that keeps a native module live for a definition lease.
///
/// A future Windows adapter will own the module handle. This crate deliberately
/// has no dynamic-library loading implementation.
pub trait LiveAddonModule {
    /// Borrows the exported definition and its bounded memory reader.
    fn definition(&self) -> Result<DefinitionLease<'_>, ModuleAccessError>;
}

/// A validated add-on definition containing no pointers into native memory.
///
/// Callback addresses are intentionally not retained by this foundation. The
/// later invocation layer must obtain and guard them while the module is live.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedAddonDefinition {
    signature: u32,
    api_revision: ApiRevision,
    name: String,
    version: Version,
    author: String,
    description: String,
    flags: AddonDefinitionFlags,
    provider: UpdateProvider,
    update_link: Option<String>,
    has_unload_callback: bool,
}

impl OwnedAddonDefinition {
    /// Returns the stable add-on signature.
    #[must_use]
    pub const fn signature(&self) -> u32 {
        self.signature
    }

    /// Returns the requested host API revision.
    #[must_use]
    pub const fn api_revision(&self) -> ApiRevision {
        self.api_revision
    }

    /// Returns the owned add-on display name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the add-on version.
    #[must_use]
    pub const fn version(&self) -> Version {
        self.version
    }

    /// Returns the owned author name.
    #[must_use]
    pub fn author(&self) -> &str {
        &self.author
    }

    /// Returns the owned description.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Returns the definition flags, including unknown retained bits.
    #[must_use]
    pub const fn flags(&self) -> AddonDefinitionFlags {
        self.flags
    }

    /// Returns the update provider, including unknown retained values.
    #[must_use]
    pub const fn provider(&self) -> UpdateProvider {
        self.provider
    }

    /// Returns the owned optional update URL.
    #[must_use]
    pub fn update_link(&self) -> Option<&str> {
        self.update_link.as_deref()
    }

    /// Returns whether the raw definition supplied an unload callback.
    #[must_use]
    pub const fn has_unload_callback(&self) -> bool {
        self.has_unload_callback
    }
}

/// Failure to validate and own a native definition.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum DefinitionError {
    /// The module could not provide a live definition lease.
    #[error("could not inspect the live add-on module: {0}")]
    ModuleAccess(#[from] ModuleAccessError),
    /// The signature must be non-zero.
    #[error("add-on signature must be non-zero")]
    ZeroSignature,
    /// A load callback is required by every API revision.
    #[error("add-on definition is missing its required load callback")]
    MissingLoadCallback,
    /// An unload callback is required unless hot loading is disabled.
    #[error("add-on definition has no unload callback and does not disable hot loading")]
    MissingUnloadCallback,
    /// The requested API revision is unsupported.
    #[error(transparent)]
    ApiRevision(#[from] ApiTableError),
    /// A required metadata pointer was null.
    #[error("required add-on {field:?} metadata pointer is null")]
    NullMetadata {
        /// Field whose pointer was null.
        field: MetadataField,
    },
    /// A platform reader could not safely access metadata.
    #[error("could not read add-on {field:?} metadata: {source}")]
    MemoryRead {
        /// Field being copied.
        field: MetadataField,
        /// Reader failure.
        #[source]
        source: ModuleReadError,
    },
    /// A platform reader returned more bytes than requested.
    #[error("add-on {field:?} reader returned {returned} bytes after a {requested}-byte request")]
    ReaderExceededBound {
        /// Field being copied.
        field: MetadataField,
        /// Requested maximum, including room for a NUL.
        requested: usize,
        /// Bytes returned by the reader.
        returned: usize,
    },
    /// No NUL terminator appeared within the configured bound.
    #[error("add-on {field:?} metadata has no NUL terminator within {maximum} bytes")]
    UnterminatedMetadata {
        /// Unterminated field.
        field: MetadataField,
        /// Maximum accepted payload size.
        maximum: usize,
    },
    /// Metadata before the NUL terminator was not valid UTF-8.
    #[error("add-on {field:?} metadata is not valid UTF-8")]
    InvalidUtf8 {
        /// Invalid field.
        field: MetadataField,
    },
}

/// Validates an exported definition and deep-copies all of its metadata while
/// the supplied module lease is alive.
pub fn validate_and_copy_definition(
    module: &impl LiveAddonModule,
    limits: MetadataLimits,
) -> Result<OwnedAddonDefinition, DefinitionError> {
    let lease = module.definition()?;
    let definition = lease.definition();

    if definition.signature == 0 {
        return Err(DefinitionError::ZeroSignature);
    }
    let api_revision = ApiRevision::try_from(definition.api_version)?;
    if definition.load.is_none() {
        return Err(DefinitionError::MissingLoadCallback);
    }
    let disables_hot_loading = definition
        .flags
        .contains(AddonDefinitionFlags::DISABLE_HOT_LOADING);
    if definition.unload.is_none() && !disables_hot_loading {
        return Err(DefinitionError::MissingUnloadCallback);
    }

    let name = copy_required(lease.memory(), definition.name, MetadataField::Name, limits)?;
    let author = copy_required(
        lease.memory(),
        definition.author,
        MetadataField::Author,
        limits,
    )?;
    let description = copy_required(
        lease.memory(),
        definition.description,
        MetadataField::Description,
        limits,
    )?;
    let update_link = copy_optional(
        lease.memory(),
        definition.update_link,
        MetadataField::UpdateLink,
        limits,
    )?;

    Ok(OwnedAddonDefinition {
        signature: definition.signature,
        api_revision,
        name,
        version: definition.version,
        author,
        description,
        flags: definition.flags,
        provider: definition.provider,
        update_link,
        has_unload_callback: definition.unload.is_some(),
    })
}

fn copy_required(
    memory: &dyn ModuleMemory,
    pointer: *const c_char,
    field: MetadataField,
    limits: MetadataLimits,
) -> Result<String, DefinitionError> {
    let address =
        NonNull::new(pointer.cast_mut()).ok_or(DefinitionError::NullMetadata { field })?;
    copy_at(memory, address, field, limits)
}

fn copy_optional(
    memory: &dyn ModuleMemory,
    pointer: *const c_char,
    field: MetadataField,
    limits: MetadataLimits,
) -> Result<Option<String>, DefinitionError> {
    let Some(address) = NonNull::new(pointer.cast_mut()) else {
        return Ok(None);
    };
    let value = copy_at(memory, address, field, limits)?;
    Ok((!value.is_empty()).then_some(value))
}

fn copy_at(
    memory: &dyn ModuleMemory,
    address: NonNull<c_char>,
    field: MetadataField,
    limits: MetadataLimits,
) -> Result<String, DefinitionError> {
    let maximum = limits.for_field(field);
    let requested = maximum + 1;
    let bytes = memory
        .read_bounded(address, requested)
        .map_err(|source| DefinitionError::MemoryRead { field, source })?;
    if bytes.len() > requested {
        return Err(DefinitionError::ReaderExceededBound {
            field,
            requested,
            returned: bytes.len(),
        });
    }
    let nul = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or(DefinitionError::UnterminatedMetadata { field, maximum })?;
    if nul > maximum {
        return Err(DefinitionError::UnterminatedMetadata { field, maximum });
    }
    let text = str::from_utf8(&bytes[..nul]).map_err(|_| DefinitionError::InvalidUtf8 { field })?;
    Ok(text.to_owned())
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, collections::HashMap, ffi::c_char, ptr};

    use nexus_abi::{AddonApi, AddonDefinitionFlags, AddonDefinitionV1, UpdateProvider, Version};

    use super::{
        DefinitionError, DefinitionLease, LiveAddonModule, MetadataField, MetadataLimits,
        ModuleAccessError, ModuleMemory, ModuleReadError, validate_and_copy_definition,
    };

    unsafe extern "C" fn load_stub(_api: *mut AddonApi) {}
    unsafe extern "C" fn unload_stub() {}

    struct FakeModule {
        definition: AddonDefinitionV1,
        memory: FakeMemory,
        live: Cell<bool>,
    }

    impl FakeModule {
        fn valid() -> Self {
            let mut memory = FakeMemory::default();
            let name = memory.insert(b"Example\0".to_vec());
            let author = memory.insert(b"Author\0".to_vec());
            let description = memory.insert(b"Description\0".to_vec());
            let update_link = memory.insert(b"https://example.invalid/addon\0".to_vec());
            Self {
                definition: AddonDefinitionV1 {
                    signature: 0x1234,
                    api_version: 6,
                    name,
                    version: Version::new(1, 2, 3, 4),
                    author,
                    description,
                    load: Some(load_stub),
                    unload: Some(unload_stub),
                    flags: AddonDefinitionFlags::NONE,
                    provider: UpdateProvider::DIRECT,
                    update_link,
                },
                memory,
                live: Cell::new(true),
            }
        }
    }

    impl Drop for FakeModule {
        fn drop(&mut self) {
            self.live.set(false);
            for bytes in self.memory.entries.values_mut() {
                bytes.fill(0xDD);
            }
        }
    }

    impl LiveAddonModule for FakeModule {
        fn definition(&self) -> Result<DefinitionLease<'_>, ModuleAccessError> {
            if !self.live.get() {
                return Err(ModuleAccessError::new("module is no longer live"));
            }
            Ok(DefinitionLease::new(&self.definition, &self.memory))
        }
    }

    #[derive(Default)]
    struct FakeMemory {
        entries: HashMap<usize, Box<[u8]>>,
        ignore_bound: bool,
    }

    impl FakeMemory {
        fn insert(&mut self, bytes: Vec<u8>) -> *const c_char {
            let bytes = bytes.into_boxed_slice();
            let pointer = bytes.as_ptr();
            self.entries.insert(pointer as usize, bytes);
            pointer.cast()
        }
    }

    impl ModuleMemory for FakeMemory {
        fn read_bounded(
            &self,
            address: std::ptr::NonNull<c_char>,
            maximum_bytes: usize,
        ) -> Result<&[u8], ModuleReadError> {
            let bytes = self
                .entries
                .get(&(address.as_ptr() as usize))
                .ok_or_else(|| ModuleReadError::new("address is outside the fake module"))?;
            if self.ignore_bound {
                return Ok(bytes);
            }
            Ok(&bytes[..bytes.len().min(maximum_bytes)])
        }
    }

    #[test]
    fn metadata_is_owned_before_the_module_lease_ends() {
        let owned = {
            let module = FakeModule::valid();
            validate_and_copy_definition(&module, MetadataLimits::default())
                .expect("valid metadata should copy")
        };

        assert_eq!(owned.name(), "Example");
        assert_eq!(owned.author(), "Author");
        assert_eq!(owned.description(), "Description");
        assert_eq!(owned.update_link(), Some("https://example.invalid/addon"));
    }

    #[test]
    fn rejects_null_required_metadata() {
        let mut module = FakeModule::valid();
        module.definition.author = ptr::null();

        let error = validate_and_copy_definition(&module, MetadataLimits::default())
            .expect_err("null author must fail");
        assert_eq!(
            error,
            DefinitionError::NullMetadata {
                field: MetadataField::Author
            }
        );
    }

    #[test]
    fn rejects_invalid_utf8() {
        let mut module = FakeModule::valid();
        module.definition.name = module.memory.insert(vec![0xFF, 0]);

        let error = validate_and_copy_definition(&module, MetadataLimits::default())
            .expect_err("invalid UTF-8 must fail");
        assert_eq!(
            error,
            DefinitionError::InvalidUtf8 {
                field: MetadataField::Name
            }
        );
    }

    #[test]
    fn rejects_metadata_without_a_bounded_terminator() {
        let mut module = FakeModule::valid();
        module.definition.description = module.memory.insert(vec![b'x'; 9]);
        let limits = MetadataLimits::new(8, 8, 8, 8).expect("small limits are valid");

        let error = validate_and_copy_definition(&module, limits)
            .expect_err("unterminated metadata must fail");
        assert_eq!(
            error,
            DefinitionError::UnterminatedMetadata {
                field: MetadataField::Description,
                maximum: 8
            }
        );
    }

    #[test]
    fn rejects_a_reader_that_ignores_the_requested_bound() {
        let mut module = FakeModule::valid();
        module.memory.ignore_bound = true;
        let limits = MetadataLimits::new(4, 32, 32, 64).expect("small limits are valid");

        let error = validate_and_copy_definition(&module, limits)
            .expect_err("oversized reader result must fail");
        assert!(matches!(
            error,
            DefinitionError::ReaderExceededBound {
                field: MetadataField::Name,
                requested: 5,
                ..
            }
        ));
    }

    #[test]
    fn rejects_unsafe_limit_configurations() {
        assert!(MetadataLimits::new(usize::MAX, 1, 1, 1).is_err());
    }

    #[test]
    fn permits_missing_unload_only_for_locked_lifetimes() {
        let mut module = FakeModule::valid();
        module.definition.unload = None;
        assert!(matches!(
            validate_and_copy_definition(&module, MetadataLimits::default()),
            Err(DefinitionError::MissingUnloadCallback)
        ));

        module.definition.flags = AddonDefinitionFlags::DISABLE_HOT_LOADING;
        let owned = validate_and_copy_definition(&module, MetadataLimits::default())
            .expect("locked lifetime may omit unload callback");
        assert!(!owned.has_unload_callback());
    }
}
