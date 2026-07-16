//! Process-wide dispatch and render-session ownership.

use core::ffi::c_void;
use core::fmt;
use core::num::NonZeroU64;
use core::ptr::NonNull;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use nexus_addon_api::ApiAssemblyError;
use nexus_host::ApiTableCatalog;
use nexus_imgui_compat::sys;
use thiserror::Error;

use crate::backend::AddonApiBackend;
use crate::shims;

static DISPATCHER: OnceLock<RwLock<DispatcherState>> = OnceLock::new();
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct DispatcherState {
    active: Option<ActiveSession>,
}

struct ActiveSession {
    token: RenderSessionToken,
    backend: Arc<dyn AddonApiBackend>,
}

/// Opaque identity of one installed render session.
///
/// The generation is supplied by the renderer while the token is allocated by
/// this crate. Both values must match before a lease is allowed to retire the
/// process-wide dispatcher.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct RenderSessionToken {
    generation: u64,
    token: NonZeroU64,
}

impl RenderSessionToken {
    /// Returns the renderer generation associated with this session.
    #[must_use]
    pub const fn generation(self) -> u64 {
        self.generation
    }

    /// Returns the non-zero process-local session token.
    #[must_use]
    pub const fn token(self) -> NonZeroU64 {
        self.token
    }
}

impl fmt::Debug for RenderSessionToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RenderSessionToken")
            .field("generation", &self.generation)
            .field("token", &self.token)
            .finish()
    }
}

/// Generation-scoped lease which retires its dispatcher entry on drop.
///
/// A stale lease cannot retire a newer installation: retirement compares the
/// complete [`RenderSessionToken`]. The backend is released after the
/// dispatcher lock is dropped so a backend destructor may safely re-enter an
/// API shim.
pub struct RenderSessionLease {
    token: RenderSessionToken,
    armed: bool,
}

impl RenderSessionLease {
    /// Returns this lease's immutable session identity.
    #[must_use]
    pub const fn token(&self) -> RenderSessionToken {
        self.token
    }

    /// Retires this session if it is still current.
    ///
    /// Calling this more than once is harmless. A poisoned dispatcher is
    /// treated as unavailable and therefore fails closed.
    pub fn retire_now(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        retire(self.token);
    }
}

impl fmt::Debug for RenderSessionLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RenderSessionLease")
            .field("token", &self.token)
            .field("armed", &self.armed)
            .finish()
    }
}

impl Drop for RenderSessionLease {
    fn drop(&mut self) {
        self.retire_now();
    }
}

/// A pinned API catalog and the lease which keeps its dispatcher live.
///
/// The catalog remains at a stable address for this value's lifetime. Native
/// add-ons must be unloaded before this installation is retired or dropped;
/// otherwise they could retain pointers into freed API tables.
pub struct InstalledAddonApi {
    lease: RenderSessionLease,
    catalog: Arc<ApiTableCatalog>,
}

impl InstalledAddonApi {
    /// Returns this installation's session identity.
    #[must_use]
    pub const fn token(&self) -> RenderSessionToken {
        self.lease.token()
    }

    /// Borrows the shared, internally pinned v1-v6 API catalog.
    #[must_use]
    pub fn catalog(&self) -> &ApiTableCatalog {
        self.catalog.as_ref()
    }

    /// Shares ownership of the catalog with the add-on lifecycle coordinator.
    ///
    /// The coordinator must be drained and dropped before this installation is
    /// retired so native add-ons cannot outlive the render-session resources.
    #[must_use]
    pub fn shared_catalog(&self) -> Arc<ApiTableCatalog> {
        Arc::clone(&self.catalog)
    }

    /// Retires dispatch for this session while retaining the catalog allocation.
    ///
    /// The runtime must unload every add-on using this catalog first.
    pub fn retire(&mut self) {
        self.lease.retire_now();
    }
}

impl fmt::Debug for InstalledAddonApi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InstalledAddonApi")
            .field("lease", &self.lease)
            .field("catalog", &"pinned v1-v6 API tables")
            .finish()
    }
}

impl Drop for InstalledAddonApi {
    fn drop(&mut self) {
        // Retire dispatch before the catalog fields are released.
        self.lease.retire_now();
    }
}

