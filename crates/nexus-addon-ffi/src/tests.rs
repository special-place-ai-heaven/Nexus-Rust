use core::ffi::{c_char, c_void};
use core::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use nexus_abi::{
    EventCallback, GameBind, InputBindCallbackV1, InputBindCallbackV2, InputBindV1, LogLevel,
    MinHookStatus, ReceiveFont, ReceiveTexture, RenderCallback, RenderPhase, Texture,
    WndProcCallback,
};

use super::{
    AddonApiBackend, ApiRevision, ApiTableRef, InstalledAddonApi, MINHOOK_ERROR_NOT_INITIALIZED,
    install_render_session,
};
use crate::{dispatcher, shims};

static TEST_SERIAL: Mutex<()> = Mutex::new(());

struct TestGuard {
    _serial: MutexGuard<'static, ()>,
}

impl Drop for TestGuard {
    fn drop(&mut self) {
        dispatcher::clear_active_for_test();
    }
}

fn begin_test() -> TestGuard {
    let serial = TEST_SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    dispatcher::clear_active_for_test();
    TestGuard { _serial: serial }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Default)]
struct RecordingBackend {
    calls: Mutex<Vec<&'static str>>,
    panic_on: Mutex<Option<&'static str>>,
    reenter_log: AtomicBool,
    reenter_drop: AtomicBool,
    panic_drop: AtomicBool,
}

impl RecordingBackend {
    fn hit(&self, operation: &'static str) {
        lock(&self.calls).push(operation);
        if *lock(&self.panic_on) == Some(operation) {
            panic!("recording backend panic");
        }
    }

    fn calls(&self) -> Vec<&'static str> {
        lock(&self.calls).clone()
    }

    fn clear_calls(&self) {
        lock(&self.calls).clear();
    }

    fn panic_on(&self, operation: Option<&'static str>) {
        *lock(&self.panic_on) = operation;
    }
}

impl Drop for RecordingBackend {
    fn drop(&mut self) {
        if self.reenter_drop.load(Ordering::SeqCst) {
            // SAFETY: the shim treats its null string as opaque; the active
            // recording backend neither dereferences nor retains it.
            unsafe { shims::ui_send_alert(core::ptr::null()) };
        }
        if self.panic_drop.load(Ordering::SeqCst) {
            panic!("recording backend destructor panic");
        }
    }
}

macro_rules! record_void {
    ($name:ident($($argument:ident: $argument_type:ty),* $(,)?)) => {
        fn $name(&self, $($argument: $argument_type),*) {
            let _ = ($($argument,)*);
            self.hit(stringify!($name));
        }
    };
}

macro_rules! record_return {
    (
        $name:ident($($argument:ident: $argument_type:ty),* $(,)?)
        -> $return_type:ty = $result:expr
    ) => {
        fn $name(&self, $($argument: $argument_type),*) -> $return_type {
            let _ = ($($argument,)*);
            self.hit(stringify!($name));
            $result
        }
    };
}

macro_rules! record_unsafe_void {
    ($name:ident($($argument:ident: $argument_type:ty),* $(,)?)) => {
        unsafe fn $name(&self, $($argument: $argument_type),*) {
            let _ = ($($argument,)*);
            self.hit(stringify!($name));
        }
    };
}

impl AddonApiBackend for RecordingBackend {
    record_void!(renderer_register(phase: RenderPhase, callback: Option<RenderCallback>));
    record_void!(renderer_deregister(callback: Option<RenderCallback>));
    record_void!(request_update(signature: i32, update_url: *const c_char));

    fn log(&self, level: LogLevel, channel: *const c_char, message: *const c_char) {
        let _ = (level, channel, message);
        self.hit("log");
        if self.reenter_log.swap(false, Ordering::SeqCst) {
            // SAFETY: the nested shim passes an opaque null pointer to this
            // recording backend and performs no native memory access.
            unsafe { shims::ui_send_alert(core::ptr::null()) };
        }
    }

    record_void!(log_v1(level: LogLevel, message: *const c_char));
    record_void!(ui_send_alert(message: *const c_char));
    record_unsafe_void!(ui_register_close_on_escape(
        identifier: *const c_char,
        state: *mut u8
    ));
    record_void!(ui_deregister_close_on_escape(identifier: *const c_char));

    record_return!(paths_get_game_directory() -> *const c_char = NonNull::<c_char>::dangling().as_ptr());
    record_return!(paths_get_addon_directory(name: *const c_char) -> *const c_char = NonNull::<c_char>::dangling().as_ptr());
    record_return!(paths_get_common_directory() -> *const c_char = NonNull::<c_char>::dangling().as_ptr());

    unsafe fn min_hook_create(
        &self,
        target: *mut c_void,
        detour: *mut c_void,
        original: *mut *mut c_void,
    ) -> MinHookStatus {
        let _ = (target, detour, original);
        self.hit("min_hook_create");
        MinHookStatus(77)
    }
    record_return!(min_hook_remove(target: *mut c_void) -> MinHookStatus = MinHookStatus(77));
    record_return!(min_hook_enable(target: *mut c_void) -> MinHookStatus = MinHookStatus(77));
    record_return!(min_hook_disable(target: *mut c_void) -> MinHookStatus = MinHookStatus(77));

