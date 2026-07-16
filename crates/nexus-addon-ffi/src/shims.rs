//! Exact native ABI shims and complete v1-v6 binding assembly.

use core::ffi::{c_char, c_void};

use nexus_abi::{
    AddonApiV6, DataLinkVTable, EventCallback, EventsVTable, FontsVTable, GameBind,
    GameBindsVTable, InputBindCallbackV1, InputBindCallbackV2, InputBindV1, InputBindsVTable,
    LocalizationVTable, LogLevel, MinHookStatus, MinHookVTable, PathsVTable, QuickAccessVTable,
    ReceiveFont, ReceiveTexture, RenderCallback, RenderPhase, RendererVTable, Texture,
    TexturesVTable, UiVTable, WndProcCallback, WndProcVTable,
};
use nexus_addon_api::{ApiBindings, LegacyApiBindings};

use crate::MINHOOK_ERROR_NOT_INITIALIZED;
use crate::dispatcher::dispatch;

macro_rules! c_void_shim {
    ($name:ident: $alias:ty, ($($argument:ident: $argument_type:ty),* $(,)?), $method:ident) => {
        pub(crate) unsafe extern "C" fn $name($($argument: $argument_type),*) {
            dispatch((), |backend| backend.$method($($argument),*));
        }

        const _: $alias = $name;
    };
}

macro_rules! c_return_shim {
    (
        $name:ident: $alias:ty,
        ($($argument:ident: $argument_type:ty),* $(,)?),
        -> $return_type:ty,
        $fallback:expr,
        $method:ident
    ) => {
        pub(crate) unsafe extern "C" fn $name($($argument: $argument_type),*) -> $return_type {
            dispatch($fallback, |backend| backend.$method($($argument),*))
        }

        const _: $alias = $name;
    };
}

macro_rules! system_return_shim {
    (
        $name:ident: $alias:ty,
        ($($argument:ident: $argument_type:ty),* $(,)?),
        -> $return_type:ty,
        $fallback:expr,
        $method:ident
    ) => {
        pub(crate) unsafe extern "system" fn $name($($argument: $argument_type),*) -> $return_type {
            dispatch($fallback, |backend| backend.$method($($argument),*))
        }

        const _: $alias = $name;
    };
}

c_void_shim!(
    renderer_register: nexus_abi::RegisterRender,
    (phase: RenderPhase, callback: Option<RenderCallback>),
    renderer_register
);
c_void_shim!(
    renderer_deregister: nexus_abi::DeregisterRender,
    (callback: Option<RenderCallback>),
    renderer_deregister
);
c_void_shim!(
    request_update: nexus_abi::RequestUpdate,
    (signature: i32, update_url: *const c_char),
    request_update
);

c_void_shim!(
    log: nexus_abi::Log,
    (level: LogLevel, channel: *const c_char, message: *const c_char),
    log
);
c_void_shim!(
    log_v1: nexus_abi::LogV1,
    (level: LogLevel, message: *const c_char),
    log_v1
);

c_void_shim!(
    ui_send_alert: nexus_abi::SendAlert,
    (message: *const c_char),
    ui_send_alert
);
c_void_shim!(
    ui_register_close_on_escape: nexus_abi::RegisterCloseOnEscape,
    (identifier: *const c_char, state: *mut u8),
    ui_register_close_on_escape
);
c_void_shim!(
    ui_deregister_close_on_escape: nexus_abi::DeregisterCloseOnEscape,
    (identifier: *const c_char),
    ui_deregister_close_on_escape
);

c_return_shim!(
    paths_get_game_directory: nexus_abi::GetGameDirectory,
    (),
    -> *const c_char,
    core::ptr::null(),
    paths_get_game_directory
);
c_return_shim!(
    paths_get_addon_directory: nexus_abi::GetAddonDirectory,
    (name: *const c_char),
    -> *const c_char,
    core::ptr::null(),
    paths_get_addon_directory
);
c_return_shim!(
    paths_get_common_directory: nexus_abi::GetCommonDirectory,
    (),
    -> *const c_char,
    core::ptr::null(),
    paths_get_common_directory
);

