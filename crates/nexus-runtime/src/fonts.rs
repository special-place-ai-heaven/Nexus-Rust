use core::marker::PhantomData;
use core::ptr::NonNull;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, CString};
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use nexus_addon_backend::{
    BackendOperationError, RenderFontService, RequiredServiceResult, SendFontCallback,
};
use nexus_control::{FailureCode, InternalFailure, RenderOperation};
use nexus_dxgi::RenderCallbackError;
use nexus_imgui_compat::sys;
use nexus_overlay::{RenderSessionAttachment, RenderSessionObserver, RenderSessionResources};
use nexus_platform::SettingsStore;
use nexus_render::{RenderStage, SwapChainId};
use nexus_ui_fonts::{
    FontCatalogError, FontCatalogHandles, FontRebuildRequest, ImGuiFontManager,
    MAX_USER_FONT_BYTES, SelectedFontHandles, UserFont, UserFontError, new_imgui_font_manager,
};
use nexus_ui_services::{
    FileFontAssetLoader, FontAssetError, FontAssetLoader, FontCallback, FontConfig, FontGetResult,
    FontRegistration, OwnerId, ResourceFont, SubscriptionId, UiScale,
};
use thiserror::Error;

const FONT_SIZE_SETTING: &str = "FontSize";
const USER_FONT_SETTING: &str = "UserFont";
const MAX_USER_FONT_NAME_UNITS: usize = 255;

static NEXT_COORDINATOR_ID: AtomicU64 = AtomicU64::new(1);

#[cfg(test)]
pub(crate) static IMGUI_TEST_LOCK: Mutex<()> = Mutex::new(());

std::thread_local! {
    static FONT_SESSIONS: RefCell<HashMap<u64, FontSession>> = RefCell::new(HashMap::new());
}

/// Stable, pointer-free identity for one selected font render session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FontSessionIdentity {
    pub(crate) swap_chain_id: SwapChainId,
    pub(crate) generation: u64,
}

/// The three distinct host font addresses published through NexusLink.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct FontAddresses {
    pub(crate) font: usize,
    pub(crate) font_big: usize,
    pub(crate) font_ui: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FontAddressCatalog {
    default: usize,
    small: FontAddresses,
    normal: FontAddresses,
    large: FontAddresses,
    larger: FontAddresses,
}

impl FontAddressCatalog {
    fn from_handles(handles: FontCatalogHandles) -> Self {
        Self {
            default: handles.default.as_ptr() as usize,
            small: addresses_from_handles(handles.selected(UiScale::SMALL)),
            normal: addresses_from_handles(handles.selected(UiScale::NORMAL)),
            large: addresses_from_handles(handles.selected(UiScale::LARGE)),
            larger: addresses_from_handles(handles.selected(UiScale::LARGER)),
        }
    }

    const fn selected(self, ui_scale: UiScale) -> FontAddresses {
        match ui_scale.0 {
            0 => self.small,
            2 => self.large,
            3 => self.larger,
            _ => self.normal,
        }
    }
}

fn addresses_from_handles(handles: SelectedFontHandles) -> FontAddresses {
    FontAddresses {
        font: handles.font.as_ptr() as usize,
        font_big: handles.font_big.as_ptr() as usize,
        font_ui: handles.font_ui.as_ptr() as usize,
    }
}

#[derive(Clone, Copy, Debug)]
struct ActiveFontSession {
    attachment_id: u64,
    identity: FontSessionIdentity,
    render_thread: ThreadId,
}

#[derive(Default)]
struct FontCoordinatorState {
    next_attachment_id: u64,
    active: Option<ActiveFontSession>,
    stopped: bool,
    /// Commands accepted off the render thread, in strict FIFO order.
    queue: VecDeque<PendingFontCommand>,
    /// Bytes retained by queued commands.
    ///
    /// A count limit alone is not a memory limit: a few hundred commands each carrying a
    /// font file would retain gigabytes, and `FontManager::register_memory` copies the
    /// bytes it is given.
    queued_bytes: usize,
}

/// Bounds on work accepted off the render thread.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FontQueueLimits {
    /// Maximum number of queued commands.
    pub(crate) commands: usize,
    /// Maximum total bytes retained by queued commands.
    pub(crate) bytes: usize,
}

impl Default for FontQueueLimits {
    fn default() -> Self {
        Self {
            commands: 256,
            bytes: 32 * 1024 * 1024,
        }
    }
}

/// One owned unit of font work accepted off the render thread.
///
/// The closure carries its own arguments and writes its typed result into the slot the
/// caller is waiting on, so the queue itself stays untyped while every command remains
/// fully owned — it retains no path, borrowed buffer, module handle or resource pointer.
struct PendingFontCommand {
    /// Attachment that accepted the command. Work accepted under an older render
    /// selection must never execute in a new ImGui context.
    attachment_id: u64,
    retained_bytes: usize,
    ticket: Arc<FontTicket>,
    run: Box<dyn FnOnce(&mut ImGuiFontManager) + Send>,
}

/// Terminal state of one queued command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FontTicketState {
    /// Waiting for the render thread.
    Queued,
    /// Rejected without executing; the reason is in the caller's slot.
    Canceled,
    /// Executed to completion.
    Completed,
}

/// One-shot completion signal for a queued command.
struct FontTicket {
    state: Mutex<FontTicketState>,
    settled: Condvar,
}

impl FontTicket {
    fn new() -> Self {
        Self {
            state: Mutex::new(FontTicketState::Queued),
            settled: Condvar::new(),
        }
    }

    fn settle(&self, state: FontTicketState) {
        *mutex_lock(&self.state) = state;
        self.settled.notify_all();
    }

    /// Blocks until the render thread settles this command.
    fn wait(&self, timeout: Duration) -> FontTicketState {
        let deadline = Instant::now() + timeout;
        let mut state = mutex_lock(&self.state);
        while *state == FontTicketState::Queued {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self.settled.wait_timeout(state, remaining) {
                Ok((next, _timeout)) => state = next,
                Err(poisoned) => {
                    let (next, _timeout) = poisoned.into_inner();
                    state = next;
                }
            }
        }
        *state
    }
}

struct FontSession {
    attachment_id: u64,
    context: NonNull<sys::ImGuiContext>,
    manager: ImGuiFontManager,
    pending_request: Option<FontRebuildRequest>,
    addresses: Option<FontAddressCatalog>,
    pending_gpu_rebuild: bool,
    failed: bool,
}

impl FontSession {
    fn new(
        attachment_id: u64,
        context: NonNull<sys::ImGuiContext>,
        request: FontRebuildRequest,
    ) -> Self {
        Self {
            attachment_id,
            context,
            manager: new_imgui_font_manager(),
            pending_request: Some(request),
            addresses: None,
            pending_gpu_rebuild: false,
            failed: false,
        }
    }