    record_unsafe_void!(events_raise(identifier: *const c_char, payload: *mut c_void));
    record_void!(events_raise_notification(identifier: *const c_char));
    record_unsafe_void!(events_raise_targeted(
        signature: u32,
        identifier: *const c_char,
        payload: *mut c_void,
    ));
    record_void!(events_raise_notification_targeted(
        signature: u32,
        identifier: *const c_char,
    ));
    record_void!(events_subscribe(identifier: *const c_char, callback: Option<EventCallback>));
    record_void!(events_unsubscribe(identifier: *const c_char, callback: Option<EventCallback>));

    record_void!(wnd_proc_register(callback: Option<WndProcCallback>));
    record_void!(wnd_proc_deregister(callback: Option<WndProcCallback>));
    record_return!(wnd_proc_send_to_game_only(
        hwnd: *mut c_void,
        message: u32,
        w_param: usize,
        l_param: isize,
    ) -> isize = 77);

    record_void!(input_binds_invoke(identifier: *const c_char, is_release: u8));
    record_void!(input_binds_register_with_string(
        identifier: *const c_char,
        callback: Option<InputBindCallbackV2>,
        bind: *const c_char,
    ));
    record_void!(input_binds_register_with_struct(
        identifier: *const c_char,
        callback: Option<InputBindCallbackV2>,
        bind: InputBindV1,
    ));
    record_void!(input_binds_register_with_string_v1(
        identifier: *const c_char,
        callback: Option<InputBindCallbackV1>,
        bind: *const c_char,
    ));
    record_void!(input_binds_register_with_struct_v1(
        identifier: *const c_char,
        callback: Option<InputBindCallbackV1>,
        bind: InputBindV1,
    ));
    record_void!(input_binds_deregister(identifier: *const c_char));

    record_void!(game_binds_press_async(bind: GameBind));
    record_void!(game_binds_release_async(bind: GameBind));
    record_void!(game_binds_invoke_async(bind: GameBind, duration: i32));
    record_void!(game_binds_press(bind: GameBind));
    record_void!(game_binds_release(bind: GameBind));
    record_return!(game_binds_is_bound(bind: GameBind) -> u8 = 1);

    record_return!(data_link_get(identifier: *const c_char) -> *mut c_void = NonNull::<u8>::dangling().as_ptr().cast());
    record_return!(data_link_share(identifier: *const c_char, size: usize) -> *mut c_void = NonNull::<u8>::dangling().as_ptr().cast());

    record_return!(textures_get(identifier: *const c_char) -> *mut Texture = NonNull::<Texture>::dangling().as_ptr());
    record_return!(textures_get_or_create_from_file(
        identifier: *const c_char,
        filename: *const c_char,
    ) -> *mut Texture = NonNull::<Texture>::dangling().as_ptr());
    record_return!(textures_get_or_create_from_resource(
        identifier: *const c_char,
        resource_id: u32,
        module: *mut c_void,
    ) -> *mut Texture = NonNull::<Texture>::dangling().as_ptr());
    record_return!(textures_get_or_create_from_url(
        identifier: *const c_char,
        remote: *const c_char,
        endpoint: *const c_char,
    ) -> *mut Texture = NonNull::<Texture>::dangling().as_ptr());
    record_return!(textures_get_or_create_from_memory(
        identifier: *const c_char,
        data: *mut c_void,
        size: usize,
    ) -> *mut Texture = NonNull::<Texture>::dangling().as_ptr());
    record_void!(textures_load_from_file(
        identifier: *const c_char,
        filename: *const c_char,
        callback: Option<ReceiveTexture>,
    ));
    record_void!(textures_load_from_resource(
        identifier: *const c_char,
        resource_id: u32,
        module: *mut c_void,
        callback: Option<ReceiveTexture>,
    ));
    record_void!(textures_load_from_url(
        identifier: *const c_char,
        remote: *const c_char,
        endpoint: *const c_char,
        callback: Option<ReceiveTexture>,
    ));
    record_void!(textures_load_from_memory(
        identifier: *const c_char,
        data: *mut c_void,
        size: usize,
        callback: Option<ReceiveTexture>,
    ));

    record_void!(quick_access_add(
        identifier: *const c_char,
        texture: *const c_char,
        hover_texture: *const c_char,
        input_bind: *const c_char,
        tooltip: *const c_char,
    ));
    record_void!(quick_access_remove(identifier: *const c_char));
    record_void!(quick_access_notify(identifier: *const c_char));
    record_void!(quick_access_add_simple(
        identifier: *const c_char,
        callback: Option<RenderCallback>,
    ));
    record_void!(quick_access_add_context_menu(
        identifier: *const c_char,
        target: *const c_char,
        callback: Option<RenderCallback>,
    ));
    record_void!(quick_access_remove_context_menu(identifier: *const c_char));