system_return_shim!(
    min_hook_create: nexus_abi::CreateHook,
    (target: *mut c_void, detour: *mut c_void, original: *mut *mut c_void),
    -> MinHookStatus,
    MINHOOK_ERROR_NOT_INITIALIZED,
    min_hook_create
);
system_return_shim!(
    min_hook_remove: nexus_abi::ChangeHook,
    (target: *mut c_void),
    -> MinHookStatus,
    MINHOOK_ERROR_NOT_INITIALIZED,
    min_hook_remove
);
system_return_shim!(
    min_hook_enable: nexus_abi::ChangeHook,
    (target: *mut c_void),
    -> MinHookStatus,
    MINHOOK_ERROR_NOT_INITIALIZED,
    min_hook_enable
);
system_return_shim!(
    min_hook_disable: nexus_abi::ChangeHook,
    (target: *mut c_void),
    -> MinHookStatus,
    MINHOOK_ERROR_NOT_INITIALIZED,
    min_hook_disable
);

c_void_shim!(
    events_raise: nexus_abi::RaiseEvent,
    (identifier: *const c_char, payload: *mut c_void),
    events_raise
);
c_void_shim!(
    events_raise_notification: nexus_abi::RaiseEventNotification,
    (identifier: *const c_char),
    events_raise_notification
);
c_void_shim!(
    events_raise_targeted: nexus_abi::RaiseEventTargeted,
    (signature: u32, identifier: *const c_char, payload: *mut c_void),
    events_raise_targeted
);
c_void_shim!(
    events_raise_notification_targeted: nexus_abi::RaiseEventNotificationTargeted,
    (signature: u32, identifier: *const c_char),
    events_raise_notification_targeted
);
c_void_shim!(
    events_subscribe: nexus_abi::SubscribeEvent,
    (identifier: *const c_char, callback: Option<EventCallback>),
    events_subscribe
);
c_void_shim!(
    events_unsubscribe: nexus_abi::SubscribeEvent,
    (identifier: *const c_char, callback: Option<EventCallback>),
    events_unsubscribe
);

c_void_shim!(
    wnd_proc_register: nexus_abi::RegisterWndProc,
    (callback: Option<WndProcCallback>),
    wnd_proc_register
);
c_void_shim!(
    wnd_proc_deregister: nexus_abi::RegisterWndProc,
    (callback: Option<WndProcCallback>),
    wnd_proc_deregister
);
c_return_shim!(
    wnd_proc_send_to_game_only: nexus_abi::SendWndProcToGame,
    (hwnd: *mut c_void, message: u32, w_param: usize, l_param: isize),
    -> isize,
    0,
    wnd_proc_send_to_game_only
);

c_void_shim!(
    input_binds_invoke: nexus_abi::InvokeInputBind,
    (identifier: *const c_char, is_release: u8),
    input_binds_invoke
);
c_void_shim!(
    input_binds_register_with_string: nexus_abi::RegisterInputBindStringV2,
    (
        identifier: *const c_char,
        callback: Option<InputBindCallbackV2>,
        bind: *const c_char
    ),
    input_binds_register_with_string
);
c_void_shim!(
    input_binds_register_with_struct: nexus_abi::RegisterInputBindStructV2,
    (
        identifier: *const c_char,
        callback: Option<InputBindCallbackV2>,
        bind: InputBindV1
    ),
    input_binds_register_with_struct
);
c_void_shim!(
    input_binds_register_with_string_v1: nexus_abi::RegisterInputBindStringV1,
    (
        identifier: *const c_char,
        callback: Option<InputBindCallbackV1>,
        bind: *const c_char
    ),
    input_binds_register_with_string_v1
);
c_void_shim!(
    input_binds_register_with_struct_v1: nexus_abi::RegisterInputBindStructV1,
    (
        identifier: *const c_char,
        callback: Option<InputBindCallbackV1>,
        bind: InputBindV1
    ),
    input_binds_register_with_struct_v1
);
c_void_shim!(
    input_binds_deregister: nexus_abi::DeregisterInputBind,
    (identifier: *const c_char),
    input_binds_deregister
);

