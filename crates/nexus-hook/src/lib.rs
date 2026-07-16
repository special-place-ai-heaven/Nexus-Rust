//! Per-instance COM vtable shadowing for Nexus.
//!
//! This crate changes only the vtable pointer stored in one COM interface
//! object. It does not patch executable code, mutate a process-wide vtable, or
//! create worker threads. A shadow owns copies of both the original entries and
//! the entries published to the object, so original methods remain available
//! without following jump chains.
//!
//! # Safety model
//!
//! Preparing and installing a shadow is unsafe because Rust cannot prove the
//! validity, layout, or lifetime of an interface pointer supplied by another
//! language. The detailed obligations are documented on
//! [`VtableShadow::copy_from`] and [`VtableShadow::install`]. In particular,
//! the installed guard must remain alive while its shadow vtable can be read,
//! and callers must quiesce method dispatch before dropping the guard.

#![cfg_attr(not(all(windows, target_arch = "x86_64")), allow(dead_code))]

#[cfg(not(all(windows, target_arch = "x86_64")))]
compile_error!("nexus-hook supports only 64-bit Windows targets");

mod error;
mod layout;
mod shadow;

pub mod dxgi;

pub use error::VtableError;
pub use layout::{
    AddRef, AddRefFn, ComInterfaceLayout, ComMethod, QueryInterface, QueryInterfaceFn, Release,
    ReleaseFn,
};
pub use shadow::{InstallState, InstalledVtable, VtableShadow};
