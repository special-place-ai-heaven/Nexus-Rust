//! Complete typed assembly of the native Nexus add-on API revisions.
//!
//! Revision 6 is the superset used by current service implementations. This
//! crate validates every modern slot, maps it into the five legacy flat tables,
//! and refuses to create a callable catalog when any supplied binding is null.

use core::ffi::c_void;

use nexus_abi::{
    AddSimpleShortcut, AddonApiV1, AddonApiV2, AddonApiV3, AddonApiV4, AddonApiV5, AddonApiV6,
    LogV1, RegisterInputBindStringV1, RegisterInputBindStructV1,
};
use nexus_host::{ApiTableCatalog, ApiTableError, ApiTables};
use thiserror::Error;

/// Legacy-only signatures that cannot be derived from the grouped revision 6.
#[derive(Clone, Copy)]
pub struct LegacyApiBindings {
    /// Revision-1 two-argument logger.
    pub log_v1: LogV1,
    /// Revision-1 through revision-3 string bind registration.
    pub register_input_bind_with_string_v1: RegisterInputBindStringV1,
    /// Revision-1 through revision-3 structured bind registration.
    pub register_input_bind_with_struct_v1: RegisterInputBindStructV1,
    /// Revision-1 through revision-5 untargeted QuickAccess context item.
    pub add_simple_shortcut: AddSimpleShortcut,
}

/// Complete service bindings used to assemble all supported API revisions.
#[derive(Clone, Copy)]
pub struct ApiBindings {
    /// Grouped modern API. Every pointer and optional function must be present.
    pub modern: AddonApiV6,
    /// Signatures retained only by legacy flat revisions.
    pub legacy: LegacyApiBindings,
}

/// Failure to assemble a complete callable API catalog.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ApiAssemblyError {
    /// A required modern pointer or function was null.
    #[error("required addon API binding `{field}` is missing")]
    MissingBinding {
        /// Closed, static field path within revision 6.
        field: &'static str,
    },
    /// The target cannot represent one of the pinned ABI table layouts.
    #[error(transparent)]
    Table(#[from] ApiTableError),
}

