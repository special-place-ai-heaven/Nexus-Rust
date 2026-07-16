//! Lifetime-safe assembly of native Nexus add-on DLLs.
//!
//! The crate separates platform loading, definition inspection, callback
//! quiescence, host cleanup, and module release. Native add-on code remains an
//! unsafe trust boundary: Rust unwind guards do not catch Windows SEH faults,
//! access violations, process termination, or an `extern "C"` panic that aborts.

mod module;
mod platform;

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod windows;

pub use module::{
    AddonLoader, AddonModule, DefinitionIssue, LoadError, ModuleError, ModuleOperation,
    ModuleState, PanicBoundary, ReleaseFailure,
};
pub use platform::{
    ADDON_DEFINITION_EXPORT, AbsoluteDllPath, LoaderPlatform, ModuleBounds, ModuleBoundsError,
    ModuleHandle, ModuleImage, PathPolicyError, PlatformError, PlatformOperation,
};

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub use windows::{WindowsModuleMemory, WindowsPlatform};
