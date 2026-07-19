//! Fail-closed discovery of the process's visible ownerless top-level window.

/// Opaque native window handle selected by platform discovery.
///
/// The client area is a discovery-time observation used by runtime selection;
/// equality and hashing intentionally remain native-handle identity only.
#[derive(Debug, Clone, Copy)]
pub struct NativeWindowHandle {
    value: usize,
    client_area: u64,
}

impl PartialEq for NativeWindowHandle {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl Eq for NativeWindowHandle {}

impl core::hash::Hash for NativeWindowHandle {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        core::hash::Hash::hash(&self.value, state);
    }
}

impl NativeWindowHandle {
    /// Returns the non-null native handle value.
    #[must_use]
    pub const fn get(self) -> usize {
        self.value
    }

    /// Returns the measured client area from this observation.
    #[must_use]
    pub const fn client_area(self) -> u64 {
        self.client_area
    }

    /// Re-inspects a native handle as this process's visible ownerless window.
    ///
    /// Invalid, hidden, foreign, owned, or unmeasurable handles fail closed.
    #[must_use]
    pub fn inspect_current_process_top_level(value: usize) -> Option<Self> {
        inspect_current_process_top_level_impl(value, None)
    }

    /// Re-inspects a native handle and requires an exact Win32 window class.
    ///
    /// The class filter is compared as UTF-16 without case folding. Empty,
    /// truncated, inaccessible, or non-matching class names fail closed.
    #[must_use]
    pub fn inspect_current_process_top_level_by_class(
        value: usize,
        expected_class: &str,
    ) -> Option<Self> {
        inspect_current_process_top_level_impl(value, Some(expected_class))
    }

    #[cfg(any(target_os = "windows", test))]
    const fn from_candidate(candidate: WindowCandidate) -> Self {
        Self {
            value: candidate.handle,
            client_area: candidate.client_area,
        }
    }
}

#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WindowCandidate {
    handle: usize,
    process_id: u32,
    owner: usize,
    visible: bool,
    root: usize,
    child: bool,
    class_matches: bool,
    client_area: u64,
}

#[cfg(any(target_os = "windows", test))]
impl WindowCandidate {
    const fn is_game_window(self, current_process_id: u32) -> bool {
        self.handle != 0
            && self.process_id == current_process_id
            && self.owner == 0
            && self.visible
            && self.root == self.handle
            && !self.child
            && self.class_matches
    }

    const fn is_preferred_to(self, current: Self) -> bool {
        if self.client_area != current.client_area {
            return self.client_area > current.client_area;
        }
        self.handle < current.handle
    }
}

#[cfg(any(target_os = "windows", test))]
const fn select_preferred_candidate(
    current_process_id: u32,
    selected: Option<WindowCandidate>,
    candidate: WindowCandidate,
) -> Option<WindowCandidate> {
    if !candidate.is_game_window(current_process_id) {
        return selected;
    }
    match selected {
        Some(current) if !candidate.is_preferred_to(current) => Some(current),
        _ => Some(candidate),
    }
}

#[cfg(test)]
fn select_game_window(
    current_process_id: u32,
    candidates: &[WindowCandidate],
) -> Option<NativeWindowHandle> {
    candidates
        .iter()
        .copied()
        .fold(None, |selected, candidate| {
            select_preferred_candidate(current_process_id, selected, candidate)
        })
        .map(NativeWindowHandle::from_candidate)
}

/// Finds the most likely visible ownerless top-level game window for this process.
///
/// The largest client area wins regardless of foreground state. The lower raw
/// handle is the deterministic tie break, so enumeration order and focus
/// changes cannot churn the selected identity. Any enumeration failure or
/// absence of an eligible measurable window returns `None`; callers may retry
/// because a process can create or replace its game window after graphics
/// interception starts.
#[must_use]
pub fn discover_current_process_top_level_window() -> Option<NativeWindowHandle> {
    discover_current_process_top_level_window_impl(None)
}

/// Discovers the preferred visible ownerless window with an exact Win32 class.
#[must_use]
pub fn discover_current_process_top_level_window_by_class(
    expected_class: &str,
) -> Option<NativeWindowHandle> {
    discover_current_process_top_level_window_impl(Some(expected_class))
}

#[cfg(target_os = "windows")]
fn inspect_current_process_top_level_impl(
    value: usize,
    expected_class: Option<&str>,
) -> Option<NativeWindowHandle> {
    inspect_window_candidate(value, std::process::id(), expected_class)
        .map(NativeWindowHandle::from_candidate)
}

