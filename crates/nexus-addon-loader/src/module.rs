use std::{
    fmt,
    mem::{align_of, forget, size_of},
    num::NonZeroUsize,
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    ptr::NonNull,
    sync::Arc,
    time::Duration,
};

use nexus_abi::{AddonApi, AddonDefinitionV1, AddonLoad, AddonUnload};
use nexus_core::CallbackGate;
use nexus_host::{
    DefinitionError, DefinitionLease, LiveAddonModule, MetadataLimits, ModuleAccessError,
    ModuleMemory, OwnedAddonDefinition, validate_and_copy_definition,
};
use thiserror::Error;

use crate::platform::{
    ADDON_DEFINITION_EXPORT, AbsoluteDllPath, LoaderPlatform, ModuleBounds, ModuleHandle,
    ModuleImage, PathPolicyError, PlatformError, PlatformOperation,
};

/// Closed validation categories for a rejected add-on definition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DefinitionIssue {
    /// The module could not provide a live definition lease.
    ModuleAccess,
    /// The signature was zero.
    ZeroSignature,
    /// The required load callback was absent.
    MissingLoadCallback,
    /// An unload callback was absent without a locked lifetime.
    MissingUnloadCallback,
    /// The API revision was unsupported.
    UnsupportedApiRevision,
    /// Required metadata was absent.
    NullMetadata,
    /// Metadata memory could not be read safely.
    MetadataMemory,
    /// A memory reader violated its requested bound.
    ReaderContract,
    /// Metadata had no terminator inside its configured bound.
    UnterminatedMetadata,
    /// Metadata was not valid UTF-8.
    InvalidUtf8,
}

impl From<DefinitionError> for DefinitionIssue {
    fn from(error: DefinitionError) -> Self {
        match error {
            DefinitionError::ModuleAccess(_) => Self::ModuleAccess,
            DefinitionError::ZeroSignature => Self::ZeroSignature,
            DefinitionError::MissingLoadCallback => Self::MissingLoadCallback,
            DefinitionError::MissingUnloadCallback => Self::MissingUnloadCallback,
            DefinitionError::ApiRevision(_) => Self::UnsupportedApiRevision,
            DefinitionError::NullMetadata { .. } => Self::NullMetadata,
            DefinitionError::MemoryRead { .. } => Self::MetadataMemory,
            DefinitionError::ReaderExceededBound { .. } => Self::ReaderContract,
            DefinitionError::UnterminatedMetadata { .. } => Self::UnterminatedMetadata,
            DefinitionError::InvalidUtf8 { .. } => Self::InvalidUtf8,
        }
    }
}

/// Boundary at which a Rust unwind was contained and redacted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanicBoundary {
    /// Platform module loading adapter.
    LoadLibrary,
    /// Platform image inspection adapter.
    InspectImage,
    /// Platform export resolver adapter.
    ResolveDefinitionExport,
    /// Exported definition function.
    DefinitionExport,
    /// Safe definition validation and copying.
    DefinitionValidation,
    /// Platform executable-address adapter.
    InspectCodeAddress,
    /// Add-on load callback.
    LoadCallback,
    /// Add-on unload callback.
    UnloadCallback,
    /// Caller-supplied host cleanup operation.
    HostCleanup,
    /// Platform module release adapter.
    FreeLibrary,
}