c_void_shim!(
    game_binds_press_async: nexus_abi::PressGameBind,
    (bind: GameBind),
    game_binds_press_async
);
c_void_shim!(
    game_binds_release_async: nexus_abi::PressGameBind,
    (bind: GameBind),
    game_binds_release_async
);
c_void_shim!(
    game_binds_invoke_async: nexus_abi::InvokeGameBind,
    (bind: GameBind, duration: i32),
    game_binds_invoke_async
);
c_void_shim!(
    game_binds_press: nexus_abi::PressGameBind,
    (bind: GameBind),
    game_binds_press
);
c_void_shim!(
    game_binds_release: nexus_abi::PressGameBind,
    (bind: GameBind),
    game_binds_release
);
c_return_shim!(
    game_binds_is_bound: nexus_abi::IsGameBindBound,
    (bind: GameBind),
    -> u8,
    0,
    game_binds_is_bound
);

c_return_shim!(
    data_link_get: nexus_abi::GetDataLinkResource,
    (identifier: *const c_char),
    -> *mut c_void,
    core::ptr::null_mut(),
    data_link_get
);
c_return_shim!(
    data_link_share: nexus_abi::ShareDataLinkResource,
    (identifier: *const c_char, size: usize),
    -> *mut c_void,
    core::ptr::null_mut(),
    data_link_share
);

c_return_shim!(
    textures_get: nexus_abi::GetTexture,
    (identifier: *const c_char),
    -> *mut Texture,
    core::ptr::null_mut(),
    textures_get
);
c_return_shim!(
    textures_get_or_create_from_file: nexus_abi::GetOrCreateTextureFromFile,
    (identifier: *const c_char, filename: *const c_char),
    -> *mut Texture,
    core::ptr::null_mut(),
    textures_get_or_create_from_file
);
c_return_shim!(
    textures_get_or_create_from_resource: nexus_abi::GetOrCreateTextureFromResource,
    (identifier: *const c_char, resource_id: u32, module: *mut c_void),
    -> *mut Texture,
    core::ptr::null_mut(),
    textures_get_or_create_from_resource
);
c_return_shim!(
    textures_get_or_create_from_url: nexus_abi::GetOrCreateTextureFromUrl,
    (
        identifier: *const c_char,
        remote: *const c_char,
        endpoint: *const c_char
    ),
    -> *mut Texture,
    core::ptr::null_mut(),
    textures_get_or_create_from_url
);
c_return_shim!(
    textures_get_or_create_from_memory: nexus_abi::GetOrCreateTextureFromMemory,
    (identifier: *const c_char, data: *mut c_void, size: usize),
    -> *mut Texture,
    core::ptr::null_mut(),
    textures_get_or_create_from_memory
);
c_void_shim!(
    textures_load_from_file: nexus_abi::LoadTextureFromFile,
    (
        identifier: *const c_char,
        filename: *const c_char,
        callback: Option<ReceiveTexture>
    ),
    textures_load_from_file
);
c_void_shim!(
    textures_load_from_resource: nexus_abi::LoadTextureFromResource,
    (
        identifier: *const c_char,
        resource_id: u32,
        module: *mut c_void,
        callback: Option<ReceiveTexture>
    ),
    textures_load_from_resource
);
c_void_shim!(
    textures_load_from_url: nexus_abi::LoadTextureFromUrl,
    (
        identifier: *const c_char,
        remote: *const c_char,
        endpoint: *const c_char,
        callback: Option<ReceiveTexture>
    ),
    textures_load_from_url
);
c_void_shim!(
    textures_load_from_memory: nexus_abi::LoadTextureFromMemory,
    (
        identifier: *const c_char,
        data: *mut c_void,
        size: usize,
        callback: Option<ReceiveTexture>
    ),
    textures_load_from_memory
);