#[cfg(not(target_os = "windows"))]
const fn inspect_current_process_top_level_impl(
    _value: usize,
    _expected_class: Option<&str>,
) -> Option<NativeWindowHandle> {
    None
}

#[cfg(target_os = "windows")]
fn inspect_window_candidate(
    handle: usize,
    current_process_id: u32,
    expected_class: Option<&str>,
) -> Option<WindowCandidate> {
    use core::mem::size_of;
    use windows_sys::Win32::{
        Foundation::{HWND, RECT},
        UI::WindowsAndMessaging::{
            GA_ROOT, GW_OWNER, GetAncestor, GetClassNameW, GetClientRect, GetWindow, GetWindowInfo,
            GetWindowThreadProcessId, IsWindowVisible, WINDOWINFO, WS_CHILD,
        },
    };

    if handle == 0 {
        return None;
    }
    let hwnd = handle as HWND;
    let mut process_id = 0;
    // SAFETY: the raw value is used only for fail-closed metadata queries, and
    // `process_id` is a live writable output for the duration of this call.
    let _ = unsafe { GetWindowThreadProcessId(hwnd, &raw mut process_id) };
    // SAFETY: these calls only query metadata for the supplied identity and
    // retain no pointer or borrowed state after returning.
    let owner = unsafe { GetWindow(hwnd, GW_OWNER) } as usize;
    // SAFETY: this call only walks the supplied identity's parent chain and
    // retains no pointer or borrowed state after returning. A null result is
    // rejected because it cannot equal the non-null candidate identity.
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) } as usize;
    // SAFETY: this call only queries the handle's current visibility bit.
    let visible = unsafe { IsWindowVisible(hwnd) } != 0;
    let mut window_info = WINDOWINFO {
        cbSize: u32::try_from(size_of::<WINDOWINFO>()).ok()?,
        ..WINDOWINFO::default()
    };
    // SAFETY: `window_info` is a correctly sized live writable output. A
    // stale or invalid native identity makes the query fail and is rejected.
    if unsafe { GetWindowInfo(hwnd, &raw mut window_info) } == 0 {
        return None;
    }
    let class_matches = expected_class.is_none_or(|expected_class| {
        // Win32 window-class names can contain 256 UTF-16 code units. Keep one
        // extra slot so a full-length name is never accepted after truncation.
        let mut class_name = [0_u16; 257];
        // SAFETY: `class_name` is a live writable UTF-16 buffer and `hwnd` is
        // inspected only as an opaque identity. Failure returns zero.
        let length = unsafe {
            GetClassNameW(
                hwnd,
                class_name.as_mut_ptr(),
                i32::try_from(class_name.len()).unwrap_or(i32::MAX),
            )
        };
        length > 0
            && expected_class
                .encode_utf16()
                .eq(class_name[..length as usize].iter().copied())
    });
    let mut candidate = WindowCandidate {
        handle,
        process_id,
        owner,
        visible,
        root,
        child: window_info.dwStyle & WS_CHILD != 0,
        class_matches,
        client_area: 0,
    };
    if !candidate.is_game_window(current_process_id) {
        return None;
    }

    let mut client_rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: `client_rect` is a live writable output, and a stale or invalid
    // native identity simply makes this metadata query fail.
    if unsafe { GetClientRect(hwnd, &raw mut client_rect) } == 0 {
        return None;
    }
    let width = client_rect.right.saturating_sub(client_rect.left).max(0) as u32;
    let height = client_rect.bottom.saturating_sub(client_rect.top).max(0) as u32;
    candidate.client_area = u64::from(width) * u64::from(height);
    Some(candidate)
}

#[cfg(target_os = "windows")]
fn discover_current_process_top_level_window_impl(
    expected_class: Option<&str>,
) -> Option<NativeWindowHandle> {
    use windows_sys::{
        Win32::{Foundation::LPARAM, UI::WindowsAndMessaging::EnumWindows},
        core::BOOL,
    };

    struct EnumerationState {
        process_id: u32,
        expected_class: Option<String>,
        selected: Option<WindowCandidate>,
    }

    unsafe extern "system" fn inspect_window(
        hwnd: windows_sys::Win32::Foundation::HWND,
        state: LPARAM,
    ) -> BOOL {
        // SAFETY: `discover_current_process_top_level_window_impl` passes this
        // callback the address of its live stack-local state for the complete
        // synchronous `EnumWindows` call.
        let Some(state) = (unsafe { (state as *mut EnumerationState).as_mut() }) else {
            return 0;
        };

        let Some(candidate) = inspect_window_candidate(
            hwnd as usize,
            state.process_id,
            state.expected_class.as_deref(),
        ) else {
            return 1;
        };
        state.selected = select_preferred_candidate(state.process_id, state.selected, candidate);
        1
    }

    let mut state = EnumerationState {
        process_id: std::process::id(),
        expected_class: expected_class.map(str::to_owned),
        selected: None,
    };
    // SAFETY: the callback ABI matches `WNDENUMPROC`; `EnumWindows` is
    // synchronous, so the stack-local state outlives every callback access.
    let enumerated = unsafe {
        EnumWindows(
            Some(inspect_window),
            (&raw mut state).cast::<()>() as LPARAM,
        )
    };
    if enumerated == 0 {
        return None;
    }
    state.selected.map(NativeWindowHandle::from_candidate)
}