/// Failure while loading and inspecting a native add-on.
#[derive(Debug, Error)]
pub enum LoadError {
    /// The caller supplied a path outside the absolute-DLL policy.
    #[error(transparent)]
    Path(#[from] PathPolicyError),
    /// A platform adapter returned a closed failure category.
    #[error("platform operation {operation:?} failed: {source}")]
    Platform {
        /// Operation that failed.
        operation: PlatformOperation,
        /// Redacted platform failure.
        #[source]
        source: PlatformError,
    },
    /// A Rust unwind was caught without retaining its payload.
    #[error("Rust unwind was contained at {0:?}")]
    RustPanic(PanicBoundary),
    /// The definition export returned null.
    #[error("add-on definition export returned null")]
    NullDefinition,
    /// The definition pointer did not satisfy its ABI alignment.
    #[error("add-on definition pointer is misaligned")]
    MisalignedDefinition,
    /// The complete definition did not lie inside the mapped image.
    #[error("add-on definition lies outside the mapped image")]
    DefinitionOutsideImage,
    /// The complete definition was not readable.
    #[error("add-on definition memory is not readable")]
    DefinitionUnreadable,
    /// Safe host validation rejected the definition.
    #[error("add-on definition was rejected: {0:?}")]
    DefinitionRejected(DefinitionIssue),
    /// The definition export did not lie inside the mapped image.
    #[error("definition export lies outside the mapped image")]
    DefinitionExportOutsideImage,
    /// The definition export was not executable.
    #[error("definition export is not executable")]
    DefinitionExportNotExecutable,
    /// The load callback did not lie inside the mapped image.
    #[error("load callback lies outside the mapped image")]
    LoadCallbackOutsideImage,
    /// The load callback was not executable.
    #[error("load callback is not executable")]
    LoadCallbackNotExecutable,
    /// The unload callback did not lie inside the mapped image.
    #[error("unload callback lies outside the mapped image")]
    UnloadCallbackOutsideImage,
    /// The unload callback was not executable.
    #[error("unload callback is not executable")]
    UnloadCallbackNotExecutable,
}

/// Explicit lifecycle state of one loaded native module.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModuleState {
    /// Definition and entrypoints are validated; native load was not invoked.
    Inspected,
    /// Native load returned and normal callback admission is open.
    Active,
    /// Native load unwound or aborted through a catchable Rust path.
    ActivationFailed,
    /// New loader-owned callbacks are rejected.
    ShutdownRequested,
    /// Every loader-owned callback guard has left.
    CallbacksQuiesced,
    /// The optional unload callback was attempted exactly once.
    UnloadCallbackComplete,
    /// The caller confirmed successful host cleanup.
    HostCleanupComplete,
    /// A release adapter unwound, so the native reference outcome is unknown.
    ReleaseUncertain,
    /// The platform module reference was released.
    Released,
}

/// State-changing module operation used in closed diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModuleOperation {
    /// Invoke the native load callback.
    Activate,
    /// Close callback admission.
    RequestShutdown,
    /// Wait for admitted callbacks.
    DrainCallbacks,
    /// Invoke the optional native unload callback.
    InvokeUnload,
    /// Run caller-supplied host cleanup.
    CompleteHostCleanup,
    /// Release the platform module reference.
    Release,
}

/// Failure of an explicit module lifecycle operation.
#[derive(Debug, Error)]
pub enum ModuleError {
    /// The operation is not valid in the current closed state.
    #[error("operation {operation:?} is invalid while module is {state:?}")]
    InvalidTransition {
        /// Attempted operation.
        operation: ModuleOperation,
        /// Current state.
        state: ModuleState,
    },
    /// Callback admission had already closed.
    #[error("module callback admission is closed")]
    CallbackAdmissionClosed,
    /// Admitted callbacks did not leave before the deadline.
    #[error("module callbacks did not quiesce before the deadline")]
    DrainTimedOut,
    /// Release was attempted while a callback guard was still active.
    #[error("module callback drain is incomplete")]
    CallbackDrainIncomplete,
    /// A Rust unwind was caught without retaining its payload.
    #[error("Rust unwind was contained at {0:?}")]
    RustPanic(PanicBoundary),
    /// The caller-supplied cleanup operation reported failure.
    #[error("host cleanup did not complete")]
    HostCleanupFailed,
    /// The platform could not release the module.
    #[error("platform operation {operation:?} failed: {source}")]
    Platform {
        /// Operation that failed.
        operation: PlatformOperation,
        /// Redacted platform failure.
        #[source]
        source: PlatformError,
    },
}

/// Reusable loader backed by an injected platform implementation.
pub struct AddonLoader<P: LoaderPlatform> {
    platform: Arc<P>,
}

impl<P: LoaderPlatform> Clone for AddonLoader<P> {
    fn clone(&self) -> Self {
        Self {
            platform: Arc::clone(&self.platform),
        }
    }
}

impl<P: LoaderPlatform> AddonLoader<P> {
    /// Creates a loader that owns its platform adapter.
    #[must_use]
    pub fn new(platform: P) -> Self {
        Self {
            platform: Arc::new(platform),
        }
    }

