//! Complete production dispatcher for every native add-on API revision.

use core::{
    ffi::{c_char, c_void},
    fmt, ptr,
};
use std::sync::{Arc, Mutex};

use nexus_abi::{
    EventCallback, GameBind, InputBindCallbackV1, InputBindCallbackV2, InputBindV1, LogLevel,
    MinHookStatus, ReceiveFont, ReceiveTexture, RenderCallback, RenderPhase, Texture,
    WndProcCallback,
};
use nexus_addon_ffi::AddonApiBackend;
use nexus_data_services::{DataLinkService, EventService};
use nexus_inline_hooks::InlineHookService;
use nexus_input::{GameInvoker, GameOnlyMessageSink, ManagedInputBinds, RawWndProcRegistry};
use nexus_platform::{LogRegistry, MinimalScheduler};
use nexus_ui_host::UiHost;
use nexus_ui_services::LocalizationService;

use crate::{
    BackendFailures, BackendOperationError, DataLinkApi, EventApi, FontApi, FontBackend,
    GameBindApi, GameBindBackend, InlineHookApi, InputBindApi, InputBindBackend, LocalizationApi,
    LocalizationBackend, LoggingApi, NativeCallBoundary, PathApi, RenderFontService,
    StablePathStore, TextureApi, TextureBackend, TextureServiceFacade, UiApi, UpdateApi,
    UpdateBackend, WndProcApi, WndProcBackend,
};

/// Every process service the native add-on API is built from.
///
/// Named separately from the two adapter bundles so an embedding runtime hands over the
/// services it already owns and does not have to know which adapter wraps which one.
pub struct AddonApiServices {
    /// UI host owning render callbacks, alerts and close-on-escape.
    pub ui_host: Arc<UiHost>,
    /// Process log registry.
    pub logs: Arc<LogRegistry>,
    /// Process-lifetime native path storage.
    pub paths: Arc<StablePathStore>,
    /// Owner-scoped inline hook service.
    pub inline_hooks: Arc<InlineHookService>,
    /// Event registry raising and dispatching named events.
    pub events: Arc<EventService>,
    /// Shared-memory resource registry.
    pub data_link: Arc<DataLinkService>,
    /// Raw window-message callback registry.
    pub wnd_proc_callbacks: Arc<RawWndProcRegistry>,
    /// Sink delivering a message to the game only.
    pub game_messages: Arc<dyn GameOnlyMessageSink>,
    /// Managed input-bind registry.
    pub input_binds: Arc<ManagedInputBinds>,
    /// Game-bind invoker lifecycle slot.
    pub game_invoker: Arc<Mutex<Option<GameInvoker>>>,
    /// Optional scheduler for asynchronous game binds.
    pub game_scheduler: Option<Arc<MinimalScheduler>>,
    /// Texture service selected for the active render session.
    pub textures: Arc<dyn TextureServiceFacade>,
    /// Localization service advanced on the render thread.
    pub localization: Arc<Mutex<LocalizationService>>,
    /// Render-thread font service.
    pub fonts: Arc<dyn RenderFontService>,
}

/// Validated adapters for the 28 operations already backed by typed services.
#[derive(Clone)]
pub struct CoreAddonApiServices {
    ui: Arc<UiApi>,
    logging: Arc<LoggingApi>,
    paths: Arc<PathApi>,
    inline_hooks: Arc<InlineHookApi>,
    events: Arc<EventApi>,
    data_link: Arc<DataLinkApi>,
}

impl CoreAddonApiServices {
    /// Creates a complete core service bundle.
    #[must_use]
    pub fn new(
        ui: Arc<UiApi>,
        logging: Arc<LoggingApi>,
        paths: Arc<PathApi>,
        inline_hooks: Arc<InlineHookApi>,
        events: Arc<EventApi>,
        data_link: Arc<DataLinkApi>,
    ) -> Self {
        Self {
            ui,
            logging,
            paths,
            inline_hooks,
            events,
            data_link,
        }
    }
}

impl fmt::Debug for CoreAddonApiServices {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CoreAddonApiServices")
            .field("ui", &self.ui)
            .field("logging", &self.logging)
            .field("paths", &self.paths)
            .field("inline_hooks", &self.inline_hooks)
            .field("events", &self.events)
            .field("data_link", &self.data_link)
            .finish()
    }
}

/// Required adapters for the remaining 34 compatibility operations.
///
/// All seven domains are constructor-required. This type deliberately has no
/// `Default` implementation and no unavailable-service substitute.
#[derive(Clone)]
pub struct RequiredAddonApiServices {
    updates: Arc<dyn UpdateBackend>,
    wnd_proc: Arc<dyn WndProcBackend>,
    input_binds: Arc<dyn InputBindBackend>,
    game_binds: Arc<dyn GameBindBackend>,
    textures: Arc<dyn TextureBackend>,
    localization: Arc<dyn LocalizationBackend>,
    fonts: Arc<dyn FontBackend>,
}

impl RequiredAddonApiServices {
    /// Creates a bundle only when every required compatibility service exists.
    #[must_use]
    pub fn new(
        updates: Arc<dyn UpdateBackend>,
        wnd_proc: Arc<dyn WndProcBackend>,
        input_binds: Arc<dyn InputBindBackend>,
        game_binds: Arc<dyn GameBindBackend>,
        textures: Arc<dyn TextureBackend>,
        localization: Arc<dyn LocalizationBackend>,
        fonts: Arc<dyn FontBackend>,
    ) -> Self {
        Self {
            updates,
            wnd_proc,
            input_binds,
            game_binds,
            textures,
            localization,
            fonts,
        }
    }
}

impl fmt::Debug for RequiredAddonApiServices {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RequiredAddonApiServices")
            .field("updates", &"configured")
            .field("wnd_proc", &"configured")
            .field("input_binds", &"configured")
            .field("game_binds", &"configured")
            .field("textures", &"configured")
            .field("localization", &"configured")
            .field("fonts", &"configured")
            .finish()
    }
}

/// Process-wide backend installed behind the v1-through-v6 native API tables.
///
/// Every method delegates to one required service. Domain adapters own caller
/// attribution, bounded native reads, stable return storage, and owner cleanup.
/// Errors are converted to the legacy ABI's closed return values because the
/// native API has no error channel for these operations.
#[derive(Clone, Debug)]
pub struct ProductionAddonApiBackend {
    core: CoreAddonApiServices,
    required: RequiredAddonApiServices,
}

impl ProductionAddonApiBackend {
    /// Creates a backend only from complete core and required service bundles.
    #[must_use]
    pub fn new(core: CoreAddonApiServices, required: RequiredAddonApiServices) -> Self {
        Self { core, required }
    }

