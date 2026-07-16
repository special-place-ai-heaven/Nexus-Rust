use core::mem::size_of;
use core::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use nexus_overlay::WindowMessage;
use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, ClipCursor, GetCursorInfo, SetCursorPos, WM_ACTIVATEAPP,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_RBUTTONDOWN, WM_RBUTTONUP,
};

const MK_LBUTTON_VALUE: usize = 0x0001;
const MK_RBUTTON_VALUE: usize = 0x0002;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CursorPoint {
    x: i32,
    y: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CursorSnapshot {
    visible: bool,
    position: CursorPoint,
}

trait CursorPlatform: Send + Sync {
    fn snapshot(&self) -> Option<CursorSnapshot>;
    fn confine_to(&self, point: CursorPoint) -> bool;
    fn release_confinement(&self) -> bool;
    fn move_to(&self, point: CursorPoint) -> bool;
}

#[derive(Debug, Default)]
struct WindowsCursorPlatform;

impl CursorPlatform for WindowsCursorPlatform {
    fn snapshot(&self) -> Option<CursorSnapshot> {
        // SAFETY: `CURSORINFO` is a plain Win32 output structure. Zero is a
        // valid initialization and `cbSize` is set before the API call.
        let mut info = unsafe { core::mem::zeroed::<CURSORINFO>() };
        info.cbSize = size_of::<CURSORINFO>() as u32;
        // SAFETY: `info` is writable for the duration of the call.
        if unsafe { GetCursorInfo(&raw mut info) } == 0 {
            return None;
        }
        Some(CursorSnapshot {
            visible: info.flags & CURSOR_SHOWING != 0,
            position: CursorPoint {
                x: info.ptScreenPos.x,
                y: info.ptScreenPos.y,
            },
        })
    }

    fn confine_to(&self, point: CursorPoint) -> bool {
        let bounds = RECT {
            left: point.x,
            top: point.y,
            right: point.x.saturating_add(1),
            bottom: point.y.saturating_add(1),
        };
        // SAFETY: `bounds` remains valid for the duration of the call and
        // Win32 copies the rectangle rather than retaining the pointer.
        unsafe { ClipCursor(&raw const bounds) != 0 }
    }

    fn release_confinement(&self) -> bool {
        // SAFETY: a null rectangle is the documented request to release the
        // process-wide cursor clipping rectangle.
        unsafe { ClipCursor(ptr::null()) != 0 }
    }

    fn move_to(&self, point: CursorPoint) -> bool {
        // SAFETY: the coordinates are plain screen-space values.
        unsafe { SetCursorPos(point.x, point.y) != 0 }
    }
}

#[derive(Debug, Default)]
struct CursorState {
    drag_active: bool,
    clip_active: bool,
    reset_pending: bool,
    last_visible: Option<CursorPoint>,
}

pub(crate) struct MouseResetService {
    lock_cursor: AtomicBool,
    reset_cursor: AtomicBool,
    state: Mutex<CursorState>,
    platform: Arc<dyn CursorPlatform>,
}

impl std::fmt::Debug for MouseResetService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MouseResetService")
            .field("lock_cursor", &self.lock_cursor.load(Ordering::Acquire))
            .field("reset_cursor", &self.reset_cursor.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl Default for MouseResetService {
    fn default() -> Self {
        Self::new(Arc::new(WindowsCursorPlatform))
    }
}

impl MouseResetService {
    fn new(platform: Arc<dyn CursorPlatform>) -> Self {
        Self {
            lock_cursor: AtomicBool::new(false),
            reset_cursor: AtomicBool::new(false),
            state: Mutex::new(CursorState::default()),
            platform,
        }
    }

    pub(crate) fn set_lock_cursor(&self, enabled: bool) {
        let was_enabled = self.lock_cursor.swap(enabled, Ordering::AcqRel);
        if was_enabled && !enabled {
            let mut state = lock_unpoison(&self.state);
            self.release_clip(&mut state);
            if !self.reset_cursor.load(Ordering::Acquire) {
                state.drag_active = state.clip_active;
            }
        }
    }

    pub(crate) fn set_reset_cursor(&self, enabled: bool) {
        self.reset_cursor.store(enabled, Ordering::Release);
        let mut state = lock_unpoison(&self.state);
        if enabled && state.drag_active {
            state.reset_pending = true;
        } else if !enabled {
            state.reset_pending = false;
            if !state.clip_active {
                state.drag_active = false;
            }
        }
    }

    pub(crate) fn handle(&self, message: WindowMessage) {
        if message.message == WM_ACTIVATEAPP {
            self.finish_drag(true);
            return;
        }
        if !is_camera_mouse_message(message.message) {
            return;
        }
        let Some(snapshot) = self.platform.snapshot() else {
            return;
        };

        let mut state = lock_unpoison(&self.state);
        if should_record_position(message, snapshot.visible) {
            state.last_visible = Some(snapshot.position);
        }

        if state.drag_active && snapshot.visible {
            self.finish_drag_locked(&mut state, true);
            return;
        }

        let buttons_down = message.wparam & (MK_LBUTTON_VALUE | MK_RBUTTON_VALUE) != 0;
        if !snapshot.visible && buttons_down {
            let lock_enabled = self.lock_cursor.load(Ordering::Acquire);
            let reset_enabled = self.reset_cursor.load(Ordering::Acquire);
            if !state.drag_active && state.last_visible.is_some() && (lock_enabled || reset_enabled)
            {
                state.drag_active = true;
                state.reset_pending = reset_enabled;
            }
            if state.drag_active && lock_enabled && !state.clip_active {
                state.clip_active = state
                    .last_visible
                    .is_some_and(|point| self.platform.confine_to(point));
            }
            if state.drag_active && reset_enabled {
                state.reset_pending = true;
            }
        }
    }

    pub(crate) fn shutdown(&self) {
        let mut state = lock_unpoison(&self.state);
        self.release_clip(&mut state);
        state.drag_active = state.clip_active;
        state.reset_pending = false;
    }

    fn finish_drag(&self, restore_position: bool) {
        let mut state = lock_unpoison(&self.state);
        self.finish_drag_locked(&mut state, restore_position);
    }

    fn finish_drag_locked(&self, state: &mut CursorState, restore_position: bool) {
        self.release_clip(state);
        if restore_position && state.reset_pending {
            state.reset_pending = state
                .last_visible
                .is_some_and(|point| !self.platform.move_to(point));
        }
        if !restore_position {
            state.reset_pending = false;
        }
        state.drag_active = state.clip_active || state.reset_pending;
    }

    fn release_clip(&self, state: &mut CursorState) {
        if state.clip_active && self.platform.release_confinement() {
            state.clip_active = false;
        }
    }
}

impl Drop for MouseResetService {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn is_camera_mouse_message(message: u32) -> bool {
    matches!(
        message,
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONUP | WM_MOUSEMOVE
    )
}

fn should_record_position(message: WindowMessage, cursor_visible: bool) -> bool {
    if !cursor_visible {
        return false;
    }
    match message.message {
        WM_LBUTTONDOWN => message.wparam & MK_RBUTTON_VALUE == 0,
        WM_RBUTTONDOWN => message.wparam & MK_LBUTTON_VALUE == 0,
        _ => false,
    }
}

fn lock_unpoison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Action {
        Confine(CursorPoint),
        Release,
        Move(CursorPoint),
    }

    #[derive(Debug)]
    struct FakePlatform {
        snapshot: Mutex<Option<CursorSnapshot>>,
        actions: Mutex<Vec<Action>>,
        succeed: AtomicBool,
    }

    impl FakePlatform {
        fn new(snapshot: CursorSnapshot) -> Self {
            Self {
                snapshot: Mutex::new(Some(snapshot)),
                actions: Mutex::new(Vec::new()),
                succeed: AtomicBool::new(true),
            }
        }

        fn set_snapshot(&self, snapshot: Option<CursorSnapshot>) {
            *lock_unpoison(&self.snapshot) = snapshot;
        }

        fn actions(&self) -> Vec<Action> {
            lock_unpoison(&self.actions).clone()
        }
    }

    impl CursorPlatform for FakePlatform {
        fn snapshot(&self) -> Option<CursorSnapshot> {
            *lock_unpoison(&self.snapshot)
        }

        fn confine_to(&self, point: CursorPoint) -> bool {
            lock_unpoison(&self.actions).push(Action::Confine(point));
            self.succeed.load(Ordering::Acquire)
        }

        fn release_confinement(&self) -> bool {
            lock_unpoison(&self.actions).push(Action::Release);
            self.succeed.load(Ordering::Acquire)
        }

        fn move_to(&self, point: CursorPoint) -> bool {
            lock_unpoison(&self.actions).push(Action::Move(point));
            self.succeed.load(Ordering::Acquire)
        }
    }

    fn message(message: u32, wparam: usize) -> WindowMessage {
        WindowMessage {
            window: 1,
            message,
            wparam,
            lparam: 0,
        }
    }

    #[test]
    fn hidden_camera_drag_confines_then_visible_cursor_releases_and_resets() {
        let point = CursorPoint { x: 120, y: 240 };
        let platform = Arc::new(FakePlatform::new(CursorSnapshot {
            visible: true,
            position: point,
        }));
        let service = MouseResetService::new(platform.clone());
        service.set_lock_cursor(true);
        service.set_reset_cursor(true);
        service.handle(message(WM_RBUTTONDOWN, MK_RBUTTON_VALUE));

        platform.set_snapshot(Some(CursorSnapshot {
            visible: false,
            position: point,
        }));
        service.handle(message(WM_MOUSEMOVE, MK_RBUTTON_VALUE));
        platform.set_snapshot(Some(CursorSnapshot {
            visible: true,
            position: CursorPoint { x: 10, y: 20 },
        }));
        service.handle(message(WM_RBUTTONUP, 0));

        assert_eq!(
            platform.actions(),
            vec![Action::Confine(point), Action::Release, Action::Move(point)]
        );
    }

    #[test]
    fn disabling_lock_during_drag_releases_without_losing_pending_reset() {
        let point = CursorPoint { x: 20, y: 30 };
        let platform = Arc::new(FakePlatform::new(CursorSnapshot {
            visible: true,
            position: point,
        }));
        let service = MouseResetService::new(platform.clone());
        service.set_lock_cursor(true);
        service.set_reset_cursor(true);
        service.handle(message(WM_LBUTTONDOWN, MK_LBUTTON_VALUE));
        platform.set_snapshot(Some(CursorSnapshot {
            visible: false,
            position: point,
        }));
        service.handle(message(WM_MOUSEMOVE, MK_LBUTTON_VALUE));

        service.set_lock_cursor(false);
        service.handle(message(WM_ACTIVATEAPP, 0));

        assert_eq!(
            platform.actions(),
            vec![Action::Confine(point), Action::Release, Action::Move(point)]
        );
    }

    #[test]
    fn failed_cursor_query_is_fail_open() {
        let platform = Arc::new(FakePlatform::new(CursorSnapshot {
            visible: false,
            position: CursorPoint::default(),
        }));
        platform.set_snapshot(None);
        let service = MouseResetService::new(platform.clone());
        service.set_lock_cursor(true);
        service.handle(message(WM_MOUSEMOVE, MK_RBUTTON_VALUE));
        assert!(platform.actions().is_empty());
    }

    #[test]
    fn hidden_cursor_without_a_known_visible_position_is_never_clipped_to_origin() {
        let platform = Arc::new(FakePlatform::new(CursorSnapshot {
            visible: false,
            position: CursorPoint::default(),
        }));
        let service = MouseResetService::new(platform.clone());
        service.set_lock_cursor(true);
        service.set_reset_cursor(true);
        service.handle(message(WM_MOUSEMOVE, MK_RBUTTON_VALUE));
        assert!(platform.actions().is_empty());
    }

    #[test]
    fn shutdown_releases_clip_without_jumping_cursor() {
        let point = CursorPoint { x: 8, y: 9 };
        let platform = Arc::new(FakePlatform::new(CursorSnapshot {
            visible: true,
            position: point,
        }));
        let service = MouseResetService::new(platform.clone());
        service.set_lock_cursor(true);
        service.set_reset_cursor(true);
        service.handle(message(WM_LBUTTONDOWN, MK_LBUTTON_VALUE));
        platform.set_snapshot(Some(CursorSnapshot {
            visible: false,
            position: point,
        }));
        service.handle(message(WM_MOUSEMOVE, MK_LBUTTON_VALUE));

        service.shutdown();

        assert_eq!(
            platform.actions(),
            vec![Action::Confine(point), Action::Release]
        );
    }
}
