//! Owner-aware inline hooks with MinHook-compatible status behavior.
//!
//! On 64-bit Windows the service uses `retour`'s stable raw detour API, but
//! serializes every registry mutation and suspends the process's other known
//! threads around each code patch. A patch is rejected when a suspended RIP is
//! inside a conservative bounded window around any target.
//!
//! Windows does not offer an atomic "freeze this process" primitive. A thread
//! created after the Tool Help snapshot can therefore escape the transaction;
//! callers must treat that thread-creation race as the remaining platform
//! limitation. All threads the transaction does open and suspend are restored
//! by RAII on success, error, and unwind.

#![deny(missing_docs)]

mod report;
mod status;

#[cfg(all(windows, target_arch = "x86_64"))]
mod quiescence;
#[cfg(not(all(windows, target_arch = "x86_64")))]
mod unsupported;
#[cfg(all(windows, target_arch = "x86_64"))]
mod windows;

pub use nexus_abi::MinHookStatus;
pub use report::{CleanupError, CleanupReport};
pub use status::{
    MH_ERROR_ALREADY_CREATED, MH_ERROR_ALREADY_INITIALIZED, MH_ERROR_DISABLED, MH_ERROR_ENABLED,
    MH_ERROR_FUNCTION_NOT_FOUND, MH_ERROR_MEMORY_ALLOC, MH_ERROR_MEMORY_PROTECT,
    MH_ERROR_MODULE_NOT_FOUND, MH_ERROR_NOT_CREATED, MH_ERROR_NOT_EXECUTABLE,
    MH_ERROR_NOT_INITIALIZED, MH_ERROR_UNSUPPORTED_FUNCTION, MH_OK, MH_UNKNOWN,
};
#[cfg(not(all(windows, target_arch = "x86_64")))]
pub use unsupported::InlineHookService;
#[cfg(all(windows, target_arch = "x86_64"))]
pub use windows::InlineHookService;