c_void_shim!(
    quick_access_add: nexus_abi::AddShortcut,
    (
        identifier: *const c_char,
        texture: *const c_char,
        hover_texture: *const c_char,
        input_bind: *const c_char,
        tooltip: *const c_char
    ),
    quick_access_add
);
c_void_shim!(
    quick_access_remove: nexus_abi::QuickAccessGeneric,
    (identifier: *const c_char),
    quick_access_remove
);
c_void_shim!(
    quick_access_notify: nexus_abi::QuickAccessGeneric,
    (identifier: *const c_char),
    quick_access_notify
);
c_void_shim!(
    quick_access_add_simple: nexus_abi::AddSimpleShortcut,
    (identifier: *const c_char, callback: Option<RenderCallback>),
    quick_access_add_simple
);
c_void_shim!(
    quick_access_add_context_menu: nexus_abi::AddContextMenu,
    (
        identifier: *const c_char,
        target: *const c_char,
        callback: Option<RenderCallback>
    ),
    quick_access_add_context_menu
);
c_void_shim!(
    quick_access_remove_context_menu: nexus_abi::QuickAccessGeneric,
    (identifier: *const c_char),
    quick_access_remove_context_menu
);

c_return_shim!(
    localization_translate: nexus_abi::Translate,
    (identifier: *const c_char),
    -> *const c_char,
    core::ptr::null(),
    localization_translate
);
c_return_shim!(
    localization_translate_to: nexus_abi::TranslateTo,
    (identifier: *const c_char, language: *const c_char),
    -> *const c_char,
    core::ptr::null(),
    localization_translate_to
);
c_void_shim!(
    localization_set_translated_string: nexus_abi::SetTranslatedString,
    (
        identifier: *const c_char,
        language: *const c_char,
        value: *const c_char
    ),
    localization_set_translated_string
);

c_void_shim!(
    fonts_get: nexus_abi::GetOrReleaseFont,
    (identifier: *const c_char, callback: Option<ReceiveFont>),
    fonts_get
);
c_void_shim!(
    fonts_release: nexus_abi::GetOrReleaseFont,
    (identifier: *const c_char, callback: Option<ReceiveFont>),
    fonts_release
);
c_void_shim!(
    fonts_add_from_file: nexus_abi::AddFontFromFile,
    (
        identifier: *const c_char,
        size: f32,
        filename: *const c_char,
        callback: Option<ReceiveFont>,
        config: *mut c_void
    ),
    fonts_add_from_file
);
c_void_shim!(
    fonts_add_from_resource: nexus_abi::AddFontFromResource,
    (
        identifier: *const c_char,
        size: f32,
        resource_id: u32,
        module: *mut c_void,
        callback: Option<ReceiveFont>,
        config: *mut c_void
    ),
    fonts_add_from_resource
);
c_void_shim!(
    fonts_add_from_memory: nexus_abi::AddFontFromMemory,
    (
        identifier: *const c_char,
        size: f32,
        data: *mut c_void,
        data_size: usize,
        callback: Option<ReceiveFont>,
        config: *mut c_void
    ),
    fonts_add_from_memory
);
c_void_shim!(
    fonts_resize: nexus_abi::ResizeFont,
    (identifier: *const c_char, size: f32),
    fonts_resize
);

