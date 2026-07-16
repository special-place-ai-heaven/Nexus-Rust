use nexus_abi::MinHookStatus;

/// An unspecified internal MinHook failure.
pub const MH_UNKNOWN: MinHookStatus = MinHookStatus(-1);
/// The MinHook operation completed successfully.
pub const MH_OK: MinHookStatus = MinHookStatus(0);
/// The MinHook service was already initialized.
pub const MH_ERROR_ALREADY_INITIALIZED: MinHookStatus = MinHookStatus(1);
/// The MinHook service has not been initialized.
pub const MH_ERROR_NOT_INITIALIZED: MinHookStatus = MinHookStatus(2);
/// A hook already exists for the requested target.
pub const MH_ERROR_ALREADY_CREATED: MinHookStatus = MinHookStatus(3);
/// No hook exists for the requested target.
pub const MH_ERROR_NOT_CREATED: MinHookStatus = MinHookStatus(4);
/// The requested hook is already enabled.
pub const MH_ERROR_ENABLED: MinHookStatus = MinHookStatus(5);
/// The requested hook is already disabled.
pub const MH_ERROR_DISABLED: MinHookStatus = MinHookStatus(6);
/// A supplied address is not executable.
pub const MH_ERROR_NOT_EXECUTABLE: MinHookStatus = MinHookStatus(7);
/// The target cannot be represented by a supported inline patch.
pub const MH_ERROR_UNSUPPORTED_FUNCTION: MinHookStatus = MinHookStatus(8);
/// Executable memory allocation failed.
pub const MH_ERROR_MEMORY_ALLOC: MinHookStatus = MinHookStatus(9);
/// Changing or safely coordinating executable memory failed.
pub const MH_ERROR_MEMORY_PROTECT: MinHookStatus = MinHookStatus(10);
/// A requested module was not found.
pub const MH_ERROR_MODULE_NOT_FOUND: MinHookStatus = MinHookStatus(11);
/// A requested exported function was not found.
pub const MH_ERROR_FUNCTION_NOT_FOUND: MinHookStatus = MinHookStatus(12);