/// Failure to install one native add-on API render session.
#[derive(Debug, Error)]
pub enum RenderSessionInstallError {
    /// The linked cimgui ABI is not the exact Dear ImGui 1.80 ABI.
    #[error("the linked cimgui ABI is not Dear ImGui 1.80")]
    IncompatibleImGui,
    /// One of the complete v1-v6 API tables could not be assembled.
    #[error(transparent)]
    Assembly(#[from] ApiAssemblyError),
    /// The process-wide dispatcher lock was poisoned.
    #[error("the native add-on dispatcher is unavailable")]
    DispatcherUnavailable,
    /// Every non-zero process-local session token has been consumed.
    #[error("the native add-on session token space is exhausted")]
    TokenExhausted,
}

/// Installs a complete v1-v6 native add-on API for one render session.
///
/// The returned catalog contains the selected swap-chain pointer, the exact
/// Dear ImGui 1.80 context pointer, the linked cimgui `igMemAlloc`/`igMemFree`
/// function addresses, and a callable shim for every modern and legacy slot.
/// Replacing an installation atomically redirects all shims to the new backend.
///
/// # Threading and reentrancy
///
/// API calls may arrive from any thread and may re-enter another API call. A
/// call clones the active backend while holding a read lock and invokes it only
/// after releasing that lock. Backend panics are contained at the FFI boundary.
///
/// # Safety
///
/// The caller must guarantee all of the following:
///
/// - `selected_swap_chain` identifies the selected live native swap chain and
///   remains valid until every add-on using the catalog has been unloaded;
/// - `imgui_context` identifies the live Dear ImGui 1.80 context for that swap
///   chain and remains valid for the same interval;
/// - before replacing, retiring, or dropping an installation, the runtime has
///   unloaded every add-on that received a pointer from its catalog;
/// - native arguments passed through a shim satisfy the backend's documented
///   validation, copying, thread, and lifetime contract.
///
/// # Errors
///
/// Returns an error if the linked ImGui ABI is incompatible, a required API
/// table slot cannot be assembled, the dispatcher is poisoned, or session
/// tokens are exhausted.
pub unsafe fn install_render_session(
    generation: u64,
    selected_swap_chain: NonNull<c_void>,
    imgui_context: NonNull<sys::ImGuiContext>,
    backend: Arc<dyn AddonApiBackend>,
) -> Result<InstalledAddonApi, RenderSessionInstallError> {
    if !nexus_imgui_compat::has_expected_version() {
        return Err(RenderSessionInstallError::IncompatibleImGui);
    }

    let bindings = shims::bindings(
        selected_swap_chain.as_ptr(),
        imgui_context.cast().as_ptr(),
        sys::igMemAlloc as *const () as *mut c_void,
        sys::igMemFree as *const () as *mut c_void,
    );
    let catalog = Arc::new(bindings.catalog()?);
    let token = allocate_token(generation)?;
    let state = dispatcher();
    let retired = {
        let mut guard = state
            .write()
            .map_err(|_| RenderSessionInstallError::DispatcherUnavailable)?;
        guard.active.replace(ActiveSession { token, backend })
    };
    // A backend may implement a reentrant Drop; never release it under a lock.
    release_retired(retired);

    Ok(InstalledAddonApi {
        lease: RenderSessionLease { token, armed: true },
        catalog,
    })
}

fn dispatcher() -> &'static RwLock<DispatcherState> {
    DISPATCHER.get_or_init(|| RwLock::new(DispatcherState::default()))
}

fn allocate_token(generation: u64) -> Result<RenderSessionToken, RenderSessionInstallError> {
    let token = NEXT_TOKEN
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .map_err(|_| RenderSessionInstallError::TokenExhausted)?;
    let token = NonZeroU64::new(token).ok_or(RenderSessionInstallError::TokenExhausted)?;
    Ok(RenderSessionToken { generation, token })
}

fn retire(token: RenderSessionToken) {
    let state = dispatcher();
    let retired = {
        let Ok(mut guard) = state.write() else {
            return;
        };
        if guard.active.as_ref().map(|active| active.token) == Some(token) {
            guard.active.take()
        } else {
            None
        }
    };
    release_retired(retired);
}

fn release_retired(retired: Option<ActiveSession>) {
    let _ = contain_panic(|| drop(retired));
}

fn active_backend() -> Option<Arc<dyn AddonApiBackend>> {
    let guard = dispatcher().read().ok()?;
    guard
        .active
        .as_ref()
        .map(|active| Arc::clone(&active.backend))
}

/// Dispatches one FFI operation without allowing Rust unwinding to cross ABI.
pub(crate) fn dispatch<R: Copy>(
    fallback: R,
    operation: impl FnOnce(&dyn AddonApiBackend) -> R,
) -> R {
    let Some(backend) = active_backend() else {
        return fallback;
    };
    let result = contain_panic(|| operation(backend.as_ref()));
    let backend_released = contain_panic(|| drop(backend)).is_some();
    match result {
        Some(value) if backend_released => value,
        _ => fallback,
    }
}

fn contain_panic<R>(operation: impl FnOnce() -> R) -> Option<R> {
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(value) => Some(value),
        Err(payload) => {
            // A user-defined panic payload may itself panic from Drop. It must
            // not regain an unwinding path across the native ABI boundary.
            core::mem::forget(payload);
            None
        }
    }
}

#[cfg(test)]
pub(crate) fn clear_active_for_test() {
    let retired = dispatcher()
        .write()
        .map(|mut guard| guard.active.take())
        .unwrap_or(None);
    release_retired(retired);
}