    /// Creates a loader around a shared platform adapter.
    #[must_use]
    pub const fn from_shared(platform: Arc<P>) -> Self {
        Self { platform }
    }

    /// Loads, inspects, validates, and deep-copies one add-on definition.
    ///
    /// The module remains live in the returned value. Any failure after a
    /// successful platform load releases the provisional module reference.
    ///
    /// # Safety
    ///
    /// Loading a DLL runs native loader code and the definition export runs
    /// arbitrary native code. The caller must trust the selected binary and
    /// ensure process-wide loader constraints permit running it. Rust unwind
    /// containment does not catch SEH faults, access violations, aborts, or
    /// process termination.
    pub unsafe fn load(
        &self,
        path: impl AsRef<Path>,
        limits: MetadataLimits,
    ) -> Result<AddonModule<P>, LoadError> {
        let path = AbsoluteDllPath::new(path)?;
        // SAFETY: this method exposes the same native-code trust obligation as
        // `load_absolute`; path parsing adds no further unsafe precondition.
        unsafe { self.load_absolute(&path, limits) }
    }

    /// Loads from a prevalidated absolute DLL path.
    ///
    /// # Safety
    ///
    /// The same native-code trust requirements as [`Self::load`] apply.
    pub unsafe fn load_absolute(
        &self,
        path: &AbsoluteDllPath,
        limits: MetadataLimits,
    ) -> Result<AddonModule<P>, LoadError> {
        let handle = catch_boundary(PanicBoundary::LoadLibrary, || {
            // SAFETY: this public method transfers the native-code trust and
            // eventual exactly-once release obligations to the returned owner.
            unsafe { self.platform.load_library(path) }
        })
        .map_err(LoadError::RustPanic)?
        .map_err(|source| platform_load_error(PlatformOperation::LoadLibrary, source))?;
        let mut provisional = ProvisionalModule::new(Arc::clone(&self.platform), handle);

        let image = catch_boundary(PanicBoundary::InspectImage, || {
            // SAFETY: `handle` was just returned by this platform and remains
            // owned by `provisional` throughout inspection.
            unsafe { self.platform.module_image(handle) }
        })
        .map_err(LoadError::RustPanic)?
        .map_err(|source| platform_load_error(PlatformOperation::InspectImage, source))?;

        let export = catch_boundary(PanicBoundary::ResolveDefinitionExport, || {
            // SAFETY: the provisional owner keeps `handle` live and the export
            // constant carries the exact ABI requested from the adapter.
            unsafe {
                self.platform
                    .resolve_definition_export(handle, ADDON_DEFINITION_EXPORT)
            }
        })
        .map_err(LoadError::RustPanic)?
        .map_err(|source| {
            platform_load_error(PlatformOperation::ResolveDefinitionExport, source)
        })?;
        validate_code_address(
            &*self.platform,
            handle,
            image.bounds(),
            export as usize,
            CodeAddress::DefinitionExport,
        )?;

        let definition_pointer = catch_boundary(PanicBoundary::DefinitionExport, || {
            // SAFETY: the platform contract resolves this exact export with the
            // `GetAddonDefinitionV1` ABI. Native faults are not Rust unwinds.
            unsafe { export() }
        })
        .map_err(LoadError::RustPanic)?;
        let definition = catch_boundary(PanicBoundary::DefinitionValidation, || {
            copy_definition(&image, definition_pointer)
        })
        .map_err(LoadError::RustPanic)??;

        let view = DefinitionView {
            definition: &definition,
            image: &image,
        };
        let owned_definition = catch_boundary(PanicBoundary::DefinitionValidation, || {
            validate_and_copy_definition(&view, limits)
        })
        .map_err(LoadError::RustPanic)?
        .map_err(|error| LoadError::DefinitionRejected(error.into()))?;

        let Some(load_callback) = definition.load else {
            return Err(LoadError::DefinitionRejected(
                DefinitionIssue::MissingLoadCallback,
            ));
        };
        validate_code_address(
            &*self.platform,
            handle,
            image.bounds(),
            load_callback as usize,
            CodeAddress::LoadCallback,
        )?;
        if let Some(unload_callback) = definition.unload {
            validate_code_address(
                &*self.platform,
                handle,
                image.bounds(),
                unload_callback as usize,
                CodeAddress::UnloadCallback,
            )?;
        }

        provisional.disarm();
        Ok(AddonModule {
            platform: Arc::clone(&self.platform),
            handle: Some(handle),
            image,
            definition,
            owned_definition,
            load_callback,
            unload_callback: definition.unload,
            gate: Arc::new(CallbackGate::open()),
            state: ModuleState::Inspected,
            release_on_drop: true,
        })
    }
}