#[cfg(not(target_os = "windows"))]
const fn discover_current_process_top_level_window_impl(
    _expected_class: Option<&str>,
) -> Option<NativeWindowHandle> {
    None
}

#[cfg(test)]
mod tests {
    use super::{NativeWindowHandle, WindowCandidate, select_game_window};

    const PROCESS: u32 = 42;

    const fn candidate(
        handle: usize,
        process_id: u32,
        owner: usize,
        visible: bool,
        client_area: u64,
    ) -> WindowCandidate {
        WindowCandidate {
            handle,
            process_id,
            owner,
            visible,
            root: handle,
            child: false,
            class_matches: true,
            client_area,
        }
    }

    #[test]
    fn selection_skips_null_foreign_owned_and_hidden_windows() {
        let candidates = [
            candidate(0, PROCESS, 0, true, 10_000),
            candidate(1, PROCESS + 1, 0, true, 10_000),
            candidate(2, PROCESS, 99, true, 10_000),
            candidate(3, PROCESS, 0, false, 10_000),
            candidate(4, PROCESS, 0, true, 1),
        ];

        let selected = select_game_window(PROCESS, &candidates)
            .expect("the only eligible process window should be selected");
        assert_eq!(selected.get(), 4);
        assert_eq!(selected.client_area(), 1);
    }

    #[test]
    fn largest_surface_wins_over_a_smaller_tool_window() {
        let candidates = [
            candidate(7, PROCESS, 0, true, 10_000),
            candidate(8, PROCESS, 0, true, 100),
        ];

        assert_eq!(
            select_game_window(PROCESS, &candidates).map(NativeWindowHandle::get),
            Some(7)
        );
    }

    #[test]
    fn selection_rejects_non_root_child_and_wrong_class_windows() {
        let non_root = WindowCandidate {
            root: 99,
            ..candidate(7, PROCESS, 0, true, 10_000)
        };
        let child = WindowCandidate {
            child: true,
            ..candidate(8, PROCESS, 0, true, 10_000)
        };
        let wrong_class = WindowCandidate {
            class_matches: false,
            ..candidate(9, PROCESS, 0, true, 10_000)
        };
        let top_level = candidate(10, PROCESS, 0, true, 1);

        assert_eq!(
            select_game_window(PROCESS, &[non_root, child, wrong_class, top_level])
                .map(NativeWindowHandle::get),
            Some(10)
        );
    }

    #[test]
    fn largest_window_wins_deterministically() {
        let candidates = [
            candidate(7, PROCESS, 0, true, 100),
            candidate(8, PROCESS, 0, true, 10_000),
            candidate(9, PROCESS, 0, true, 1_000),
        ];

        assert_eq!(
            select_game_window(PROCESS, &candidates).map(NativeWindowHandle::get),
            Some(8)
        );
    }

    #[test]
    fn lower_handle_breaks_equal_area_ties_independent_of_enumeration_order() {
        let forward = [
            candidate(7, PROCESS, 0, true, 10_000),
            candidate(8, PROCESS, 0, true, 10_000),
        ];
        let reverse = [forward[1], forward[0]];

        assert_eq!(
            select_game_window(PROCESS, &forward).map(NativeWindowHandle::get),
            Some(7)
        );
        assert_eq!(
            select_game_window(PROCESS, &reverse).map(NativeWindowHandle::get),
            Some(7)
        );
    }

    #[test]
    fn selection_fails_closed_without_an_eligible_window() {
        let candidates = [
            candidate(1, PROCESS + 1, 0, true, 10_000),
            candidate(2, PROCESS, 1, true, 10_000),
            candidate(3, PROCESS, 0, false, 10_000),
        ];

        assert_eq!(select_game_window(PROCESS, &candidates), None);
    }
}
