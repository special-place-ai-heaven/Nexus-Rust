//! Exact x64 layouts for the native addon API revisions.
//!
//! The legacy C++ structs inherit from an empty base class. MSVC applies empty
//! base optimization, so each Rust table starts directly with the first slot.
//! Function names and signatures mirror the headers under
//! `src/Host/Addons/API`; owning and validation logic belongs above this crate.

#![allow(missing_docs)]

use core::ffi::{c_char, c_void};

pub type RenderCallback = unsafe extern "C" fn();
pub type RegisterRender = unsafe extern "C" fn(RenderPhase, Option<RenderCallback>);
pub type DeregisterRender = unsafe extern "C" fn(Option<RenderCallback>);

pub type RequestUpdate = unsafe extern "C" fn(i32, *const c_char);

pub type GetGameDirectory = unsafe extern "C" fn() -> *const c_char;
pub type GetAddonDirectory = unsafe extern "C" fn(*const c_char) -> *const c_char;
pub type GetCommonDirectory = unsafe extern "C" fn() -> *const c_char;

pub type CreateHook =
    unsafe extern "system" fn(*mut c_void, *mut c_void, *mut *mut c_void) -> MinHookStatus;
pub type ChangeHook = unsafe extern "system" fn(*mut c_void) -> MinHookStatus;

pub type LogV1 = unsafe extern "C" fn(LogLevel, *const c_char);
pub type Log = unsafe extern "C" fn(LogLevel, *const c_char, *const c_char);

pub type SendAlert = unsafe extern "C" fn(*const c_char);
pub type RegisterCloseOnEscape = unsafe extern "C" fn(*const c_char, *mut u8);
pub type DeregisterCloseOnEscape = unsafe extern "C" fn(*const c_char);

pub type EventCallback = unsafe extern "C" fn(*mut c_void);
pub type RaiseEvent = unsafe extern "C" fn(*const c_char, *mut c_void);
pub type RaiseEventNotification = unsafe extern "C" fn(*const c_char);
pub type RaiseEventTargeted = unsafe extern "C" fn(u32, *const c_char, *mut c_void);
pub type RaiseEventNotificationTargeted = unsafe extern "C" fn(u32, *const c_char);
pub type SubscribeEvent = unsafe extern "C" fn(*const c_char, Option<EventCallback>);

pub type WndProcCallback = unsafe extern "C" fn(*mut c_void, u32, usize, isize) -> u32;
pub type RegisterWndProc = unsafe extern "C" fn(Option<WndProcCallback>);
pub type SendWndProcToGame = unsafe extern "C" fn(*mut c_void, u32, usize, isize) -> isize;

pub type InputBindCallbackV1 = unsafe extern "C" fn(*const c_char);
pub type InputBindCallbackV2 = unsafe extern "C" fn(*const c_char, u8);
pub type RegisterInputBindStringV1 =
    unsafe extern "C" fn(*const c_char, Option<InputBindCallbackV1>, *const c_char);
pub type RegisterInputBindStructV1 =
    unsafe extern "C" fn(*const c_char, Option<InputBindCallbackV1>, InputBindV1);
pub type RegisterInputBindStringV2 =
    unsafe extern "C" fn(*const c_char, Option<InputBindCallbackV2>, *const c_char);
pub type RegisterInputBindStructV2 =
    unsafe extern "C" fn(*const c_char, Option<InputBindCallbackV2>, InputBindV1);
pub type InvokeInputBind = unsafe extern "C" fn(*const c_char, u8);
pub type DeregisterInputBind = unsafe extern "C" fn(*const c_char);

pub type PressGameBind = unsafe extern "C" fn(GameBind);
pub type InvokeGameBind = unsafe extern "C" fn(GameBind, i32);
pub type IsGameBindBound = unsafe extern "C" fn(GameBind) -> u8;

pub type GetDataLinkResource = unsafe extern "C" fn(*const c_char) -> *mut c_void;
pub type ShareDataLinkResource = unsafe extern "C" fn(*const c_char, usize) -> *mut c_void;