/// One live add-on module with explicit activation and release state.
pub struct AddonModule<P: LoaderPlatform> {
    platform: Arc<P>,
    handle: Option<ModuleHandle>,
    image: ModuleImage<P::Memory>,
    definition: AddonDefinitionV1,
    owned_definition: OwnedAddonDefinition,
    load_callback: AddonLoad,
    unload_callback: Option<AddonUnload>,
    gate: Arc<CallbackGate>,
    state: ModuleState,
    release_on_drop: bool,
}

impl<P: LoaderPlatform> AddonModule<P> {
    /// Returns the current closed lifecycle state.
    #[must_use]
    pub const fn state(&self) -> ModuleState {
        self.state
    }

    /// Returns deep-copied metadata that remains valid after release.
    #[must_use]
    pub const fn owned_definition(&self) -> &OwnedAddonDefinition {
        &self.owned_definition
    }

    /// Returns the mapped image size without exposing its address.
    #[must_use]
    pub const fn image_size(&self) -> usize {
        self.image.bounds().image_size()
    }

    /// Returns the validated mapped-image base without changing its reference count.
    #[must_use]
    pub const fn image_base(&self) -> NonZeroUsize {
        self.image.bounds().base()
    }

    /// Clones the gate that registration wrappers use for callback admission.
    #[must_use]
    pub fn callback_gate(&self) -> Arc<CallbackGate> {
        Arc::clone(&self.gate)
    }

    /// Invokes the validated native load callback without holding a loader lock.
    ///
    /// On a caught Rust unwind the gate closes and the state becomes
    /// [`ModuleState::ActivationFailed`]. The caller must still quiesce,
    /// complete host cleanup, and release explicitly because the callback may
    /// have registered resources before it failed.
    ///
    /// # Safety
    ///
    /// `api` must point to the complete API layout requested by the add-on and
    /// remain valid for every use permitted by that ABI. The native callback is
    /// trusted code; SEH faults, aborts, and process termination are not caught.
    pub unsafe fn activate(&mut self, api: NonNull<AddonApi>) -> Result<(), ModuleError> {
        self.ensure_state(ModuleOperation::Activate, ModuleState::Inspected)?;
        let Some(guard) = self.gate.try_enter() else {
            return Err(ModuleError::CallbackAdmissionClosed);
        };
        let callback = self.load_callback;
        let outcome = catch_boundary(PanicBoundary::LoadCallback, || {
            // SAFETY: the caller supplies the API pointer under this method's
            // documented ABI precondition; the callback was validated as live.
            unsafe { callback(api.as_ptr()) };
        });
        drop(guard);

        match outcome {
            Ok(()) => {
                self.state = ModuleState::Active;
                Ok(())
            }
            Err(boundary) => {
                self.gate.close();
                self.state = ModuleState::ActivationFailed;
                Err(ModuleError::RustPanic(boundary))
            }
        }
    }

    /// Permanently closes loader-owned callback admission.
    pub fn request_shutdown(&mut self) -> Result<(), ModuleError> {
        match self.state {
            ModuleState::Active | ModuleState::ActivationFailed => {
                self.gate.close();
                self.state = ModuleState::ShutdownRequested;
                Ok(())
            }
            state => Err(ModuleError::InvalidTransition {
                operation: ModuleOperation::RequestShutdown,
                state,
            }),
        }
    }