    fn advance(&mut self, localization_changed: bool, localized_texts: &[&CStr]) {
        if self.failed {
            return;
        }
        if localization_changed {
            self.manager.reload();
        }

        if let Some(request) = self.pending_request.take() {
            self.apply_request(request, localized_texts);
            return;
        }

        match self.manager.advance(localized_texts) {
            Ok(report) if report.rebuilt => match FontCatalogHandles::resolve(&self.manager) {
                Ok(handles) => self.install(handles, true),
                Err(error) => self.fail(error),
            },
            Ok(_report) => {}
            Err(error) => self.fail(FontCatalogError::Rebuild(error)),
        }
    }

    fn apply_request(&mut self, request: FontRebuildRequest, localized_texts: &[&CStr]) {
        match request.apply_pre_new_frame(&mut self.manager, OwnerId::HOST, localized_texts) {
            Ok(applied) => self.install(applied.handles, applied.advance.rebuilt),
            Err(error) => {
                let can_fallback =
                    request.has_user_font() && !matches!(error, FontCatalogError::Replacement(_));
                self.invalidate_for(error);
                crate::diagnostics::report_proxy_failure(&error);

                if can_fallback {
                    let fallback = FontRebuildRequest::new(Some(request.default_size()), None);
                    match fallback.apply_pre_new_frame(
                        &mut self.manager,
                        OwnerId::HOST,
                        localized_texts,
                    ) {
                        Ok(applied) => {
                            self.failed = false;
                            self.install(applied.handles, applied.advance.rebuilt);
                        }
                        Err(fallback_error) => self.fail(fallback_error),
                    }
                }
            }
        }
    }

    fn install(&mut self, handles: FontCatalogHandles, rebuilt: bool) {
        let addresses = FontAddressCatalog::from_handles(handles);
        self.addresses = Some(addresses);
        self.pending_gpu_rebuild |= rebuilt;
        self.failed = false;
        set_default_font(addresses.default as *mut sys::ImFont);
    }

    fn invalidate_for(&mut self, error: FontCatalogError) {
        self.addresses = None;
        self.failed = true;
        if !matches!(error, FontCatalogError::Replacement(_)) {
            self.pending_gpu_rebuild = true;
        }
        set_default_font(core::ptr::null_mut());
    }

    fn fail(&mut self, error: FontCatalogError) {
        self.invalidate_for(error);
        crate::diagnostics::report_proxy_failure(&error);
    }
}