impl ApiBindings {
    /// Validates the modern superset and builds every typed revision.
    ///
    /// # Errors
    ///
    /// Returns the first closed field path whose binding is missing.
    pub fn assemble(self) -> Result<ApiTables, ApiAssemblyError> {
        validate_modern(&self.modern)?;
        let modern = self.modern;
        let legacy = self.legacy;

        Ok(ApiTables {
            v1: AddonApiV1 {
                swap_chain: modern.swap_chain,
                imgui_context: modern.imgui_context,
                imgui_malloc: modern.imgui_malloc,
                imgui_free: modern.imgui_free,
                register_render: modern.renderer.register,
                deregister_render: modern.renderer.deregister,
                get_game_directory: modern.paths.get_game_directory,
                get_addon_directory: modern.paths.get_addon_directory,
                get_common_directory: modern.paths.get_common_directory,
                create_hook: modern.min_hook.create,
                remove_hook: modern.min_hook.remove,
                enable_hook: modern.min_hook.enable,
                disable_hook: modern.min_hook.disable,
                log: Some(legacy.log_v1),
                raise_event: modern.events.raise,
                subscribe_event: modern.events.subscribe,
                unsubscribe_event: modern.events.unsubscribe,
                register_wnd_proc: modern.wnd_proc.register,
                deregister_wnd_proc: modern.wnd_proc.deregister,
                register_input_bind_with_string: Some(legacy.register_input_bind_with_string_v1),
                register_input_bind_with_struct: Some(legacy.register_input_bind_with_struct_v1),
                deregister_input_bind: modern.input_binds.deregister,
                get_resource: modern.data_link.get,
                share_resource: modern.data_link.share,
                get_texture: modern.textures.get,
                load_texture_from_file: modern.textures.load_from_file,
                load_texture_from_resource: modern.textures.load_from_resource,
                load_texture_from_url: modern.textures.load_from_url,
                add_shortcut: modern.quick_access.add,
                remove_shortcut: modern.quick_access.remove,
                add_simple_shortcut: Some(legacy.add_simple_shortcut),
                remove_simple_shortcut: modern.quick_access.remove_context_menu,
            },
            v2: AddonApiV2 {
                swap_chain: modern.swap_chain,
                imgui_context: modern.imgui_context,
                imgui_malloc: modern.imgui_malloc,
                imgui_free: modern.imgui_free,
                register_render: modern.renderer.register,
                deregister_render: modern.renderer.deregister,
                get_game_directory: modern.paths.get_game_directory,
                get_addon_directory: modern.paths.get_addon_directory,
                get_common_directory: modern.paths.get_common_directory,
                create_hook: modern.min_hook.create,
                remove_hook: modern.min_hook.remove,
                enable_hook: modern.min_hook.enable,
                disable_hook: modern.min_hook.disable,
                log: modern.log,
                raise_event: modern.events.raise,
                raise_event_notification: modern.events.raise_notification,
                subscribe_event: modern.events.subscribe,
                unsubscribe_event: modern.events.unsubscribe,
                register_wnd_proc: modern.wnd_proc.register,
                deregister_wnd_proc: modern.wnd_proc.deregister,
                send_wnd_proc_to_game_only: modern.wnd_proc.send_to_game_only,
                register_input_bind_with_string: Some(legacy.register_input_bind_with_string_v1),
                register_input_bind_with_struct: Some(legacy.register_input_bind_with_struct_v1),
                deregister_input_bind: modern.input_binds.deregister,
                get_resource: modern.data_link.get,
                share_resource: modern.data_link.share,
                get_texture: modern.textures.get,
                get_texture_or_create_from_file: modern.textures.get_or_create_from_file,
                get_texture_or_create_from_resource: modern.textures.get_or_create_from_resource,
                get_texture_or_create_from_url: modern.textures.get_or_create_from_url,
                get_texture_or_create_from_memory: modern.textures.get_or_create_from_memory,
                load_texture_from_file: modern.textures.load_from_file,
                load_texture_from_resource: modern.textures.load_from_resource,
                load_texture_from_url: modern.textures.load_from_url,
                load_texture_from_memory: modern.textures.load_from_memory,
                add_shortcut: modern.quick_access.add,
                remove_shortcut: modern.quick_access.remove,
                push_notification: modern.quick_access.notify,
                add_simple_shortcut: Some(legacy.add_simple_shortcut),
                remove_simple_shortcut: modern.quick_access.remove_context_menu,
                translate: modern.localization.translate,
                translate_to: modern.localization.translate_to,
            },
            v3: AddonApiV3 {
                swap_chain: modern.swap_chain,
                imgui_context: modern.imgui_context,
                imgui_malloc: modern.imgui_malloc,
                imgui_free: modern.imgui_free,
                register_render: modern.renderer.register,
                deregister_render: modern.renderer.deregister,
                get_game_directory: modern.paths.get_game_directory,
                get_addon_directory: modern.paths.get_addon_directory,
                get_common_directory: modern.paths.get_common_directory,
                create_hook: modern.min_hook.create,
                remove_hook: modern.min_hook.remove,
                enable_hook: modern.min_hook.enable,
                disable_hook: modern.min_hook.disable,
                log: modern.log,
                send_alert: modern.ui.send_alert,
                raise_event: modern.events.raise,
                raise_event_notification: modern.events.raise_notification,
                raise_event_targeted: modern.events.raise_targeted,
                raise_event_notification_targeted: modern.events.raise_notification_targeted,
                subscribe_event: modern.events.subscribe,
                unsubscribe_event: modern.events.unsubscribe,
                register_wnd_proc: modern.wnd_proc.register,
                deregister_wnd_proc: modern.wnd_proc.deregister,
                send_wnd_proc_to_game_only: modern.wnd_proc.send_to_game_only,
                register_input_bind_with_string: Some(legacy.register_input_bind_with_string_v1),
                register_input_bind_with_struct: Some(legacy.register_input_bind_with_struct_v1),
                deregister_input_bind: modern.input_binds.deregister,
                get_resource: modern.data_link.get,
                share_resource: modern.data_link.share,
                get_texture: modern.textures.get,
                get_texture_or_create_from_file: modern.textures.get_or_create_from_file,
                get_texture_or_create_from_resource: modern.textures.get_or_create_from_resource,
                get_texture_or_create_from_url: modern.textures.get_or_create_from_url,
                get_texture_or_create_from_memory: modern.textures.get_or_create_from_memory,
                load_texture_from_file: modern.textures.load_from_file,
                load_texture_from_resource: modern.textures.load_from_resource,
                load_texture_from_url: modern.textures.load_from_url,
                load_texture_from_memory: modern.textures.load_from_memory,
                add_shortcut: modern.quick_access.add,
                remove_shortcut: modern.quick_access.remove,
                push_notification: modern.quick_access.notify,
                add_simple_shortcut: Some(legacy.add_simple_shortcut),
                remove_simple_shortcut: modern.quick_access.remove_context_menu,
                translate: modern.localization.translate,
                translate_to: modern.localization.translate_to,
            },
            v4: AddonApiV4 {
                swap_chain: modern.swap_chain,
                imgui_context: modern.imgui_context,
                imgui_malloc: modern.imgui_malloc,
                imgui_free: modern.imgui_free,
                register_render: modern.renderer.register,
                deregister_render: modern.renderer.deregister,
                request_update: modern.request_update,
                get_game_directory: modern.paths.get_game_directory,
                get_addon_directory: modern.paths.get_addon_directory,
                get_common_directory: modern.paths.get_common_directory,
                create_hook: modern.min_hook.create,
                remove_hook: modern.min_hook.remove,
                enable_hook: modern.min_hook.enable,
                disable_hook: modern.min_hook.disable,
                log: modern.log,
                send_alert: modern.ui.send_alert,
                raise_event: modern.events.raise,
                raise_event_notification: modern.events.raise_notification,
                raise_event_targeted: modern.events.raise_targeted,
                raise_event_notification_targeted: modern.events.raise_notification_targeted,
                subscribe_event: modern.events.subscribe,
                unsubscribe_event: modern.events.unsubscribe,
                register_wnd_proc: modern.wnd_proc.register,
                deregister_wnd_proc: modern.wnd_proc.deregister,
                send_wnd_proc_to_game_only: modern.wnd_proc.send_to_game_only,
                register_input_bind_with_string: modern.input_binds.register_with_string,
                register_input_bind_with_struct: modern.input_binds.register_with_struct,
                deregister_input_bind: modern.input_binds.deregister,
                get_resource: modern.data_link.get,
                share_resource: modern.data_link.share,
                get_texture: modern.textures.get,
                get_texture_or_create_from_file: modern.textures.get_or_create_from_file,
                get_texture_or_create_from_resource: modern.textures.get_or_create_from_resource,
                get_texture_or_create_from_url: modern.textures.get_or_create_from_url,
                get_texture_or_create_from_memory: modern.textures.get_or_create_from_memory,
                load_texture_from_file: modern.textures.load_from_file,
                load_texture_from_resource: modern.textures.load_from_resource,
                load_texture_from_url: modern.textures.load_from_url,
                load_texture_from_memory: modern.textures.load_from_memory,
                add_shortcut: modern.quick_access.add,
                remove_shortcut: modern.quick_access.remove,
                push_notification: modern.quick_access.notify,
                add_simple_shortcut: Some(legacy.add_simple_shortcut),
                remove_simple_shortcut: modern.quick_access.remove_context_menu,
                translate: modern.localization.translate,
                translate_to: modern.localization.translate_to,
                get_font: modern.fonts.get,
                release_font: modern.fonts.release,
                add_font_from_file: modern.fonts.add_from_file,
                add_font_from_resource: modern.fonts.add_from_resource,
                add_font_from_memory: modern.fonts.add_from_memory,
            },
            v5: AddonApiV5 {
                swap_chain: modern.swap_chain,
                imgui_context: modern.imgui_context,
                imgui_malloc: modern.imgui_malloc,
                imgui_free: modern.imgui_free,
                register_render: modern.renderer.register,
                deregister_render: modern.renderer.deregister,
                request_update: modern.request_update,
                get_game_directory: modern.paths.get_game_directory,
                get_addon_directory: modern.paths.get_addon_directory,
                get_common_directory: modern.paths.get_common_directory,
                create_hook: modern.min_hook.create,
                remove_hook: modern.min_hook.remove,
                enable_hook: modern.min_hook.enable,
                disable_hook: modern.min_hook.disable,
                log: modern.log,
                send_alert: modern.ui.send_alert,
                raise_event: modern.events.raise,
                raise_event_notification: modern.events.raise_notification,
                raise_event_targeted: modern.events.raise_targeted,
                raise_event_notification_targeted: modern.events.raise_notification_targeted,
                subscribe_event: modern.events.subscribe,
                unsubscribe_event: modern.events.unsubscribe,
                register_wnd_proc: modern.wnd_proc.register,
                deregister_wnd_proc: modern.wnd_proc.deregister,
                send_wnd_proc_to_game_only: modern.wnd_proc.send_to_game_only,
                invoke_input_bind: modern.input_binds.invoke,
                register_input_bind_with_string: modern.input_binds.register_with_string,
                register_input_bind_with_struct: modern.input_binds.register_with_struct,
                deregister_input_bind: modern.input_binds.deregister,
                get_resource: modern.data_link.get,
                share_resource: modern.data_link.share,
                get_texture: modern.textures.get,
                get_texture_or_create_from_file: modern.textures.get_or_create_from_file,
                get_texture_or_create_from_resource: modern.textures.get_or_create_from_resource,
                get_texture_or_create_from_url: modern.textures.get_or_create_from_url,
                get_texture_or_create_from_memory: modern.textures.get_or_create_from_memory,
                load_texture_from_file: modern.textures.load_from_file,
                load_texture_from_resource: modern.textures.load_from_resource,
                load_texture_from_url: modern.textures.load_from_url,
                load_texture_from_memory: modern.textures.load_from_memory,
                add_shortcut: modern.quick_access.add,
                remove_shortcut: modern.quick_access.remove,
                push_notification: modern.quick_access.notify,
                add_simple_shortcut: Some(legacy.add_simple_shortcut),
                remove_simple_shortcut: modern.quick_access.remove_context_menu,
                translate: modern.localization.translate,
                translate_to: modern.localization.translate_to,
                set_translated_string: modern.localization.set_translated_string,
                get_font: modern.fonts.get,
                release_font: modern.fonts.release,
                add_font_from_file: modern.fonts.add_from_file,
                add_font_from_resource: modern.fonts.add_from_resource,
                add_font_from_memory: modern.fonts.add_from_memory,
            },
            v6: modern,
        })
    }