    /// Waits a bounded duration for loader-owned callback guards to leave.
    pub fn wait_for_callbacks(&mut self, timeout: Duration) -> Result<(), ModuleError> {
        self.ensure_state(
            ModuleOperation::DrainCallbacks,
            ModuleState::ShutdownRequested,
        )?;
        if !self.gate.wait_for_drain(timeout) {
            return Err(ModuleError::DrainTimedOut);
        }
        self.state = ModuleState::CallbacksQuiesced;
        Ok(())
    }

    /// Attempts the optional unload callback exactly once, without loader locks.
    ///
    /// The state advances even when a Rust unwind is caught, preventing a
    /// potentially side-effecting unload callback from being invoked twice.
    ///
    /// # Safety
    ///
    /// The caller must have closed and quiesced every external host callback
    /// gate associated with this module. Native faults and process termination
    /// are not caught.
    pub unsafe fn invoke_unload(&mut self) -> Result<(), ModuleError> {
        self.ensure_state(
            ModuleOperation::InvokeUnload,
            ModuleState::CallbacksQuiesced,
        )?;
        let outcome = self.unload_callback.map_or(Ok(()), |callback| {
            catch_boundary(PanicBoundary::UnloadCallback, || {
                // SAFETY: the callback address was validated while the module
                // remained live and the caller established callback quiescence.
                unsafe { callback() };
            })
        });
        self.state = ModuleState::UnloadCallbackComplete;
        outcome.map_err(ModuleError::RustPanic)
    }

    /// Runs host cleanup and records completion only after a successful return.
    ///
    /// The cleanup error and panic payload are deliberately discarded so
    /// diagnostics cannot include caller-controlled data. A failed or panicked
    /// cleanup may be retried; host cleanup implementations therefore need to
    /// be phase-idempotent.
    pub fn complete_host_cleanup<F, E>(&mut self, cleanup: F) -> Result<(), ModuleError>
    where
        F: FnOnce() -> Result<(), E>,
    {
        self.ensure_state(
            ModuleOperation::CompleteHostCleanup,
            ModuleState::UnloadCallbackComplete,
        )?;
        let outcome =
            catch_boundary(PanicBoundary::HostCleanup, cleanup).map_err(ModuleError::RustPanic)?;
        if let Err(error) = outcome {
            // Cleanup errors are caller-controlled. Their destructors are as
            // untrusted as the cleanup closure, so contain a destructor panic
            // and forget its payload before returning the closed error class.
            let _ = contain_unwind(|| drop(error));
            return Err(ModuleError::HostCleanupFailed);
        }
        self.state = ModuleState::HostCleanupComplete;
        Ok(())
    }

    /// Releases an unactivated or completely cleaned module.
    ///
    /// On an ordinary platform error, ownership is returned in
    /// [`ReleaseFailure`] for retry. A platform unwind makes the release result
    /// unknowable, so the raw token is pinned and cannot be retried.
    pub fn release(mut self) -> Result<(), ReleaseFailure<P>> {
        if !matches!(
            self.state,
            ModuleState::Inspected | ModuleState::HostCleanupComplete
        ) {
            let error = ModuleError::InvalidTransition {
                operation: ModuleOperation::Release,
                state: self.state,
            };
            return Err(ReleaseFailure::new(self, error));
        }
        if self.state == ModuleState::Inspected {
            self.gate.close();
        }
        if self.gate.in_flight() != 0 {
            return Err(ReleaseFailure::new(
                self,
                ModuleError::CallbackDrainIncomplete,
            ));
        }
        let Some(handle) = self.handle else {
            let error = ModuleError::InvalidTransition {
                operation: ModuleOperation::Release,
                state: self.state,
            };
            return Err(ReleaseFailure::new(self, error));
        };

        let outcome = catch_boundary(PanicBoundary::FreeLibrary, || {
            // SAFETY: the state check above proves either native activation
            // never ran or callback drain and host cleanup both completed.
            unsafe { self.platform.free_library(handle) }
        });
        match outcome {
            Ok(Ok(())) => {
                self.handle = None;
                self.state = ModuleState::Released;
                Ok(())
            }
            Ok(Err(source)) => Err(ReleaseFailure::new(
                self,
                ModuleError::Platform {
                    operation: PlatformOperation::FreeLibrary,
                    source,
                },
            )),
            Err(boundary) => {
                // The adapter may have unwound before or after decrementing the
                // native reference. Discard the token and pin the outcome so a
                // retry cannot double-release an already unmapped module.
                self.handle = None;
                self.state = ModuleState::ReleaseUncertain;
                Err(ReleaseFailure::new(self, ModuleError::RustPanic(boundary)))
            }
        }
    }