fn set_default_font(font: *mut sys::ImFont) {
    // SAFETY: callers first verify that this session's exact context is current
    // on its owning render thread. The pointer belongs to that context's atlas
    // and is never retained outside Dear ImGui's IO state.
    if let Some(io) = unsafe { sys::igGetIO().as_mut() } {
        io.FontDefault = font;
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
enum FontAttachError {
    #[error("the font render-session coordinator is stopped")]
    Stopped,
    #[error("the font render-session generation was exhausted")]
    LifecycleExhausted,
    #[error("render-thread font state is temporarily unavailable")]
    ThreadStateUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("a render-thread font operation panicked")]
struct FontSessionPanic;

#[derive(Debug, Error)]
enum UserFontLoadError {
    #[error("the configured user-font name is invalid")]
    InvalidName,
    #[error("the configured user font could not be opened")]
    Open(#[source] io::Error),
    #[error("the configured user-font metadata is unavailable")]
    Metadata(#[source] io::Error),
    #[error("the configured user font could not be read")]
    Read(#[source] io::Error),
    #[error("the configured user-font data is empty")]
    Empty,
    #[error("the configured user-font data exceeds the bounded limit")]
    TooLarge,
    #[error("the configured user-font data is invalid")]
    InvalidData(#[source] UserFontError),
}

/// Closed reasons an inline font operation could not run.
///
/// Every variant is atomic: no partial registration or mutation is observable through any
/// of them, which is what lets the add-on-facing adapter map them straight onto the legacy
/// ABI's closed return values.
///
/// Landed ahead of its consumer: the add-on-facing `RenderFontService` adapter that maps
/// these onto the ABI's closed return values is the next checkpoint.
#[allow(
    dead_code,
    reason = "consumed by the RenderFontService adapter, landing next"
)]
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum RuntimeFontBridgeError {
    /// No font session is attached on this thread.
    #[error("no font render session is active on this thread")]
    NoActiveSession,
    /// The calling thread holds no current ImGui context.
    #[error("no current ImGui context")]
    NoImGuiContext,
    /// The attachment or context changed after the call was accepted.
    #[error("the font attachment was superseded")]
    StaleAttachment,
    /// A font callback re-entered while the manager was mutably borrowed.
    #[error("the font manager is already borrowed by this thread")]
    Reentrant,
    /// The attachment was marked failed, so no further work may run against it.
    #[error("the font attachment was marked failed")]
    AttachmentFailed,
    /// The bounded queue is at its command or byte limit.
    #[error("the font command queue is full")]
    QueueFull,
    /// The render thread did not drain the command before the deadline.
    #[error("the font command timed out before the render thread drained it")]
    Timeout,
}

/// Process-owned facade whose native font manager remains render-thread local.
pub(crate) struct RuntimeFontCoordinator {
    coordinator_id: u64,
    request: FontRebuildRequest,
    state: Mutex<FontCoordinatorState>,
}

impl RuntimeFontCoordinator {
    pub(crate) fn load(settings: &SettingsStore, fonts_directory: &Path) -> Arc<Self> {
        let default_size = match settings.get::<f32>(FONT_SIZE_SETTING) {
            Ok(size) => size,
            Err(error) => {
                crate::diagnostics::report_proxy_failure(&error);
                None
            }
        };
        let configured_user_font = match settings.get::<String>(USER_FONT_SETTING) {
            Ok(name) => name,
            Err(error) => {
                crate::diagnostics::report_proxy_failure(&error);
                None
            }
        };
        let user_font =
            match load_configured_user_font(fonts_directory, configured_user_font.as_deref()) {
                Ok(font) => font,
                Err(error) => {
                    crate::diagnostics::report_proxy_failure(&error);
                    None
                }
            };
        Arc::new(Self::new(FontRebuildRequest::new(default_size, user_font)))
    }

    fn new(request: FontRebuildRequest) -> Self {
        Self {
            coordinator_id: NEXT_COORDINATOR_ID.fetch_add(1, Ordering::Relaxed),
            request,
            state: Mutex::new(FontCoordinatorState::default()),
        }
    }

    fn attach_session(
        self: &Arc<Self>,
        identity: FontSessionIdentity,
        context: NonNull<sys::ImGuiContext>,
    ) -> Result<FontSessionLease, FontAttachError> {
        let render_thread = std::thread::current().id();
        let (attachment_id, previous_session) = {
            let mut state = mutex_lock(&self.state);
            if state.stopped {
                return Err(FontAttachError::Stopped);
            }
            let attachment_id = state
                .next_attachment_id
                .checked_add(1)
                .ok_or(FontAttachError::LifecycleExhausted)?;
            let session = FontSession::new(attachment_id, context, self.request.clone());
            let previous_session = replace_thread_session(self.coordinator_id, session)?;
            state.next_attachment_id = attachment_id;
            state.active = Some(ActiveFontSession {
                attachment_id,
                identity,
                render_thread,
            });
            (attachment_id, previous_session)
        };
        drop_font_session(previous_session);

        Ok(FontSessionLease {
            coordinator: Arc::clone(self),
            attachment_id,
            _thread_bound: PhantomData,
        })
    }

    /// Applies localization-driven atlas work in exact pre-`NewFrame` order.
    pub(crate) fn advance(
        &self,
        context: *mut sys::ImGuiContext,
        stage: RenderStage,
        localization_changed: bool,
        localized_texts: &[CString],
    ) {
        if stage != RenderStage::Addons {
            return;
        }
        let Some(context) = NonNull::new(context) else {
            return;
        };
        if current_imgui_context() != Some(context) {
            return;
        }
        let Some(attachment_id) = self.active_attachment_on_current_thread() else {
            return;
        };
        let localized_texts = localized_texts
            .iter()
            .map(CString::as_c_str)
            .collect::<Vec<_>>();

        let result = contain_font_operation(|| {
            let _ = FONT_SESSIONS.try_with(|sessions| {
                let Ok(mut sessions) = sessions.try_borrow_mut() else {
                    return;
                };
                let Some(session) = sessions.get_mut(&self.coordinator_id) else {
                    return;
                };
                if session.attachment_id != attachment_id || session.context != context {
                    return;
                }
                // Queued work drains first, so global ordering stays FIFO and an add-on's
                // registration is visible to the atlas rebuild in this same frame.
                self.drain_queue(attachment_id, &mut session.manager);
                session.advance(localization_changed, &localized_texts);
            });
        });
        if let Err(error) = result {
            self.mark_failed(attachment_id, context);
            crate::diagnostics::report_proxy_failure(&error);
        }
    }

    /// Consumes one renderer font-texture rebuild request during `prepare` only.
    pub(crate) fn take_gpu_rebuild(
        &self,
        context: *mut sys::ImGuiContext,
        stage: RenderStage,
    ) -> bool {
        if stage != RenderStage::Addons {
            return false;
        }
        let Some(context) = NonNull::new(context) else {
            return false;
        };
        if current_imgui_context() != Some(context) {
            return false;
        }
        let Some(attachment_id) = self.active_attachment_on_current_thread() else {
            return false;
        };

        FONT_SESSIONS
            .try_with(|sessions| {
                let Ok(mut sessions) = sessions.try_borrow_mut() else {
                    return false;
                };
                let Some(session) = sessions.get_mut(&self.coordinator_id) else {
                    return false;
                };
                if session.attachment_id != attachment_id || session.context != context {
                    return false;
                }
                std::mem::take(&mut session.pending_gpu_rebuild)
            })
            .unwrap_or(false)
    }

    /// Selects the exact regular, large, and UI pointers for Mumble UI scale.
    #[must_use]
    pub(crate) fn selected_addresses(&self, ui_scale: UiScale) -> FontAddresses {
        let Some(context) = current_imgui_context() else {
            return FontAddresses::default();
        };
        let Some(attachment_id) = self.active_attachment_on_current_thread() else {
            return FontAddresses::default();
        };

        FONT_SESSIONS
            .try_with(|sessions| {
                let Ok(sessions) = sessions.try_borrow() else {
                    return FontAddresses::default();
                };
                let Some(session) = sessions.get(&self.coordinator_id) else {
                    return FontAddresses::default();
                };
                if session.attachment_id != attachment_id || session.context != context {
                    return FontAddresses::default();
                }
                session
                    .addresses
                    .map_or_else(FontAddresses::default, |catalog| catalog.selected(ui_scale))
            })
            .unwrap_or_default()
    }

    /// Returns the selected pointer-free generation for diagnostics and tests.
    #[must_use]
    pub(crate) fn active_identity(&self) -> Option<FontSessionIdentity> {
        mutex_lock(&self.state).active.map(|active| active.identity)
    }

    /// Stops publication. Thread-local native state is dropped on its owner.
    pub(crate) fn shutdown(&self) {
        let active = {
            let mut state = mutex_lock(&self.state);
            state.stopped = true;
            state.active.take()
        };
        // Release every waiter immediately. Leaving them blocked to their own deadlines
        // would stall shutdown by exactly that timeout for no purpose.
        self.cancel_all_queued();
        if let Some(active) = active
            && active.render_thread == std::thread::current().id()
        {
            drop_font_session(take_thread_session(
                self.coordinator_id,
                active.attachment_id,
            ));
        }
    }

    /// Runs one font-manager operation inline on the render thread.
    ///
    /// This is the seam every add-on-facing font operation routes through. The manager is
    /// **thread-local** to the render thread (`FONT_SESSIONS`), so a call arriving on any
    /// other thread cannot reach it at all and must be queued by the caller instead — the
    /// queue is structural, not an optimisation. See `CONFORMANCE.md` §2.9.
    ///
    /// The active session, the current ImGui context and the attachment are all revalidated
    /// immediately before execution, exactly as [`Self::advance`] does, so work accepted
    /// under an older render selection can never run in a new context.
    ///
    /// A failed `try_borrow_mut` means a re-entrant call arrived from inside a font
    /// callback. That rejects closed rather than waiting or panicking, matching the
    /// discipline `nexus-addon-cleanup` already uses for the two cleanup phases.
    ///
    /// A panic inside `operation` may have followed a partial mutation, which containment
    /// alone cannot undo, so the attachment is marked failed rather than left active with an
    /// ordinary rejection returned.
    #[allow(
        dead_code,
        reason = "consumed by the RenderFontService adapter, landing next"
    )]
    pub(crate) fn with_active_manager<R>(
        &self,
        operation: impl FnOnce(&mut ImGuiFontManager) -> R,
    ) -> Result<R, RuntimeFontBridgeError> {
        let attachment_id = self
            .active_attachment_on_current_thread()
            .ok_or(RuntimeFontBridgeError::NoActiveSession)?;
        let context = current_imgui_context().ok_or(RuntimeFontBridgeError::NoImGuiContext)?;

        let mut outcome = None;
        let contained = contain_font_operation(|| {
            let _ = FONT_SESSIONS.try_with(|sessions| {
                let Ok(mut sessions) = sessions.try_borrow_mut() else {
                    outcome = Some(Err(RuntimeFontBridgeError::Reentrant));
                    return;
                };
                let Some(session) = sessions.get_mut(&self.coordinator_id) else {
                    outcome = Some(Err(RuntimeFontBridgeError::NoActiveSession));
                    return;
                };
                if session.attachment_id != attachment_id || session.context != context {
                    outcome = Some(Err(RuntimeFontBridgeError::StaleAttachment));
                    return;
                }
                if session.failed {
                    outcome = Some(Err(RuntimeFontBridgeError::AttachmentFailed));
                    return;
                }
                outcome = Some(Ok(operation(&mut session.manager)));
            });
        });

        if let Err(error) = contained {
            self.mark_failed(attachment_id, context);
            crate::diagnostics::report_proxy_failure(&error);
            return Err(RuntimeFontBridgeError::AttachmentFailed);
        }
        outcome.unwrap_or(Err(RuntimeFontBridgeError::NoActiveSession))
    }

    /// Accepts one command from a thread that is not the render thread and waits for it.
    ///
    /// The manager is thread-local to the render thread, so an off-thread caller cannot
    /// execute the operation itself; queue-and-wait is the only way such a call can be
    /// synchronous, which the native API requires. See `CONFORMANCE.md` §2.9.
    ///
    /// Rejection is atomic: a command that cannot be accepted never runs, and one that is
    /// canceled after acceptance never partially mutated the manager.
    #[allow(
        dead_code,
        reason = "consumed by the RenderFontService adapter, landing next"
    )]
    pub(crate) fn enqueue_for_render_thread<T: Send + 'static>(
        &self,
        retained_bytes: usize,
        limits: FontQueueLimits,
        timeout: Duration,
        operation: impl FnOnce(&mut ImGuiFontManager) -> T + Send + 'static,
    ) -> Result<T, RuntimeFontBridgeError> {
        let slot = Arc::new(Mutex::new(None::<T>));
        let ticket = Arc::new(FontTicket::new());

        {
            let mut state = mutex_lock(&self.state);
            if state.stopped {
                return Err(RuntimeFontBridgeError::NoActiveSession);
            }
            let attachment_id = state
                .active
                .map(|active| active.attachment_id)
                .ok_or(RuntimeFontBridgeError::NoActiveSession)?;
            if state.queue.len() >= limits.commands
                || state.queued_bytes.saturating_add(retained_bytes) > limits.bytes
            {
                return Err(RuntimeFontBridgeError::QueueFull);
            }
            state.queued_bytes = state.queued_bytes.saturating_add(retained_bytes);

            let command_slot = Arc::clone(&slot);
            let command_ticket = Arc::clone(&ticket);
            state.queue.push_back(PendingFontCommand {
                attachment_id,
                retained_bytes,
                ticket: command_ticket,
                run: Box::new(move |manager| {
                    *mutex_lock(&command_slot) = Some(operation(manager));
                }),
            });
        }

        match ticket.wait(timeout) {
            FontTicketState::Completed => mutex_lock(&slot)
                .take()
                .ok_or(RuntimeFontBridgeError::AttachmentFailed),
            FontTicketState::Canceled => Err(RuntimeFontBridgeError::StaleAttachment),
            // A command still queued at the deadline is canceled so it can never execute
            // later against a caller that has already given up on it.
            FontTicketState::Queued => {
                self.cancel_queued(&ticket);
                Err(RuntimeFontBridgeError::Timeout)
            }
        }
    }

    /// Removes one still-queued command, releasing its retained bytes.
    fn cancel_queued(&self, ticket: &Arc<FontTicket>) {
        let mut state = mutex_lock(&self.state);
        if let Some(index) = state
            .queue
            .iter()
            .position(|command| Arc::ptr_eq(&command.ticket, ticket))
            && let Some(command) = state.queue.remove(index)
        {
            state.queued_bytes = state.queued_bytes.saturating_sub(command.retained_bytes);
            command.ticket.settle(FontTicketState::Canceled);
        }
    }

    /// Executes queued work for `attachment_id` in FIFO order, on the render thread.
    ///
    /// Commands accepted by a superseded attachment are canceled rather than executed, so
    /// work can never run in an ImGui context it was not accepted under. Returns the number
    /// of commands settled, for tests and diagnostics.
    fn drain_queue(&self, attachment_id: u64, manager: &mut ImGuiFontManager) -> usize {
        let mut settled = 0;
        loop {
            let command = {
                let mut state = mutex_lock(&self.state);
                match state.queue.pop_front() {
                    Some(command) => {
                        state.queued_bytes =
                            state.queued_bytes.saturating_sub(command.retained_bytes);
                        command
                    }
                    None => break,
                }
            };
            if command.attachment_id == attachment_id {
                (command.run)(manager);
                command.ticket.settle(FontTicketState::Completed);
            } else {
                command.ticket.settle(FontTicketState::Canceled);
            }
            settled += 1;
        }
        settled
    }

    /// Cancels every queued command, releasing all retained bytes.
    fn cancel_all_queued(&self) {
        let (queued, _bytes) = {
            let mut state = mutex_lock(&self.state);
            state.queued_bytes = 0;
            (state.queue.drain(..).collect::<Vec<_>>(), ())
        };
        for command in queued {
            command.ticket.settle(FontTicketState::Canceled);
        }
    }

    /// Whether this call is already on the render thread with a live attachment.
    ///
    /// Decided before dispatch because a command can only be consumed once, so the inline
    /// and queued paths cannot both be attempted with the same closure.
    fn is_render_thread_attached(&self) -> bool {
        self.active_attachment_on_current_thread().is_some()
    }

    fn active_attachment_on_current_thread(&self) -> Option<u64> {
        let state = mutex_lock(&self.state);
        if state.stopped {
            return None;
        }
        state.active.and_then(|active| {
            (active.render_thread == std::thread::current().id()).then_some(active.attachment_id)
        })
    }

    fn detach(&self, attachment_id: u64) {
        {
            let mut state = mutex_lock(&self.state);
            if state.active.map(|active| active.attachment_id) == Some(attachment_id) {
                state.active = None;
            }
        }
        // Nothing will drain these now, and they were accepted by an attachment that no
        // longer exists, so they must be canceled rather than left to time out.
        self.cancel_all_queued();
        drop_font_session(take_thread_session(self.coordinator_id, attachment_id));
    }

    fn mark_failed(&self, attachment_id: u64, context: NonNull<sys::ImGuiContext>) {
        let _ = contain_font_operation(|| {
            let _ = FONT_SESSIONS.try_with(|sessions| {
                let Ok(mut sessions) = sessions.try_borrow_mut() else {
                    return;
                };
                let Some(session) = sessions.get_mut(&self.coordinator_id) else {
                    return;
                };
                if session.attachment_id == attachment_id && session.context == context {
                    session.failed = true;
                    session.addresses = None;
                    session.pending_gpu_rebuild = true;
                    set_default_font(core::ptr::null_mut());
                }
            });
        });
    }
}