    record_return!(localization_translate(identifier: *const c_char) -> *const c_char = NonNull::<c_char>::dangling().as_ptr());
    record_return!(localization_translate_to(
        identifier: *const c_char,
        language: *const c_char,
    ) -> *const c_char = NonNull::<c_char>::dangling().as_ptr());
    record_void!(localization_set_translated_string(
        identifier: *const c_char,
        language: *const c_char,
        value: *const c_char,
    ));

    record_void!(fonts_get(identifier: *const c_char, callback: Option<ReceiveFont>));
    record_void!(fonts_release(identifier: *const c_char, callback: Option<ReceiveFont>));
    record_void!(fonts_add_from_file(
        identifier: *const c_char,
        size: f32,
        filename: *const c_char,
        callback: Option<ReceiveFont>,
        config: *mut c_void,
    ));
    record_void!(fonts_add_from_resource(
        identifier: *const c_char,
        size: f32,
        resource_id: u32,
        module: *mut c_void,
        callback: Option<ReceiveFont>,
        config: *mut c_void,
    ));
    record_void!(fonts_add_from_memory(
        identifier: *const c_char,
        size: f32,
        data: *mut c_void,
        data_size: usize,
        callback: Option<ReceiveFont>,
        config: *mut c_void,
    ));
    record_void!(fonts_resize(identifier: *const c_char, size: f32));
}

struct TestRenderResources {
    swap_chain: Box<u8>,
    imgui_context: Box<u8>,
}

impl TestRenderResources {
    fn new() -> Self {
        Self {
            swap_chain: Box::new(0),
            imgui_context: Box::new(0),
        }
    }

    fn pointers(
        &mut self,
    ) -> (
        NonNull<c_void>,
        NonNull<nexus_imgui_compat::sys::ImGuiContext>,
    ) {
        (
            NonNull::from(self.swap_chain.as_mut()).cast(),
            NonNull::from(self.imgui_context.as_mut()).cast(),
        )
    }

    fn install(&mut self, generation: u64, backend: Arc<dyn AddonApiBackend>) -> InstalledAddonApi {
        let (swap_chain, imgui_context) = self.pointers();
        // SAFETY: both opaque allocations are owned by `self`, remain alive
        // for the returned installation's test scope, and no test backend
        // dereferences either native pointer.
        match unsafe { install_render_session(generation, swap_chain, imgui_context, backend) } {
            Ok(installation) => installation,
            Err(error) => panic!("test installation failed: {error}"),
        }
    }
}

fn as_backend(backend: &Arc<RecordingBackend>) -> Arc<dyn AddonApiBackend> {
    Arc::clone(backend) as Arc<dyn AddonApiBackend>
}

