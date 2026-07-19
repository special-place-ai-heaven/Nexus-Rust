use core::ffi::{c_char, c_void};
use core::fmt;
use core::num::NonZeroUsize;
use std::sync::Arc;

use nexus_abi::{RenderCallback, RenderPhase as AbiRenderPhase};
use nexus_core::OwnerToken;
use nexus_ui_host::{
    AlertKind, CheckedVisibilityAccess, ContextRegistrationOutcome, NativeRenderCallback,
    NativeVisibilityPointer, NotificationOutcome, NotifyOutcome, OwnerHandle, QA_GENERIC_KEY,
    RegisterRenderOutcome, RenderPhase as UiRenderPhase, ShortcutMutation, UiCallback, UiHost,
    VisibilityTarget,
};

use crate::{
    BackendFailure, BackendOperationError, CallBoundaryError, NativeCallBoundary, NativeText,
};

/// Caller-attributed adapter for native add-on UI registrations and commands.
pub struct UiApi {
    boundary: Arc<NativeCallBoundary>,
    host: Arc<UiHost>,
}

struct BoundaryVisibilityAccess {
    boundary: Arc<NativeCallBoundary>,
    owner: OwnerToken,
}

// SAFETY: every operation validates the address against the bound live owner
// before using the checked process-memory boundary. Known foreign addon ranges
// are rejected; unindexed heap/TLS storage relies on the unsafe registration
// proof. This adapter never calls UI registration, deregistration, or cleanup,
// so it cannot synchronously wait for the entry invoking it.
unsafe impl CheckedVisibilityAccess for BoundaryVisibilityAccess {
    fn read_visible(&self, address: NonZeroUsize) -> Option<bool> {
        self.boundary
            .validate_retained_address(self.owner, address)
            .ok()?;
        self.boundary
            .snapshot_u8(address.get() as *const u8)
            .ok()
            .map(|value| value != 0)
    }

    fn write_hidden(&self, address: NonZeroUsize) -> bool {
        if self
            .boundary
            .validate_retained_address(self.owner, address)
            .is_err()
        {
            return false;
        }
        // SAFETY: `NativeVisibilityPointer` invokes this adapter only while the
        // exact owner-generation activity gate is held. Escape cleanup removes
        // and drains the retained cell before the add-on module may unload;
        // `write_u8` separately validates the page on every operation.
        unsafe { self.boundary.write_u8(address.get() as *mut u8, 0).is_ok() }
    }
}

impl UiApi {
    /// Creates a UI adapter around the process UI host.
    #[must_use]
    pub fn new(boundary: Arc<NativeCallBoundary>, host: Arc<UiHost>) -> Self {
        Self { boundary, host }
    }

    /// Registers a non-null render callback for one legacy render phase.
    ///
    /// A null callback remains the legacy no-op and does not attempt caller
    /// attribution or phase validation.
    pub fn renderer_register(
        &self,
        phase: AbiRenderPhase,
        callback: Option<RenderCallback>,
    ) -> Result<Option<RegisterRenderOutcome>, BackendOperationError> {
        let Some(callback) = callback else {
            return Ok(None);
        };
        let callback = self.native_callback(callback)?;
        let phase = self.service_result(UiRenderPhase::try_from(phase))?;
        self.service_result(self.host.render().register(phase, callback))
            .map(Some)
    }

    /// Deregisters a non-null render callback from every render phase.
    ///
    /// A null callback remains the legacy no-op.
    pub fn renderer_deregister(
        &self,
        callback: Option<RenderCallback>,
    ) -> Result<usize, BackendOperationError> {
        let Some(callback) = callback else {
            return Ok(0);
        };
        let callback = self.native_callback(callback)?;
        Ok(self.host.render().deregister(&callback))
    }