    /// Builds the pinned catalog handed to the add-on host.
    ///
    /// # Errors
    ///
    /// Returns a missing-binding or table-layout error.
    pub fn catalog(self) -> Result<ApiTableCatalog, ApiAssemblyError> {
        Ok(ApiTableCatalog::from_tables(self.assemble()?)?)
    }
}

fn validate_modern(api: &AddonApiV6) -> Result<(), ApiAssemblyError> {
    require_pointer(api.swap_chain, "swap_chain")?;
    require_pointer(api.imgui_context, "imgui_context")?;
    require_pointer(api.imgui_malloc, "imgui_malloc")?;
    require_pointer(api.imgui_free, "imgui_free")?;

    require(api.renderer.register, "renderer.register")?;
    require(api.renderer.deregister, "renderer.deregister")?;
    require(api.request_update, "request_update")?;
    require(api.log, "log")?;
    require(api.ui.send_alert, "ui.send_alert")?;
    require(
        api.ui.register_close_on_escape,
        "ui.register_close_on_escape",
    )?;
    require(
        api.ui.deregister_close_on_escape,
        "ui.deregister_close_on_escape",
    )?;
    require(api.paths.get_game_directory, "paths.get_game_directory")?;
    require(api.paths.get_addon_directory, "paths.get_addon_directory")?;
    require(api.paths.get_common_directory, "paths.get_common_directory")?;
    require(api.min_hook.create, "min_hook.create")?;
    require(api.min_hook.remove, "min_hook.remove")?;
    require(api.min_hook.enable, "min_hook.enable")?;
    require(api.min_hook.disable, "min_hook.disable")?;
    require(api.events.raise, "events.raise")?;
    require(api.events.raise_notification, "events.raise_notification")?;
    require(api.events.raise_targeted, "events.raise_targeted")?;
    require(
        api.events.raise_notification_targeted,
        "events.raise_notification_targeted",
    )?;
    require(api.events.subscribe, "events.subscribe")?;
    require(api.events.unsubscribe, "events.unsubscribe")?;
    require(api.wnd_proc.register, "wnd_proc.register")?;
    require(api.wnd_proc.deregister, "wnd_proc.deregister")?;
    require(api.wnd_proc.send_to_game_only, "wnd_proc.send_to_game_only")?;
    require(api.input_binds.invoke, "input_binds.invoke")?;
    require(
        api.input_binds.register_with_string,
        "input_binds.register_with_string",
    )?;
    require(
        api.input_binds.register_with_struct,
        "input_binds.register_with_struct",
    )?;
    require(api.input_binds.deregister, "input_binds.deregister")?;
    require(api.game_binds.press_async, "game_binds.press_async")?;
    require(api.game_binds.release_async, "game_binds.release_async")?;
    require(api.game_binds.invoke_async, "game_binds.invoke_async")?;
    require(api.game_binds.press, "game_binds.press")?;
    require(api.game_binds.release, "game_binds.release")?;
    require(api.game_binds.is_bound, "game_binds.is_bound")?;
    require(api.data_link.get, "data_link.get")?;
    require(api.data_link.share, "data_link.share")?;
    require(api.textures.get, "textures.get")?;
    require(
        api.textures.get_or_create_from_file,
        "textures.get_or_create_from_file",
    )?;
    require(
        api.textures.get_or_create_from_resource,
        "textures.get_or_create_from_resource",
    )?;
    require(
        api.textures.get_or_create_from_url,
        "textures.get_or_create_from_url",
    )?;
    require(
        api.textures.get_or_create_from_memory,
        "textures.get_or_create_from_memory",
    )?;
    require(api.textures.load_from_file, "textures.load_from_file")?;
    require(
        api.textures.load_from_resource,
        "textures.load_from_resource",
    )?;
    require(api.textures.load_from_url, "textures.load_from_url")?;
    require(api.textures.load_from_memory, "textures.load_from_memory")?;
    require(api.quick_access.add, "quick_access.add")?;
    require(api.quick_access.remove, "quick_access.remove")?;
    require(api.quick_access.notify, "quick_access.notify")?;
    require(
        api.quick_access.add_context_menu,
        "quick_access.add_context_menu",
    )?;
    require(
        api.quick_access.remove_context_menu,
        "quick_access.remove_context_menu",
    )?;
    require(api.localization.translate, "localization.translate")?;
    require(api.localization.translate_to, "localization.translate_to")?;
    require(
        api.localization.set_translated_string,
        "localization.set_translated_string",
    )?;
    require(api.fonts.get, "fonts.get")?;
    require(api.fonts.release, "fonts.release")?;
    require(api.fonts.add_from_file, "fonts.add_from_file")?;
    require(api.fonts.add_from_resource, "fonts.add_from_resource")?;
    require(api.fonts.add_from_memory, "fonts.add_from_memory")?;
    require(api.fonts.resize, "fonts.resize")
}

