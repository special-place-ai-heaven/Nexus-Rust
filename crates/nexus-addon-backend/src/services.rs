//! Required service boundaries for the complete native add-on API.
//!
//! These traits split the compatibility surface into independently owned,
//! object-safe domains. A production backend cannot be constructed without an
//! implementation for every domain; there is intentionally no unavailable or
//! no-op service bundle.

use core::ffi::{c_char, c_void};

use nexus_abi::{
    GameBind, InputBindCallbackV1, InputBindCallbackV2, InputBindV1, ReceiveFont, ReceiveTexture,
    Texture, WndProcCallback,
};

use crate::BackendOperationError;

/// Result returned by one validated required-service operation.
pub type RequiredServiceResult<T> = Result<T, BackendOperationError>;

/// Executes legacy add-on update requests.
///
/// Implementations must attribute the actual caller, copy the URL through the
/// bounded native-memory boundary, and enqueue work without retaining the raw
/// pointer.
pub trait UpdateBackend: Send + Sync + 'static {
    /// Requests an update for one add-on signature.
    fn request_update(
        &self,
        signature: i32,
        update_url: *const c_char,
    ) -> RequiredServiceResult<()>;
}

/// Owns legacy game-window procedure callbacks and game-window posting.
///
/// Registered callbacks must be tied to one exact owner generation and hold
/// that owner's callback gate for the complete foreign call. Deregistration
/// must match both the authenticated owner generation and callback address;
/// admitted callbacks must drain before the module can unload.
pub trait WndProcBackend: Send + Sync + 'static {
    /// Registers one optional native window-procedure callback.
    fn register(&self, callback: Option<WndProcCallback>) -> RequiredServiceResult<()>;
    /// Deregisters one optional callback for its authenticated owner generation.
    fn deregister(&self, callback: Option<WndProcCallback>) -> RequiredServiceResult<()>;
    /// Posts one message to the currently attached game window.
    ///
    /// Legacy compatibility ignores `hwnd`. Messages below `WM_USER` must be
    /// translated with the legacy passthrough offset of 7997 before posting.
    /// The result is the asynchronous `PostMessage` success byte widened to
    /// `isize` (`1` for accepted, `0` for rejected), not a window-procedure
    /// result.
    fn send_to_game_only(
        &self,
        hwnd: *mut c_void,
        message: u32,
        w_param: usize,
        l_param: isize,
    ) -> RequiredServiceResult<isize>;
}

/// Owns modern and legacy add-on input-bind registrations.
///
/// Implementations must copy identifiers and textual binds synchronously.
/// Callback addresses must be attributed to an exact live owner generation;
/// every invocation must hold that owner's callback gate for the complete
/// foreign call. Composite owner cleanup must remove registrations and drain
/// admitted callbacks before module unload. A null callback still creates or
/// preserves the persisted binding, but installs no native handler.
/// Registration and replacement are authenticated-owner scoped: one owner may
/// not replace another owner's identically named handler. Publication must
/// return an exact receipt so a closing-generation race can roll back only the
/// newly published registration.
pub trait InputBindBackend: Send + Sync + 'static {
    /// Invokes one named bind.
    fn invoke(&self, identifier: *const c_char, is_release: u8) -> RequiredServiceResult<()>;
    /// Registers a modern textual bind.
    fn register_with_string(
        &self,
        identifier: *const c_char,
        callback: Option<InputBindCallbackV2>,
        bind: *const c_char,
    ) -> RequiredServiceResult<()>;
    /// Registers a modern structured bind.
    fn register_with_struct(
        &self,
        identifier: *const c_char,
        callback: Option<InputBindCallbackV2>,
        bind: InputBindV1,
    ) -> RequiredServiceResult<()>;
    /// Registers a revision-1-through-3 textual bind.
    fn register_with_string_v1(
        &self,
        identifier: *const c_char,
        callback: Option<InputBindCallbackV1>,
        bind: *const c_char,
    ) -> RequiredServiceResult<()>;
    /// Registers a revision-1-through-3 structured bind.
    fn register_with_struct_v1(
        &self,
        identifier: *const c_char,
        callback: Option<InputBindCallbackV1>,
        bind: InputBindV1,
    ) -> RequiredServiceResult<()>;
    /// Deregisters one named bind for its authenticated owner.
    fn deregister(&self, identifier: *const c_char) -> RequiredServiceResult<()>;
}

/// Executes GW2 game-bind operations.
pub trait GameBindBackend: Send + Sync + 'static {
    /// Enqueues a key press; success means the task was accepted.
    fn press_async(&self, bind: GameBind) -> RequiredServiceResult<()>;
    /// Enqueues a key release; success means the task was accepted.
    fn release_async(&self, bind: GameBind) -> RequiredServiceResult<()>;
    /// Enqueues a timed press and release; success means the task was accepted.
    /// A nonpositive duration performs press then release without a delay.
    fn invoke_async(&self, bind: GameBind, duration: i32) -> RequiredServiceResult<()>;
    /// Presses a game bind synchronously.
    fn press(&self, bind: GameBind) -> RequiredServiceResult<()>;
    /// Releases a game bind synchronously.
    fn release(&self, bind: GameBind) -> RequiredServiceResult<()>;
    /// Returns the native byte-boolean binding state.
    fn is_bound(&self, bind: GameBind) -> RequiredServiceResult<u8>;
}