    /// Copies and queues one informational alert for the current add-on.
    pub fn ui_send_alert(
        &self,
        message: *const c_char,
    ) -> Result<NotifyOutcome, BackendOperationError> {
        let owner = self.owner_handle()?;
        let message = self.boundary.snapshot_message(message)?;
        self.service_result(
            self.host
                .alerts()
                .notify(&owner, AlertKind::Info, message.as_str()),
        )
    }

    /// Registers one checked, owner-scoped legacy visibility cell.
    ///
    /// # Safety
    ///
    /// If registration succeeds, `state` must remain the same live, writable
    /// one-byte allocation until this owner deregisters the returned window
    /// name and that call returns, or owner cleanup finishes draining the
    /// registration. The add-on must synchronize every other access to that
    /// byte so none conflicts with host reads or writes. Addresses mapped to a
    /// different add-on image are rejected. Heap and TLS addresses cannot be
    /// independently attributed and therefore rely on this caller proof.
    pub unsafe fn ui_register_close_on_escape(
        &self,
        identifier: *const c_char,
        state: *mut u8,
    ) -> Result<(), BackendOperationError> {
        let owner_token = self.boundary.resolve_owner(None)?;
        let owner = self.service_result(self.host.owner(owner_token.into()))?;
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        let address = NonZeroUsize::new(state as usize).ok_or_else(|| {
            self.boundary
                .failures()
                .record(BackendFailure::NativeMemory);
            BackendOperationError::Boundary(CallBoundaryError::NativeMemory)
        })?;
        self.boundary
            .validate_retained_address(owner_token, address)?;
        self.boundary.snapshot_u8(state)?;
        let access: Arc<dyn CheckedVisibilityAccess> = Arc::new(BoundaryVisibilityAccess {
            boundary: Arc::clone(&self.boundary),
            owner: owner_token,
        });
        let target = VisibilityTarget::native(unsafe {
            // SAFETY: this method's contract forwards the allocation lifetime
            // and synchronization obligations. The adapter binds every access
            // to `owner_token`, and the registry drains it before removal.
            NativeVisibilityPointer::checked(owner.clone(), address, access)
        });
        self.service_result(self.host.escape_closing().register(
            &owner,
            identifier.as_str(),
            target,
        ))
        .map(|_| ())
    }

    /// Deregisters one close-on-Escape window name after caller attribution.
    pub fn ui_deregister_close_on_escape(
        &self,
        identifier: *const c_char,
    ) -> Result<bool, BackendOperationError> {
        let owner = self.owner_handle()?;
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        Ok(self
            .host
            .escape_closing()
            .deregister_window_for_owner(&owner, identifier.as_str()))
    }

    /// Adds one full QuickAccess shortcut.
    ///
    /// The input-bind and tooltip pointers are the two verified legacy nullable
    /// fields; null maps to an empty string. All non-null strings are copied
    /// before the UI host sees them.
    pub fn quick_access_add(
        &self,
        identifier: *const c_char,
        texture: *const c_char,
        hover_texture: *const c_char,
        input_bind: *const c_char,
        tooltip: *const c_char,
    ) -> Result<ShortcutMutation, BackendOperationError> {
        let owner = self.owner_handle()?;
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        let texture = self.boundary.snapshot_identifier(texture)?;
        let hover_texture = self.boundary.snapshot_identifier(hover_texture)?;
        let input_bind = self.snapshot_optional_identifier(input_bind)?;
        let tooltip = self.snapshot_optional_message(tooltip)?;

        self.service_result(self.host.quick_access().add_shortcut(
            &owner,
            identifier.as_str(),
            texture.as_str(),
            hover_texture.as_str(),
            input_bind.as_ref().map_or("", NativeText::as_str),
            tooltip.as_ref().map_or("", NativeText::as_str),
        ))
    }

    /// Removes one QuickAccess shortcut after caller attribution.
    pub fn quick_access_remove(
        &self,
        identifier: *const c_char,
    ) -> Result<bool, BackendOperationError> {
        let owner = self.owner_handle()?;
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        self.service_result(
            self.host
                .quick_access()
                .remove_shortcut_for_owner(&owner, identifier.as_str()),
        )
    }

