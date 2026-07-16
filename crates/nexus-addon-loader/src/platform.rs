use std::{
    ffi::CStr,
    fmt,
    num::NonZeroUsize,
    path::{Path, PathBuf},
};

use nexus_abi::GetAddonDefinitionV1;
use nexus_host::{ModuleMemory, ModuleReadError};
use thiserror::Error;

/// Exact exported symbol required from a Nexus add-on DLL.
pub const ADDON_DEFINITION_EXPORT: &CStr = c"GetAddonDef";

/// An owned absolute DLL path.
// Deliberately omits `Display`; diagnostics must not accidentally reveal a path.
#[derive(Clone, Eq, PartialEq)]
pub struct AbsoluteDllPath(PathBuf);

impl AbsoluteDllPath {
    /// Validates that `path` is absolute and identifies a DLL file.
    pub fn new(path: impl AsRef<Path>) -> Result<Self, PathPolicyError> {
        let path = path.as_ref();
        if !path.is_absolute() {
            return Err(PathPolicyError::NotAbsolute);
        }
        let Some(extension) = path.extension().and_then(|value| value.to_str()) else {
            return Err(PathPolicyError::NotDll);
        };
        if !extension.eq_ignore_ascii_case("dll") || path.file_name().is_none() {
            return Err(PathPolicyError::NotDll);
        }
        Ok(Self(path.to_path_buf()))
    }

    /// Borrows the validated path for a platform implementation.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

impl fmt::Debug for AbsoluteDllPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AbsoluteDllPath(<redacted>)")
    }
}

impl TryFrom<PathBuf> for AbsoluteDllPath {
    type Error = PathPolicyError;

    fn try_from(path: PathBuf) -> Result<Self, Self::Error> {
        Self::new(path)
    }
}

/// Failure of the path-only DLL search policy.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum PathPolicyError {
    /// Relative paths can be affected by the process current directory.
    #[error("add-on DLL path must be absolute")]
    NotAbsolute,
    /// Only paths naming a DLL are accepted.
    #[error("add-on path must identify a DLL")]
    NotDll,
}

/// Opaque platform module token.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct ModuleHandle(NonZeroUsize);

impl ModuleHandle {
    /// Wraps a non-zero token returned by a platform loader.
    #[must_use]
    pub const fn from_non_zero(raw: NonZeroUsize) -> Self {
        Self(raw)
    }

    /// Returns the raw token for a platform implementation.
    #[must_use]
    pub const fn raw(self) -> NonZeroUsize {
        self.0
    }
}

impl fmt::Debug for ModuleHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ModuleHandle(<redacted>)")
    }
}

/// Validated half-open address range of one mapped module image.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct ModuleBounds {
    base: NonZeroUsize,
    size: usize,
    end: usize,
}

impl ModuleBounds {
    /// Creates bounds after checking zero-sized and overflowing images.
    pub fn new(base: NonZeroUsize, size: usize) -> Result<Self, ModuleBoundsError> {
        if size == 0 {
            return Err(ModuleBoundsError::Empty);
        }
        let end = base
            .get()
            .checked_add(size)
            .ok_or(ModuleBoundsError::Overflow)?;
        Ok(Self { base, size, end })
    }

    /// Returns the mapped image size without disclosing its address.
    #[must_use]
    pub const fn image_size(self) -> usize {
        self.size
    }

    /// Returns whether an address lies inside the image.
    #[must_use]
    pub const fn contains_address(self, address: NonZeroUsize) -> bool {
        address.get() >= self.base.get() && address.get() < self.end
    }

    /// Returns whether an entire byte range lies inside the image.
    #[must_use]
    pub fn contains_range(self, address: NonZeroUsize, length: usize) -> bool {
        if address.get() < self.base.get() {
            return false;
        }
        address
            .get()
            .checked_add(length)
            .is_some_and(|end| end <= self.end)
    }

    /// Caps a read to the remaining bytes in the image.
    #[must_use]
    pub(crate) fn cap_read(self, address: NonZeroUsize, requested: usize) -> Option<usize> {
        self.contains_address(address)
            .then(|| requested.min(self.end - address.get()))
    }

    pub(crate) const fn base(self) -> NonZeroUsize {
        self.base
    }
}

impl fmt::Debug for ModuleBounds {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModuleBounds")
            .field("image_size", &self.size)
            .finish_non_exhaustive()
    }
}

/// Invalid mapped-image bounds.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ModuleBoundsError {
    /// A mapped image cannot be empty.
    #[error("module image is empty")]
    Empty,
    /// The half-open image range overflowed the address space.
    #[error("module image bounds overflowed")]
    Overflow,
}

/// Bounded mapped image plus its platform memory reader.
pub struct ModuleImage<M> {
    bounds: ModuleBounds,
    memory: M,
}