pub type ReceiveTexture = unsafe extern "C" fn(*const c_char, *mut Texture);
pub type GetTexture = unsafe extern "C" fn(*const c_char) -> *mut Texture;
pub type GetOrCreateTextureFromFile =
    unsafe extern "C" fn(*const c_char, *const c_char) -> *mut Texture;
pub type GetOrCreateTextureFromResource =
    unsafe extern "C" fn(*const c_char, u32, *mut c_void) -> *mut Texture;
pub type GetOrCreateTextureFromUrl =
    unsafe extern "C" fn(*const c_char, *const c_char, *const c_char) -> *mut Texture;
pub type GetOrCreateTextureFromMemory =
    unsafe extern "C" fn(*const c_char, *mut c_void, usize) -> *mut Texture;
pub type LoadTextureFromFile =
    unsafe extern "C" fn(*const c_char, *const c_char, Option<ReceiveTexture>);
pub type LoadTextureFromResource =
    unsafe extern "C" fn(*const c_char, u32, *mut c_void, Option<ReceiveTexture>);
pub type LoadTextureFromUrl =
    unsafe extern "C" fn(*const c_char, *const c_char, *const c_char, Option<ReceiveTexture>);
pub type LoadTextureFromMemory =
    unsafe extern "C" fn(*const c_char, *mut c_void, usize, Option<ReceiveTexture>);

pub type AddShortcut =
    unsafe extern "C" fn(*const c_char, *const c_char, *const c_char, *const c_char, *const c_char);
pub type AddSimpleShortcut = unsafe extern "C" fn(*const c_char, Option<RenderCallback>);
pub type AddContextMenu =
    unsafe extern "C" fn(*const c_char, *const c_char, Option<RenderCallback>);
pub type QuickAccessGeneric = unsafe extern "C" fn(*const c_char);

pub type Translate = unsafe extern "C" fn(*const c_char) -> *const c_char;
pub type TranslateTo = unsafe extern "C" fn(*const c_char, *const c_char) -> *const c_char;
pub type SetTranslatedString = unsafe extern "C" fn(*const c_char, *const c_char, *const c_char);

pub type ReceiveFont = unsafe extern "C" fn(*const c_char, *mut c_void);
pub type GetOrReleaseFont = unsafe extern "C" fn(*const c_char, Option<ReceiveFont>);
pub type AddFontFromFile =
    unsafe extern "C" fn(*const c_char, f32, *const c_char, Option<ReceiveFont>, *mut c_void);
pub type AddFontFromResource =
    unsafe extern "C" fn(*const c_char, f32, u32, *mut c_void, Option<ReceiveFont>, *mut c_void);
pub type AddFontFromMemory =
    unsafe extern "C" fn(*const c_char, f32, *mut c_void, usize, Option<ReceiveFont>, *mut c_void);
pub type ResizeFont = unsafe extern "C" fn(*const c_char, f32);

/// Render callback phase used by every API revision.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderPhase(pub u32);

impl RenderPhase {
    pub const PRE_RENDER: Self = Self(0);
    pub const RENDER: Self = Self(1);
    pub const POST_RENDER: Self = Self(2);
    pub const OPTIONS_RENDER: Self = Self(3);
}

/// Legacy logging level, represented as an open value for FFI robustness.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogLevel(pub u32);

/// MinHook status returned by the API's compatibility hooks.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MinHookStatus(pub i32);

/// Guild Wars 2 bind identifier, retaining unknown future values.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GameBind(pub u32);

/// Backward-compatible input binding passed by value.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputBindV1 {
    pub key: u16,
    pub alt: u8,
    pub ctrl: u8,
    pub shift: u8,
}