impl Default for RuntimeFontCoordinator {
    fn default() -> Self {
        Self::new(FontRebuildRequest::default())
    }
}

impl fmt::Debug for RuntimeFontCoordinator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeFontCoordinator")
            .field("active_identity", &self.active_identity())
            .finish_non_exhaustive()
    }
}

struct FontSessionLease {
    coordinator: Arc<RuntimeFontCoordinator>,
    attachment_id: u64,
    _thread_bound: PhantomData<Rc<()>>,
}

impl Drop for FontSessionLease {
    fn drop(&mut self) {
        self.coordinator.detach(self.attachment_id);
    }
}

struct RuntimeRenderObserver {
    fonts: Arc<RuntimeFontCoordinator>,
    textures: Arc<dyn RenderSessionObserver>,
}

impl RenderSessionObserver for RuntimeRenderObserver {
    fn attach(
        &self,
        resources: RenderSessionResources<'_>,
    ) -> Result<Box<dyn RenderSessionAttachment>, RenderCallbackError> {
        let font = self
            .fonts
            .attach_session(
                FontSessionIdentity {
                    swap_chain_id: resources.swap_chain_id(),
                    generation: resources.generation(),
                },
                resources.imgui_context(),
            )
            .map_err(map_attach_error)?;
        let texture = self.textures.attach(resources)?;
        Ok(Box::new(CombinedRenderSessionLease {
            font: Some(Box::new(font)),
            texture: Some(texture),
        }))
    }
}

