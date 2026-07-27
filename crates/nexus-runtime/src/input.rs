use std::path::PathBuf;
use std::sync::Arc;

use nexus_input::{
    CallbackLimits, InlineExecutor, InputMessage, ManagedInputBinds, Modifier, MouseButton,
    PersistenceError, RawMessage, RawRoute, RawWndProcRegistry,
};
use nexus_overlay::{WindowMessage, WindowMessageRoute, WindowMessageRouter};
use nexus_platform::{SettingsStore, SubscriptionId};
use nexus_ui_host::{ESCAPE_VIRTUAL_KEY, EscapeCloseOutcome, EscapeKeyEvent, UiHost};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    WM_ACTIVATEAPP, WM_KEYDOWN, WM_KEYUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
    WM_USER, WM_XBUTTONDOWN, WM_XBUTTONUP, XBUTTON1, XBUTTON2,
};

use crate::cursor::MouseResetService;

const VK_SHIFT_VALUE: usize = 0x10;
const VK_CONTROL_VALUE: usize = 0x11;
const VK_MENU_VALUE: usize = 0x12;
const EXTENDED_SCAN_CODE_PREFIX: u16 = 0xE000;
const PASSTHROUGH_OFFSET: u32 = 7_997;
const PASSTHROUGH_FIRST: u32 = WM_USER + PASSTHROUGH_OFFSET;
const PASSTHROUGH_LAST: u32 = PASSTHROUGH_FIRST + WM_USER - 1;
const LOCK_CURSOR_SETTING: &str = "CameraControl_LockCursor";
const RESET_CURSOR_SETTING: &str = "CameraControl_ResetCursor";

pub(crate) struct RuntimeInputServices {
    path: PathBuf,
    managed: Arc<ManagedInputBinds>,
    raw: Arc<RawWndProcRegistry>,
    ui_host: Arc<UiHost>,
    settings: Arc<SettingsStore>,
    setting_subscriptions: [SubscriptionId; 2],
    mouse_reset: Arc<MouseResetService>,
}

impl RuntimeInputServices {
    /// Managed input-bind registry handed to the add-on API.
    #[allow(
        dead_code,
        reason = "called by the render-session install, landing next"
    )]
    pub(crate) fn managed_binds(&self) -> Arc<ManagedInputBinds> {
        Arc::clone(&self.managed)
    }

    /// Raw window-message callback registry handed to the add-on API.
    #[allow(
        dead_code,
        reason = "called by the render-session install, landing next"
    )]
    pub(crate) fn raw_wnd_proc(&self) -> Arc<RawWndProcRegistry> {
        Arc::clone(&self.raw)
    }

    pub(crate) fn load(
        path: PathBuf,
        ui_host: Arc<UiHost>,
        settings: Arc<SettingsStore>,
    ) -> (Arc<Self>, Option<PersistenceError>) {
        let managed = Arc::new(ManagedInputBinds::new(
            Arc::new(InlineExecutor),
            CallbackLimits::default(),
        ));
        let load_error = path
            .is_file()
            .then(|| managed.load_json_file(&path))
            .transpose()
            .err();
        let mouse_reset = Arc::new(MouseResetService::default());
        let lock_service = Arc::downgrade(&mouse_reset);
        let lock_subscription =
            settings.subscribe_typed::<bool, _>(LOCK_CURSOR_SETTING, move |enabled| {
                if let Some(service) = lock_service.upgrade() {
                    service.set_lock_cursor(enabled);
                }
            });
        let reset_service = Arc::downgrade(&mouse_reset);
        let reset_subscription =
            settings.subscribe_typed::<bool, _>(RESET_CURSOR_SETTING, move |enabled| {
                if let Some(service) = reset_service.upgrade() {
                    service.set_reset_cursor(enabled);
                }
            });
        let services = Arc::new(Self {
            path,
            managed,
            raw: Arc::new(RawWndProcRegistry::new(CallbackLimits::default())),
            ui_host,
            settings,
            setting_subscriptions: [lock_subscription, reset_subscription],
            mouse_reset,
        });
        (services, load_error)
    }

    pub(crate) fn has_handler(&self, identifier: &str) -> bool {
        self.managed.has_handler(identifier)
    }

    fn close_on_escape(&self, message: WindowMessage) -> bool {
        let Ok(virtual_key) = u32::try_from(message.wparam) else {
            return false;
        };
        let event = EscapeKeyEvent {
            is_key_down: message.message == WM_KEYDOWN,
            virtual_key,
            was_down: bit_is_set(message.lparam, 30),
        };
        if !event.is_key_down || event.virtual_key != ESCAPE_VIRTUAL_KEY || event.was_down {
            return false;
        }

        let windows = self.ui_host.escape_closing().registered_windows();
        let mut window_stack = Vec::with_capacity(windows.len().saturating_add(1));
        window_stack.push("");
        window_stack.extend(windows.iter().map(|window| window.as_ref()));
        matches!(
            self.ui_host.escape_closing().handle(event, &window_stack),
            EscapeCloseOutcome::Consumed { .. }
        )
    }

    pub(crate) fn shutdown(&self) -> Result<(), PersistenceError> {
        for subscription in self.setting_subscriptions {
            let _ = self.settings.unsubscribe(subscription);
        }
        self.mouse_reset.shutdown();
        self.managed.release_all();
        self.managed.save_json_file(&self.path)
    }
}