/// Texture descriptor shared with native addons.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Texture {
    pub width: u32,
    pub height: u32,
    pub resource: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RendererVTable {
    pub register: Option<RegisterRender>,
    pub deregister: Option<DeregisterRender>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct UiVTable {
    pub send_alert: Option<SendAlert>,
    pub register_close_on_escape: Option<RegisterCloseOnEscape>,
    pub deregister_close_on_escape: Option<DeregisterCloseOnEscape>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PathsVTable {
    pub get_game_directory: Option<GetGameDirectory>,
    pub get_addon_directory: Option<GetAddonDirectory>,
    pub get_common_directory: Option<GetCommonDirectory>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MinHookVTable {
    pub create: Option<CreateHook>,
    pub remove: Option<ChangeHook>,
    pub enable: Option<ChangeHook>,
    pub disable: Option<ChangeHook>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct EventsVTable {
    pub raise: Option<RaiseEvent>,
    pub raise_notification: Option<RaiseEventNotification>,
    pub raise_targeted: Option<RaiseEventTargeted>,
    pub raise_notification_targeted: Option<RaiseEventNotificationTargeted>,
    pub subscribe: Option<SubscribeEvent>,
    pub unsubscribe: Option<SubscribeEvent>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct WndProcVTable {
    pub register: Option<RegisterWndProc>,
    pub deregister: Option<RegisterWndProc>,
    pub send_to_game_only: Option<SendWndProcToGame>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct InputBindsVTable {
    pub invoke: Option<InvokeInputBind>,
    pub register_with_string: Option<RegisterInputBindStringV2>,
    pub register_with_struct: Option<RegisterInputBindStructV2>,
    pub deregister: Option<DeregisterInputBind>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GameBindsVTable {
    pub press_async: Option<PressGameBind>,
    pub release_async: Option<PressGameBind>,
    pub invoke_async: Option<InvokeGameBind>,
    pub press: Option<PressGameBind>,
    pub release: Option<PressGameBind>,
    pub is_bound: Option<IsGameBindBound>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DataLinkVTable {
    pub get: Option<GetDataLinkResource>,
    pub share: Option<ShareDataLinkResource>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TexturesVTable {
    pub get: Option<GetTexture>,
    pub get_or_create_from_file: Option<GetOrCreateTextureFromFile>,
    pub get_or_create_from_resource: Option<GetOrCreateTextureFromResource>,
    pub get_or_create_from_url: Option<GetOrCreateTextureFromUrl>,
    pub get_or_create_from_memory: Option<GetOrCreateTextureFromMemory>,
    pub load_from_file: Option<LoadTextureFromFile>,
    pub load_from_resource: Option<LoadTextureFromResource>,
    pub load_from_url: Option<LoadTextureFromUrl>,
    pub load_from_memory: Option<LoadTextureFromMemory>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct QuickAccessVTable {
    pub add: Option<AddShortcut>,
    pub remove: Option<QuickAccessGeneric>,
    pub notify: Option<QuickAccessGeneric>,
    pub add_context_menu: Option<AddContextMenu>,
    pub remove_context_menu: Option<QuickAccessGeneric>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LocalizationVTable {
    pub translate: Option<Translate>,
    pub translate_to: Option<TranslateTo>,
    pub set_translated_string: Option<SetTranslatedString>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FontsVTable {
    pub get: Option<GetOrReleaseFont>,
    pub release: Option<GetOrReleaseFont>,
    pub add_from_file: Option<AddFontFromFile>,
    pub add_from_resource: Option<AddFontFromResource>,
    pub add_from_memory: Option<AddFontFromMemory>,
    pub resize: Option<ResizeFont>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AddonApiV1 {
    pub swap_chain: *mut c_void,
    pub imgui_context: *mut c_void,
    pub imgui_malloc: *mut c_void,
    pub imgui_free: *mut c_void,
    pub register_render: Option<RegisterRender>,
    pub deregister_render: Option<DeregisterRender>,
    pub get_game_directory: Option<GetGameDirectory>,
    pub get_addon_directory: Option<GetAddonDirectory>,
    pub get_common_directory: Option<GetCommonDirectory>,
    pub create_hook: Option<CreateHook>,
    pub remove_hook: Option<ChangeHook>,
    pub enable_hook: Option<ChangeHook>,
    pub disable_hook: Option<ChangeHook>,
    pub log: Option<LogV1>,
    pub raise_event: Option<RaiseEvent>,
    pub subscribe_event: Option<SubscribeEvent>,
    pub unsubscribe_event: Option<SubscribeEvent>,
    pub register_wnd_proc: Option<RegisterWndProc>,
    pub deregister_wnd_proc: Option<RegisterWndProc>,
    pub register_input_bind_with_string: Option<RegisterInputBindStringV1>,
    pub register_input_bind_with_struct: Option<RegisterInputBindStructV1>,
    pub deregister_input_bind: Option<DeregisterInputBind>,
    pub get_resource: Option<GetDataLinkResource>,
    pub share_resource: Option<ShareDataLinkResource>,
    pub get_texture: Option<GetTexture>,
    pub load_texture_from_file: Option<LoadTextureFromFile>,
    pub load_texture_from_resource: Option<LoadTextureFromResource>,
    pub load_texture_from_url: Option<LoadTextureFromUrl>,
    pub add_shortcut: Option<AddShortcut>,
    pub remove_shortcut: Option<QuickAccessGeneric>,
    pub add_simple_shortcut: Option<AddSimpleShortcut>,
    pub remove_simple_shortcut: Option<QuickAccessGeneric>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AddonApiV2 {
    pub swap_chain: *mut c_void,
    pub imgui_context: *mut c_void,
    pub imgui_malloc: *mut c_void,
    pub imgui_free: *mut c_void,
    pub register_render: Option<RegisterRender>,
    pub deregister_render: Option<DeregisterRender>,
    pub get_game_directory: Option<GetGameDirectory>,
    pub get_addon_directory: Option<GetAddonDirectory>,
    pub get_common_directory: Option<GetCommonDirectory>,
    pub create_hook: Option<CreateHook>,
    pub remove_hook: Option<ChangeHook>,
    pub enable_hook: Option<ChangeHook>,
    pub disable_hook: Option<ChangeHook>,
    pub log: Option<Log>,
    pub raise_event: Option<RaiseEvent>,
    pub raise_event_notification: Option<RaiseEventNotification>,
    pub subscribe_event: Option<SubscribeEvent>,
    pub unsubscribe_event: Option<SubscribeEvent>,
    pub register_wnd_proc: Option<RegisterWndProc>,
    pub deregister_wnd_proc: Option<RegisterWndProc>,
    pub send_wnd_proc_to_game_only: Option<SendWndProcToGame>,
    pub register_input_bind_with_string: Option<RegisterInputBindStringV1>,
    pub register_input_bind_with_struct: Option<RegisterInputBindStructV1>,
    pub deregister_input_bind: Option<DeregisterInputBind>,
    pub get_resource: Option<GetDataLinkResource>,
    pub share_resource: Option<ShareDataLinkResource>,
    pub get_texture: Option<GetTexture>,
    pub get_texture_or_create_from_file: Option<GetOrCreateTextureFromFile>,
    pub get_texture_or_create_from_resource: Option<GetOrCreateTextureFromResource>,
    pub get_texture_or_create_from_url: Option<GetOrCreateTextureFromUrl>,
    pub get_texture_or_create_from_memory: Option<GetOrCreateTextureFromMemory>,
    pub load_texture_from_file: Option<LoadTextureFromFile>,
    pub load_texture_from_resource: Option<LoadTextureFromResource>,
    pub load_texture_from_url: Option<LoadTextureFromUrl>,
    pub load_texture_from_memory: Option<LoadTextureFromMemory>,
    pub add_shortcut: Option<AddShortcut>,
    pub remove_shortcut: Option<QuickAccessGeneric>,
    pub push_notification: Option<QuickAccessGeneric>,
    pub add_simple_shortcut: Option<AddSimpleShortcut>,
    pub remove_simple_shortcut: Option<QuickAccessGeneric>,
    pub translate: Option<Translate>,
    pub translate_to: Option<TranslateTo>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AddonApiV3 {
    pub swap_chain: *mut c_void,
    pub imgui_context: *mut c_void,
    pub imgui_malloc: *mut c_void,
    pub imgui_free: *mut c_void,
    pub register_render: Option<RegisterRender>,
    pub deregister_render: Option<DeregisterRender>,
    pub get_game_directory: Option<GetGameDirectory>,
    pub get_addon_directory: Option<GetAddonDirectory>,
    pub get_common_directory: Option<GetCommonDirectory>,
    pub create_hook: Option<CreateHook>,
    pub remove_hook: Option<ChangeHook>,
    pub enable_hook: Option<ChangeHook>,
    pub disable_hook: Option<ChangeHook>,
    pub log: Option<Log>,
    pub send_alert: Option<SendAlert>,
    pub raise_event: Option<RaiseEvent>,
    pub raise_event_notification: Option<RaiseEventNotification>,
    pub raise_event_targeted: Option<RaiseEventTargeted>,
    pub raise_event_notification_targeted: Option<RaiseEventNotificationTargeted>,
    pub subscribe_event: Option<SubscribeEvent>,
    pub unsubscribe_event: Option<SubscribeEvent>,
    pub register_wnd_proc: Option<RegisterWndProc>,
    pub deregister_wnd_proc: Option<RegisterWndProc>,
    pub send_wnd_proc_to_game_only: Option<SendWndProcToGame>,
    pub register_input_bind_with_string: Option<RegisterInputBindStringV1>,
    pub register_input_bind_with_struct: Option<RegisterInputBindStructV1>,
    pub deregister_input_bind: Option<DeregisterInputBind>,
    pub get_resource: Option<GetDataLinkResource>,
    pub share_resource: Option<ShareDataLinkResource>,
    pub get_texture: Option<GetTexture>,
    pub get_texture_or_create_from_file: Option<GetOrCreateTextureFromFile>,
    pub get_texture_or_create_from_resource: Option<GetOrCreateTextureFromResource>,
    pub get_texture_or_create_from_url: Option<GetOrCreateTextureFromUrl>,
    pub get_texture_or_create_from_memory: Option<GetOrCreateTextureFromMemory>,
    pub load_texture_from_file: Option<LoadTextureFromFile>,
    pub load_texture_from_resource: Option<LoadTextureFromResource>,
    pub load_texture_from_url: Option<LoadTextureFromUrl>,
    pub load_texture_from_memory: Option<LoadTextureFromMemory>,
    pub add_shortcut: Option<AddShortcut>,
    pub remove_shortcut: Option<QuickAccessGeneric>,
    pub push_notification: Option<QuickAccessGeneric>,
    pub add_simple_shortcut: Option<AddSimpleShortcut>,
    pub remove_simple_shortcut: Option<QuickAccessGeneric>,
    pub translate: Option<Translate>,
    pub translate_to: Option<TranslateTo>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AddonApiV4 {
    pub swap_chain: *mut c_void,
    pub imgui_context: *mut c_void,
    pub imgui_malloc: *mut c_void,
    pub imgui_free: *mut c_void,
    pub register_render: Option<RegisterRender>,
    pub deregister_render: Option<DeregisterRender>,
    pub request_update: Option<RequestUpdate>,
    pub get_game_directory: Option<GetGameDirectory>,
    pub get_addon_directory: Option<GetAddonDirectory>,
    pub get_common_directory: Option<GetCommonDirectory>,
    pub create_hook: Option<CreateHook>,
    pub remove_hook: Option<ChangeHook>,
    pub enable_hook: Option<ChangeHook>,
    pub disable_hook: Option<ChangeHook>,
    pub log: Option<Log>,
    pub send_alert: Option<SendAlert>,
    pub raise_event: Option<RaiseEvent>,
    pub raise_event_notification: Option<RaiseEventNotification>,
    pub raise_event_targeted: Option<RaiseEventTargeted>,
    pub raise_event_notification_targeted: Option<RaiseEventNotificationTargeted>,
    pub subscribe_event: Option<SubscribeEvent>,
    pub unsubscribe_event: Option<SubscribeEvent>,
    pub register_wnd_proc: Option<RegisterWndProc>,
    pub deregister_wnd_proc: Option<RegisterWndProc>,
    pub send_wnd_proc_to_game_only: Option<SendWndProcToGame>,
    pub register_input_bind_with_string: Option<RegisterInputBindStringV2>,
    pub register_input_bind_with_struct: Option<RegisterInputBindStructV2>,
    pub deregister_input_bind: Option<DeregisterInputBind>,
    pub get_resource: Option<GetDataLinkResource>,
    pub share_resource: Option<ShareDataLinkResource>,
    pub get_texture: Option<GetTexture>,
    pub get_texture_or_create_from_file: Option<GetOrCreateTextureFromFile>,
    pub get_texture_or_create_from_resource: Option<GetOrCreateTextureFromResource>,
    pub get_texture_or_create_from_url: Option<GetOrCreateTextureFromUrl>,
    pub get_texture_or_create_from_memory: Option<GetOrCreateTextureFromMemory>,
    pub load_texture_from_file: Option<LoadTextureFromFile>,
    pub load_texture_from_resource: Option<LoadTextureFromResource>,
    pub load_texture_from_url: Option<LoadTextureFromUrl>,
    pub load_texture_from_memory: Option<LoadTextureFromMemory>,
    pub add_shortcut: Option<AddShortcut>,
    pub remove_shortcut: Option<QuickAccessGeneric>,
    pub push_notification: Option<QuickAccessGeneric>,
    pub add_simple_shortcut: Option<AddSimpleShortcut>,
    pub remove_simple_shortcut: Option<QuickAccessGeneric>,
    pub translate: Option<Translate>,
    pub translate_to: Option<TranslateTo>,
    pub get_font: Option<GetOrReleaseFont>,
    pub release_font: Option<GetOrReleaseFont>,
    pub add_font_from_file: Option<AddFontFromFile>,
    pub add_font_from_resource: Option<AddFontFromResource>,
    pub add_font_from_memory: Option<AddFontFromMemory>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AddonApiV5 {
    pub swap_chain: *mut c_void,
    pub imgui_context: *mut c_void,
    pub imgui_malloc: *mut c_void,
    pub imgui_free: *mut c_void,
    pub register_render: Option<RegisterRender>,
    pub deregister_render: Option<DeregisterRender>,
    pub request_update: Option<RequestUpdate>,
    pub get_game_directory: Option<GetGameDirectory>,
    pub get_addon_directory: Option<GetAddonDirectory>,
    pub get_common_directory: Option<GetCommonDirectory>,
    pub create_hook: Option<CreateHook>,
    pub remove_hook: Option<ChangeHook>,
    pub enable_hook: Option<ChangeHook>,
    pub disable_hook: Option<ChangeHook>,
    pub log: Option<Log>,
    pub send_alert: Option<SendAlert>,
    pub raise_event: Option<RaiseEvent>,
    pub raise_event_notification: Option<RaiseEventNotification>,
    pub raise_event_targeted: Option<RaiseEventTargeted>,
    pub raise_event_notification_targeted: Option<RaiseEventNotificationTargeted>,
    pub subscribe_event: Option<SubscribeEvent>,
    pub unsubscribe_event: Option<SubscribeEvent>,
    pub register_wnd_proc: Option<RegisterWndProc>,
    pub deregister_wnd_proc: Option<RegisterWndProc>,
    pub send_wnd_proc_to_game_only: Option<SendWndProcToGame>,
    pub invoke_input_bind: Option<InvokeInputBind>,
    pub register_input_bind_with_string: Option<RegisterInputBindStringV2>,
    pub register_input_bind_with_struct: Option<RegisterInputBindStructV2>,
    pub deregister_input_bind: Option<DeregisterInputBind>,
    pub get_resource: Option<GetDataLinkResource>,
    pub share_resource: Option<ShareDataLinkResource>,
    pub get_texture: Option<GetTexture>,
    pub get_texture_or_create_from_file: Option<GetOrCreateTextureFromFile>,
    pub get_texture_or_create_from_resource: Option<GetOrCreateTextureFromResource>,
    pub get_texture_or_create_from_url: Option<GetOrCreateTextureFromUrl>,
    pub get_texture_or_create_from_memory: Option<GetOrCreateTextureFromMemory>,
    pub load_texture_from_file: Option<LoadTextureFromFile>,
    pub load_texture_from_resource: Option<LoadTextureFromResource>,
    pub load_texture_from_url: Option<LoadTextureFromUrl>,
    pub load_texture_from_memory: Option<LoadTextureFromMemory>,
    pub add_shortcut: Option<AddShortcut>,
    pub remove_shortcut: Option<QuickAccessGeneric>,
    pub push_notification: Option<QuickAccessGeneric>,
    pub add_simple_shortcut: Option<AddSimpleShortcut>,
    pub remove_simple_shortcut: Option<QuickAccessGeneric>,
    pub translate: Option<Translate>,
    pub translate_to: Option<TranslateTo>,
    pub set_translated_string: Option<SetTranslatedString>,
    pub get_font: Option<GetOrReleaseFont>,
    pub release_font: Option<GetOrReleaseFont>,
    pub add_font_from_file: Option<AddFontFromFile>,
    pub add_font_from_resource: Option<AddFontFromResource>,
    pub add_font_from_memory: Option<AddFontFromMemory>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AddonApiV6 {
    pub swap_chain: *mut c_void,
    pub imgui_context: *mut c_void,
    pub imgui_malloc: *mut c_void,
    pub imgui_free: *mut c_void,
    pub renderer: RendererVTable,
    pub request_update: Option<RequestUpdate>,
    pub log: Option<Log>,
    pub ui: UiVTable,
    pub paths: PathsVTable,
    pub min_hook: MinHookVTable,
    pub events: EventsVTable,
    pub wnd_proc: WndProcVTable,
    pub input_binds: InputBindsVTable,
    pub game_binds: GameBindsVTable,
    pub data_link: DataLinkVTable,
    pub textures: TexturesVTable,
    pub quick_access: QuickAccessVTable,
    pub localization: LocalizationVTable,
    pub fonts: FontsVTable,
}

#[cfg(test)]
mod tests {
    use core::mem::{align_of, offset_of, size_of};

    use super::{
        AddonApiV1, AddonApiV2, AddonApiV3, AddonApiV4, AddonApiV5, AddonApiV6, FontsVTable,
        InputBindV1, Texture,
    };

    #[test]
    fn value_types_match_msvc_x64() {
        assert_eq!(size_of::<InputBindV1>(), 6);
        assert_eq!(align_of::<InputBindV1>(), 2);
        assert_eq!(offset_of!(InputBindV1, shift), 4);
        assert_eq!(size_of::<Texture>(), 16);
        assert_eq!(align_of::<Texture>(), 8);
        assert_eq!(offset_of!(Texture, resource), 8);
    }

    #[test]
    fn flat_api_tables_match_msvc_x64() {
        assert_eq!(size_of::<AddonApiV1>(), 32 * 8);
        assert_eq!(size_of::<AddonApiV2>(), 42 * 8);
        assert_eq!(size_of::<AddonApiV3>(), 45 * 8);
        assert_eq!(size_of::<AddonApiV4>(), 51 * 8);
        assert_eq!(size_of::<AddonApiV5>(), 53 * 8);
        assert_eq!(align_of::<AddonApiV1>(), 8);
        assert_eq!(offset_of!(AddonApiV1, swap_chain), 0);
        assert_eq!(offset_of!(AddonApiV1, remove_simple_shortcut), 31 * 8);
        assert_eq!(offset_of!(AddonApiV5, request_update), 6 * 8);
        assert_eq!(offset_of!(AddonApiV5, invoke_input_bind), 25 * 8);
        assert_eq!(offset_of!(AddonApiV5, add_font_from_memory), 52 * 8);
    }

    #[test]
    fn grouped_v6_table_flattens_without_padding() {
        assert_eq!(size_of::<AddonApiV6>(), 62 * 8);
        assert_eq!(align_of::<AddonApiV6>(), 8);
        assert_eq!(offset_of!(AddonApiV6, renderer), 4 * 8);
        assert_eq!(offset_of!(AddonApiV6, request_update), 6 * 8);
        assert_eq!(offset_of!(AddonApiV6, ui), 8 * 8);
        assert_eq!(offset_of!(AddonApiV6, game_binds), 31 * 8);
        assert_eq!(offset_of!(AddonApiV6, textures), 39 * 8);
        assert_eq!(offset_of!(AddonApiV6, fonts), 56 * 8);
        assert_eq!(size_of::<FontsVTable>(), 6 * 8);
    }
}