#[test]
fn every_modern_and_legacy_shim_dispatches_to_the_exact_backend_operation() {
    let _guard = begin_test();
    let backend = Arc::new(RecordingBackend::default());
    let mut resources = TestRenderResources::new();
    let _installation = resources.install(7, as_backend(&backend));
    let bind = InputBindV1 {
        key: 1,
        alt: 0,
        ctrl: 0,
        shift: 0,
    };

    // SAFETY: all pointers are opaque to the recording backend, every
    // callback is absent, and no shim dereferences or retains an argument.
    unsafe {
        shims::renderer_register(RenderPhase::RENDER, None);
        shims::renderer_deregister(None);
        shims::request_update(1, core::ptr::null());
        shims::log(LogLevel(1), core::ptr::null(), core::ptr::null());
        shims::log_v1(LogLevel(1), core::ptr::null());
        shims::ui_send_alert(core::ptr::null());
        shims::ui_register_close_on_escape(core::ptr::null(), core::ptr::null_mut());
        shims::ui_deregister_close_on_escape(core::ptr::null());
        assert!(!shims::paths_get_game_directory().is_null());
        assert!(!shims::paths_get_addon_directory(core::ptr::null()).is_null());
        assert!(!shims::paths_get_common_directory().is_null());
        assert_eq!(
            shims::min_hook_create(
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
            .0,
            77
        );
        assert_eq!(shims::min_hook_remove(core::ptr::null_mut()).0, 77);
        assert_eq!(shims::min_hook_enable(core::ptr::null_mut()).0, 77);
        assert_eq!(shims::min_hook_disable(core::ptr::null_mut()).0, 77);
        shims::events_raise(core::ptr::null(), core::ptr::null_mut());
        shims::events_raise_notification(core::ptr::null());
        shims::events_raise_targeted(1, core::ptr::null(), core::ptr::null_mut());
        shims::events_raise_notification_targeted(1, core::ptr::null());
        shims::events_subscribe(core::ptr::null(), None);
        shims::events_unsubscribe(core::ptr::null(), None);
        shims::wnd_proc_register(None);
        shims::wnd_proc_deregister(None);
        assert_eq!(
            shims::wnd_proc_send_to_game_only(core::ptr::null_mut(), 0, 0, 0),
            77
        );
        shims::input_binds_invoke(core::ptr::null(), 0);
        shims::input_binds_register_with_string(core::ptr::null(), None, core::ptr::null());
        shims::input_binds_register_with_struct(core::ptr::null(), None, bind);
        shims::input_binds_register_with_string_v1(core::ptr::null(), None, core::ptr::null());
        shims::input_binds_register_with_struct_v1(core::ptr::null(), None, bind);
        shims::input_binds_deregister(core::ptr::null());
        shims::game_binds_press_async(GameBind(1));
        shims::game_binds_release_async(GameBind(1));
        shims::game_binds_invoke_async(GameBind(1), 1);
        shims::game_binds_press(GameBind(1));
        shims::game_binds_release(GameBind(1));
        assert_eq!(shims::game_binds_is_bound(GameBind(1)), 1);
        assert!(!shims::data_link_get(core::ptr::null()).is_null());
        assert!(!shims::data_link_share(core::ptr::null(), 1).is_null());
        assert!(!shims::textures_get(core::ptr::null()).is_null());
        assert!(
            !shims::textures_get_or_create_from_file(core::ptr::null(), core::ptr::null())
                .is_null()
        );
        assert!(
            !shims::textures_get_or_create_from_resource(
                core::ptr::null(),
                1,
                core::ptr::null_mut(),
            )
            .is_null()
        );
        assert!(
            !shims::textures_get_or_create_from_url(
                core::ptr::null(),
                core::ptr::null(),
                core::ptr::null(),
            )
            .is_null()
        );
        assert!(
            !shims::textures_get_or_create_from_memory(
                core::ptr::null(),
                core::ptr::null_mut(),
                1,
            )
            .is_null()
        );
        shims::textures_load_from_file(core::ptr::null(), core::ptr::null(), None);
        shims::textures_load_from_resource(core::ptr::null(), 1, core::ptr::null_mut(), None);
        shims::textures_load_from_url(
            core::ptr::null(),
            core::ptr::null(),
            core::ptr::null(),
            None,
        );
        shims::textures_load_from_memory(core::ptr::null(), core::ptr::null_mut(), 1, None);
        shims::quick_access_add(
            core::ptr::null(),
            core::ptr::null(),
            core::ptr::null(),
            core::ptr::null(),
            core::ptr::null(),
        );
        shims::quick_access_remove(core::ptr::null());
        shims::quick_access_notify(core::ptr::null());
        shims::quick_access_add_simple(core::ptr::null(), None);
        shims::quick_access_add_context_menu(core::ptr::null(), core::ptr::null(), None);
        shims::quick_access_remove_context_menu(core::ptr::null());
        assert!(!shims::localization_translate(core::ptr::null()).is_null());
        assert!(!shims::localization_translate_to(core::ptr::null(), core::ptr::null()).is_null());
        shims::localization_set_translated_string(
            core::ptr::null(),
            core::ptr::null(),
            core::ptr::null(),
        );
        shims::fonts_get(core::ptr::null(), None);
        shims::fonts_release(core::ptr::null(), None);
        shims::fonts_add_from_file(
            core::ptr::null(),
            1.0,
            core::ptr::null(),
            None,
            core::ptr::null_mut(),
        );
        shims::fonts_add_from_resource(
            core::ptr::null(),
            1.0,
            1,
            core::ptr::null_mut(),
            None,
            core::ptr::null_mut(),
        );
        shims::fonts_add_from_memory(
            core::ptr::null(),
            1.0,
            core::ptr::null_mut(),
            1,
            None,
            core::ptr::null_mut(),
        );
        shims::fonts_resize(core::ptr::null(), 1.0);
    }

    assert_eq!(
        backend.calls(),
        vec![
            "renderer_register",
            "renderer_deregister",
            "request_update",
            "log",
            "log_v1",
            "ui_send_alert",
            "ui_register_close_on_escape",
            "ui_deregister_close_on_escape",
            "paths_get_game_directory",
            "paths_get_addon_directory",
            "paths_get_common_directory",
            "min_hook_create",
            "min_hook_remove",
            "min_hook_enable",
            "min_hook_disable",
            "events_raise",
            "events_raise_notification",
            "events_raise_targeted",
            "events_raise_notification_targeted",
            "events_subscribe",
            "events_unsubscribe",
            "wnd_proc_register",
            "wnd_proc_deregister",
            "wnd_proc_send_to_game_only",
            "input_binds_invoke",
            "input_binds_register_with_string",
            "input_binds_register_with_struct",
            "input_binds_register_with_string_v1",
            "input_binds_register_with_struct_v1",
            "input_binds_deregister",
            "game_binds_press_async",
            "game_binds_release_async",
            "game_binds_invoke_async",
            "game_binds_press",
            "game_binds_release",
            "game_binds_is_bound",
            "data_link_get",
            "data_link_share",
            "textures_get",
            "textures_get_or_create_from_file",
            "textures_get_or_create_from_resource",
            "textures_get_or_create_from_url",
            "textures_get_or_create_from_memory",
            "textures_load_from_file",
            "textures_load_from_resource",
            "textures_load_from_url",
            "textures_load_from_memory",
            "quick_access_add",
            "quick_access_remove",
            "quick_access_notify",
            "quick_access_add_simple",
            "quick_access_add_context_menu",
            "quick_access_remove_context_menu",
            "localization_translate",
            "localization_translate_to",
            "localization_set_translated_string",
            "fonts_get",
            "fonts_release",
            "fonts_add_from_file",
            "fonts_add_from_resource",
            "fonts_add_from_memory",
            "fonts_resize",
        ]
    );
}

#[test]
fn missing_retired_and_panicking_services_fail_closed_by_return_category() {
    let _guard = begin_test();

    // SAFETY: these calls use only opaque null arguments and no backend is
    // installed, so the dispatcher returns without native memory access.
    unsafe {
        shims::log(LogLevel(0), core::ptr::null(), core::ptr::null());
        assert!(shims::paths_get_game_directory().is_null());
        assert!(shims::data_link_get(core::ptr::null()).is_null());
        assert!(shims::textures_get(core::ptr::null()).is_null());
        assert_eq!(shims::game_binds_is_bound(GameBind(0)), 0);
        assert_eq!(
            shims::wnd_proc_send_to_game_only(core::ptr::null_mut(), 0, 0, 0),
            0
        );
        assert_eq!(
            shims::min_hook_remove(core::ptr::null_mut()).0,
            MINHOOK_ERROR_NOT_INITIALIZED.0
        );
    }

    let backend = Arc::new(RecordingBackend::default());
    let mut resources = TestRenderResources::new();
    let mut installation = resources.install(3, as_backend(&backend));

    for operation in [
        "log",
        "paths_get_game_directory",
        "data_link_get",
        "textures_get",
        "game_binds_is_bound",
        "wnd_proc_send_to_game_only",
        "min_hook_remove",
        "events_raise",
    ] {
        backend.panic_on(Some(operation));
        // SAFETY: every pointer remains opaque to the recording backend. Its
        // selected panic is caught before unwinding can cross the ABI.
        unsafe {
            match operation {
                "log" => shims::log(LogLevel(0), core::ptr::null(), core::ptr::null()),
                "paths_get_game_directory" => {
                    assert!(shims::paths_get_game_directory().is_null());
                }
                "data_link_get" => assert!(shims::data_link_get(core::ptr::null()).is_null()),
                "textures_get" => assert!(shims::textures_get(core::ptr::null()).is_null()),
                "game_binds_is_bound" => assert_eq!(shims::game_binds_is_bound(GameBind(0)), 0),
                "wnd_proc_send_to_game_only" => assert_eq!(
                    shims::wnd_proc_send_to_game_only(core::ptr::null_mut(), 0, 0, 0),
                    0
                ),
                "min_hook_remove" => assert_eq!(
                    shims::min_hook_remove(core::ptr::null_mut()).0,
                    MINHOOK_ERROR_NOT_INITIALIZED.0
                ),
                "events_raise" => {
                    shims::events_raise(core::ptr::null(), core::ptr::null_mut());
                }
                _ => unreachable!(),
            }
        }
    }
    backend.panic_on(None);

    installation.retire();
    backend.clear_calls();
    // SAFETY: retired dispatch fails before inspecting the opaque pointer.
    unsafe { shims::ui_send_alert(core::ptr::null()) };
    assert!(backend.calls().is_empty());
}

#[test]
fn replacement_is_reentrant_and_stale_drop_cannot_retire_the_new_session() {
    let _guard = begin_test();
    let old_backend = Arc::new(RecordingBackend::default());
    old_backend.reenter_drop.store(true, Ordering::SeqCst);
    let new_backend = Arc::new(RecordingBackend::default());
    let mut old_resources = TestRenderResources::new();
    let old_installation = old_resources.install(10, as_backend(&old_backend));
    let old_token = old_installation.token();
    drop(old_backend);

    let mut new_resources = TestRenderResources::new();
    let new_installation = new_resources.install(10, as_backend(&new_backend));
    assert_eq!(
        old_token.generation(),
        new_installation.token().generation()
    );
    assert_ne!(old_token.token(), new_installation.token().token());
    assert_eq!(new_backend.calls(), vec!["ui_send_alert"]);
    new_backend.clear_calls();

    drop(old_installation);
    // SAFETY: the opaque arguments are not dereferenced by the active backend.
    unsafe { shims::log(LogLevel(0), core::ptr::null(), core::ptr::null()) };
    assert_eq!(new_backend.calls(), vec!["log"]);

    drop(new_installation);
    new_backend.clear_calls();
    // SAFETY: the retired dispatcher ignores every opaque argument.
    unsafe { shims::log(LogLevel(0), core::ptr::null(), core::ptr::null()) };
    assert!(new_backend.calls().is_empty());
}

#[test]
fn backend_calls_can_reenter_the_dispatcher_without_lock_recursion() {
    let _guard = begin_test();
    let backend = Arc::new(RecordingBackend::default());
    backend.reenter_log.store(true, Ordering::SeqCst);
    let mut resources = TestRenderResources::new();
    let _installation = resources.install(1, as_backend(&backend));

    // SAFETY: the recording backend treats both string pointers as opaque.
    unsafe { shims::log(LogLevel(0), core::ptr::null(), core::ptr::null()) };
    assert_eq!(backend.calls(), vec!["log", "ui_send_alert"]);
}

#[test]
fn backend_destructor_panics_are_contained_during_retirement() {
    let _guard = begin_test();
    let backend = Arc::new(RecordingBackend::default());
    backend.panic_drop.store(true, Ordering::SeqCst);
    let mut resources = TestRenderResources::new();
    let installation = resources.install(9, as_backend(&backend));
    drop(backend);

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(installation)));
    assert!(outcome.is_ok());
    // SAFETY: retirement removed the service before its destructor ran, so
    // dispatch fails closed without inspecting the opaque argument.
    unsafe { shims::ui_send_alert(core::ptr::null()) };
}

