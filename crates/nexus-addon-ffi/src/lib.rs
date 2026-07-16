//! Native add-on API dispatch, render-session catalog, and RAII retirement.
//!
//! This crate is the only Rust/native call boundary for Nexus's v1-v6 add-on
//! APIs. It exposes exact ABI shims backed by one process-wide, generation-
//! scoped service. Native pointers remain opaque here: validation, copying,
//! and resource lifetime ownership belong to [`AddonApiBackend`].

#![deny(unsafe_op_in_unsafe_fn)]

mod backend;
mod caller;
mod dispatcher;
mod shims;

pub use backend::AddonApiBackend;
pub use caller::{AddonCallerResolver, AddonOwnerScope, AddressOwnerResolver};
pub use dispatcher::{
    InstalledAddonApi, RenderSessionInstallError, RenderSessionLease, RenderSessionToken,
    install_render_session,
};
pub use nexus_host::{ApiRevision, ApiTableCatalog, ApiTableRef};

/// MinHook's `MH_ERROR_NOT_INITIALIZED` status returned when dispatch is absent.
///
/// Unlike other missing-service results, MinHook does not use a generic zero:
/// zero is `MH_OK`, while the native MinHook contract assigns value 2 to
/// `MH_ERROR_NOT_INITIALIZED`.
pub const MINHOOK_ERROR_NOT_INITIALIZED: nexus_abi::MinHookStatus = nexus_abi::MinHookStatus(2);

#[cfg(test)]
mod tests;