    /// Builds the whole backend from the process services an embedding runtime owns.
    ///
    /// This is the single place the thirteen adapters are wired, so a runtime cannot
    /// half-install the API by composing some bundles and forgetting others.
    ///
    /// `failures` must be the same counter set the boundary was built with: the game-bind
    /// adapter owns no boundary and records against it directly, so passing a different
    /// set would silently split the diagnostics in two.
    #[must_use]
    pub fn compose(
        boundary: Arc<NativeCallBoundary>,
        failures: Arc<BackendFailures>,
        services: AddonApiServices,
    ) -> Self {
        let core = CoreAddonApiServices::new(
            Arc::new(UiApi::new(Arc::clone(&boundary), services.ui_host)),
            Arc::new(LoggingApi::new(Arc::clone(&boundary), services.logs)),
            Arc::new(PathApi::new(Arc::clone(&boundary), services.paths)),
            Arc::new(InlineHookApi::new(
                Arc::clone(&boundary),
                services.inline_hooks,
            )),
            Arc::new(EventApi::new(Arc::clone(&boundary), services.events)),
            Arc::new(DataLinkApi::new(Arc::clone(&boundary), services.data_link)),
        );

        let required = RequiredAddonApiServices::new(
            Arc::new(UpdateApi::new(Arc::clone(&boundary))),
            Arc::new(WndProcApi::new(
                Arc::clone(&boundary),
                services.wnd_proc_callbacks,
                services.game_messages,
            )),
            Arc::new(InputBindApi::new(
                Arc::clone(&boundary),
                services.input_binds,
            )),
            Arc::new(GameBindApi::new(
                failures,
                services.game_invoker,
                services.game_scheduler,
            )),
            Arc::new(TextureApi::new(Arc::clone(&boundary), services.textures)),
            Arc::new(LocalizationApi::new(
                Arc::clone(&boundary),
                services.localization,
            )),
            Arc::new(FontApi::new(boundary, services.fonts)),
        );

        Self::new(core, required)
    }
}

impl AddonApiBackend for ProductionAddonApiBackend {
    fn renderer_register(&self, phase: RenderPhase, callback: Option<RenderCallback>) {
        discard(self.core.ui.renderer_register(phase, callback));
    }

    fn renderer_deregister(&self, callback: Option<RenderCallback>) {
        discard(self.core.ui.renderer_deregister(callback));
    }

    fn request_update(&self, signature: i32, update_url: *const c_char) {
        discard(self.required.updates.request_update(signature, update_url));
    }

    fn log(&self, level: LogLevel, channel: *const c_char, message: *const c_char) {
        discard(self.core.logging.log(level, channel, message));
    }

    fn log_v1(&self, level: LogLevel, message: *const c_char) {
        discard(self.core.logging.log_v1(level, message));
    }

    fn ui_send_alert(&self, message: *const c_char) {
        discard(self.core.ui.ui_send_alert(message));
    }

    unsafe fn ui_register_close_on_escape(&self, identifier: *const c_char, state: *mut u8) {
        discard(unsafe {
            // SAFETY: the native API contract is forwarded unchanged. UiApi
            // validates the allocation and binds it to exact owner cleanup.
            self.core.ui.ui_register_close_on_escape(identifier, state)
        });
    }

    fn ui_deregister_close_on_escape(&self, identifier: *const c_char) {
        discard(self.core.ui.ui_deregister_close_on_escape(identifier));
    }

    fn paths_get_game_directory(&self) -> *const c_char {
        value_or(self.core.paths.game_directory(), ptr::null())
    }

    fn paths_get_addon_directory(&self, name: *const c_char) -> *const c_char {
        value_or(self.core.paths.addon_directory(name), ptr::null())
    }

    fn paths_get_common_directory(&self) -> *const c_char {
        value_or(self.core.paths.common_directory(), ptr::null())
    }

    unsafe fn min_hook_create(
        &self,
        target: *mut c_void,
        detour: *mut c_void,
        original: *mut *mut c_void,
    ) -> MinHookStatus {
        unsafe {
            // SAFETY: the exact legacy target, detour, output, and lifetime
            // contract is forwarded to the validating owner-scoped adapter.
            self.core.inline_hooks.create(target, detour, original)
        }
    }

    fn min_hook_remove(&self, target: *mut c_void) -> MinHookStatus {
        self.core.inline_hooks.remove(target)
    }

    fn min_hook_enable(&self, target: *mut c_void) -> MinHookStatus {
        self.core.inline_hooks.enable(target)
    }

    fn min_hook_disable(&self, target: *mut c_void) -> MinHookStatus {
        self.core.inline_hooks.disable(target)
    }

    unsafe fn events_raise(&self, identifier: *const c_char, payload: *mut c_void) {
        discard(unsafe {
            // SAFETY: the backend trait requires the caller to keep the opaque
            // payload valid for this complete synchronous dispatch.
            self.core.events.raise(identifier, payload)
        });
    }

    fn events_raise_notification(&self, identifier: *const c_char) {
        discard(self.core.events.raise_notification(identifier));
    }

    unsafe fn events_raise_targeted(
        &self,
        signature: u32,
        identifier: *const c_char,
        payload: *mut c_void,
    ) {
        discard(unsafe {
            // SAFETY: the backend trait requires the caller to keep the opaque
            // payload valid for this complete synchronous targeted dispatch.
            self.core
                .events
                .raise_targeted(signature, identifier, payload)
        });
    }

    fn events_raise_notification_targeted(&self, signature: u32, identifier: *const c_char) {
        discard(
            self.core
                .events
                .raise_notification_targeted(signature, identifier),
        );
    }

    fn events_subscribe(&self, identifier: *const c_char, callback: Option<EventCallback>) {
        discard(self.core.events.subscribe(identifier, callback));
    }

    fn events_unsubscribe(&self, identifier: *const c_char, callback: Option<EventCallback>) {
        discard(self.core.events.unsubscribe(identifier, callback));
    }

    fn wnd_proc_register(&self, callback: Option<WndProcCallback>) {
        discard(self.required.wnd_proc.register(callback));
    }

    fn wnd_proc_deregister(&self, callback: Option<WndProcCallback>) {
        discard(self.required.wnd_proc.deregister(callback));
    }

    fn wnd_proc_send_to_game_only(
        &self,
        hwnd: *mut c_void,
        message: u32,
        w_param: usize,
        l_param: isize,
    ) -> isize {
        value_or(
            self.required
                .wnd_proc
                .send_to_game_only(hwnd, message, w_param, l_param),
            0,
        )
    }

    fn input_binds_invoke(&self, identifier: *const c_char, is_release: u8) {
        discard(self.required.input_binds.invoke(identifier, is_release));
    }

    fn input_binds_register_with_string(
        &self,
        identifier: *const c_char,
        callback: Option<InputBindCallbackV2>,
        bind: *const c_char,
    ) {
        discard(
            self.required
                .input_binds
                .register_with_string(identifier, callback, bind),
        );
    }

    fn input_binds_register_with_struct(
        &self,
        identifier: *const c_char,
        callback: Option<InputBindCallbackV2>,
        bind: InputBindV1,
    ) {
        discard(
            self.required
                .input_binds
                .register_with_struct(identifier, callback, bind),
        );
    }

    fn input_binds_register_with_string_v1(
        &self,
        identifier: *const c_char,
        callback: Option<InputBindCallbackV1>,
        bind: *const c_char,
    ) {
        discard(
            self.required
                .input_binds
                .register_with_string_v1(identifier, callback, bind),
        );
    }