/// Assembles every modern and legacy callable slot for the selected session.
pub(crate) fn bindings(
    swap_chain: *mut c_void,
    imgui_context: *mut c_void,
    imgui_malloc: *mut c_void,
    imgui_free: *mut c_void,
) -> ApiBindings {
    ApiBindings {
        modern: AddonApiV6 {
            swap_chain,
            imgui_context,
            imgui_malloc,
            imgui_free,
            renderer: RendererVTable {
                register: Some(renderer_register),
                deregister: Some(renderer_deregister),
            },
            request_update: Some(request_update),
            log: Some(log),
            ui: UiVTable {
                send_alert: Some(ui_send_alert),
                register_close_on_escape: Some(ui_register_close_on_escape),
                deregister_close_on_escape: Some(ui_deregister_close_on_escape),
            },
            paths: PathsVTable {
                get_game_directory: Some(paths_get_game_directory),
                get_addon_directory: Some(paths_get_addon_directory),
                get_common_directory: Some(paths_get_common_directory),
            },
            min_hook: MinHookVTable {
                create: Some(min_hook_create),
                remove: Some(min_hook_remove),
                enable: Some(min_hook_enable),
                disable: Some(min_hook_disable),
            },
            events: EventsVTable {
                raise: Some(events_raise),
                raise_notification: Some(events_raise_notification),
                raise_targeted: Some(events_raise_targeted),
                raise_notification_targeted: Some(events_raise_notification_targeted),
                subscribe: Some(events_subscribe),
                unsubscribe: Some(events_unsubscribe),
            },
            wnd_proc: WndProcVTable {
                register: Some(wnd_proc_register),
                deregister: Some(wnd_proc_deregister),
                send_to_game_only: Some(wnd_proc_send_to_game_only),
            },
            input_binds: InputBindsVTable {
                invoke: Some(input_binds_invoke),
                register_with_string: Some(input_binds_register_with_string),
                register_with_struct: Some(input_binds_register_with_struct),
                deregister: Some(input_binds_deregister),
            },
            game_binds: GameBindsVTable {
                press_async: Some(game_binds_press_async),
                release_async: Some(game_binds_release_async),
                invoke_async: Some(game_binds_invoke_async),
                press: Some(game_binds_press),
                release: Some(game_binds_release),
                is_bound: Some(game_binds_is_bound),
            },
            data_link: DataLinkVTable {
                get: Some(data_link_get),
                share: Some(data_link_share),
            },
            textures: TexturesVTable {
                get: Some(textures_get),
                get_or_create_from_file: Some(textures_get_or_create_from_file),
                get_or_create_from_resource: Some(textures_get_or_create_from_resource),
                get_or_create_from_url: Some(textures_get_or_create_from_url),
                get_or_create_from_memory: Some(textures_get_or_create_from_memory),
                load_from_file: Some(textures_load_from_file),
                load_from_resource: Some(textures_load_from_resource),
                load_from_url: Some(textures_load_from_url),
                load_from_memory: Some(textures_load_from_memory),
            },
            quick_access: QuickAccessVTable {
                add: Some(quick_access_add),
                remove: Some(quick_access_remove),
                notify: Some(quick_access_notify),
                add_context_menu: Some(quick_access_add_context_menu),
                remove_context_menu: Some(quick_access_remove_context_menu),
            },
            localization: LocalizationVTable {
                translate: Some(localization_translate),
                translate_to: Some(localization_translate_to),
                set_translated_string: Some(localization_set_translated_string),
            },
            fonts: FontsVTable {
                get: Some(fonts_get),
                release: Some(fonts_release),
                add_from_file: Some(fonts_add_from_file),
                add_from_resource: Some(fonts_add_from_resource),
                add_from_memory: Some(fonts_add_from_memory),
                resize: Some(fonts_resize),
            },
        },
        legacy: LegacyApiBindings {
            log_v1,
            register_input_bind_with_string_v1: input_binds_register_with_string_v1,
            register_input_bind_with_struct_v1: input_binds_register_with_struct_v1,
            add_simple_shortcut: quick_access_add_simple,
        },
    }
}