    fn ensure_state(
        &self,
        operation: ModuleOperation,
        required: ModuleState,
    ) -> Result<(), ModuleError> {
        if self.state == required {
            Ok(())
        } else {
            Err(ModuleError::InvalidTransition {
                operation,
                state: self.state,
            })
        }
    }
}

impl<P: LoaderPlatform> LiveAddonModule for AddonModule<P> {
    fn definition(&self) -> Result<DefinitionLease<'_>, ModuleAccessError> {
        if self.handle.is_none() || self.state == ModuleState::Released {
            return Err(ModuleAccessError::new("native add-on module is not live"));
        }
        Ok(DefinitionLease::new(&self.definition, &self.image))
    }
}

impl<P: LoaderPlatform> Drop for AddonModule<P> {
    fn drop(&mut self) {
        if self.state == ModuleState::Inspected {
            self.gate.close();
        }
        let safe_to_release = self.release_on_drop
            && self.gate.in_flight() == 0
            && matches!(
                self.state,
                ModuleState::Inspected | ModuleState::HostCleanupComplete
            );
        let Some(handle) = self.handle.take() else {
            return;
        };
        if !safe_to_release {
            // Safety beats reclamation: active modules remain pinned rather
            // than risking callbacks into unmapped code.
            return;
        }
        let _ = contain_unwind(|| {
            // SAFETY: `safe_to_release` proves activation never ran or the
            // explicit callback-drain and host-cleanup sequence completed.
            unsafe { self.platform.free_library(handle) }
        });
    }
}

/// Failed or uncertain release with module state retained for containment.
///
/// Dropping this value pins the retained token; it never performs an implicit
/// second platform release attempt.
pub struct ReleaseFailure<P: LoaderPlatform> {
    module: Box<AddonModule<P>>,
    error: ModuleError,
}

impl<P: LoaderPlatform> ReleaseFailure<P> {
    fn new(mut module: AddonModule<P>, error: ModuleError) -> Self {
        // A reported release failure must never trigger an implicit second
        // platform attempt when the error value is dropped.
        module.release_on_drop = false;
        Self {
            module: Box::new(module),
            error,
        }
    }

    /// Borrows the closed release error.
    #[must_use]
    pub const fn error(&self) -> &ModuleError {
        &self.error
    }

    /// Returns whether the platform guaranteed that an explicit retry is safe.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        self.module.handle.is_some()
            && matches!(
                self.module.state,
                ModuleState::Inspected | ModuleState::HostCleanupComplete
            )
    }

    /// Returns the retained state and closed error for retry or containment.
    #[must_use]
    pub fn into_parts(self) -> (AddonModule<P>, ModuleError) {
        (*self.module, self.error)
    }
}

impl<P: LoaderPlatform> fmt::Debug for ReleaseFailure<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReleaseFailure")
            .field("state", &self.module.state)
            .field("error", &self.error)
            .finish()
    }
}

impl<P: LoaderPlatform> fmt::Display for ReleaseFailure<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)
    }
}

impl<P: LoaderPlatform> std::error::Error for ReleaseFailure<P> {}

struct ProvisionalModule<P: LoaderPlatform> {
    platform: Arc<P>,
    handle: Option<ModuleHandle>,
}

impl<P: LoaderPlatform> ProvisionalModule<P> {
    fn new(platform: Arc<P>, handle: ModuleHandle) -> Self {
        Self {
            platform,
            handle: Some(handle),
        }
    }

    fn disarm(&mut self) {
        self.handle = None;
    }
}

impl<P: LoaderPlatform> Drop for ProvisionalModule<P> {
    fn drop(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        let _ = contain_unwind(|| {
            // SAFETY: this provisional token never escaped and no load or
            // unload entrypoint was invoked through the assembly layer.
            unsafe { self.platform.free_library(handle) }
        });
    }
}

