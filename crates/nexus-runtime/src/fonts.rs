use core::marker::PhantomData;
use core::ptr::NonNull;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Component, Path};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::ThreadId;

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
use nexus_ui_services::{OwnerId, UiScale};
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
        if let Some(active) = active
            && active.render_thread == std::thread::current().id()
        {
            drop_font_session(take_thread_session(
                self.coordinator_id,
                active.attachment_id,
            ));
        }
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

#[cfg(test)]
mod tests {
    use core::ptr::NonNull;
    use std::fs;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex, MutexGuard};

    use nexus_imgui_compat::sys;
    use nexus_render::{RenderStage, SwapChainId};
    use nexus_ui_fonts::{FontRebuildRequest, MAX_USER_FONT_BYTES, UserFont};
    use nexus_ui_services::{FontConfig, OwnerId, UiScale};

    use super::{
        CombinedRenderSessionLease, FONT_SESSIONS, FontAddressCatalog, FontAddresses,
        FontSessionIdentity, RuntimeFontCoordinator, contain_font_operation,
        load_configured_user_font, validate_user_font_file_len, validate_user_font_name,
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