macro_rules! address {
    ($function:path) => {
        $function as *const () as usize
    };
}

fn assert_revision_slots(table: ApiTableRef<'_>, expected: &[usize]) {
    let word_count = table.layout().size() / size_of::<usize>();
    assert_eq!(word_count, expected.len());
    // SAFETY: `ApiTableRef` exposes one word-aligned, pinned allocation whose
    // exact layout size was validated by `ApiTableCatalog`; every native ABI
    // slot in these x64 tables is one pointer-sized word.
    let actual = unsafe {
        std::slice::from_raw_parts(table.as_opaque_ptr().cast::<usize>().as_ptr(), word_count)
    };
    assert_eq!(actual, expected);
}

#[test]
fn pinned_catalog_maps_every_exact_v1_through_v6_slot() {
    let _guard = begin_test();
    let backend = Arc::new(RecordingBackend::default());
    let mut resources = TestRenderResources::new();
    let (swap_chain, imgui_context) = resources.pointers();
    let installation = resources.install(42, as_backend(&backend));
    let catalog = installation.catalog();
    assert!(catalog.is_populated());
    assert_eq!(installation.token().generation(), 42);

    let raw = [
        swap_chain.as_ptr() as usize,
        imgui_context.as_ptr() as usize,
        nexus_imgui_compat::sys::igMemAlloc as *const () as usize,
        nexus_imgui_compat::sys::igMemFree as *const () as usize,
    ];

    let v1: [usize; 32] = [
        raw[0],
        raw[1],
        raw[2],
        raw[3],
        address!(shims::renderer_register),
        address!(shims::renderer_deregister),
        address!(shims::paths_get_game_directory),
        address!(shims::paths_get_addon_directory),
        address!(shims::paths_get_common_directory),
        address!(shims::min_hook_create),
        address!(shims::min_hook_remove),
        address!(shims::min_hook_enable),
        address!(shims::min_hook_disable),
        address!(shims::log_v1),
        address!(shims::events_raise),
        address!(shims::events_subscribe),
        address!(shims::events_unsubscribe),
        address!(shims::wnd_proc_register),
        address!(shims::wnd_proc_deregister),
        address!(shims::input_binds_register_with_string_v1),
        address!(shims::input_binds_register_with_struct_v1),
        address!(shims::input_binds_deregister),
        address!(shims::data_link_get),
        address!(shims::data_link_share),
        address!(shims::textures_get),
        address!(shims::textures_load_from_file),
        address!(shims::textures_load_from_resource),
        address!(shims::textures_load_from_url),
        address!(shims::quick_access_add),
        address!(shims::quick_access_remove),
        address!(shims::quick_access_add_simple),
        address!(shims::quick_access_remove_context_menu),
    ];
    let v2: [usize; 42] = [
        raw[0],
        raw[1],
        raw[2],
        raw[3],
        address!(shims::renderer_register),
        address!(shims::renderer_deregister),
        address!(shims::paths_get_game_directory),
        address!(shims::paths_get_addon_directory),
        address!(shims::paths_get_common_directory),
        address!(shims::min_hook_create),
        address!(shims::min_hook_remove),
        address!(shims::min_hook_enable),
        address!(shims::min_hook_disable),
        address!(shims::log),
        address!(shims::events_raise),
        address!(shims::events_raise_notification),
        address!(shims::events_subscribe),
        address!(shims::events_unsubscribe),
        address!(shims::wnd_proc_register),
        address!(shims::wnd_proc_deregister),
        address!(shims::wnd_proc_send_to_game_only),
        address!(shims::input_binds_register_with_string_v1),
        address!(shims::input_binds_register_with_struct_v1),
        address!(shims::input_binds_deregister),
        address!(shims::data_link_get),
        address!(shims::data_link_share),
        address!(shims::textures_get),
        address!(shims::textures_get_or_create_from_file),
        address!(shims::textures_get_or_create_from_resource),
        address!(shims::textures_get_or_create_from_url),
        address!(shims::textures_get_or_create_from_memory),
        address!(shims::textures_load_from_file),
        address!(shims::textures_load_from_resource),
        address!(shims::textures_load_from_url),
        address!(shims::textures_load_from_memory),
        address!(shims::quick_access_add),
        address!(shims::quick_access_remove),
        address!(shims::quick_access_notify),
        address!(shims::quick_access_add_simple),
        address!(shims::quick_access_remove_context_menu),
        address!(shims::localization_translate),
        address!(shims::localization_translate_to),
    ];
    let v3: [usize; 45] = [
        raw[0],
        raw[1],
        raw[2],
        raw[3],
        address!(shims::renderer_register),
        address!(shims::renderer_deregister),
        address!(shims::paths_get_game_directory),
        address!(shims::paths_get_addon_directory),
        address!(shims::paths_get_common_directory),
        address!(shims::min_hook_create),
        address!(shims::min_hook_remove),
        address!(shims::min_hook_enable),
        address!(shims::min_hook_disable),
        address!(shims::log),
        address!(shims::ui_send_alert),
        address!(shims::events_raise),
        address!(shims::events_raise_notification),
        address!(shims::events_raise_targeted),
        address!(shims::events_raise_notification_targeted),
        address!(shims::events_subscribe),
        address!(shims::events_unsubscribe),
        address!(shims::wnd_proc_register),
        address!(shims::wnd_proc_deregister),
        address!(shims::wnd_proc_send_to_game_only),
        address!(shims::input_binds_register_with_string_v1),
        address!(shims::input_binds_register_with_struct_v1),
        address!(shims::input_binds_deregister),
        address!(shims::data_link_get),
        address!(shims::data_link_share),
        address!(shims::textures_get),
        address!(shims::textures_get_or_create_from_file),
        address!(shims::textures_get_or_create_from_resource),
        address!(shims::textures_get_or_create_from_url),
        address!(shims::textures_get_or_create_from_memory),
        address!(shims::textures_load_from_file),
        address!(shims::textures_load_from_resource),
        address!(shims::textures_load_from_url),
        address!(shims::textures_load_from_memory),
        address!(shims::quick_access_add),
        address!(shims::quick_access_remove),
        address!(shims::quick_access_notify),
        address!(shims::quick_access_add_simple),
        address!(shims::quick_access_remove_context_menu),
        address!(shims::localization_translate),
        address!(shims::localization_translate_to),
    ];
    let v4: [usize; 51] = [
        raw[0],
        raw[1],
        raw[2],
        raw[3],
        address!(shims::renderer_register),
        address!(shims::renderer_deregister),
        address!(shims::request_update),
        address!(shims::paths_get_game_directory),
        address!(shims::paths_get_addon_directory),
        address!(shims::paths_get_common_directory),
        address!(shims::min_hook_create),
        address!(shims::min_hook_remove),
        address!(shims::min_hook_enable),
        address!(shims::min_hook_disable),
        address!(shims::log),
        address!(shims::ui_send_alert),
        address!(shims::events_raise),
        address!(shims::events_raise_notification),
        address!(shims::events_raise_targeted),
        address!(shims::events_raise_notification_targeted),
        address!(shims::events_subscribe),
        address!(shims::events_unsubscribe),
        address!(shims::wnd_proc_register),
        address!(shims::wnd_proc_deregister),
        address!(shims::wnd_proc_send_to_game_only),
        address!(shims::input_binds_register_with_string),
        address!(shims::input_binds_register_with_struct),
        address!(shims::input_binds_deregister),
        address!(shims::data_link_get),
        address!(shims::data_link_share),
        address!(shims::textures_get),
        address!(shims::textures_get_or_create_from_file),
        address!(shims::textures_get_or_create_from_resource),
        address!(shims::textures_get_or_create_from_url),
        address!(shims::textures_get_or_create_from_memory),
        address!(shims::textures_load_from_file),
        address!(shims::textures_load_from_resource),
        address!(shims::textures_load_from_url),
        address!(shims::textures_load_from_memory),
        address!(shims::quick_access_add),
        address!(shims::quick_access_remove),
        address!(shims::quick_access_notify),
        address!(shims::quick_access_add_simple),
        address!(shims::quick_access_remove_context_menu),
        address!(shims::localization_translate),
        address!(shims::localization_translate_to),
        address!(shims::fonts_get),
        address!(shims::fonts_release),
        address!(shims::fonts_add_from_file),
        address!(shims::fonts_add_from_resource),
        address!(shims::fonts_add_from_memory),
    ];
    let v5: [usize; 53] = [
        raw[0],
        raw[1],
        raw[2],
        raw[3],
        address!(shims::renderer_register),
        address!(shims::renderer_deregister),
        address!(shims::request_update),
        address!(shims::paths_get_game_directory),
        address!(shims::paths_get_addon_directory),
        address!(shims::paths_get_common_directory),
        address!(shims::min_hook_create),
        address!(shims::min_hook_remove),
        address!(shims::min_hook_enable),
        address!(shims::min_hook_disable),
        address!(shims::log),
        address!(shims::ui_send_alert),
        address!(shims::events_raise),
        address!(shims::events_raise_notification),
        address!(shims::events_raise_targeted),
        address!(shims::events_raise_notification_targeted),
        address!(shims::events_subscribe),
        address!(shims::events_unsubscribe),
        address!(shims::wnd_proc_register),
        address!(shims::wnd_proc_deregister),
        address!(shims::wnd_proc_send_to_game_only),
        address!(shims::input_binds_invoke),
        address!(shims::input_binds_register_with_string),
        address!(shims::input_binds_register_with_struct),
        address!(shims::input_binds_deregister),
        address!(shims::data_link_get),
        address!(shims::data_link_share),
        address!(shims::textures_get),
        address!(shims::textures_get_or_create_from_file),
        address!(shims::textures_get_or_create_from_resource),
        address!(shims::textures_get_or_create_from_url),
        address!(shims::textures_get_or_create_from_memory),
        address!(shims::textures_load_from_file),
        address!(shims::textures_load_from_resource),
        address!(shims::textures_load_from_url),
        address!(shims::textures_load_from_memory),
        address!(shims::quick_access_add),
        address!(shims::quick_access_remove),
        address!(shims::quick_access_notify),
        address!(shims::quick_access_add_simple),
        address!(shims::quick_access_remove_context_menu),
        address!(shims::localization_translate),
        address!(shims::localization_translate_to),
        address!(shims::localization_set_translated_string),
        address!(shims::fonts_get),
        address!(shims::fonts_release),
        address!(shims::fonts_add_from_file),
        address!(shims::fonts_add_from_resource),
        address!(shims::fonts_add_from_memory),
    ];
    let v6: [usize; 62] = [
        raw[0],
        raw[1],
        raw[2],
        raw[3],
        address!(shims::renderer_register),
        address!(shims::renderer_deregister),
        address!(shims::request_update),
        address!(shims::log),
        address!(shims::ui_send_alert),
        address!(shims::ui_register_close_on_escape),
        address!(shims::ui_deregister_close_on_escape),
        address!(shims::paths_get_game_directory),
        address!(shims::paths_get_addon_directory),
        address!(shims::paths_get_common_directory),
        address!(shims::min_hook_create),
        address!(shims::min_hook_remove),
        address!(shims::min_hook_enable),
        address!(shims::min_hook_disable),
        address!(shims::events_raise),
        address!(shims::events_raise_notification),
        address!(shims::events_raise_targeted),
        address!(shims::events_raise_notification_targeted),
        address!(shims::events_subscribe),
        address!(shims::events_unsubscribe),
        address!(shims::wnd_proc_register),
        address!(shims::wnd_proc_deregister),
        address!(shims::wnd_proc_send_to_game_only),
        address!(shims::input_binds_invoke),
        address!(shims::input_binds_register_with_string),
        address!(shims::input_binds_register_with_struct),
        address!(shims::input_binds_deregister),
        address!(shims::game_binds_press_async),
        address!(shims::game_binds_release_async),
        address!(shims::game_binds_invoke_async),
        address!(shims::game_binds_press),
        address!(shims::game_binds_release),
        address!(shims::game_binds_is_bound),
        address!(shims::data_link_get),
        address!(shims::data_link_share),
        address!(shims::textures_get),
        address!(shims::textures_get_or_create_from_file),
        address!(shims::textures_get_or_create_from_resource),
        address!(shims::textures_get_or_create_from_url),
        address!(shims::textures_get_or_create_from_memory),
        address!(shims::textures_load_from_file),
        address!(shims::textures_load_from_resource),
        address!(shims::textures_load_from_url),
        address!(shims::textures_load_from_memory),
        address!(shims::quick_access_add),
        address!(shims::quick_access_remove),
        address!(shims::quick_access_notify),
        address!(shims::quick_access_add_context_menu),
        address!(shims::quick_access_remove_context_menu),
        address!(shims::localization_translate),
        address!(shims::localization_translate_to),
        address!(shims::localization_set_translated_string),
        address!(shims::fonts_get),
        address!(shims::fonts_release),
        address!(shims::fonts_add_from_file),
        address!(shims::fonts_add_from_resource),
        address!(shims::fonts_add_from_memory),
        address!(shims::fonts_resize),
    ];

    assert_revision_slots(catalog.get(ApiRevision::V1), &v1);
    assert_revision_slots(catalog.get(ApiRevision::V2), &v2);
    assert_revision_slots(catalog.get(ApiRevision::V3), &v3);
    assert_revision_slots(catalog.get(ApiRevision::V4), &v4);
    assert_revision_slots(catalog.get(ApiRevision::V5), &v5);
    assert_revision_slots(catalog.get(ApiRevision::V6), &v6);

    for revision in ApiRevision::ALL {
        let first = catalog.get(revision).as_opaque_ptr();
        let second = installation.catalog().get(revision).as_opaque_ptr();
        assert_eq!(first, second);
    }
}
