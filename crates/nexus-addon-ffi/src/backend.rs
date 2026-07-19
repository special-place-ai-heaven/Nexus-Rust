use core::ffi::{c_char, c_void};

use nexus_abi::{
    EventCallback, GameBind, InputBindCallbackV1, InputBindCallbackV2, InputBindV1, LogLevel,
    MinHookStatus, ReceiveFont, ReceiveTexture, RenderCallback, RenderPhase, Texture,
    WndProcCallback,
};

/// Required process service behind every native add-on API revision.
///
/// The dispatcher may call this object from arbitrary add-on threads and may
/// reenter it when a backend operation itself invokes an API shim. It clones
/// the active `Arc` and releases dispatcher locks before every call. Backends
/// must therefore be thread-safe and reentrancy-safe.
///
/// All native strings, callbacks, handles, and buffers are intentionally passed
/// through as opaque values. The dispatcher never dereferences or logs them.
/// The backend owns validation, bounded copying, callback-lifetime tracking,
/// and any unsafe native access required by a service implementation.
pub trait AddonApiBackend: Send + Sync + 'static {
    /// Registers a render callback for a phase.
    fn renderer_register(&self, phase: RenderPhase, callback: Option<RenderCallback>);
    /// Deregisters a render callback.
    fn renderer_deregister(&self, callback: Option<RenderCallback>);
    /// Requests an add-on update using the native signature and URL pointer.
    fn request_update(&self, signature: i32, update_url: *const c_char);

    /// Handles the revision-2-and-newer logging contract.
    fn log(&self, level: LogLevel, channel: *const c_char, message: *const c_char);
    /// Handles the revision-1 two-argument logging contract.
    fn log_v1(&self, level: LogLevel, message: *const c_char);

    /// Sends an on-screen alert.
    fn ui_send_alert(&self, message: *const c_char);
    /// Registers a close-on-Escape boolean.
    ///
    /// # Safety
    ///
    /// On successful registration, `state` must remain the same live, writable
    /// one-byte allocation until deregistration returns or owner cleanup has
    /// drained the registration. Every other access to the byte must be
    /// synchronized so it cannot conflict with backend reads or writes. Known
    /// foreign add-on image addresses are rejected, while heap and TLS storage
    /// necessarily rely on this native caller proof.
    unsafe fn ui_register_close_on_escape(&self, identifier: *const c_char, state: *mut u8);
    /// Deregisters a close-on-Escape boolean.
    fn ui_deregister_close_on_escape(&self, identifier: *const c_char);

    /// Returns the process-lifetime game-directory string owned by the backend.
    fn paths_get_game_directory(&self) -> *const c_char;
    /// Returns an add-on-directory string whose lifetime is owned by the backend.
    fn paths_get_addon_directory(&self, name: *const c_char) -> *const c_char;
    /// Returns the process-lifetime common-directory string owned by the backend.
    fn paths_get_common_directory(&self) -> *const c_char;

    /// Creates a MinHook-compatible hook.
    ///
    /// # Safety
    ///
    /// `target` and `detour` must denote live functions with compatible ABIs
    /// and signatures for the full hook lifetime. A non-null `original` must
    /// denote one live, aligned, exclusively writable pointer-sized object.
    /// Every user of the published trampoline must have returned before hook
    /// removal or owner cleanup can destroy it.
    unsafe fn min_hook_create(
        &self,
        target: *mut c_void,
        detour: *mut c_void,
        original: *mut *mut c_void,
    ) -> MinHookStatus;
    /// Removes a MinHook-compatible hook.
    fn min_hook_remove(&self, target: *mut c_void) -> MinHookStatus;
    /// Enables a MinHook-compatible hook.
    fn min_hook_enable(&self, target: *mut c_void) -> MinHookStatus;
    /// Disables a MinHook-compatible hook.
    fn min_hook_disable(&self, target: *mut c_void) -> MinHookStatus;

    /// Raises an untargeted event with an opaque payload.
    fn events_raise(&self, identifier: *const c_char, payload: *mut c_void);
    /// Raises an untargeted notification event.
    fn events_raise_notification(&self, identifier: *const c_char);
    /// Raises a signature-targeted event with an opaque payload.
    fn events_raise_targeted(
        &self,
        signature: u32,
        identifier: *const c_char,
        payload: *mut c_void,
    );
    /// Raises a signature-targeted notification event.
    fn events_raise_notification_targeted(&self, signature: u32, identifier: *const c_char);
    /// Subscribes a callback to an event.
    fn events_subscribe(&self, identifier: *const c_char, callback: Option<EventCallback>);
    /// Unsubscribes a callback from an event.
    fn events_unsubscribe(&self, identifier: *const c_char, callback: Option<EventCallback>);

    /// Registers a window-procedure callback.
    fn wnd_proc_register(&self, callback: Option<WndProcCallback>);
    /// Deregisters a window-procedure callback.
    fn wnd_proc_deregister(&self, callback: Option<WndProcCallback>);
    /// Sends one message directly to the game window procedure.
    fn wnd_proc_send_to_game_only(
        &self,
        hwnd: *mut c_void,
        message: u32,
        w_param: usize,
        l_param: isize,
    ) -> isize;

    /// Invokes a modern input bind.
    fn input_binds_invoke(&self, identifier: *const c_char, is_release: u8);
    /// Registers a modern string-described input bind.
    fn input_binds_register_with_string(
        &self,
        identifier: *const c_char,
        callback: Option<InputBindCallbackV2>,
        bind: *const c_char,
    );
    /// Registers a modern structured input bind.
    fn input_binds_register_with_struct(
        &self,
        identifier: *const c_char,
        callback: Option<InputBindCallbackV2>,
        bind: InputBindV1,
    );
    /// Registers a revision-1-through-3 string-described input bind.
    fn input_binds_register_with_string_v1(
        &self,
        identifier: *const c_char,
        callback: Option<InputBindCallbackV1>,
        bind: *const c_char,
    );
    /// Registers a revision-1-through-3 structured input bind.
    fn input_binds_register_with_struct_v1(
        &self,
        identifier: *const c_char,
        callback: Option<InputBindCallbackV1>,
        bind: InputBindV1,
    );
    /// Deregisters an input bind.
    fn input_binds_deregister(&self, identifier: *const c_char);

    /// Asynchronously presses a game bind.
    fn game_binds_press_async(&self, bind: GameBind);
    /// Asynchronously releases a game bind.
    fn game_binds_release_async(&self, bind: GameBind);
    /// Asynchronously invokes a game bind for a duration.
    fn game_binds_invoke_async(&self, bind: GameBind, duration: i32);
    /// Synchronously presses a game bind.
    fn game_binds_press(&self, bind: GameBind);
    /// Synchronously releases a game bind.
    fn game_binds_release(&self, bind: GameBind);
    /// Returns the native byte-boolean bound state.
    fn game_binds_is_bound(&self, bind: GameBind) -> u8;

    /// Gets a named data-link resource.
    fn data_link_get(&self, identifier: *const c_char) -> *mut c_void;
    /// Shares or gets a named data-link resource.
    fn data_link_share(&self, identifier: *const c_char, size: usize) -> *mut c_void;

    /// Gets a texture descriptor.
    fn textures_get(&self, identifier: *const c_char) -> *mut Texture;
    /// Gets or creates a texture from a file.
    fn textures_get_or_create_from_file(
        &self,
        identifier: *const c_char,
        filename: *const c_char,
    ) -> *mut Texture;
    /// Gets or creates a texture from a native resource.
    fn textures_get_or_create_from_resource(
        &self,
        identifier: *const c_char,
        resource_id: u32,
        module: *mut c_void,
    ) -> *mut Texture;
    /// Gets or creates a texture from a URL.
    fn textures_get_or_create_from_url(
        &self,
        identifier: *const c_char,
        remote: *const c_char,
        endpoint: *const c_char,
    ) -> *mut Texture;
    /// Gets or creates a texture from an opaque memory buffer.
    fn textures_get_or_create_from_memory(
        &self,
        identifier: *const c_char,
        data: *mut c_void,
        size: usize,
    ) -> *mut Texture;
    /// Starts an asynchronous file texture load.
    fn textures_load_from_file(
        &self,
        identifier: *const c_char,
        filename: *const c_char,
        callback: Option<ReceiveTexture>,
    );
    /// Starts an asynchronous resource texture load.
    fn textures_load_from_resource(
        &self,
        identifier: *const c_char,
        resource_id: u32,
        module: *mut c_void,
        callback: Option<ReceiveTexture>,
    );
    /// Starts an asynchronous URL texture load.
    fn textures_load_from_url(
        &self,
        identifier: *const c_char,
        remote: *const c_char,
        endpoint: *const c_char,
        callback: Option<ReceiveTexture>,
    );
    /// Starts an asynchronous memory texture load.
    fn textures_load_from_memory(
        &self,
        identifier: *const c_char,
        data: *mut c_void,
        size: usize,
        callback: Option<ReceiveTexture>,
    );

    /// Adds a full QuickAccess shortcut.
    fn quick_access_add(
        &self,
        identifier: *const c_char,
        texture: *const c_char,
        hover_texture: *const c_char,
        input_bind: *const c_char,
        tooltip: *const c_char,
    );
    /// Removes a QuickAccess shortcut.
    fn quick_access_remove(&self, identifier: *const c_char);
    /// Pushes a QuickAccess notification.
    fn quick_access_notify(&self, identifier: *const c_char);
    /// Adds a revision-1-through-5 simple shortcut.
    fn quick_access_add_simple(&self, identifier: *const c_char, callback: Option<RenderCallback>);
    /// Adds a targeted QuickAccess context-menu item.
    fn quick_access_add_context_menu(
        &self,
        identifier: *const c_char,
        target: *const c_char,
        callback: Option<RenderCallback>,
    );
    /// Removes a QuickAccess context-menu item.
    fn quick_access_remove_context_menu(&self, identifier: *const c_char);

    /// Returns a backend-owned translation for the active language.
    fn localization_translate(&self, identifier: *const c_char) -> *const c_char;
    /// Returns a backend-owned translation for a requested language.
    fn localization_translate_to(
        &self,
        identifier: *const c_char,
        language: *const c_char,
    ) -> *const c_char;
    /// Sets a translated string.
    fn localization_set_translated_string(
        &self,
        identifier: *const c_char,
        language: *const c_char,
        value: *const c_char,
    );

    /// Gets a font and reports it through the native callback.
    fn fonts_get(&self, identifier: *const c_char, callback: Option<ReceiveFont>);
    /// Releases a font and reports completion through the native callback.
    fn fonts_release(&self, identifier: *const c_char, callback: Option<ReceiveFont>);
    /// Adds a font from a file.
    fn fonts_add_from_file(
        &self,
        identifier: *const c_char,
        size: f32,
        filename: *const c_char,
        callback: Option<ReceiveFont>,
        config: *mut c_void,
    );
    /// Adds a font from a native resource.
    fn fonts_add_from_resource(
        &self,
        identifier: *const c_char,
        size: f32,
        resource_id: u32,
        module: *mut c_void,
        callback: Option<ReceiveFont>,
        config: *mut c_void,
    );
    /// Adds a font from an opaque memory buffer.
    fn fonts_add_from_memory(
        &self,
        identifier: *const c_char,
        size: f32,
        data: *mut c_void,
        data_size: usize,
        callback: Option<ReceiveFont>,
        config: *mut c_void,
    );
    /// Resizes a registered font.
    fn fonts_resize(&self, identifier: *const c_char, size: f32);
}