    fn input_binds_register_with_struct_v1(
        &self,
        identifier: *const c_char,
        callback: Option<InputBindCallbackV1>,
        bind: InputBindV1,
    ) {
        discard(
            self.required
                .input_binds
                .register_with_struct_v1(identifier, callback, bind),
        );
    }

    fn input_binds_deregister(&self, identifier: *const c_char) {
        discard(self.required.input_binds.deregister(identifier));
    }

    fn game_binds_press_async(&self, bind: GameBind) {
        discard(self.required.game_binds.press_async(bind));
    }

    fn game_binds_release_async(&self, bind: GameBind) {
        discard(self.required.game_binds.release_async(bind));
    }

    fn game_binds_invoke_async(&self, bind: GameBind, duration: i32) {
        discard(self.required.game_binds.invoke_async(bind, duration));
    }

    fn game_binds_press(&self, bind: GameBind) {
        discard(self.required.game_binds.press(bind));
    }

    fn game_binds_release(&self, bind: GameBind) {
        discard(self.required.game_binds.release(bind));
    }

    fn game_binds_is_bound(&self, bind: GameBind) -> u8 {
        value_or(self.required.game_binds.is_bound(bind), 0)
    }

    fn data_link_get(&self, identifier: *const c_char) -> *mut c_void {
        value_or(self.core.data_link.get(identifier), ptr::null_mut())
    }

    fn data_link_share(&self, identifier: *const c_char, size: usize) -> *mut c_void {
        value_or(self.core.data_link.share(identifier, size), ptr::null_mut())
    }

    fn textures_get(&self, identifier: *const c_char) -> *mut Texture {
        value_or(self.required.textures.get(identifier), ptr::null_mut())
    }

    fn textures_get_or_create_from_file(
        &self,
        identifier: *const c_char,
        filename: *const c_char,
    ) -> *mut Texture {
        value_or(
            self.required
                .textures
                .get_or_create_from_file(identifier, filename),
            ptr::null_mut(),
        )
    }

    fn textures_get_or_create_from_resource(
        &self,
        identifier: *const c_char,
        resource_id: u32,
        module: *mut c_void,
    ) -> *mut Texture {
        value_or(
            self.required
                .textures
                .get_or_create_from_resource(identifier, resource_id, module),
            ptr::null_mut(),
        )
    }

    fn textures_get_or_create_from_url(
        &self,
        identifier: *const c_char,
        remote: *const c_char,
        endpoint: *const c_char,
    ) -> *mut Texture {
        value_or(
            self.required
                .textures
                .get_or_create_from_url(identifier, remote, endpoint),
            ptr::null_mut(),
        )
    }

    fn textures_get_or_create_from_memory(
        &self,
        identifier: *const c_char,
        data: *mut c_void,
        size: usize,
    ) -> *mut Texture {
        value_or(
            self.required
                .textures
                .get_or_create_from_memory(identifier, data, size),
            ptr::null_mut(),
        )
    }

    fn textures_load_from_file(
        &self,
        identifier: *const c_char,
        filename: *const c_char,
        callback: Option<ReceiveTexture>,
    ) {
        discard(
            self.required
                .textures
                .load_from_file(identifier, filename, callback),
        );
    }

    fn textures_load_from_resource(
        &self,
        identifier: *const c_char,
        resource_id: u32,
        module: *mut c_void,
        callback: Option<ReceiveTexture>,
    ) {
        discard(self.required.textures.load_from_resource(
            identifier,
            resource_id,
            module,
            callback,
        ));
    }

    fn textures_load_from_url(
        &self,
        identifier: *const c_char,
        remote: *const c_char,
        endpoint: *const c_char,
        callback: Option<ReceiveTexture>,
    ) {
        discard(
            self.required
                .textures
                .load_from_url(identifier, remote, endpoint, callback),
        );
    }

    fn textures_load_from_memory(
        &self,
        identifier: *const c_char,
        data: *mut c_void,
        size: usize,
        callback: Option<ReceiveTexture>,
    ) {
        discard(
            self.required
                .textures
                .load_from_memory(identifier, data, size, callback),
        );
    }

    fn quick_access_add(
        &self,
        identifier: *const c_char,
        texture: *const c_char,
        hover_texture: *const c_char,
        input_bind: *const c_char,
        tooltip: *const c_char,
    ) {
        discard(self.core.ui.quick_access_add(
            identifier,
            texture,
            hover_texture,
            input_bind,
            tooltip,
        ));
    }

    fn quick_access_remove(&self, identifier: *const c_char) {
        discard(self.core.ui.quick_access_remove(identifier));
    }

    fn quick_access_notify(&self, identifier: *const c_char) {
        discard(self.core.ui.quick_access_notify(identifier));
    }

    fn quick_access_add_simple(&self, identifier: *const c_char, callback: Option<RenderCallback>) {
        discard(self.core.ui.quick_access_add_simple(identifier, callback));
    }

    fn quick_access_add_context_menu(
        &self,
        identifier: *const c_char,
        target: *const c_char,
        callback: Option<RenderCallback>,
    ) {
        discard(
            self.core
                .ui
                .quick_access_add_context_menu(identifier, target, callback),
        );
    }

    fn quick_access_remove_context_menu(&self, identifier: *const c_char) {
        discard(self.core.ui.quick_access_remove_context_menu(identifier));
    }

    fn localization_translate(&self, identifier: *const c_char) -> *const c_char {
        value_or(
            self.required.localization.translate(identifier),
            ptr::null(),
        )
    }

    fn localization_translate_to(
        &self,
        identifier: *const c_char,
        language: *const c_char,
    ) -> *const c_char {
        value_or(
            self.required
                .localization
                .translate_to(identifier, language),
            ptr::null(),
        )
    }

    fn localization_set_translated_string(
        &self,
        identifier: *const c_char,
        language: *const c_char,
        value: *const c_char,
    ) {
        discard(
            self.required
                .localization
                .set_translated_string(identifier, language, value),
        );
    }

    fn fonts_get(&self, identifier: *const c_char, callback: Option<ReceiveFont>) {
        discard(self.required.fonts.get(identifier, callback));
    }

    fn fonts_release(&self, identifier: *const c_char, callback: Option<ReceiveFont>) {
        discard(self.required.fonts.release(identifier, callback));
    }

    fn fonts_add_from_file(
        &self,
        identifier: *const c_char,
        size: f32,
        filename: *const c_char,
        callback: Option<ReceiveFont>,
        config: *mut c_void,
    ) {
        discard(
            self.required
                .fonts
                .add_from_file(identifier, size, filename, callback, config),
        );
    }

    fn fonts_add_from_resource(
        &self,
        identifier: *const c_char,
        size: f32,
        resource_id: u32,
        module: *mut c_void,
        callback: Option<ReceiveFont>,
        config: *mut c_void,
    ) {
        discard(self.required.fonts.add_from_resource(
            identifier,
            size,
            resource_id,
            module,
            callback,
            config,
        ));
    }