struct CombinedRenderSessionLease {
    font: Option<Box<dyn RenderSessionAttachment>>,
    texture: Option<Box<dyn RenderSessionAttachment>>,
}

impl Drop for CombinedRenderSessionLease {
    fn drop(&mut self) {
        drop(self.font.take());
        drop(self.texture.take());
    }
}

/// Composes context-bound fonts with the existing D3D11 texture observer.
pub(crate) fn production_observer(
    fonts: Arc<RuntimeFontCoordinator>,
    textures: Arc<dyn RenderSessionObserver>,
) -> Arc<dyn RenderSessionObserver> {
    Arc::new(RuntimeRenderObserver { fonts, textures })
}

fn replace_thread_session(
    coordinator_id: u64,
    session: FontSession,
) -> Result<Option<FontSession>, FontAttachError> {
    FONT_SESSIONS
        .try_with(|sessions| {
            let mut sessions = sessions
                .try_borrow_mut()
                .map_err(|_error| FontAttachError::ThreadStateUnavailable)?;
            Ok(sessions.insert(coordinator_id, session))
        })
        .map_err(|_error| FontAttachError::ThreadStateUnavailable)?
}

fn take_thread_session(coordinator_id: u64, attachment_id: u64) -> Option<FontSession> {
    FONT_SESSIONS
        .try_with(|sessions| {
            let Ok(mut sessions) = sessions.try_borrow_mut() else {
                return None;
            };
            let matches = sessions
                .get(&coordinator_id)
                .is_some_and(|session| session.attachment_id == attachment_id);
            matches.then(|| sessions.remove(&coordinator_id)).flatten()
        })
        .ok()
        .flatten()
}

fn drop_font_session(session: Option<FontSession>) {
    let _ = contain_font_operation(|| drop(session));
}

fn current_imgui_context() -> Option<NonNull<sys::ImGuiContext>> {
    // SAFETY: reading the process Dear ImGui current-context slot has no side
    // effects. Callers compare it with their render-thread-owned context.
    NonNull::new(unsafe { sys::igGetCurrentContext() })
}

fn contain_font_operation<T>(operation: impl FnOnce() -> T) -> Result<T, FontSessionPanic> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation)) {
        Ok(value) => Ok(value),
        Err(payload) => {
            // A foreign panic payload may have an adversarial `Drop`
            // implementation. Do not allow destroying it to begin a second
            // unwind across the render-session boundary.
            std::mem::forget(payload);
            Err(FontSessionPanic)
        }
    }
}

const fn map_attach_error(_error: FontAttachError) -> RenderCallbackError {
    RenderCallbackError::new(
        RenderOperation::PrepareTarget,
        FailureCode::Internal(InternalFailure::InvalidState),
    )
}

fn load_configured_user_font(
    fonts_directory: &Path,
    configured_name: Option<&str>,
) -> Result<Option<UserFont>, UserFontLoadError> {
    let Some(configured_name) = configured_name.filter(|name| !name.is_empty()) else {
        return Ok(None);
    };
    let relative = validate_user_font_name(configured_name)?;
    let file = File::open(fonts_directory.join(relative)).map_err(UserFontLoadError::Open)?;
    let length = file.metadata().map_err(UserFontLoadError::Metadata)?.len();
    let capacity = validate_user_font_file_len(length)?;
    let limit = u64::try_from(MAX_USER_FONT_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut bytes = Vec::with_capacity(capacity);
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(UserFontLoadError::Read)?;
    if bytes.len() > MAX_USER_FONT_BYTES {
        return Err(UserFontLoadError::TooLarge);
    }
    UserFont::from_bytes(bytes)
        .map(Some)
        .map_err(UserFontLoadError::InvalidData)
}

fn validate_user_font_name(name: &str) -> Result<&Path, UserFontLoadError> {
    if name.encode_utf16().count() > MAX_USER_FONT_NAME_UNITS {
        return Err(UserFontLoadError::InvalidName);
    }
    let path = Path::new(name);
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(UserFontLoadError::InvalidName);
    }
    let valid_extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("ttf"));
    if !valid_extension {
        return Err(UserFontLoadError::InvalidName);
    }
    Ok(path)
}

fn validate_user_font_file_len(length: u64) -> Result<usize, UserFontLoadError> {
    if length == 0 {
        return Err(UserFontLoadError::Empty);
    }
    if length > u64::try_from(MAX_USER_FONT_BYTES).unwrap_or(u64::MAX) {
        return Err(UserFontLoadError::TooLarge);
    }
    usize::try_from(length).map_err(|_error| UserFontLoadError::TooLarge)
}

fn mutex_lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Add-on-facing font service over the render-thread-local font manager.
///
/// Every operation is synchronous from the native API's perspective: a call already on the
/// render thread executes inline, and a call from any other thread is queued and waited on,
/// because the manager cannot be reached from another thread at all. See `CONFORMANCE.md`
/// §2.9.
///
/// File and resource bytes are copied on the calling thread **before** any command is
/// queued, so a queued command never retains a path, a borrowed buffer, a module handle or
/// a resource pointer.
pub(crate) struct RuntimeFontBridge {
    coordinator: Arc<RuntimeFontCoordinator>,
    limits: FontQueueLimits,
    timeout: Duration,
}

impl RuntimeFontBridge {
    #[allow(
        dead_code,
        reason = "installed with the addon API backend, landing next"
    )]
    pub(crate) fn new(coordinator: Arc<RuntimeFontCoordinator>) -> Self {
        Self {
            coordinator,
            limits: FontQueueLimits::default(),
            timeout: Duration::from_secs(5),
        }
    }

    /// Runs one operation inline or queued, whichever the calling thread allows.
    ///
    /// Both closed error families collapse to `ServiceRejected`: the legacy ABI has no
    /// error channel for these operations, and every variant is already atomic, so no
    /// partial registration is observable through any of them.
    fn dispatch<T: Send + 'static>(
        &self,
        retained_bytes: usize,
        operation: impl FnOnce(&mut ImGuiFontManager) -> T + Send + 'static,
    ) -> RequiredServiceResult<T> {
        let outcome = if self.coordinator.is_render_thread_attached() {
            self.coordinator.with_active_manager(operation)
        } else {
            self.coordinator.enqueue_for_render_thread(
                retained_bytes,
                self.limits,
                self.timeout,
                operation,
            )
        };
        outcome.map_err(|_error| BackendOperationError::ServiceRejected)
    }

    /// Copies asset bytes on this thread, so nothing borrowed reaches a queued command.
    fn load_asset(
        load: impl FnOnce(&mut FileFontAssetLoader) -> Result<Vec<u8>, FontAssetError>,
    ) -> RequiredServiceResult<Vec<u8>> {
        let mut loader = FileFontAssetLoader;
        load(&mut loader).map_err(|_error| BackendOperationError::ServiceRejected)
    }
}

