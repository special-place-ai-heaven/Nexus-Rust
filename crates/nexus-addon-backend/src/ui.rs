use core::ffi::c_char;
use core::fmt;
use core::num::NonZeroUsize;
use std::sync::Arc;

use nexus_abi::{RenderCallback, RenderPhase as AbiRenderPhase};
use nexus_ui_host::{
    AlertKind, ContextRegistrationOutcome, NativeRenderCallback, NotificationOutcome,
    NotifyOutcome, OwnerHandle, QA_GENERIC_KEY, RegisterRenderOutcome,
    RenderPhase as UiRenderPhase, ShortcutMutation, UiCallback, UiHost,
};

use crate::{BackendFailure, BackendOperationError, NativeCallBoundary, NativeText};

/// Caller-attributed adapter for native add-on UI registrations and commands.
pub struct UiApi {
    boundary: Arc<NativeCallBoundary>,
    host: Arc<UiHost>,
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
        let owner = self.owner_handle(None)?;
        let message = self.boundary.snapshot_message(message)?;
        self.service_result(
            self.host
                .alerts()
                .notify(&owner, AlertKind::Info, message.as_str()),
        )
    }

    /// Rejects native close-on-Escape registration until retained cells can be
    /// checked without directly dereferencing a foreign pointer.
    ///
    /// This deliberately never constructs `NativeVisibilityPointer`, reads the
    /// identifier, or retains `state`. The missing bridge must combine checked
    /// native-memory reads/writes with an owner-generation lifetime proof before
    /// this surface can be enabled safely.
    pub fn ui_register_close_on_escape(
        &self,
        _identifier: *const c_char,
        _state: *mut u8,
    ) -> Result<(), BackendOperationError> {
        let _owner = self.owner_handle(None)?;
        self.boundary.failures().record(BackendFailure::Unsupported);
        Err(BackendOperationError::Unsupported)
    }

    /// Deregisters one close-on-Escape window name after caller attribution.
    pub fn ui_deregister_close_on_escape(
        &self,
        identifier: *const c_char,
    ) -> Result<bool, BackendOperationError> {
        let _owner = self.owner_handle(None)?;
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        Ok(self
            .host
            .escape_closing()
            .deregister_window(identifier.as_str()))
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
        let owner = self.owner_handle(None)?;
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
        let _owner = self.owner_handle(None)?;
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        self.service_result(
            self.host
                .quick_access()
                .remove_shortcut(identifier.as_str()),
        )
    }

    /// Pushes the legacy `Generic` notification for one shortcut.
    pub fn quick_access_notify(
        &self,
        identifier: *const c_char,
    ) -> Result<NotificationOutcome, BackendOperationError> {
        let owner = self.owner_handle(None)?;
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
        let _owner = self.owner_handle(None)?;
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        self.service_result(
            self.host
                .quick_access()
                .remove_context_item(identifier.as_str()),
        )
    }

    fn owner_handle(
        &self,
        function_hint: Option<NonZeroUsize>,
    ) -> Result<OwnerHandle, BackendOperationError> {
        let owner = self.boundary.resolve_owner(function_hint)?;
        self.service_result(self.host.owner(owner.into()))
    }

    fn native_callback(
        &self,
        callback: RenderCallback,
    ) -> Result<UiCallback, BackendOperationError> {
        let owner = self.owner_handle(callback_hint(callback))?;
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

fn callback_hint(callback: RenderCallback) -> Option<NonZeroUsize> {
    NonZeroUsize::new(callback as usize)
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
        ContextRegistrationOutcome, OwnerGeneration, QA_GENERIC_KEY, RegisterRenderOutcome,
        RenderPhase as UiRenderPhase, ShortcutRegistrationOutcome, UiHost, VisibilityTarget,
    };

    use super::UiApi;
    use crate::{BackendFailures, BackendOperationError, NativeCallBoundary};

    const OWNER: OwnerToken = OwnerToken {
        signature: 0xA11E,
        generation: 7,
    };
    static RENDER_CALLS: AtomicUsize = AtomicUsize::new(0);

    struct FixedOwner;

    impl AddressOwnerResolver for FixedOwner {
        fn owner_for_address(&self, _address: NonZeroUsize) -> Option<OwnerToken> {
            Some(OWNER)
        }

        fn is_current_owner(&self, owner: OwnerToken) -> bool {
            owner == OWNER
        }
    }

    unsafe extern "C" fn count_render() {
        RENDER_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    fn api() -> (
        UiApi,
        Arc<AddonCallerResolver>,
        Arc<BackendFailures>,
        Arc<UiHost>,
    ) {
        let callers = Arc::new(AddonCallerResolver::new(Arc::new(FixedOwner)));
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
        )
    }

    #[test]
    fn render_callbacks_use_address_attribution_and_exact_generation_cleanup() {
        RENDER_CALLS.store(0, Ordering::Relaxed);
        let (api, _callers, _failures, host) = api();

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
    fn close_registration_is_unsupported_without_retaining_the_foreign_cell() {
        let (api, callers, failures, host) = api();
        let identifier = CString::new("Sensitive Window").expect("test identifier");
        let mut state = 1_u8;
        let _scope = callers.enter_owner_scope(OWNER).expect("owner scope");

        assert_eq!(
            api.ui_register_close_on_escape(identifier.as_ptr(), &mut state),
            Err(BackendOperationError::Unsupported)
        );
        assert_eq!(state, 1);
        assert!(host.escape_closing().registered_windows().is_empty());
        assert_eq!(failures.snapshot().unsupported, 1);
        assert!(!format!("{api:?}").contains("Sensitive Window"));
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
        let _scope = callers.enter_owner_scope(OWNER).expect("owner scope");

        assert_eq!(
            api.ui_deregister_close_on_escape(identifier.as_ptr()),
            Ok(true)
        );
        assert!(host.escape_closing().registered_windows().is_empty());
    }
}