struct DefinitionView<'module, M> {
    definition: &'module AddonDefinitionV1,
    image: &'module ModuleImage<M>,
}

impl<M: ModuleMemory> LiveAddonModule for DefinitionView<'_, M> {
    fn definition(&self) -> Result<DefinitionLease<'_>, ModuleAccessError> {
        Ok(DefinitionLease::new(self.definition, self.image))
    }
}

#[derive(Clone, Copy)]
enum CodeAddress {
    DefinitionExport,
    LoadCallback,
    UnloadCallback,
}

fn validate_code_address<P: LoaderPlatform>(
    platform: &P,
    handle: ModuleHandle,
    bounds: ModuleBounds,
    raw_address: usize,
    kind: CodeAddress,
) -> Result<(), LoadError> {
    let Some(address) = NonZeroUsize::new(raw_address) else {
        return Err(code_outside_error(kind));
    };
    if !bounds.contains_address(address) {
        return Err(code_outside_error(kind));
    }
    let executable = catch_boundary(PanicBoundary::InspectCodeAddress, || {
        // SAFETY: the provisional owner keeps `handle` live; `address` is used
        // only as a numeric candidate until the adapter validates it.
        unsafe { platform.is_executable_address(handle, address) }
    })
    .map_err(LoadError::RustPanic)?
    .map_err(|source| platform_load_error(PlatformOperation::InspectCodeAddress, source))?;
    if !executable {
        return Err(code_not_executable_error(kind));
    }
    Ok(())
}

fn code_outside_error(kind: CodeAddress) -> LoadError {
    match kind {
        CodeAddress::DefinitionExport => LoadError::DefinitionExportOutsideImage,
        CodeAddress::LoadCallback => LoadError::LoadCallbackOutsideImage,
        CodeAddress::UnloadCallback => LoadError::UnloadCallbackOutsideImage,
    }
}

fn code_not_executable_error(kind: CodeAddress) -> LoadError {
    match kind {
        CodeAddress::DefinitionExport => LoadError::DefinitionExportNotExecutable,
        CodeAddress::LoadCallback => LoadError::LoadCallbackNotExecutable,
        CodeAddress::UnloadCallback => LoadError::UnloadCallbackNotExecutable,
    }
}

fn copy_definition<M: ModuleMemory>(
    image: &ModuleImage<M>,
    pointer: *mut AddonDefinitionV1,
) -> Result<AddonDefinitionV1, LoadError> {
    let Some(pointer) = NonNull::new(pointer) else {
        return Err(LoadError::NullDefinition);
    };
    let Some(address) = NonZeroUsize::new(pointer.as_ptr() as usize) else {
        return Err(LoadError::NullDefinition);
    };
    if address.get() % align_of::<AddonDefinitionV1>() != 0 {
        return Err(LoadError::MisalignedDefinition);
    }
    if !image
        .bounds()
        .contains_range(address, size_of::<AddonDefinitionV1>())
    {
        return Err(LoadError::DefinitionOutsideImage);
    }
    let readable = image
        .read_bounded(pointer.cast(), size_of::<AddonDefinitionV1>())
        .map_err(|_error| LoadError::DefinitionUnreadable)?;
    if readable.len() < size_of::<AddonDefinitionV1>() {
        return Err(LoadError::DefinitionUnreadable);
    }

    // SAFETY: the non-null pointer has the required alignment, the complete
    // object lies within the live image, and the platform reader proved the
    // full range readable. The ABI type is `Copy`, so no ownership is moved.
    Ok(unsafe { pointer.as_ptr().read() })
}

fn platform_load_error(operation: PlatformOperation, source: PlatformError) -> LoadError {
    LoadError::Platform { operation, source }
}

fn catch_boundary<T>(
    boundary: PanicBoundary,
    operation: impl FnOnce() -> T,
) -> Result<T, PanicBoundary> {
    contain_unwind(operation).map_err(|()| boundary)
}

fn contain_unwind<T>(operation: impl FnOnce() -> T) -> Result<T, ()> {
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(value) => Ok(value),
        Err(payload) => {
            // A panic payload is arbitrary caller-controlled Rust data. Its
            // destructor may itself panic, so it must never run after capture.
            forget(payload);
            Err(())
        }
    }
}