fn require_pointer(pointer: *mut c_void, field: &'static str) -> Result<(), ApiAssemblyError> {
    if pointer.is_null() {
        Err(ApiAssemblyError::MissingBinding { field })
    } else {
        Ok(())
    }
}

fn require<T>(binding: Option<T>, field: &'static str) -> Result<(), ApiAssemblyError> {
    if binding.is_none() {
        Err(ApiAssemblyError::MissingBinding { field })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use core::{ffi::c_void, mem};

    use nexus_abi::{
        AddonApiV1, AddonApiV6, DataLinkVTable, EventsVTable, FontsVTable, GameBindsVTable,
        InputBindsVTable, LocalizationVTable, MinHookVTable, PathsVTable, QuickAccessVTable,
        RendererVTable, TexturesVTable, UiVTable, WndProcVTable,
    };
    use nexus_host::ApiRevision;

    use super::{ApiAssemblyError, ApiBindings, LegacyApiBindings};

    unsafe extern "C" fn marker() {}

    unsafe fn marker_as<T: Copy>() -> T {
        let marker_pointer: unsafe extern "C" fn() = marker;
        assert_eq!(mem::size_of::<T>(), mem::size_of_val(&marker_pointer));
        // SAFETY: every requested `T` is a function-pointer alias with the same
        // representation. Tests never invoke the deliberately signature-erased
        // marker; they only verify non-null table assembly and field mapping.
        unsafe { mem::transmute_copy(&marker_pointer) }
    }

    fn present<T: Copy>() -> Option<T> {
        // SAFETY: see `marker_as`; the returned pointer is never invoked.
        Some(unsafe { marker_as() })
    }

    fn legacy_marker<T: Copy>() -> T {
        // SAFETY: every call requests a legacy function-pointer alias and the
        // returned marker is inspected but never invoked.
        unsafe { marker_as() }
    }

    fn valid_bindings() -> ApiBindings {
        let pointer = marker as *const () as *mut c_void;
        ApiBindings {
            modern: AddonApiV6 {
                swap_chain: pointer,
                imgui_context: pointer,
                imgui_malloc: pointer,
                imgui_free: pointer,
                renderer: RendererVTable {
                    register: present(),
                    deregister: present(),
                },
                request_update: present(),
                log: present(),
                ui: UiVTable {
                    send_alert: present(),
                    register_close_on_escape: present(),
                    deregister_close_on_escape: present(),
                },
                paths: PathsVTable {
                    get_game_directory: present(),
                    get_addon_directory: present(),
                    get_common_directory: present(),
                },
                min_hook: MinHookVTable {
                    create: present(),
                    remove: present(),
                    enable: present(),
                    disable: present(),
                },
                events: EventsVTable {
                    raise: present(),
                    raise_notification: present(),
                    raise_targeted: present(),
                    raise_notification_targeted: present(),
                    subscribe: present(),
                    unsubscribe: present(),
                },
                wnd_proc: WndProcVTable {
                    register: present(),
                    deregister: present(),
                    send_to_game_only: present(),
                },
                input_binds: InputBindsVTable {
                    invoke: present(),
                    register_with_string: present(),
                    register_with_struct: present(),
                    deregister: present(),
                },
                game_binds: GameBindsVTable {
                    press_async: present(),
                    release_async: present(),
                    invoke_async: present(),
                    press: present(),
                    release: present(),
                    is_bound: present(),
                },
                data_link: DataLinkVTable {
                    get: present(),
                    share: present(),
                },
                textures: TexturesVTable {
                    get: present(),
                    get_or_create_from_file: present(),
                    get_or_create_from_resource: present(),
                    get_or_create_from_url: present(),
                    get_or_create_from_memory: present(),
                    load_from_file: present(),
                    load_from_resource: present(),
                    load_from_url: present(),
                    load_from_memory: present(),
                },
                quick_access: QuickAccessVTable {
                    add: present(),
                    remove: present(),
                    notify: present(),
                    add_context_menu: present(),
                    remove_context_menu: present(),
                },
                localization: LocalizationVTable {
                    translate: present(),
                    translate_to: present(),
                    set_translated_string: present(),
                },
                fonts: FontsVTable {
                    get: present(),
                    release: present(),
                    add_from_file: present(),
                    add_from_resource: present(),
                    add_from_memory: present(),
                    resize: present(),
                },
            },
            legacy: LegacyApiBindings {
                log_v1: legacy_marker(),
                register_input_bind_with_string_v1: legacy_marker(),
                register_input_bind_with_struct_v1: legacy_marker(),
                add_simple_shortcut: legacy_marker(),
            },
        }
    }

    #[test]
    fn rejects_the_exact_missing_modern_slot() {
        let mut bindings = valid_bindings();
        bindings.modern.fonts.resize = None;

        assert_eq!(
            bindings.assemble().err(),
            Some(ApiAssemblyError::MissingBinding {
                field: "fonts.resize"
            })
        );
    }

    #[test]
    fn maps_the_modern_superset_into_every_pinned_revision() {
        let marker_pointer = marker as *const () as *mut c_void;
        let catalog = valid_bindings()
            .catalog()
            .expect("all bindings are present");

        assert!(catalog.is_populated());
        let mut addresses = ApiRevision::ALL
            .map(|revision| catalog.get(revision).as_opaque_ptr().as_ptr() as usize);
        addresses.sort_unstable();
        assert!(addresses.windows(2).all(|pair| pair[0] != pair[1]));
        // SAFETY: the populated catalog stores this exact revision layout for
        // its complete lifetime.
        let v1 = unsafe {
            catalog
                .get(ApiRevision::V1)
                .as_opaque_ptr()
                .cast::<AddonApiV1>()
                .as_ref()
        };
        assert_eq!(v1.swap_chain, marker_pointer);
        assert!(v1.log.is_some());
        assert!(v1.add_simple_shortcut.is_some());
    }

    #[test]
    fn null_base_pointer_is_never_treated_as_a_callable_catalog() {
        let mut bindings = valid_bindings();
        bindings.modern.imgui_context = core::ptr::null_mut();

        assert_eq!(
            bindings.catalog().err(),
            Some(ApiAssemblyError::MissingBinding {
                field: "imgui_context"
            })
        );
    }
}