    /// Pushes the legacy `Generic` notification for one shortcut.
    pub fn quick_access_notify(
        &self,
        identifier: *const c_char,
    ) -> Result<NotificationOutcome, BackendOperationError> {
        let owner = self.owner_handle()?;
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        self.service_result(self.host.quick_access().push_notification(
            &owner,
            identifier.as_str(),
            QA_GENERIC_KEY,
        ))
    }

    /// Adds a callback to the built-in QuickAccess menu shortcut.
    ///
    /// A null callback remains the legacy no-op.
    pub fn quick_access_add_simple(
        &self,
        identifier: *const c_char,
        callback: Option<RenderCallback>,
    ) -> Result<Option<ContextRegistrationOutcome>, BackendOperationError> {
        let Some(callback) = callback else {
            return Ok(None);
        };
        let callback = self.native_callback(callback)?;
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        self.service_result(
            self.host
                .quick_access()
                .add_simple_shortcut(identifier.as_str(), callback),
        )
        .map(Some)
    }

    /// Adds a callback to a targeted QuickAccess context menu.
    ///
    /// A null callback remains the legacy no-op.
    pub fn quick_access_add_context_menu(
        &self,
        identifier: *const c_char,
        target: *const c_char,
        callback: Option<RenderCallback>,
    ) -> Result<Option<ContextRegistrationOutcome>, BackendOperationError> {
        let Some(callback) = callback else {
            return Ok(None);
        };
        let callback = self.native_callback(callback)?;
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        let target = self.boundary.snapshot_identifier(target)?;
        self.service_result(self.host.quick_access().add_context_item(
            identifier.as_str(),
            target.as_str(),
            callback,
        ))
        .map(Some)
    }

    /// Removes a context-menu item from every QuickAccess location.
    pub fn quick_access_remove_context_menu(
        &self,
        identifier: *const c_char,
    ) -> Result<usize, BackendOperationError> {
        let owner = self.owner_handle()?;
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        self.service_result(
            self.host
                .quick_access()
                .remove_context_item_for_owner(&owner, identifier.as_str()),
        )
    }

    fn owner_handle(&self) -> Result<OwnerHandle, BackendOperationError> {
        let owner = self.boundary.resolve_owner(None)?;
        self.service_result(self.host.owner(owner.into()))
    }

    fn native_callback(
        &self,
        callback: RenderCallback,
    ) -> Result<UiCallback, BackendOperationError> {
        let owner = self
            .boundary
            .resolve_owner_for_registered_address((callback as *const ()).cast::<c_void>())?;
        let owner = self.service_result(self.host.owner(owner.into()))?;
        let callback = unsafe {
            // SAFETY: caller attribution proves that the callback address
            // belongs to the exact live owner generation. Composite teardown
            // removes and drains the UiHost registration before module unload.
            NativeRenderCallback::new(owner, callback)
        };
        Ok(UiCallback::native(callback))
    }

    fn snapshot_optional_identifier(
        &self,
        value: *const c_char,
    ) -> Result<Option<NativeText>, BackendOperationError> {
        if value.is_null() {
            Ok(None)
        } else {
            self.boundary
                .snapshot_identifier(value)
                .map(Some)
                .map_err(Into::into)
        }
    }

    fn snapshot_optional_message(
        &self,
        value: *const c_char,
    ) -> Result<Option<NativeText>, BackendOperationError> {
        if value.is_null() {
            Ok(None)
        } else {
            self.boundary
                .snapshot_message(value)
                .map(Some)
                .map_err(Into::into)
        }
    }

    fn service_result<T, E>(&self, result: Result<T, E>) -> Result<T, BackendOperationError> {
        result.map_err(|_| {
            self.boundary
                .failures()
                .record(BackendFailure::ServiceRejected);
            BackendOperationError::ServiceRejected
        })
    }
}