impl RenderFontService for RuntimeFontBridge {
    fn get(
        &self,
        owner: OwnerId,
        identifier: String,
        callback: SendFontCallback,
    ) -> RequiredServiceResult<FontGetResult> {
        self.dispatch(0, move |manager| {
            manager.get(owner, &identifier, callback as FontCallback)
        })?
        .map_err(|_error| BackendOperationError::ServiceRejected)
    }

    fn release(
        &self,
        identifier: String,
        subscription: SubscriptionId,
    ) -> RequiredServiceResult<bool> {
        self.dispatch(0, move |manager| manager.release(&identifier, subscription))
    }

    fn add_from_file(
        &self,
        owner: OwnerId,
        identifier: String,
        size: f32,
        filename: PathBuf,
        callback: Option<SendFontCallback>,
        config: FontConfig,
    ) -> RequiredServiceResult<FontRegistration> {
        let data = Self::load_asset(|loader| loader.load_file(&filename))?;
        self.add_owned_bytes(owner, identifier, size, data, callback, config)
    }

    fn add_from_resource(
        &self,
        owner: OwnerId,
        identifier: String,
        size: f32,
        resource: ResourceFont,
        callback: Option<SendFontCallback>,
        config: FontConfig,
    ) -> RequiredServiceResult<FontRegistration> {
        let data = Self::load_asset(|loader| loader.load_resource(resource))?;
        self.add_owned_bytes(owner, identifier, size, data, callback, config)
    }

    fn add_from_memory(
        &self,
        owner: OwnerId,
        identifier: String,
        size: f32,
        data: Vec<u8>,
        callback: Option<SendFontCallback>,
        config: FontConfig,
    ) -> RequiredServiceResult<FontRegistration> {
        self.add_owned_bytes(owner, identifier, size, data, callback, config)
    }

    fn resize(&self, identifier: String, size: f32) -> RequiredServiceResult<bool> {
        self.dispatch(0, move |manager| manager.resize(&identifier, size))?
            .map_err(|_error| BackendOperationError::ServiceRejected)
    }

    fn cleanup_owner(&self, owner: OwnerId) -> RequiredServiceResult<usize> {
        self.dispatch(0, move |manager| manager.cleanup_owner(owner))
    }

    fn cleanup_owner_callbacks(&self, owner: OwnerId) -> RequiredServiceResult<usize> {
        self.dispatch(0, move |manager| manager.cleanup_owner_callbacks(owner))
    }

    fn cleanup_owner_resources(&self, owner: OwnerId) -> RequiredServiceResult<usize> {
        self.dispatch(0, move |manager| manager.cleanup_owner_resources(owner))
    }
}

impl RuntimeFontBridge {
    /// The single registration path: all three sources become owned bytes first.
    fn add_owned_bytes(
        &self,
        owner: OwnerId,
        identifier: String,
        size: f32,
        data: Vec<u8>,
        callback: Option<SendFontCallback>,
        config: FontConfig,
    ) -> RequiredServiceResult<FontRegistration> {
        // The queue accounts for the bytes this command retains, because the manager
        // copies them again on registration.
        let retained_bytes = data.len();
        self.dispatch(retained_bytes, move |manager| {
            manager.register_memory(
                owner,
                &identifier,
                size,
                &data,
                config,
                callback.map(|callback| callback as FontCallback),
            )
        })?
        .map_err(|_error| BackendOperationError::ServiceRejected)
    }
}

#[cfg(test)]
mod tests {
    use core::ptr::NonNull;
    use std::fs;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::time::Duration;

    use nexus_imgui_compat::sys;
    use nexus_render::{RenderStage, SwapChainId};
    use nexus_ui_fonts::{FontRebuildRequest, MAX_USER_FONT_BYTES, UserFont};
    use nexus_ui_services::{FontConfig, OwnerId, UiScale};

    use super::{
        BackendOperationError, CombinedRenderSessionLease, FONT_SESSIONS, FontAddressCatalog,
        FontAddresses, FontQueueLimits, FontSessionIdentity, PathBuf, RenderFontService,
        RuntimeFontBridge, RuntimeFontBridgeError, RuntimeFontCoordinator, contain_font_operation,
        load_configured_user_font, mutex_lock, validate_user_font_file_len,
        validate_user_font_name,
    };

    fn context_lock() -> MutexGuard<'static, ()> {
        super::IMGUI_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn identity(value: u64) -> FontSessionIdentity {
        FontSessionIdentity {
            swap_chain_id: SwapChainId::new(value),
            generation: value,
        }
    }

    #[test]
    fn initial_catalog_builds_once_and_publishes_distinct_normal_handles() {
        let _guard = context_lock();
        // SAFETY: this test owns one current context on this thread.
        let context = unsafe { sys::igCreateContext(core::ptr::null_mut()) };
        let context = NonNull::new(context).expect("test context");
        let coordinator = Arc::new(RuntimeFontCoordinator::default());
        let lease = coordinator
            .attach_session(identity(1), context)
            .expect("font session");

        coordinator.advance(context.as_ptr(), RenderStage::Addons, false, &[]);
        let addresses = coordinator.selected_addresses(UiScale::NORMAL);
        assert_ne!(addresses.font, 0);
        assert_ne!(addresses.font_big, 0);
        assert_ne!(addresses.font_ui, 0);
        assert_ne!(addresses.font, addresses.font_big);
        assert_ne!(addresses.font, addresses.font_ui);
        assert_ne!(addresses.font_big, addresses.font_ui);
        assert!(coordinator.take_gpu_rebuild(context.as_ptr(), RenderStage::Addons));
        assert!(!coordinator.take_gpu_rebuild(context.as_ptr(), RenderStage::Addons));

        drop(lease);
        // SAFETY: the session lease dropped all TLS state before destruction.
        unsafe { sys::igDestroyContext(context.as_ptr()) };
    }