impl<M> ModuleImage<M> {
    /// Couples validated bounds with the memory reader for the same module.
    #[must_use]
    pub const fn new(bounds: ModuleBounds, memory: M) -> Self {
        Self { bounds, memory }
    }

    /// Returns the validated image bounds.
    #[must_use]
    pub const fn bounds(&self) -> ModuleBounds {
        self.bounds
    }
}

impl<M: ModuleMemory> ModuleMemory for ModuleImage<M> {
    fn read_bounded(
        &self,
        address: std::ptr::NonNull<std::ffi::c_char>,
        maximum_bytes: usize,
    ) -> Result<&[u8], ModuleReadError> {
        let numeric = NonZeroUsize::new(address.as_ptr() as usize)
            .ok_or_else(|| ModuleReadError::new("module memory address was null"))?;
        let maximum_bytes = self
            .bounds
            .cap_read(numeric, maximum_bytes)
            .ok_or_else(|| ModuleReadError::new("module memory address is outside the image"))?;
        let bytes = self.memory.read_bounded(address, maximum_bytes)?;
        if bytes.len() > maximum_bytes {
            return Err(ModuleReadError::new(
                "platform memory reader exceeded its requested bound",
            ));
        }
        Ok(bytes)
    }
}

/// Closed platform failure categories suitable for redacted diagnostics.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum PlatformError {
    /// The path could not be represented for the native API.
    #[error("DLL path cannot be represented by the platform API")]
    PathEncoding,
    /// The native loader rejected the module.
    #[error("platform failed to load the add-on module")]
    LoadLibrary,
    /// The exact Nexus definition export was absent.
    #[error("add-on definition export is missing")]
    MissingDefinitionExport,
    /// The platform could not determine trustworthy image bounds.
    #[error("platform could not inspect the mapped module image")]
    ModuleImage,
    /// A virtual-memory region could not be inspected.
    #[error("platform could not inspect module memory")]
    MemoryQuery,
    /// The platform rejected a memory range as unreadable.
    #[error("module memory range is not readable")]
    MemoryNotReadable,
    /// An expected code address was not executable.
    #[error("module code address is not executable")]
    AddressNotExecutable,
    /// The native loader could not decrement the module reference count.
    #[error("platform failed to release the add-on module")]
    FreeLibrary,
}

/// Platform operation associated with a closed failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlatformOperation {
    /// Load the absolute DLL path.
    LoadLibrary,
    /// Determine mapped-image bounds and memory access.
    InspectImage,
    /// Resolve the exact definition export.
    ResolveDefinitionExport,
    /// Validate an exported or callback code address.
    InspectCodeAddress,
    /// Release the native module reference.
    FreeLibrary,
}

/// Testable native-module platform boundary.
///
/// # Safety
///
/// Implementations must keep every successful `ModuleHandle` live until a
/// matching successful `free_library`, return an image reader for that same
/// mapping, resolve only the requested symbol with its exact ABI, and never
/// return borrowed bytes that outlive or fall outside the live mapping.
pub unsafe trait LoaderPlatform: Send + Sync + 'static {
    /// Memory reader coupled to one returned module image.
    type Memory: ModuleMemory + Send + Sync + 'static;

    /// Loads an absolute DLL without consulting the process current directory.
    ///
    /// # Safety
    ///
    /// The caller must accept native loader execution and eventually release
    /// every successfully returned token exactly once.
    unsafe fn load_library(&self, path: &AbsoluteDllPath) -> Result<ModuleHandle, PlatformError>;

    /// Resolves `export` with the `GetAddonDefinitionV1` ABI.
    ///
    /// # Safety
    ///
    /// `module` must be a currently live token returned by this adapter.
    unsafe fn resolve_definition_export(
        &self,
        module: ModuleHandle,
        export: &'static CStr,
    ) -> Result<GetAddonDefinitionV1, PlatformError>;

    /// Determines validated bounds and a reader for the mapped image.
    ///
    /// # Safety
    ///
    /// `module` must be a currently live token returned by this adapter.
    unsafe fn module_image(
        &self,
        module: ModuleHandle,
    ) -> Result<ModuleImage<Self::Memory>, PlatformError>;

    /// Checks that `address` is executable code in `module`.
    ///
    /// # Safety
    ///
    /// `module` must be live and `address` must be treated only as a numeric
    /// candidate until this method returns `true`.
    unsafe fn is_executable_address(
        &self,
        module: ModuleHandle,
        address: NonZeroUsize,
    ) -> Result<bool, PlatformError>;

    /// Releases one loader reference without consuming the token on failure.
    ///
    /// # Safety
    ///
    /// `module` must be a live, unreleased token with all callbacks quiesced
    /// and host-owned references removed.
    unsafe fn free_library(&self, module: ModuleHandle) -> Result<(), PlatformError>;
}