    fn fonts_add_from_memory(
        &self,
        identifier: *const c_char,
        size: f32,
        data: *mut c_void,
        data_size: usize,
        callback: Option<ReceiveFont>,
        config: *mut c_void,
    ) {
        discard(
            self.required
                .fonts
                .add_from_memory(identifier, size, data, data_size, callback, config),
        );
    }

    fn fonts_resize(&self, identifier: *const c_char, size: f32) {
        discard(self.required.fonts.resize(identifier, size));
    }
}

fn discard<T>(result: Result<T, BackendOperationError>) {
    drop(result);
}

fn value_or<T: Copy>(result: Result<T, BackendOperationError>, fallback: T) -> T {
    result.unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroUsize;
    use std::ffi::{CStr, CString};
    use std::sync::{Mutex, MutexGuard, PoisonError};

    use nexus_addon_ffi::{AddonCallerResolver, AddressOwnerResolver};
    use nexus_core::OwnerToken;
    use nexus_data_services::{
        DataLinkService, EventService, MappingBackend, MappingFailure, MappingView,
    };
    use nexus_inline_hooks::InlineHookService;
    use nexus_input::{CallbackLimits, GameSinkError, InlineExecutor};
    use nexus_native_memory::NativeMemoryReader;
    use nexus_platform::{LogRegistry, PathIndex, PathKey, PathRoots};
    use nexus_textures::{
        LoadOptions, OwnerGeneration, RequestOutcome, TextureCallback, TextureHandle,
    };
    use nexus_ui_host::UiHost;
    use nexus_ui_services::{
        FontConfig, FontGetResult, FontRegistration, OwnerId, ResourceFont, SubscriptionId,
    };

    use std::path::PathBuf;

    use super::*;
    use crate::{
        BackendFailures, NativeCallBoundary, RequiredServiceResult, SendFontCallback,
        StablePathStore, TextureFacadeError, TextureSourceFactory, TextureSourceFailurePolicy,
    };

    type SpyResult<T> = Result<T, BackendOperationError>;

    const WND_PROC_RESULT: isize = 0x1357;
    const GAME_BIND_RESULT: u8 = 1;

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct RecordedCall {
        operation: &'static str,
        arguments: Vec<usize>,
    }

    impl RecordedCall {
        fn new<const N: usize>(operation: &'static str, arguments: [usize; N]) -> Self {
            Self {
                operation,
                arguments: arguments.into(),
            }
        }
    }

    struct RecordingRequired {
        calls: Mutex<Vec<RecordedCall>>,
        texture_result: usize,
        translation_result: usize,
    }

    impl RecordingRequired {
        fn new(texture_result: *mut Texture, translation_result: *const c_char) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                texture_result: texture_result as usize,
                translation_result: translation_result as usize,
            }
        }

        fn record<const N: usize>(&self, operation: &'static str, arguments: [usize; N]) {
            lock(&self.calls).push(RecordedCall::new(operation, arguments));
        }

        fn calls(&self) -> Vec<RecordedCall> {
            lock(&self.calls).clone()
        }
    }

    impl UpdateBackend for RecordingRequired {
        fn request_update(&self, signature: i32, update_url: *const c_char) -> SpyResult<()> {
            self.record("request_update", [signature as usize, update_url as usize]);
            Ok(())
        }
    }

    impl WndProcBackend for RecordingRequired {
        fn register(&self, callback: Option<WndProcCallback>) -> SpyResult<()> {
            self.record(
                "wnd_proc_register",
                [callback.map_or(0, |callback| callback as usize)],
            );
            Ok(())
        }

        fn deregister(&self, callback: Option<WndProcCallback>) -> SpyResult<()> {
            self.record(
                "wnd_proc_deregister",
                [callback.map_or(0, |callback| callback as usize)],
            );
            Ok(())
        }

        fn send_to_game_only(
            &self,
            hwnd: *mut c_void,
            message: u32,
            w_param: usize,
            l_param: isize,
        ) -> SpyResult<isize> {
            self.record(
                "wnd_proc_send_to_game_only",
                [hwnd as usize, message as usize, w_param, l_param as usize],
            );
            Ok(WND_PROC_RESULT)
        }
    }

    impl InputBindBackend for RecordingRequired {
        fn invoke(&self, identifier: *const c_char, is_release: u8) -> SpyResult<()> {
            self.record(
                "input_binds_invoke",
                [identifier as usize, is_release as usize],
            );
            Ok(())
        }

        fn register_with_string(
            &self,
            identifier: *const c_char,
            callback: Option<InputBindCallbackV2>,
            bind: *const c_char,
        ) -> SpyResult<()> {
            self.record(
                "input_binds_register_with_string",
                [
                    identifier as usize,
                    callback.map_or(0, |callback| callback as usize),
                    bind as usize,
                ],
            );
            Ok(())
        }

        fn register_with_struct(
            &self,
            identifier: *const c_char,
            callback: Option<InputBindCallbackV2>,
            bind: InputBindV1,
        ) -> SpyResult<()> {
            self.record(
                "input_binds_register_with_struct",
                [
                    identifier as usize,
                    callback.map_or(0, |callback| callback as usize),
                    bind.key as usize,
                    bind.alt as usize,
                    bind.ctrl as usize,
                    bind.shift as usize,
                ],
            );
            Ok(())
        }

        fn register_with_string_v1(
            &self,
            identifier: *const c_char,
            callback: Option<InputBindCallbackV1>,
            bind: *const c_char,
        ) -> SpyResult<()> {
            self.record(
                "input_binds_register_with_string_v1",
                [
                    identifier as usize,
                    callback.map_or(0, |callback| callback as usize),
                    bind as usize,
                ],
            );
            Ok(())
        }

        fn register_with_struct_v1(
            &self,
            identifier: *const c_char,
            callback: Option<InputBindCallbackV1>,
            bind: InputBindV1,
        ) -> SpyResult<()> {
            self.record(
                "input_binds_register_with_struct_v1",
                [
                    identifier as usize,
                    callback.map_or(0, |callback| callback as usize),
                    bind.key as usize,
                    bind.alt as usize,
                    bind.ctrl as usize,
                    bind.shift as usize,
                ],
            );
            Ok(())
        }

        fn deregister(&self, identifier: *const c_char) -> SpyResult<()> {
            self.record("input_binds_deregister", [identifier as usize]);
            Ok(())
        }
    }

    impl GameBindBackend for RecordingRequired {
        fn press_async(&self, bind: GameBind) -> SpyResult<()> {
            self.record("game_binds_press_async", [bind.0 as usize]);
            Ok(())
        }

        fn release_async(&self, bind: GameBind) -> SpyResult<()> {
            self.record("game_binds_release_async", [bind.0 as usize]);
            Ok(())
        }

        fn invoke_async(&self, bind: GameBind, duration: i32) -> SpyResult<()> {
            self.record(
                "game_binds_invoke_async",
                [bind.0 as usize, duration as usize],
            );
            Ok(())
        }

        fn press(&self, bind: GameBind) -> SpyResult<()> {
            self.record("game_binds_press", [bind.0 as usize]);
            Ok(())
        }

        fn release(&self, bind: GameBind) -> SpyResult<()> {
            self.record("game_binds_release", [bind.0 as usize]);
            Ok(())
        }

        fn is_bound(&self, bind: GameBind) -> SpyResult<u8> {
            self.record("game_binds_is_bound", [bind.0 as usize]);
            Ok(GAME_BIND_RESULT)
        }
    }

    impl TextureBackend for RecordingRequired {
        fn get(&self, identifier: *const c_char) -> SpyResult<*mut Texture> {
            self.record("textures_get", [identifier as usize]);
            Ok(self.texture_result as *mut Texture)
        }

        fn get_or_create_from_file(
            &self,
            identifier: *const c_char,
            filename: *const c_char,
        ) -> SpyResult<*mut Texture> {
            self.record(
                "textures_get_or_create_from_file",
                [identifier as usize, filename as usize],
            );
            Ok(self.texture_result as *mut Texture)
        }

        fn get_or_create_from_resource(
            &self,
            identifier: *const c_char,
            resource_id: u32,
            module: *mut c_void,
        ) -> SpyResult<*mut Texture> {
            self.record(
                "textures_get_or_create_from_resource",
                [identifier as usize, resource_id as usize, module as usize],
            );
            Ok(self.texture_result as *mut Texture)
        }

        fn get_or_create_from_url(
            &self,
            identifier: *const c_char,
            remote: *const c_char,
            endpoint: *const c_char,
        ) -> SpyResult<*mut Texture> {
            self.record(
                "textures_get_or_create_from_url",
                [identifier as usize, remote as usize, endpoint as usize],
            );
            Ok(self.texture_result as *mut Texture)
        }

        fn get_or_create_from_memory(
            &self,
            identifier: *const c_char,
            data: *mut c_void,
            size: usize,
        ) -> SpyResult<*mut Texture> {
            self.record(
                "textures_get_or_create_from_memory",
                [identifier as usize, data as usize, size],
            );
            Ok(self.texture_result as *mut Texture)
        }

        fn load_from_file(
            &self,
            identifier: *const c_char,
            filename: *const c_char,
            callback: Option<ReceiveTexture>,
        ) -> SpyResult<()> {
            self.record(
                "textures_load_from_file",
                [
                    identifier as usize,
                    filename as usize,
                    callback.map_or(0, |callback| callback as usize),
                ],
            );
            Ok(())
        }

        fn load_from_resource(
            &self,
            identifier: *const c_char,
            resource_id: u32,
            module: *mut c_void,
            callback: Option<ReceiveTexture>,
        ) -> SpyResult<()> {
            self.record(
                "textures_load_from_resource",
                [
                    identifier as usize,
                    resource_id as usize,
                    module as usize,
                    callback.map_or(0, |callback| callback as usize),
                ],
            );
            Ok(())
        }

        fn load_from_url(
            &self,
            identifier: *const c_char,
            remote: *const c_char,
            endpoint: *const c_char,
            callback: Option<ReceiveTexture>,
        ) -> SpyResult<()> {
            self.record(
                "textures_load_from_url",
                [
                    identifier as usize,
                    remote as usize,
                    endpoint as usize,
                    callback.map_or(0, |callback| callback as usize),
                ],
            );
            Ok(())
        }

        fn load_from_memory(
            &self,
            identifier: *const c_char,
            data: *mut c_void,
            size: usize,
            callback: Option<ReceiveTexture>,
        ) -> SpyResult<()> {
            self.record(
                "textures_load_from_memory",
                [
                    identifier as usize,
                    data as usize,
                    size,
                    callback.map_or(0, |callback| callback as usize),
                ],
            );
            Ok(())
        }
    }

    impl LocalizationBackend for RecordingRequired {
        fn translate(&self, identifier: *const c_char) -> SpyResult<*const c_char> {
            self.record("localization_translate", [identifier as usize]);
            Ok(self.translation_result as *const c_char)
        }

        fn translate_to(
            &self,
            identifier: *const c_char,
            language: *const c_char,
        ) -> SpyResult<*const c_char> {
            self.record(
                "localization_translate_to",
                [identifier as usize, language as usize],
            );
            Ok(self.translation_result as *const c_char)
        }

        fn set_translated_string(
            &self,
            identifier: *const c_char,
            language: *const c_char,
            value: *const c_char,
        ) -> SpyResult<()> {
            self.record(
                "localization_set_translated_string",
                [identifier as usize, language as usize, value as usize],
            );
            Ok(())
        }
    }

    impl FontBackend for RecordingRequired {
        fn get(&self, identifier: *const c_char, callback: Option<ReceiveFont>) -> SpyResult<()> {
            self.record(
                "fonts_get",
                [
                    identifier as usize,
                    callback.map_or(0, |callback| callback as usize),
                ],
            );
            Ok(())
        }

        fn release(
            &self,
            identifier: *const c_char,
            callback: Option<ReceiveFont>,
        ) -> SpyResult<()> {
            self.record(
                "fonts_release",
                [
                    identifier as usize,
                    callback.map_or(0, |callback| callback as usize),
                ],
            );
            Ok(())
        }

        fn add_from_file(
            &self,
            identifier: *const c_char,
            size: f32,
            filename: *const c_char,
            callback: Option<ReceiveFont>,
            config: *mut c_void,
        ) -> SpyResult<()> {
            self.record(
                "fonts_add_from_file",
                [
                    identifier as usize,
                    size.to_bits() as usize,
                    filename as usize,
                    callback.map_or(0, |callback| callback as usize),
                    config as usize,
                ],
            );
            Ok(())
        }

        fn add_from_resource(
            &self,
            identifier: *const c_char,
            size: f32,
            resource_id: u32,
            module: *mut c_void,
            callback: Option<ReceiveFont>,
            config: *mut c_void,
        ) -> SpyResult<()> {
            self.record(
                "fonts_add_from_resource",
                [
                    identifier as usize,
                    size.to_bits() as usize,
                    resource_id as usize,
                    module as usize,
                    callback.map_or(0, |callback| callback as usize),
                    config as usize,
                ],
            );
            Ok(())
        }

        fn add_from_memory(
            &self,
            identifier: *const c_char,
            size: f32,
            data: *mut c_void,
            data_size: usize,
            callback: Option<ReceiveFont>,
            config: *mut c_void,
        ) -> SpyResult<()> {
            self.record(
                "fonts_add_from_memory",
                [
                    identifier as usize,
                    size.to_bits() as usize,
                    data as usize,
                    data_size,
                    callback.map_or(0, |callback| callback as usize),
                    config as usize,
                ],
            );
            Ok(())
        }

        fn resize(&self, identifier: *const c_char, size: f32) -> SpyResult<()> {
            self.record(
                "fonts_resize",
                [identifier as usize, size.to_bits() as usize],
            );
            Ok(())
        }
    }

    struct NoOwners;

    impl AddressOwnerResolver for NoOwners {
        fn owner_for_address(&self, _address: NonZeroUsize) -> Option<OwnerToken> {
            None
        }

        fn is_current_owner(&self, _owner: OwnerToken) -> bool {
            false
        }
    }

    struct RejectMappings;

    impl MappingBackend for RejectMappings {
        fn open_or_create(
            &self,
            _name: &str,
            _size: NonZeroUsize,
        ) -> Result<Arc<dyn MappingView>, MappingFailure> {
            Err(MappingFailure::UnsupportedPlatform)
        }
    }

    /// Accepts every caller, so a composed adapter can serve a real call.
    struct OneOwner(OwnerToken);

    impl AddressOwnerResolver for OneOwner {
        fn owner_for_address(&self, _address: NonZeroUsize) -> Option<OwnerToken> {
            Some(self.0)
        }

        fn is_current_owner(&self, _owner: OwnerToken) -> bool {
            true
        }
    }

    struct NoGameMessages;

    impl GameOnlyMessageSink for NoGameMessages {
        fn send_to_game_only(
            &self,
            _message: u32,
            _w_param: usize,
            _l_param: isize,
        ) -> Result<(), GameSinkError> {
            Ok(())
        }
    }

    struct NoTextures;

    impl TextureServiceFacade for NoTextures {
        fn get(&self, _identifier: &str) -> Result<Option<TextureHandle>, TextureFacadeError> {
            Ok(None)
        }

        fn load_with_source(
            &self,
            _identifier: &str,
            _options: LoadOptions,
            _callback: Option<TextureCallback>,
            _source: TextureSourceFactory<'_>,
            _failure_policy: TextureSourceFailurePolicy,
        ) -> Result<RequestOutcome, TextureFacadeError> {
            Err(TextureFacadeError::Rejected)
        }

        fn cleanup_owner_generation(
            &self,
            _owner: OwnerGeneration,
        ) -> Result<usize, TextureFacadeError> {
            Ok(0)
        }
    }

    struct NoFonts;

    impl RenderFontService for NoFonts {
        fn get(
            &self,
            _owner: OwnerId,
            _identifier: String,
            _callback: SendFontCallback,
        ) -> RequiredServiceResult<FontGetResult> {
            Err(BackendOperationError::ServiceRejected)
        }

        fn release(
            &self,
            _identifier: String,
            _subscription: SubscriptionId,
        ) -> RequiredServiceResult<bool> {
            Ok(false)
        }

        fn add_from_file(
            &self,
            _owner: OwnerId,
            _identifier: String,
            _size: f32,
            _filename: PathBuf,
            _callback: Option<SendFontCallback>,
            _config: FontConfig,
        ) -> RequiredServiceResult<FontRegistration> {
            Err(BackendOperationError::ServiceRejected)
        }

        fn add_from_resource(
            &self,
            _owner: OwnerId,
            _identifier: String,
            _size: f32,
            _resource: ResourceFont,
            _callback: Option<SendFontCallback>,
            _config: FontConfig,
        ) -> RequiredServiceResult<FontRegistration> {
            Err(BackendOperationError::ServiceRejected)
        }

        fn add_from_memory(
            &self,
            _owner: OwnerId,
            _identifier: String,
            _size: f32,
            _data: Vec<u8>,
            _callback: Option<SendFontCallback>,
            _config: FontConfig,
        ) -> RequiredServiceResult<FontRegistration> {
            Err(BackendOperationError::ServiceRejected)
        }

        fn resize(&self, _identifier: String, _size: f32) -> RequiredServiceResult<bool> {
            Ok(false)
        }

        fn cleanup_owner(&self, _owner: OwnerId) -> RequiredServiceResult<usize> {
            Ok(0)
        }

        fn cleanup_owner_callbacks(&self, _owner: OwnerId) -> RequiredServiceResult<usize> {
            Ok(0)
        }

        fn cleanup_owner_resources(&self, _owner: OwnerId) -> RequiredServiceResult<usize> {
            Ok(0)
        }
    }

    /// `compose` is the only place the thirteen adapters are wired, so a runtime cannot
    /// half-install the API. This drives one real call through a fully composed backend:
    /// before `compose` existed, `ProductionAddonApiBackend` had no non-test constructor at
    /// all, so nothing proved the bundles could be built from the services a runtime owns.
    #[test]
    fn compose_builds_a_backend_that_serves_a_real_call() {
        let owner = OwnerToken {
            signature: 0x1234,
            generation: 1,
        };
        let failures = Arc::new(BackendFailures::new());
        let callers = Arc::new(AddonCallerResolver::new(Arc::new(OneOwner(owner))));
        let boundary = Arc::new(NativeCallBoundary::new(
            callers,
            NativeMemoryReader::default(),
            Arc::clone(&failures),
        ));

        let root = std::env::temp_dir().join("nexus-addon-backend-compose");
        let path_index = PathIndex::prepare(PathRoots::new(
            root.join("Nexus.dll"),
            root.join("system"),
            root.join("documents"),
        ))
        .expect("test paths should be representable");
        let expected = path_index.get(PathKey::GameDirectory).to_path_buf();

        let backend = ProductionAddonApiBackend::compose(
            boundary,
            failures,
            AddonApiServices {
                ui_host: Arc::new(UiHost::default()),
                logs: Arc::new(LogRegistry::new()),
                paths: Arc::new(
                    StablePathStore::from_index(&path_index, 4)
                        .expect("test paths should have no interior nul"),
                ),
                inline_hooks: Arc::new(InlineHookService::new()),
                events: Arc::new(EventService::new()),
                data_link: Arc::new(DataLinkService::new(Arc::new(RejectMappings))),
                wnd_proc_callbacks: Arc::new(RawWndProcRegistry::new(CallbackLimits::default())),
                game_messages: Arc::new(NoGameMessages),
                input_binds: Arc::new(ManagedInputBinds::new(
                    Arc::new(InlineExecutor),
                    CallbackLimits::default(),
                )),
                game_invoker: Arc::new(Mutex::new(None)),
                game_scheduler: None,
                textures: Arc::new(NoTextures),
                localization: Arc::new(Mutex::new(
                    LocalizationService::new("en", 8).expect("test localization should start"),
                )),
                fonts: Arc::new(NoFonts),
            },
        );

        // A path getter is the cheapest call that proves attribution, the boundary and a
        // core adapter are all wired: the reference never returns null here.
        let pointer = backend.paths_get_game_directory();
        assert!(
            !pointer.is_null(),
            "a composed backend must serve the path getters"
        );
        // SAFETY: the pointer is process-lifetime storage owned by the composed backend.
        let value = unsafe { CStr::from_ptr(pointer) };
        assert_eq!(value.to_string_lossy(), expected.to_string_lossy());
    }

    fn unused_core() -> CoreAddonApiServices {
        let callers = Arc::new(AddonCallerResolver::new(Arc::new(NoOwners)));
        let boundary = Arc::new(NativeCallBoundary::new(
            callers,
            NativeMemoryReader::default(),
            Arc::new(BackendFailures::new()),
        ));
        let root = std::env::temp_dir().join("nexus-addon-backend-forwarding");
        let path_index = PathIndex::prepare(PathRoots::new(
            root.join("Nexus.dll"),
            root.join("system"),
            root.join("documents"),
        ))
        .expect("test paths should be representable");
        let paths = Arc::new(
            StablePathStore::from_index(&path_index, 4)
                .expect("test paths should have no interior nul"),
        );

        CoreAddonApiServices::new(
            Arc::new(UiApi::new(
                Arc::clone(&boundary),
                Arc::new(UiHost::default()),
            )),
            Arc::new(LoggingApi::new(
                Arc::clone(&boundary),
                Arc::new(LogRegistry::new()),
            )),
            Arc::new(PathApi::new(Arc::clone(&boundary), paths)),
            Arc::new(InlineHookApi::new(
                Arc::clone(&boundary),
                Arc::new(InlineHookService::new()),
            )),
            Arc::new(EventApi::new(
                Arc::clone(&boundary),
                Arc::new(EventService::new()),
            )),
            Arc::new(DataLinkApi::new(
                boundary,
                Arc::new(DataLinkService::new(Arc::new(RejectMappings))),
            )),
        )
    }

    fn required_services(recorder: &Arc<RecordingRequired>) -> RequiredAddonApiServices {
        let updates: Arc<dyn UpdateBackend> = Arc::<RecordingRequired>::clone(recorder);
        let wnd_proc: Arc<dyn WndProcBackend> = Arc::<RecordingRequired>::clone(recorder);
        let input_binds: Arc<dyn InputBindBackend> = Arc::<RecordingRequired>::clone(recorder);
        let game_binds: Arc<dyn GameBindBackend> = Arc::<RecordingRequired>::clone(recorder);
        let textures: Arc<dyn TextureBackend> = Arc::<RecordingRequired>::clone(recorder);
        let localization: Arc<dyn LocalizationBackend> = Arc::<RecordingRequired>::clone(recorder);
        let fonts: Arc<dyn FontBackend> = Arc::<RecordingRequired>::clone(recorder);

        RequiredAddonApiServices::new(
            updates,
            wnd_proc,
            input_binds,
            game_binds,
            textures,
            localization,
            fonts,
        )
    }

    unsafe extern "C" fn wnd_proc_callback(
        _hwnd: *mut c_void,
        _message: u32,
        _w_param: usize,
        _l_param: isize,
    ) -> u32 {
        0
    }

    unsafe extern "C" fn input_callback_v1(_identifier: *const c_char) {}

    unsafe extern "C" fn input_callback_v2(_identifier: *const c_char, _is_release: u8) {}

    unsafe extern "C" fn receive_texture(_identifier: *const c_char, _texture: *mut Texture) {}

    unsafe extern "C" fn receive_font(_identifier: *const c_char, _font: *mut c_void) {}

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[test]
    fn production_type_implements_the_complete_native_backend_contract() {
        fn assert_backend<T: AddonApiBackend>() {}

        assert_backend::<ProductionAddonApiBackend>();
    }

    #[test]
    fn required_services_receive_all_34_operations_with_exact_arguments() {
        let mut texture = Box::new(Texture {
            width: 19,
            height: 23,
            resource: ptr::null_mut(),
        });
        let texture_pointer = ptr::from_mut(texture.as_mut());
        let translation = CString::new("translated value").expect("test translation");
        let recorder = Arc::new(RecordingRequired::new(
            texture_pointer,
            translation.as_ptr(),
        ));
        let backend = ProductionAddonApiBackend::new(unused_core(), required_services(&recorder));

        let identifier = CString::new("test.identifier").expect("test identifier");
        let filename = CString::new("test.file").expect("test filename");
        let bind_text = CString::new("CTRL+SHIFT+K").expect("test bind");
        let remote = CString::new("https://example.invalid").expect("test remote");
        let endpoint = CString::new("asset.png").expect("test endpoint");
        let language = CString::new("test-language").expect("test language");
        let translated_value = CString::new("stored value").expect("test value");
        let mut module = Box::new(0_u8);
        let module_pointer = ptr::from_mut(module.as_mut()).cast::<c_void>();
        let mut data = vec![1_u8, 2, 3, 4, 5, 6, 7];
        let data_pointer = data.as_mut_ptr().cast::<c_void>();
        let mut config = Box::new(0_u8);
        let config_pointer = ptr::from_mut(config.as_mut()).cast::<c_void>();
        let bind = InputBindV1 {
            key: 0x1234,
            alt: 1,
            ctrl: 0,
            shift: 1,
        };

        backend.request_update(-17, remote.as_ptr());
        backend.wnd_proc_register(Some(wnd_proc_callback));
        backend.wnd_proc_deregister(Some(wnd_proc_callback));
        assert_eq!(
            backend.wnd_proc_send_to_game_only(module_pointer, 0x2222, 0x3333, -31),
            WND_PROC_RESULT
        );

        backend.input_binds_invoke(identifier.as_ptr(), 1);
        backend.input_binds_register_with_string(
            identifier.as_ptr(),
            Some(input_callback_v2),
            bind_text.as_ptr(),
        );
        backend.input_binds_register_with_struct(
            identifier.as_ptr(),
            Some(input_callback_v2),
            bind,
        );
        backend.input_binds_register_with_string_v1(
            identifier.as_ptr(),
            Some(input_callback_v1),
            bind_text.as_ptr(),
        );
        backend.input_binds_register_with_struct_v1(
            identifier.as_ptr(),
            Some(input_callback_v1),
            bind,
        );
        backend.input_binds_deregister(identifier.as_ptr());

        backend.game_binds_press_async(GameBind(101));
        backend.game_binds_release_async(GameBind(102));
        backend.game_binds_invoke_async(GameBind(103), -55);
        backend.game_binds_press(GameBind(104));
        backend.game_binds_release(GameBind(105));
        assert_eq!(backend.game_binds_is_bound(GameBind(106)), GAME_BIND_RESULT);

        assert_eq!(backend.textures_get(identifier.as_ptr()), texture_pointer);
        assert_eq!(
            backend.textures_get_or_create_from_file(identifier.as_ptr(), filename.as_ptr()),
            texture_pointer
        );
        assert_eq!(
            backend.textures_get_or_create_from_resource(identifier.as_ptr(), 201, module_pointer,),
            texture_pointer
        );
        assert_eq!(
            backend.textures_get_or_create_from_url(
                identifier.as_ptr(),
                remote.as_ptr(),
                endpoint.as_ptr(),
            ),
            texture_pointer
        );
        assert_eq!(
            backend.textures_get_or_create_from_memory(
                identifier.as_ptr(),
                data_pointer,
                data.len(),
            ),
            texture_pointer
        );
        backend.textures_load_from_file(
            identifier.as_ptr(),
            filename.as_ptr(),
            Some(receive_texture),
        );
        backend.textures_load_from_resource(
            identifier.as_ptr(),
            202,
            module_pointer,
            Some(receive_texture),
        );
        backend.textures_load_from_url(
            identifier.as_ptr(),
            remote.as_ptr(),
            endpoint.as_ptr(),
            Some(receive_texture),
        );
        backend.textures_load_from_memory(
            identifier.as_ptr(),
            data_pointer,
            data.len(),
            Some(receive_texture),
        );

        assert_eq!(
            backend.localization_translate(identifier.as_ptr()),
            translation.as_ptr()
        );
        assert_eq!(
            backend.localization_translate_to(identifier.as_ptr(), language.as_ptr()),
            translation.as_ptr()
        );
        backend.localization_set_translated_string(
            identifier.as_ptr(),
            language.as_ptr(),
            translated_value.as_ptr(),
        );

        backend.fonts_get(identifier.as_ptr(), Some(receive_font));
        backend.fonts_release(identifier.as_ptr(), Some(receive_font));
        backend.fonts_add_from_file(
            identifier.as_ptr(),
            12.5,
            filename.as_ptr(),
            Some(receive_font),
            config_pointer,
        );
        backend.fonts_add_from_resource(
            identifier.as_ptr(),
            13.5,
            301,
            module_pointer,
            Some(receive_font),
            config_pointer,
        );
        backend.fonts_add_from_memory(
            identifier.as_ptr(),
            14.5,
            data_pointer,
            data.len(),
            Some(receive_font),
            config_pointer,
        );
        backend.fonts_resize(identifier.as_ptr(), 15.5);

        assert_eq!(
            recorder.calls(),
            vec![
                RecordedCall::new(
                    "request_update",
                    [(-17_i32) as usize, remote.as_ptr() as usize]
                ),
                RecordedCall::new(
                    "wnd_proc_register",
                    [wnd_proc_callback as *const () as usize],
                ),
                RecordedCall::new(
                    "wnd_proc_deregister",
                    [wnd_proc_callback as *const () as usize],
                ),
                RecordedCall::new(
                    "wnd_proc_send_to_game_only",
                    [
                        module_pointer as usize,
                        0x2222,
                        0x3333,
                        (-31_isize) as usize
                    ],
                ),
                RecordedCall::new("input_binds_invoke", [identifier.as_ptr() as usize, 1],),
                RecordedCall::new(
                    "input_binds_register_with_string",
                    [
                        identifier.as_ptr() as usize,
                        input_callback_v2 as *const () as usize,
                        bind_text.as_ptr() as usize,
                    ],
                ),
                RecordedCall::new(
                    "input_binds_register_with_struct",
                    [
                        identifier.as_ptr() as usize,
                        input_callback_v2 as *const () as usize,
                        bind.key as usize,
                        bind.alt as usize,
                        bind.ctrl as usize,
                        bind.shift as usize,
                    ],
                ),
                RecordedCall::new(
                    "input_binds_register_with_string_v1",
                    [
                        identifier.as_ptr() as usize,
                        input_callback_v1 as *const () as usize,
                        bind_text.as_ptr() as usize,
                    ],
                ),
                RecordedCall::new(
                    "input_binds_register_with_struct_v1",
                    [
                        identifier.as_ptr() as usize,
                        input_callback_v1 as *const () as usize,
                        bind.key as usize,
                        bind.alt as usize,
                        bind.ctrl as usize,
                        bind.shift as usize,
                    ],
                ),
                RecordedCall::new("input_binds_deregister", [identifier.as_ptr() as usize]),
                RecordedCall::new("game_binds_press_async", [101]),
                RecordedCall::new("game_binds_release_async", [102]),
                RecordedCall::new("game_binds_invoke_async", [103, (-55_i32) as usize]),
                RecordedCall::new("game_binds_press", [104]),
                RecordedCall::new("game_binds_release", [105]),
                RecordedCall::new("game_binds_is_bound", [106]),
                RecordedCall::new("textures_get", [identifier.as_ptr() as usize]),
                RecordedCall::new(
                    "textures_get_or_create_from_file",
                    [identifier.as_ptr() as usize, filename.as_ptr() as usize],
                ),
                RecordedCall::new(
                    "textures_get_or_create_from_resource",
                    [identifier.as_ptr() as usize, 201, module_pointer as usize],
                ),
                RecordedCall::new(
                    "textures_get_or_create_from_url",
                    [
                        identifier.as_ptr() as usize,
                        remote.as_ptr() as usize,
                        endpoint.as_ptr() as usize,
                    ],
                ),
                RecordedCall::new(
                    "textures_get_or_create_from_memory",
                    [
                        identifier.as_ptr() as usize,
                        data_pointer as usize,
                        data.len()
                    ],
                ),
                RecordedCall::new(
                    "textures_load_from_file",
                    [
                        identifier.as_ptr() as usize,
                        filename.as_ptr() as usize,
                        receive_texture as *const () as usize,
                    ],
                ),
                RecordedCall::new(
                    "textures_load_from_resource",
                    [
                        identifier.as_ptr() as usize,
                        202,
                        module_pointer as usize,
                        receive_texture as *const () as usize,
                    ],
                ),
                RecordedCall::new(
                    "textures_load_from_url",
                    [
                        identifier.as_ptr() as usize,
                        remote.as_ptr() as usize,
                        endpoint.as_ptr() as usize,
                        receive_texture as *const () as usize,
                    ],
                ),
                RecordedCall::new(
                    "textures_load_from_memory",
                    [
                        identifier.as_ptr() as usize,
                        data_pointer as usize,
                        data.len(),
                        receive_texture as *const () as usize,
                    ],
                ),
                RecordedCall::new("localization_translate", [identifier.as_ptr() as usize]),
                RecordedCall::new(
                    "localization_translate_to",
                    [identifier.as_ptr() as usize, language.as_ptr() as usize],
                ),
                RecordedCall::new(
                    "localization_set_translated_string",
                    [
                        identifier.as_ptr() as usize,
                        language.as_ptr() as usize,
                        translated_value.as_ptr() as usize,
                    ],
                ),
                RecordedCall::new(
                    "fonts_get",
                    [
                        identifier.as_ptr() as usize,
                        receive_font as *const () as usize,
                    ],
                ),
                RecordedCall::new(
                    "fonts_release",
                    [
                        identifier.as_ptr() as usize,
                        receive_font as *const () as usize,
                    ],
                ),
                RecordedCall::new(
                    "fonts_add_from_file",
                    [
                        identifier.as_ptr() as usize,
                        12.5_f32.to_bits() as usize,
                        filename.as_ptr() as usize,
                        receive_font as *const () as usize,
                        config_pointer as usize,
                    ],
                ),
                RecordedCall::new(
                    "fonts_add_from_resource",
                    [
                        identifier.as_ptr() as usize,
                        13.5_f32.to_bits() as usize,
                        301,
                        module_pointer as usize,
                        receive_font as *const () as usize,
                        config_pointer as usize,
                    ],
                ),
                RecordedCall::new(
                    "fonts_add_from_memory",
                    [
                        identifier.as_ptr() as usize,
                        14.5_f32.to_bits() as usize,
                        data_pointer as usize,
                        data.len(),
                        receive_font as *const () as usize,
                        config_pointer as usize,
                    ],
                ),
                RecordedCall::new(
                    "fonts_resize",
                    [identifier.as_ptr() as usize, 15.5_f32.to_bits() as usize],
                ),
            ]
        );
    }
}