impl WindowMessageRouter for RuntimeInputServices {
    fn before_ui(&self, message: WindowMessage) -> WindowMessageRoute {
        let report = self.raw.route(RawMessage {
            window: message.window,
            message: message.message,
            wparam: message.wparam,
            lparam: message.lparam,
        });
        match report.route {
            RawRoute::Continue => WindowMessageRoute::Continue(message),
            RawRoute::Consume => WindowMessageRoute::Consume,
        }
    }

    fn after_ui(&self, message: WindowMessage) -> WindowMessageRoute {
        if self.close_on_escape(message)
            || input_message(message).is_some_and(|input| self.managed.route(input).consumed)
        {
            WindowMessageRoute::Consume
        } else {
            WindowMessageRoute::Continue(message)
        }
    }

    fn redirect_game_only(&self, mut message: WindowMessage) -> WindowMessage {
        if (PASSTHROUGH_FIRST..=PASSTHROUGH_LAST).contains(&message.message) {
            message.message -= PASSTHROUGH_FIRST;
        }
        self.mouse_reset.handle(message);
        message
    }
}

fn input_message(message: WindowMessage) -> Option<InputMessage> {
    match message.message {
        WM_ACTIVATEAPP => Some(InputMessage::ActivateApp),
        WM_KEYDOWN | WM_SYSKEYDOWN if message.wparam <= 0xFF => Some(InputMessage::KeyDown {
            modifier: modifier(message.wparam),
            scan_code: scan_code(message.lparam),
            repeat: bit_is_set(message.lparam, 30),
        }),
        WM_KEYUP | WM_SYSKEYUP if message.wparam <= 0xFF => Some(InputMessage::KeyUp {
            modifier: modifier(message.wparam),
            scan_code: scan_code(message.lparam),
        }),
        WM_MBUTTONDOWN => Some(InputMessage::MouseDown(MouseButton::Middle)),
        WM_MBUTTONUP => Some(InputMessage::MouseUp(MouseButton::Middle)),
        WM_XBUTTONDOWN => x_button(message.wparam).map(InputMessage::MouseDown),
        WM_XBUTTONUP => x_button(message.wparam).map(InputMessage::MouseUp),
        _ => None,
    }
}

const fn modifier(virtual_key: usize) -> Option<Modifier> {
    match virtual_key {
        VK_MENU_VALUE => Some(Modifier::Alt),
        VK_CONTROL_VALUE => Some(Modifier::Control),
        VK_SHIFT_VALUE => Some(Modifier::Shift),
        _ => None,
    }
}

fn scan_code(lparam: isize) -> u16 {
    let bits = lparam as usize;
    let scan_code = ((bits >> 16) & 0xFF) as u16;
    if bit_is_set(lparam, 24) {
        EXTENDED_SCAN_CODE_PREFIX | scan_code
    } else {
        scan_code
    }
}

fn x_button(wparam: usize) -> Option<MouseButton> {
    match ((wparam >> 16) & 0xFFFF) as u16 {
        XBUTTON1 => Some(MouseButton::X1),
        XBUTTON2 => Some(MouseButton::X2),
        _ => None,
    }
}