    /// The seam every add-on-facing font operation routes through: it must reach the
    /// thread-local manager on the render thread and reject closed everywhere else.
    #[test]
    fn the_inline_seam_reaches_the_manager_and_rejects_closed_off_thread() {
        let _guard = context_lock();
        // SAFETY: this test owns one current context on this thread.
        let context = unsafe { sys::igCreateContext(core::ptr::null_mut()) };
        let context = NonNull::new(context).expect("test context");
        let coordinator = Arc::new(RuntimeFontCoordinator::default());

        // Before any attachment there is no session to run against.
        assert_eq!(
            coordinator.with_active_manager(|_manager| ()),
            Err(RuntimeFontBridgeError::NoActiveSession)
        );

        let lease = coordinator
            .attach_session(identity(1), context)
            .expect("font session");
        coordinator.advance(context.as_ptr(), RenderStage::Addons, false, &[]);

        // On the render thread the operation runs against the real manager and its return
        // value comes back synchronously.
        let removed = coordinator
            .with_active_manager(|manager| manager.cleanup_owner_callbacks(OwnerId::new(9, 1)))
            .expect("the render thread must reach the manager");
        assert_eq!(removed, 0);

        // A re-entrant call from inside an operation must reject closed rather than
        // deadlock on itself or panic.
        let reentrant = coordinator
            .with_active_manager(|_manager| coordinator.with_active_manager(|_inner| ()))
            .expect("the outer call still runs");
        assert_eq!(reentrant, Err(RuntimeFontBridgeError::Reentrant));

        // The manager is thread-local, so another thread cannot reach it at all. This is
        // why the off-thread path must queue rather than borrow.
        let foreign = Arc::clone(&coordinator);
        let from_other_thread = std::thread::spawn(move || foreign.with_active_manager(|_m| ()))
            .join()
            .expect("the probe thread must not panic");
        assert_eq!(
            from_other_thread,
            Err(RuntimeFontBridgeError::NoActiveSession)
        );

        drop(lease);
        // A detached attachment must not keep serving work.
        assert_eq!(
            coordinator.with_active_manager(|_manager| ()),
            Err(RuntimeFontBridgeError::NoActiveSession)
        );
        // SAFETY: the session lease dropped all TLS state before destruction.
        unsafe { sys::igDestroyContext(context.as_ptr()) };
    }