/// Owns ABI texture descriptors and asynchronous texture callbacks.
///
/// Every returned descriptor and shader-resource pointer must remain stable for
/// its documented compatibility lifetime. Input strings and memory buffers must
/// be copied before the call returns, and native resources must be extracted
/// synchronously rather than retaining the module handle. Native callbacks must
/// be generation-bound, hold the owner's callback gate for their complete
/// foreign call, and drain before owner unload.
pub trait TextureBackend: Send + Sync + 'static {
    /// Gets an existing texture descriptor.
    fn get(&self, identifier: *const c_char) -> RequiredServiceResult<*mut Texture>;
    /// Gets or creates a texture from a file.
    fn get_or_create_from_file(
        &self,
        identifier: *const c_char,
        filename: *const c_char,
    ) -> RequiredServiceResult<*mut Texture>;
    /// Gets or creates a texture from a native resource.
    fn get_or_create_from_resource(
        &self,
        identifier: *const c_char,
        resource_id: u32,
        module: *mut c_void,
    ) -> RequiredServiceResult<*mut Texture>;
    /// Gets or creates a texture from a URL.
    fn get_or_create_from_url(
        &self,
        identifier: *const c_char,
        remote: *const c_char,
        endpoint: *const c_char,
    ) -> RequiredServiceResult<*mut Texture>;
    /// Gets or creates a texture from an opaque memory buffer.
    fn get_or_create_from_memory(
        &self,
        identifier: *const c_char,
        data: *mut c_void,
        size: usize,
    ) -> RequiredServiceResult<*mut Texture>;
    /// Starts an asynchronous file load.
    fn load_from_file(
        &self,
        identifier: *const c_char,
        filename: *const c_char,
        callback: Option<ReceiveTexture>,
    ) -> RequiredServiceResult<()>;
    /// Starts an asynchronous resource load.
    fn load_from_resource(
        &self,
        identifier: *const c_char,
        resource_id: u32,
        module: *mut c_void,
        callback: Option<ReceiveTexture>,
    ) -> RequiredServiceResult<()>;
    /// Starts an asynchronous URL load.
    fn load_from_url(
        &self,
        identifier: *const c_char,
        remote: *const c_char,
        endpoint: *const c_char,
        callback: Option<ReceiveTexture>,
    ) -> RequiredServiceResult<()>;
    /// Starts an asynchronous memory load.
    fn load_from_memory(
        &self,
        identifier: *const c_char,
        data: *mut c_void,
        size: usize,
        callback: Option<ReceiveTexture>,
    ) -> RequiredServiceResult<()>;
}

/// Owns stable native localization strings.
///
/// Returned pointers must remain valid across subsequent translation calls and
/// every input string must be copied through the bounded native boundary.
pub trait LocalizationBackend: Send + Sync + 'static {
    /// Translates using the active language.
    fn translate(&self, identifier: *const c_char) -> RequiredServiceResult<*const c_char>;
    /// Translates using one requested language.
    fn translate_to(
        &self,
        identifier: *const c_char,
        language: *const c_char,
    ) -> RequiredServiceResult<*const c_char>;
    /// Sets one translated string.
    fn set_translated_string(
        &self,
        identifier: *const c_char,
        language: *const c_char,
        value: *const c_char,
    ) -> RequiredServiceResult<()>;
}

/// Owns native font registrations and completion callbacks.
///
/// Implementations must copy source paths and memory before returning. A native
/// font configuration must be bounded-deep-copied synchronously, including its
/// terminated `ImFontConfig::GlyphRanges`; no raw configuration or nested
/// pointer may be retained. Callback addresses must be validated for the exact
/// owner generation, hold its callback gate for the complete foreign call, and
/// must not outlive that owner's cleanup barrier.
pub trait FontBackend: Send + Sync + 'static {
    /// Gets a font through the native completion callback.
    fn get(
        &self,
        identifier: *const c_char,
        callback: Option<ReceiveFont>,
    ) -> RequiredServiceResult<()>;
    /// Releases a font through the native completion callback.
    fn release(
        &self,
        identifier: *const c_char,
        callback: Option<ReceiveFont>,
    ) -> RequiredServiceResult<()>;
    /// Adds a font from a file.
    fn add_from_file(
        &self,
        identifier: *const c_char,
        size: f32,
        filename: *const c_char,
        callback: Option<ReceiveFont>,
        config: *mut c_void,
    ) -> RequiredServiceResult<()>;
    /// Adds a font from a native resource.
    fn add_from_resource(
        &self,
        identifier: *const c_char,
        size: f32,
        resource_id: u32,
        module: *mut c_void,
        callback: Option<ReceiveFont>,
        config: *mut c_void,
    ) -> RequiredServiceResult<()>;
    /// Adds a font from an opaque memory buffer.
    fn add_from_memory(
        &self,
        identifier: *const c_char,
        size: f32,
        data: *mut c_void,
        data_size: usize,
        callback: Option<ReceiveFont>,
        config: *mut c_void,
    ) -> RequiredServiceResult<()>;
    /// Resizes one registered font.
    fn resize(&self, identifier: *const c_char, size: f32) -> RequiredServiceResult<()>;
}