impl fmt::Debug for UiApi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UiApi")
            .field("boundary", &self.boundary)
            .finish_non_exhaustive()
    }
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use core::num::NonZeroUsize;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::ffi::CString;
    use std::sync::Arc;

    use nexus_abi::RenderPhase as AbiRenderPhase;
    use nexus_addon_ffi::{AddonCallerResolver, AddressOwnerResolver};
    use nexus_core::OwnerToken;
    use nexus_native_memory::NativeMemoryReader;
    use nexus_ui_host::{
        CheckedVisibilityAccess, ContextRegistrationOutcome, OwnerGeneration, QA_GENERIC_KEY,
        RegisterRenderOutcome, RenderPhase as UiRenderPhase, ShortcutRegistrationOutcome, UiHost,
        VisibilityTarget,
    };

    use super::UiApi;
    use crate::{BackendFailures, BackendOperationError, NativeCallBoundary};

    const OWNER: OwnerToken = OwnerToken {
        signature: 0xA11E,
        generation: 7,
    };
    const FOREIGN_OWNER: OwnerToken = OwnerToken {
        signature: 0xB0B,
        generation: 3,
    };
    const STALE_OWNER: OwnerToken = OwnerToken {
        signature: 0xA11E,
        generation: 6,
    };
    static RENDER_CALLS: AtomicUsize = AtomicUsize::new(0);

    #[derive(Default)]
    struct MappedOwners {
        visibility_address: AtomicUsize,
        visibility_is_foreign: AtomicBool,
    }

    impl MappedOwners {
        fn bind_visibility(&self, state: *mut u8) {
            self.visibility_is_foreign.store(false, Ordering::Release);
            self.visibility_address
                .store(state as usize, Ordering::Release);
        }

        fn bind_foreign_visibility(&self, state: *mut u8) {
            self.visibility_is_foreign.store(true, Ordering::Release);
            self.visibility_address
                .store(state as usize, Ordering::Release);
        }

        fn unbind_visibility(&self) {
            self.visibility_address.store(0, Ordering::Release);
        }
    }

    impl AddressOwnerResolver for MappedOwners {
        fn owner_for_address(&self, address: NonZeroUsize) -> Option<OwnerToken> {
            match address.get() {
                value if value == count_render as *const () as usize => Some(OWNER),
                value if value == foreign_render as *const () as usize => Some(FOREIGN_OWNER),
                value if value == stale_render as *const () as usize => Some(STALE_OWNER),
                value if value == self.visibility_address.load(Ordering::Acquire) => {
                    Some(if self.visibility_is_foreign.load(Ordering::Acquire) {
                        FOREIGN_OWNER
                    } else {
                        OWNER
                    })
                }
                _ => None,
            }
        }

        fn is_current_owner(&self, owner: OwnerToken) -> bool {
            owner == OWNER || owner == FOREIGN_OWNER
        }
    }

    unsafe extern "C" fn count_render() {
        RENDER_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    unsafe extern "C" fn foreign_render() {}

    unsafe extern "C" fn stale_render() {}

    fn api_with_owners() -> (
        UiApi,
        Arc<AddonCallerResolver>,
        Arc<BackendFailures>,
        Arc<UiHost>,
        Arc<MappedOwners>,
    ) {
        let owners = Arc::new(MappedOwners::default());
        let callers = Arc::new(AddonCallerResolver::new(owners.clone()));
        let failures = Arc::new(BackendFailures::new());
        let boundary = Arc::new(NativeCallBoundary::new(
            Arc::clone(&callers),
            NativeMemoryReader::default(),
            Arc::clone(&failures),
        ));
        let host = Arc::new(UiHost::default());
        (
            UiApi::new(boundary, Arc::clone(&host)),
            callers,
            failures,
            host,
            owners,
        )
    }

    fn api() -> (
        UiApi,
        Arc<AddonCallerResolver>,
        Arc<BackendFailures>,
        Arc<UiHost>,
    ) {
        let (api, callers, failures, host, _owners) = api_with_owners();
        (api, callers, failures, host)
    }

    #[test]
    fn render_callbacks_use_address_attribution_and_exact_generation_cleanup() {
        RENDER_CALLS.store(0, Ordering::Relaxed);
        let (api, callers, _failures, host) = api();
        let _scope = callers.enter_owner_scope(OWNER).expect("owner scope");

        assert_eq!(
            api.renderer_register(AbiRenderPhase(1), Some(count_render)),
            Ok(Some(RegisterRenderOutcome::Registered))
        );
        let report = host.render().snapshot(UiRenderPhase::Render).invoke_all();
        assert_eq!(report.invoked, 1);
        assert_eq!(RENDER_CALLS.load(Ordering::Relaxed), 1);

        assert_eq!(api.renderer_deregister(Some(count_render)), Ok(1));
        assert!(host.render().snapshot(UiRenderPhase::Render).is_empty());
    }

    #[test]
    fn verified_nullable_callbacks_are_noops_before_native_access() {
        let (api, _callers, failures, _host) = api();

        assert_eq!(
            api.renderer_register(AbiRenderPhase(u32::MAX), None),
            Ok(None)
        );
        assert_eq!(api.renderer_deregister(None), Ok(0));
        assert_eq!(
            api.quick_access_add_simple(core::ptr::null(), None),
            Ok(None)
        );
        assert_eq!(
            api.quick_access_add_context_menu(core::ptr::null(), core::ptr::null(), None,),
            Ok(None)
        );
        assert_eq!(failures.snapshot().caller_attribution, 0);
        assert_eq!(failures.snapshot().native_memory, 0);
    }

    #[test]
    fn close_registration_uses_checked_memory_and_cleanup_removes_the_cell() {
        let (api, callers, failures, host, owners) = api_with_owners();
        let identifier = CString::new("Sensitive Window").expect("test identifier");
        let mut state = 1_u8;
        let address = NonZeroUsize::new((&mut state as *mut u8) as usize)
            .expect("stack test cell is non-null");
        owners.bind_visibility(&mut state);

        {
            let _scope = callers
                .enter_owner_scope(FOREIGN_OWNER)
                .expect("foreign owner scope");
            assert!(matches!(
                unsafe {
                    // SAFETY: ownership validation rejects this mismatched
                    // caller before retaining or accessing the live test cell.
                    api.ui_register_close_on_escape(identifier.as_ptr(), &mut state)
                },
                Err(BackendOperationError::Boundary(_))
            ));
        }
        assert!(host.escape_closing().registered_windows().is_empty());

        let _scope = callers.enter_owner_scope(OWNER).expect("owner scope");

        assert!(matches!(
            unsafe {
                // SAFETY: a null pointer is rejected and cannot create a
                // retained registration, so no allocation obligation begins.
                api.ui_register_close_on_escape(identifier.as_ptr(), core::ptr::null_mut())
            },
            Err(BackendOperationError::Boundary(_))
        ));
        assert!(host.escape_closing().registered_windows().is_empty());
        owners.unbind_visibility();
        assert_eq!(
            unsafe {
                // SAFETY: `state` remains the same live writable byte through
                // cleanup below, and this single-threaded test performs no
                // conflicting access while the registry is handling Escape.
                api.ui_register_close_on_escape(identifier.as_ptr(), &mut state)
            },
            Ok(())
        );
        assert_eq!(
            host.escape_closing().registered_windows().as_ref(),
            &[Arc::<str>::from("Sensitive Window")]
        );

        let access = super::BoundaryVisibilityAccess {
            boundary: Arc::clone(&api.boundary),
            owner: OWNER,
        };
        assert_eq!(access.read_visible(address), Some(true));
        assert!(access.write_hidden(address));
        assert_eq!(state, 0);
        state = 1;
        owners.bind_foreign_visibility(&mut state);
        assert!(!access.write_hidden(address));
        assert_eq!(state, 1);
        owners.bind_visibility(&mut state);

        assert!(matches!(
            host.escape_closing().handle(
                nexus_ui_host::EscapeKeyEvent {
                    is_key_down: true,
                    virtual_key: nexus_ui_host::ESCAPE_VIRTUAL_KEY,
                    was_down: false,
                },
                &["Fallback", "Sensitive Window"],
            ),
            nexus_ui_host::EscapeCloseOutcome::Consumed { .. }
        ));
        assert_eq!(state, 0);
        state = 1;

        let cleanup = host.cleanup_owner_generation(OwnerGeneration::from(OWNER));
        assert_eq!(cleanup.escape_windows, 1);
        assert!(host.escape_closing().registered_windows().is_empty());
        assert!(matches!(
            host.escape_closing().handle(
                nexus_ui_host::EscapeKeyEvent {
                    is_key_down: true,
                    virtual_key: nexus_ui_host::ESCAPE_VIRTUAL_KEY,
                    was_down: false,
                },
                &["Fallback", "Sensitive Window"],
            ),
            nexus_ui_host::EscapeCloseOutcome::Passed
        ));
        assert_eq!(state, 1);
        assert_eq!(failures.snapshot().native_memory, 1);
        assert_eq!(failures.snapshot().caller_attribution, 2);
        assert!(!format!("{api:?}").contains("Sensitive Window"));
    }

    #[test]
    fn callback_addresses_cannot_impersonate_another_or_stale_owner() {
        let (api, callers, _failures, host) = api();

        {
            let _scope = callers
                .enter_owner_scope(FOREIGN_OWNER)
                .expect("foreign owner scope");
            assert_eq!(
                api.renderer_register(AbiRenderPhase(1), Some(foreign_render)),
                Ok(Some(RegisterRenderOutcome::Registered))
            );
        }

        {
            let _scope = callers.enter_owner_scope(OWNER).expect("owner scope");
            assert!(matches!(
                api.renderer_deregister(Some(foreign_render)),
                Err(BackendOperationError::Boundary(_))
            ));
            assert!(matches!(
                api.renderer_register(AbiRenderPhase(1), Some(stale_render)),
                Err(BackendOperationError::Boundary(_))
            ));
        }
        assert_eq!(host.render().snapshot(UiRenderPhase::Render).len(), 1);

        let _scope = callers
            .enter_owner_scope(FOREIGN_OWNER)
            .expect("foreign owner scope");
        assert_eq!(api.renderer_deregister(Some(foreign_render)), Ok(1));
        assert!(host.render().snapshot(UiRenderPhase::Render).is_empty());
    }

    #[test]
    fn strings_are_copied_and_optional_quick_access_fields_match_legacy() {
        let (api, callers, _failures, host) = api();
        let _scope = callers.enter_owner_scope(OWNER).expect("owner scope");

        let mutation = {
            let identifier = CString::new("qa.copied").expect("test identifier");
            let texture = CString::new("tx.normal").expect("test texture");
            let hover = CString::new("tx.hover").expect("test hover texture");
            api.quick_access_add(
                identifier.as_ptr(),
                texture.as_ptr(),
                hover.as_ptr(),
                core::ptr::null(),
                core::ptr::null(),
            )
            .expect("add shortcut")
        };
        assert_eq!(mutation.outcome, ShortcutRegistrationOutcome::Registered);

        let message = CString::new("copied alert").expect("test alert");
        api.ui_send_alert(message.as_ptr()).expect("queue alert");
        drop(message);
        let alert = host
            .alerts()
            .advance(1)
            .alert
            .expect("queued alert snapshot");
        assert_eq!(alert.message.as_ref(), "copied alert");
        assert_eq!(alert.owner, OwnerGeneration::from(OWNER));

        let identifier = CString::new("qa.copied").expect("test identifier");
        api.quick_access_notify(identifier.as_ptr())
            .expect("push notification");
        let context = CString::new("qa.context").expect("test context");
        assert_eq!(
            api.quick_access_add_context_menu(
                context.as_ptr(),
                identifier.as_ptr(),
                Some(count_render),
            ),
            Ok(Some(ContextRegistrationOutcome::Attached))
        );

        let snapshot = host.quick_access().snapshot(true, false);
        assert_eq!(snapshot.shortcuts.len(), 1);
        let shortcut = &snapshot.shortcuts[0];
        assert_eq!(shortcut.id.as_ref(), "qa.copied");
        assert_eq!(shortcut.input_bind.as_ref(), "");
        assert_eq!(shortcut.tooltip.as_ref(), "");
        assert_eq!(shortcut.notifications.len(), 1);
        assert_eq!(shortcut.notifications[0].as_ref(), QA_GENERIC_KEY);
        assert_eq!(shortcut.context_items.len(), 1);

        assert_eq!(
            api.quick_access_remove_context_menu(context.as_ptr()),
            Ok(1)
        );
        assert_eq!(api.quick_access_remove(identifier.as_ptr()), Ok(true));
    }

    #[test]
    fn quick_access_removal_is_scoped_to_the_authenticated_owner() {
        let (api, callers, _failures, host) = api();
        let identifier = CString::new("qa.owner-scoped").expect("test identifier");
        let context = CString::new("qa.owner-scoped.context").expect("test context");
        let texture = CString::new("tx.owner-scoped").expect("test texture");

        {
            let _scope = callers.enter_owner_scope(OWNER).expect("owner scope");
            api.quick_access_add(
                identifier.as_ptr(),
                texture.as_ptr(),
                texture.as_ptr(),
                core::ptr::null(),
                core::ptr::null(),
            )
            .expect("owner shortcut");
            assert_eq!(
                api.quick_access_add_context_menu(
                    context.as_ptr(),
                    identifier.as_ptr(),
                    Some(count_render),
                ),
                Ok(Some(ContextRegistrationOutcome::Attached))
            );
        }

        {
            let _scope = callers
                .enter_owner_scope(FOREIGN_OWNER)
                .expect("foreign owner scope");
            assert_eq!(
                api.quick_access_remove_context_menu(context.as_ptr()),
                Ok(0)
            );
            assert_eq!(api.quick_access_remove(identifier.as_ptr()), Ok(false));
        }

        let snapshot = host.quick_access().snapshot(true, false);
        assert_eq!(snapshot.shortcuts.len(), 1);
        assert_eq!(snapshot.shortcuts[0].context_items.len(), 1);

        let _scope = callers.enter_owner_scope(OWNER).expect("owner scope");
        assert_eq!(
            api.quick_access_remove_context_menu(context.as_ptr()),
            Ok(1)
        );
        assert_eq!(api.quick_access_remove(identifier.as_ptr()), Ok(true));
    }

    #[test]
    fn close_deregistration_handles_a_safe_managed_registration() {
        let (api, callers, _failures, host) = api();
        let owner = host
            .owner(OwnerGeneration::from(OWNER))
            .expect("active owner");
        host.escape_closing()
            .register(
                &owner,
                "managed window",
                VisibilityTarget::managed(Arc::new(AtomicBool::new(true))),
            )
            .expect("managed registration");
        let identifier = CString::new("managed window").expect("test identifier");
        let foreign_owner = host
            .owner(OwnerGeneration::from(FOREIGN_OWNER))
            .expect("foreign active owner");

        {
            let _scope = callers
                .enter_owner_scope(FOREIGN_OWNER)
                .expect("foreign owner scope");
            assert_eq!(
                api.ui_deregister_close_on_escape(identifier.as_ptr()),
                Ok(false)
            );
        }
        assert!(foreign_owner.is_active());
        assert_eq!(host.escape_closing().registered_windows().len(), 1);

        let _scope = callers.enter_owner_scope(OWNER).expect("owner scope");

        assert_eq!(
            api.ui_deregister_close_on_escape(identifier.as_ptr()),
            Ok(true)
        );
        assert!(host.escape_closing().registered_windows().is_empty());
    }
}