fn bit_is_set(value: isize, bit: u32) -> bool {
    (value as usize) & (1_usize << bit) != 0
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use nexus_input::{InputBind, InputDevice, OwnerGeneration as InputOwnerGeneration, RawRoute};
    use nexus_overlay::{WindowMessage, WindowMessageRoute, WindowMessageRouter};
    use nexus_platform::SettingsStore;
    use nexus_ui_host::{
        ESCAPE_VIRTUAL_KEY, OwnerGeneration as UiOwnerGeneration, UiHost, VisibilityTarget,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        WM_KEYDOWN, WM_SYSKEYUP, WM_USER, WM_XBUTTONDOWN, XBUTTON2,
    };

    use super::{
        PASSTHROUGH_FIRST, PASSTHROUGH_LAST, RuntimeInputServices, input_message, scan_code,
    };

    fn message(message: u32, wparam: usize, lparam: isize) -> WindowMessage {
        WindowMessage {
            window: 1,
            message,
            wparam,
            lparam,
        }
    }

    fn load_services(
        ui_host: Arc<UiHost>,
    ) -> (
        Arc<RuntimeInputServices>,
        Option<nexus_input::PersistenceError>,
    ) {
        RuntimeInputServices::load(
            std::path::PathBuf::from("missing-test-input-bindings.json"),
            ui_host,
            Arc::new(SettingsStore::empty("missing-test-settings.json")),
        )
    }

    #[test]
    fn win32_keyboard_messages_keep_extended_scan_and_repeat_bits() {
        let lparam = ((0x1D_usize << 16) | (1_usize << 24) | (1_usize << 30)) as isize;
        assert_eq!(scan_code(lparam), 0xE01D);
        assert!(matches!(
            input_message(message(WM_KEYDOWN, 0x11, lparam)),
            Some(nexus_input::InputMessage::KeyDown {
                modifier: Some(nexus_input::Modifier::Control),
                scan_code: 0xE01D,
                repeat: true,
            })
        ));
        assert!(matches!(
            input_message(message(WM_SYSKEYUP, 0x12, 0x38 << 16)),
            Some(nexus_input::InputMessage::KeyUp {
                modifier: Some(nexus_input::Modifier::Alt),
                scan_code: 0x38,
            })
        ));
    }

    #[test]
    fn extended_mouse_button_uses_the_high_word() {
        assert!(matches!(
            input_message(message(WM_XBUTTONDOWN, usize::from(XBUTTON2) << 16, 0)),
            Some(nexus_input::InputMessage::MouseDown(
                nexus_input::MouseButton::X2
            ))
        ));
    }

    #[test]
    fn raw_callbacks_can_consume_before_managed_input() {
        let (services, error) = load_services(Arc::new(UiHost::default()));
        assert!(error.is_none());
        let _token = services
            .raw
            .register(InputOwnerGeneration::new(7, 1), |_| RawRoute::Consume);
        assert_eq!(
            services.before_ui(message(WM_KEYDOWN, 0x41, 0x1E << 16)),
            WindowMessageRoute::Consume
        );
        assert!(Arc::strong_count(&services.managed) >= 1);
    }

    #[test]
    fn wndproc_stages_keep_raw_callbacks_before_managed_bindings() {
        let (services, error) = load_services(Arc::new(UiHost::default()));
        assert!(error.is_none());
        let events = Arc::new(Mutex::new(Vec::new()));
        let raw_events = Arc::clone(&events);
        let _token = services
            .raw
            .register(InputOwnerGeneration::new(9, 1), move |_| {
                raw_events
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push("raw");
                RawRoute::Continue
            });
        let managed_events = Arc::clone(&events);
        assert!(
            services
                .managed
                .register_v2(
                    "wndproc-order",
                    InputOwnerGeneration::new(9, 1),
                    move |_, release| {
                        if !release {
                            managed_events
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .push("managed");
                        }
                        true
                    },
                    InputBind::new(false, false, false, InputDevice::KEYBOARD, 0x1E),
                )
                .is_ok()
        );

        let original = message(WM_KEYDOWN, 0x41, 0x1E << 16);
        let WindowMessageRoute::Continue(after_raw) = services.before_ui(original) else {
            panic!("raw routing should continue to the UI stage");
        };
        assert_eq!(
            events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            ["raw"]
        );
        assert_eq!(services.after_ui(after_raw), WindowMessageRoute::Consume);
        assert_eq!(
            events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            ["raw", "managed"]
        );
    }

    #[test]
    fn game_only_redirect_matches_the_legacy_closed_range() {
        let (services, _) = load_services(Arc::new(UiHost::default()));
        assert_eq!(
            services
                .redirect_game_only(message(PASSTHROUGH_FIRST, 0, 0))
                .message,
            0
        );
        assert_eq!(
            services
                .redirect_game_only(message(PASSTHROUGH_LAST, 0, 0))
                .message,
            WM_USER - 1
        );
        assert_eq!(
            services
                .redirect_game_only(message(PASSTHROUGH_LAST + 1, 0, 0))
                .message,
            PASSTHROUGH_LAST + 1
        );
    }

    #[test]
    fn escape_closes_the_last_registered_visible_window_before_managed_input() {
        let host = Arc::new(UiHost::default());
        let Ok(owner) = host.owner(UiOwnerGeneration::new(8, 1)) else {
            panic!("test owner should be active");
        };
        let first = Arc::new(AtomicBool::new(true));
        let second = Arc::new(AtomicBool::new(true));
        assert!(
            host.escape_closing()
                .register(
                    &owner,
                    "first",
                    VisibilityTarget::managed(Arc::clone(&first))
                )
                .is_ok()
        );
        assert!(
            host.escape_closing()
                .register(
                    &owner,
                    "second",
                    VisibilityTarget::managed(Arc::clone(&second)),
                )
                .is_ok()
        );
        let (services, error) = load_services(Arc::clone(&host));
        assert!(error.is_none());

        let escape = message(WM_KEYDOWN, ESCAPE_VIRTUAL_KEY as usize, 0);
        assert_eq!(services.after_ui(escape), WindowMessageRoute::Consume);
        assert!(first.load(Ordering::Acquire));
        assert!(!second.load(Ordering::Acquire));

        second.store(true, Ordering::Release);
        let repeat = message(
            WM_KEYDOWN,
            ESCAPE_VIRTUAL_KEY as usize,
            (1_usize << 30) as isize,
        );
        assert_eq!(
            services.after_ui(repeat),
            WindowMessageRoute::Continue(repeat)
        );
        assert!(second.load(Ordering::Acquire));
    }
}