    /// An off-thread call must complete synchronously by being drained on the render
    /// thread, and must be bounded and cancelable rather than able to wait forever.
    #[test]
    fn queued_font_work_completes_on_the_render_thread_and_is_bounded() {
        let _guard = context_lock();
        // SAFETY: this test owns one current context on this thread.
        let context = unsafe { sys::igCreateContext(core::ptr::null_mut()) };
        let context = NonNull::new(context).expect("test context");
        let coordinator = Arc::new(RuntimeFontCoordinator::default());
        let lease = coordinator
            .attach_session(identity(1), context)
            .expect("font session");
        coordinator.advance(context.as_ptr(), RenderStage::Addons, false, &[]);

        // A queued command runs only when the render thread drains it, and its return
        // value reaches the waiting caller.
        let worker = Arc::clone(&coordinator);
        let waiting = std::thread::spawn(move || {
            worker.enqueue_for_render_thread(
                0,
                FontQueueLimits::default(),
                Duration::from_secs(5),
                |manager| manager.cleanup_owner_callbacks(OwnerId::new(11, 1)),
            )
        });

        // Drain from the render thread until the waiter is served.
        let mut served = None;
        for _ in 0..200 {
            coordinator.advance(context.as_ptr(), RenderStage::Addons, false, &[]);
            if waiting.is_finished() {
                served = Some(waiting.join().expect("waiter must not panic"));
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(
            served.expect("the render thread must serve the queued command"),
            Ok(0)
        );

        // The byte bound rejects before accepting, so nothing is retained.
        let bounded = FontQueueLimits {
            commands: 8,
            bytes: 16,
        };
        let worker = Arc::clone(&coordinator);
        let refused = std::thread::spawn(move || {
            worker.enqueue_for_render_thread(17, bounded, Duration::from_secs(5), |_manager| ())
        })
        .join()
        .expect("waiter must not panic");
        assert_eq!(refused, Err(RuntimeFontBridgeError::QueueFull));

        // Detaching cancels queued work instead of leaving a caller blocked.
        let worker = Arc::clone(&coordinator);
        let canceled = std::thread::spawn(move || {
            worker.enqueue_for_render_thread(
                0,
                FontQueueLimits::default(),
                Duration::from_secs(5),
                |_manager| (),
            )
        });
        // Give the command time to land in the queue before detaching.
        for _ in 0..1000 {
            if mutex_lock(&coordinator.state).queue.len() == 1 {
                break;
            }
            std::thread::yield_now();
        }
        drop(lease);
        assert_eq!(
            canceled.join().expect("waiter must not panic"),
            Err(RuntimeFontBridgeError::StaleAttachment)
        );

        // SAFETY: the session lease dropped all TLS state before destruction.
        unsafe { sys::igDestroyContext(context.as_ptr()) };
    }

    /// The add-on-facing adapter must serve the same operation synchronously from either
    /// thread context: inline on the render thread, queued and drained from anywhere else.
    #[test]
    fn the_addon_facing_adapter_serves_both_thread_contexts() {
        let _guard = context_lock();
        // SAFETY: this test owns one current context on this thread.
        let context = unsafe { sys::igCreateContext(core::ptr::null_mut()) };
        let context = NonNull::new(context).expect("test context");
        let coordinator = Arc::new(RuntimeFontCoordinator::default());
        let lease = coordinator
            .attach_session(identity(1), context)
            .expect("font session");
        coordinator.advance(context.as_ptr(), RenderStage::Addons, false, &[]);

        let bridge = Arc::new(RuntimeFontBridge::new(Arc::clone(&coordinator)));

        // Render thread: executes inline and returns synchronously.
        assert_eq!(
            RenderFontService::cleanup_owner_callbacks(&*bridge, OwnerId::new(21, 1)),
            Ok(0)
        );

        // Another thread: queued, then served when the render thread drains.
        let worker = Arc::clone(&bridge);
        let waiting = std::thread::spawn(move || {
            RenderFontService::cleanup_owner_resources(&*worker, OwnerId::new(21, 1))
        });
        let mut served = None;
        for _ in 0..200 {
            coordinator.advance(context.as_ptr(), RenderStage::Addons, false, &[]);
            if waiting.is_finished() {
                served = Some(waiting.join().expect("waiter must not panic"));
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(
            served.expect("the render thread must serve the queued adapter call"),
            Ok(0)
        );

        // A missing font file is refused closed rather than panicking or queueing a path.
        assert_eq!(
            RenderFontService::add_from_file(
                &*bridge,
                OwnerId::new(21, 1),
                "absent".to_owned(),
                16.0,
                PathBuf::from("does-not-exist.ttf"),
                None,
                FontConfig::default(),
            ),
            Err(BackendOperationError::ServiceRejected)
        );

        drop(lease);
        // With no attachment every operation rejects closed instead of blocking.
        assert_eq!(
            RenderFontService::cleanup_owner(&*bridge, OwnerId::new(21, 1)),
            Err(BackendOperationError::ServiceRejected)
        );

        // SAFETY: the session lease dropped all TLS state before destruction.
        unsafe { sys::igDestroyContext(context.as_ptr()) };
    }

    #[test]
    fn address_catalog_matches_all_legacy_scale_branches() {
        let catalog = FontAddressCatalog {
            default: 1,
            small: FontAddresses {
                font: 10,
                font_big: 11,
                font_ui: 12,
            },
            normal: FontAddresses {
                font: 20,
                font_big: 21,
                font_ui: 22,
            },
            large: FontAddresses {
                font: 30,
                font_big: 31,
                font_ui: 32,
            },
            larger: FontAddresses {
                font: 40,
                font_big: 41,
                font_ui: 42,
            },
        };
        assert_eq!(catalog.selected(UiScale::SMALL), catalog.small);
        assert_eq!(catalog.selected(UiScale::NORMAL), catalog.normal);
        assert_eq!(catalog.selected(UiScale::LARGE), catalog.large);
        assert_eq!(catalog.selected(UiScale::LARGER), catalog.larger);
        assert_eq!(catalog.selected(UiScale(99)), catalog.normal);
    }

    #[test]
    fn settings_policy_is_bounded_and_accepts_only_one_ttf_filename() {
        assert_eq!(FontRebuildRequest::new(None, None).default_size(), 15.0);
        assert_eq!(
            FontRebuildRequest::new(Some(-5.0), None).default_size(),
            1.0
        );
        assert_eq!(
            FontRebuildRequest::new(Some(75.0), None).default_size(),
            50.0
        );
        assert!(validate_user_font_name("custom.TTF").is_ok());
        assert!(validate_user_font_name("sub/custom.ttf").is_err());
        assert!(validate_user_font_name("custom.otf").is_err());
        assert!(validate_user_font_file_len(0).is_err());
        assert!(validate_user_font_file_len(MAX_USER_FONT_BYTES as u64).is_ok());
        assert!(validate_user_font_file_len(MAX_USER_FONT_BYTES as u64 + 1).is_err());

        let directory = std::env::temp_dir().join(format!(
            "nexus-runtime-font-{}-{}",
            std::process::id(),
            super::NEXT_COORDINATOR_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).expect("test directory");
        fs::write(directory.join("custom.ttf"), [1_u8, 2, 3]).expect("test font");
        let loaded = load_configured_user_font(&directory, Some("custom.ttf"))
            .expect("bounded file")
            .expect("configured font");
        assert_eq!(loaded.byte_len(), 3);
        fs::remove_dir_all(directory).expect("test cleanup");
    }

    #[test]
    fn stale_lease_cannot_detach_a_reselected_context() {
        let _guard = context_lock();
        // SAFETY: this test owns both contexts and switches them explicitly.
        let first = unsafe { sys::igCreateContext(core::ptr::null_mut()) };
        let first = NonNull::new(first).expect("first context");
        let coordinator = Arc::new(RuntimeFontCoordinator::default());
        let first_lease = coordinator
            .attach_session(identity(10), first)
            .expect("first session");

        // SAFETY: this test retains ownership of both contexts on this thread.
        let second = unsafe { sys::igCreateContext(core::ptr::null_mut()) };
        let second = NonNull::new(second).expect("second context");
        // Dear ImGui 1.80 preserves an existing current context when another
        // context is created. Mirror the overlay's explicit render selection.
        // SAFETY: querying the current context has no side effects.
        assert_eq!(unsafe { sys::igGetCurrentContext() }, first.as_ptr());
        // SAFETY: `second` is live, owned, and selected only on this thread.
        unsafe { sys::igSetCurrentContext(second.as_ptr()) };
        let second_lease = coordinator
            .attach_session(identity(11), second)
            .expect("second session");
        drop(first_lease);
        assert_eq!(coordinator.active_identity(), Some(identity(11)));

        coordinator.advance(second.as_ptr(), RenderStage::Addons, false, &[]);
        assert_ne!(coordinator.selected_addresses(UiScale::NORMAL).font, 0);
        drop(second_lease);
        assert_eq!(coordinator.active_identity(), None);
        assert_eq!(
            coordinator.selected_addresses(UiScale::NORMAL),
            FontAddresses::default()
        );

        // SAFETY: both sessions are detached before their contexts are destroyed.
        unsafe {
            sys::igDestroyContext(second.as_ptr());
            sys::igSetCurrentContext(first.as_ptr());
            sys::igDestroyContext(first.as_ptr());
        }
    }

    #[test]
    fn replacement_error_and_panics_are_contained_without_stale_publication() {
        struct PanicOnDrop;

        impl Drop for PanicOnDrop {
            fn drop(&mut self) {
                panic!("panic payload destructor must not run");
            }
        }

        let _guard = context_lock();
        // SAFETY: this test owns one current context on this thread.
        let context = unsafe { sys::igCreateContext(core::ptr::null_mut()) };
        let context = NonNull::new(context).expect("test context");
        let coordinator = Arc::new(RuntimeFontCoordinator::default());
        let lease = coordinator
            .attach_session(identity(20), context)
            .expect("font session");

        FONT_SESSIONS.with(|sessions| {
            let mut sessions = sessions.borrow_mut();
            let session = sessions
                .get_mut(&coordinator.coordinator_id)
                .expect("thread session");
            session
                .manager
                .register_memory(
                    OwnerId::new(7, 1),
                    "FONT_DEFAULT",
                    15.0,
                    &[1],
                    FontConfig::default(),
                    None,
                )
                .expect("foreign reservation");
        });
        coordinator.advance(context.as_ptr(), RenderStage::Addons, false, &[]);
        assert_eq!(
            coordinator.selected_addresses(UiScale::NORMAL),
            FontAddresses::default()
        );
        assert!(!coordinator.take_gpu_rebuild(context.as_ptr(), RenderStage::Addons));
        assert!(contain_font_operation(|| panic!("intentional font panic")).is_err());
        assert!(
            contain_font_operation(|| std::panic::panic_any(PanicOnDrop)).is_err(),
            "panic payloads must be contained without running their destructor"
        );

        drop(lease);
        // SAFETY: the session lease dropped all TLS state before destruction.
        unsafe { sys::igDestroyContext(context.as_ptr()) };
    }

    #[test]
    fn combined_lease_drops_font_state_before_texture_state() {
        struct Probe {
            label: &'static str,
            events: Arc<Mutex<Vec<&'static str>>>,
        }
        impl Drop for Probe {
            fn drop(&mut self) {
                self.events
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(self.label);
            }
        }

        let events = Arc::new(Mutex::new(Vec::new()));
        let lease = CombinedRenderSessionLease {
            font: Some(Box::new(Probe {
                label: "font",
                events: Arc::clone(&events),
            })),
            texture: Some(Box::new(Probe {
                label: "texture",
                events: Arc::clone(&events),
            })),
        };
        drop(lease);
        assert_eq!(
            events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            ["font", "texture"]
        );
    }

    #[test]
    fn coordinator_is_process_safe_while_manager_stays_thread_bound() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RuntimeFontCoordinator>();

        let request = FontRebuildRequest::new(
            Some(16.0),
            Some(UserFont::from_bytes(vec![1]).expect("owned bytes")),
        );
        let coordinator = RuntimeFontCoordinator::new(request);
        assert_eq!(coordinator.request.default_size(), 16.0);
    }
}
