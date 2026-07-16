use std::ffi::{CStr, CString, OsString};
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use nexus_abi::{
    DL_MUMBLE_LINK, DL_MUMBLE_LINK_IDENTITY, EV_MUMBLE_IDENTITY_UPDATED, MumbleData,
    MumbleIdentity, MumbleUiScale,
};
use nexus_data_services::{
    DataLinkService, EventService, FontSnapshot, MumbleResourceSource, NexusLinkPublisher,
    NexusLinkSnapshot, QuickAccessPosition as NexusLinkQuickAccessPosition,
    QuickAccessSnapshot as NexusLinkQuickAccessSnapshot, RenderSnapshot, ResourceLease,
    WindowsMappingBackend,
};
use nexus_gw2::{DerivedTelemetry, IdentityUpdate, MumblePoll, MumbleReader};
use nexus_overlay::{
    NoopRenderSessionObserver, NoopWindowMessageRouter, RenderSessionObserver, WindowMessageRouter,
};
use nexus_platform::{
    FileLogSink, LogLevel, LogRegistry, MinimalScheduler, PathError, PathIndex, PathKey, PathRoots,
    SettingsStore, TaskHandle, TaskPriority,
};
use nexus_ui_host::{
    QuickAccessPosition as UiQuickAccessPosition, QuickAccessSnapshot as UiQuickAccessSnapshot,
    UiHost,
};
use nexus_ui_services::{
    DirectoryLocaleSource, LocalizationService, ScalingService, ScalingSink, ScalingSinkError,
    ScalingSnapshot, UiScale,
};
use thiserror::Error;
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleFileNameW, GetModuleHandleExW,
};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::UI::Shell::{CSIDL_PERSONAL, SHGFP_TYPE_CURRENT, SHGetFolderPathW};

const MAX_EXTENDED_PATH_UNITS: usize = 32_768;
const SHELL_PATH_UNITS: usize = 260;
const LOCALIZATION_QUEUE_CAPACITY: usize = 4_096;
const LANGUAGE_SETTING: &str = "Language";
const DPI_SCALING_SETTING: &str = "DPIScaling";
const LAST_UI_SCALE_SETTING: &str = "LastUIScale";

static SERVICES: OnceLock<Option<RuntimeServices>> = OnceLock::new();
static UI_HOST: OnceLock<Arc<UiHost>> = OnceLock::new();
static TEXTURES: OnceLock<Arc<crate::textures::RuntimeTextureCoordinator>> = OnceLock::new();

struct RuntimeServices {
    _paths: PathIndex,
    _settings: Arc<SettingsStore>,
    _data_link: Arc<DataLinkService>,
    _events: Arc<EventService>,
    nexus_link: Option<NexusLinkPublisher>,
    mumble: Option<MumbleRuntime>,
    input: Arc<crate::input::RuntimeInputServices>,
    game_input: Arc<crate::game_input::RuntimeGameInput>,
    ui_host: Arc<UiHost>,
    fonts: Arc<crate::fonts::RuntimeFontCoordinator>,
    textures: Arc<crate::textures::RuntimeTextureCoordinator>,
    render_observer: Arc<dyn RenderSessionObserver>,
    localization: Option<RuntimeLocalization>,
    scaling: RuntimeScaling,
    logger: LogRegistry,
    scheduler: Option<MinimalScheduler>,
}

impl RuntimeServices {
    fn build() -> Result<Self, ServiceInitError> {
        let roots = PathRoots::new(
            current_module_path()?,
            system_directory()?,
            documents_directory()?,
        );
        let paths = PathIndex::prepare(roots).map_err(ServiceInitError::PreparePaths)?;
        paths
            .create_directories()
            .map_err(ServiceInitError::CreateDirectories)?;

        let settings_path = paths.get(PathKey::Settings).to_path_buf();
        let settings = Arc::new(match SettingsStore::open(settings_path.clone()) {
            Ok(settings) => settings,
            Err(error) => {
                crate::diagnostics::report_proxy_failure(&error);
                SettingsStore::empty(settings_path)
            }
        });

        let logger = LogRegistry::new();
        match FileLogSink::create(paths.get(PathKey::Log), LogLevel::All) {
            Ok(sink) => {
                let _ = logger.register(Arc::new(sink));
            }
            Err(error) => crate::diagnostics::report_proxy_failure(&error),
        }

        let scheduler = match MinimalScheduler::new() {
            Ok(scheduler) => Some(scheduler),
            Err(error) => {
                crate::diagnostics::report_proxy_failure(&error);
                None
            }
        };

        let data_link = Arc::new(DataLinkService::new(Arc::new(WindowsMappingBackend)));
        let events = Arc::new(EventService::new());
        let nexus_link = match NexusLinkPublisher::open(&data_link, None) {
            Ok(publisher) => Some(publisher),
            Err(error) => {
                crate::diagnostics::report_proxy_failure(&error);
                None
            }
        };
        let mumble = initialize_mumble(&data_link, &events, scheduler.as_ref());
        let ui_host = ui_host();
        let fonts = crate::fonts::RuntimeFontCoordinator::load(
            settings.as_ref(),
            paths.get(PathKey::FontsDirectory),
        );
        let textures = texture_coordinator();
        let texture_observer = crate::textures::production_observer(
            Arc::clone(&textures),
            paths.get(PathKey::TexturesDirectory).to_path_buf(),
        );
        let render_observer =
            crate::fonts::production_observer(Arc::clone(&fonts), texture_observer);
        let (game_input, game_input_error) =
            crate::game_input::RuntimeGameInput::load(paths.get(PathKey::GameBinds).to_path_buf());
        if let Some(error) = game_input_error {
            crate::diagnostics::report_proxy_failure(&error);
        }
        let render_observer =
            crate::game_input::production_observer(Arc::clone(&game_input), render_observer);
        let (input, input_error) = crate::input::RuntimeInputServices::load(
            paths.get(PathKey::InputBinds).to_path_buf(),
            Arc::clone(&ui_host),
            Arc::clone(&settings),
        );
        if let Some(error) = input_error {
            crate::diagnostics::report_proxy_failure(&error);
        }
        let localization = RuntimeLocalization::load(&paths, settings.as_ref());
        let scaling = RuntimeScaling::load(Arc::clone(&settings));

        Ok(Self {
            _paths: paths,
            _settings: settings,
            _data_link: data_link,
            _events: events,
            nexus_link,
            mumble,
            input,
            game_input,
            ui_host,
            fonts,
            textures,
            render_observer,
            localization,
            scaling,
            logger,
            scheduler,
        })
    }

    fn shutdown_game_input(&self) {
        let report = self.game_input.shutdown();
        if let Some(error) = report.release_error {
            crate::diagnostics::report_proxy_failure(&error);
        }
        if let Some(error) = report.persistence_error {
            crate::diagnostics::report_proxy_failure(&error);
        }
    }

    fn shutdown(&self) {
        self.shutdown_game_input();
        self.fonts.shutdown();
        self.textures.shutdown();
        if let Err(error) = self.input.shutdown() {
            crate::diagnostics::report_proxy_failure(&error);
        }
        if let Some(scheduler) = &self.scheduler
            && let Err(error) = scheduler.shutdown_and_drain()
        {
            crate::diagnostics::report_proxy_failure(&error);
        }
        let _ = self.logger.flush();
    }
}

struct RuntimeLocalizationState {
    service: LocalizationService,
    texts: Arc<[CString]>,
}

struct RuntimeLocalization {
    state: Mutex<RuntimeLocalizationState>,
}

#[derive(Clone)]
pub(crate) struct LocalizationFrame {
    pub(crate) changed: bool,
    pub(crate) texts: Arc<[CString]>,
}

impl Default for LocalizationFrame {
    fn default() -> Self {
        Self {
            changed: false,
            texts: Vec::new().into(),
        }
    }
}

impl RuntimeLocalization {
    fn load(paths: &PathIndex, settings: &SettingsStore) -> Option<Self> {
        let language = match settings.get::<String>(LANGUAGE_SETTING) {
            Ok(Some(language)) if !language.is_empty() => language,
            Ok(_) => "en".to_owned(),
            Err(error) => {
                crate::diagnostics::report_proxy_failure(&error);
                "en".to_owned()
            }
        };
        let mut service = match LocalizationService::new(&language, LOCALIZATION_QUEUE_CAPACITY) {
            Ok(service) => service,
            Err(error) => {
                crate::diagnostics::report_proxy_failure(&error);
                LocalizationService::new("en", LOCALIZATION_QUEUE_CAPACITY).ok()?
            }
        };
        let mut source = DirectoryLocaleSource::new(paths.get(PathKey::LocalesDirectory));
        if let Err(error) = service.reload(&mut source) {
            crate::diagnostics::report_proxy_failure(&error);
        }
        let texts = localization_texts(&service);
        Some(Self {
            state: Mutex::new(RuntimeLocalizationState { service, texts }),
        })
    }

    fn advance(&self) -> LocalizationFrame {
        let mut state = mutex_lock(&self.state);
        let report = state.service.advance();
        let changed =
            report.applied_texts > 0 || report.removed_overrides > 0 || report.language_changed;
        if changed {
            state.texts = localization_texts(&state.service);
        }
        LocalizationFrame {
            changed,
            texts: Arc::clone(&state.texts),
        }
    }
}

fn localization_texts(service: &LocalizationService) -> Arc<[CString]> {
    service
        .all_texts()
        .into_iter()
        .map(CStr::to_owned)
        .collect::<Vec<_>>()
        .into()
}

#[derive(Debug, Error)]
enum ServiceInitError {
    #[error("runtime module lookup failed")]
    ModuleLookup(#[source] io::Error),
    #[error("runtime module path is unavailable")]
    ModulePath,
    #[error("system directory lookup failed")]
    SystemDirectory(#[source] io::Error),
    #[error("documents directory lookup failed")]
    DocumentsDirectory,
    #[error("runtime path preparation failed")]
    PreparePaths(#[source] PathError),
    #[error("runtime directory creation failed")]
    CreateDirectories(#[source] PathError),
    #[error("the Mumble shared-memory name is unavailable")]
    MumbleName,
}

struct MumbleRuntime {
    latest: Arc<Mutex<Option<MumblePoll>>>,
    latest_identity: Arc<Mutex<MumbleIdentity>>,
    _task: TaskHandle,
}

impl MumbleRuntime {
    fn state(&self) -> (DerivedTelemetry, MumbleUiScale) {
        let derived = mutex_lock(&self.latest)
            .as_ref()
            .map_or_else(DerivedTelemetry::default, |poll| poll.derived);
        let ui_size = mutex_lock(&self.latest_identity).ui_size;
        (derived, ui_size)
    }
}

fn initialize_mumble(
    data_link: &DataLinkService,
    events: &Arc<EventService>,
    scheduler: Option<&MinimalScheduler>,
) -> Option<MumbleRuntime> {
    let identity = match data_link.share_internal(
        DL_MUMBLE_LINK_IDENTITY,
        core::mem::size_of::<MumbleIdentity>(),
    ) {
        Ok(identity) => identity,
        Err(error) => {
            crate::diagnostics::report_proxy_failure(&error);
            return None;
        }
    };
    let option = crate::runtime::mumble_option();
    let Some(mapping_name) = option.reader_name().to_str() else {
        crate::diagnostics::report_proxy_failure(&ServiceInitError::MumbleName);
        return None;
    };
    let resource = match data_link.share_public(
        DL_MUMBLE_LINK,
        core::mem::size_of::<MumbleData>(),
        Some(mapping_name),
    ) {
        Ok(resource) => resource,
        Err(error) => {
            crate::diagnostics::report_proxy_failure(&error);
            return None;
        }
    };
    let source = match MumbleResourceSource::new(resource) {
        Ok(source) => source,
        Err(error) => {
            crate::diagnostics::report_proxy_failure(&error);
            return None;
        }
    };
    if !option.polling_enabled() {
        return None;
    }
    let scheduler = scheduler?;

    let reader = Mutex::new(MumbleReader::new(source));
    let latest = Arc::new(Mutex::new(None));
    let latest_for_task = Arc::clone(&latest);
    let latest_identity = Arc::new(Mutex::new(MumbleIdentity::default()));
    let latest_identity_for_task = Arc::clone(&latest_identity);
    let events = Arc::clone(events);
    let task =
        match scheduler.schedule_every(Duration::from_millis(50), TaskPriority::Normal, move |_| {
            let poll = mutex_lock(&reader).poll(crate::dxgi::primary_frame_count());
            let Ok(poll) = poll else {
                return;
            };
            if let Ok(IdentityUpdate::Updated(identity_update)) = &poll.identity {
                write_identity(&identity, *identity_update);
                *mutex_lock(&latest_identity_for_task) = *identity_update;
                // SAFETY: `identity` retains an exact MumbleIdentity allocation
                // throughout synchronous dispatch, and subscribers own the
                // event-specific interpretation of this borrowed payload.
                let _ = unsafe { events.raise(EV_MUMBLE_IDENTITY_UPDATED, identity.as_mut_ptr()) };
            }
            *mutex_lock(&latest_for_task) = Some(poll);
        }) {
            Ok(task) => task,
            Err(error) => {
                crate::diagnostics::report_proxy_failure(&error);
                return None;
            }
        };
    Some(MumbleRuntime {
        latest,
        latest_identity,
        _task: task,
    })
}

struct ScalingOutputs {
    nexus_scale: AtomicU32,
    font_global_scale: AtomicU32,
}

impl ScalingOutputs {
    fn new() -> Self {
        Self {
            nexus_scale: AtomicU32::new(1.0_f32.to_bits()),
            font_global_scale: AtomicU32::new(1.0_f32.to_bits()),
        }
    }

    fn font_global_scale(&self) -> f32 {
        f32::from_bits(self.font_global_scale.load(Ordering::Acquire))
    }
}

struct RuntimeScalingSink {
    settings: Arc<SettingsStore>,
    outputs: Arc<ScalingOutputs>,
}

impl ScalingSink for RuntimeScalingSink {
    fn publish_nexus_scale(&mut self, scale: f32) -> Result<(), ScalingSinkError> {
        self.outputs
            .nexus_scale
            .store(scale.to_bits(), Ordering::Release);
        Ok(())
    }

    fn publish_font_global_scale(&mut self, scale: f32) -> Result<(), ScalingSinkError> {
        self.outputs
            .font_global_scale
            .store(scale.to_bits(), Ordering::Release);
        Ok(())
    }

    fn persist_game_scale(&mut self, scale: f32) -> Result<(), ScalingSinkError> {
        self.settings
            .set(LAST_UI_SCALE_SETTING, &scale)
            .map(|_| ())
            .map_err(|_| ScalingSinkError::SettingsUnavailable)
    }
}

struct RuntimeScaling {
    service: Mutex<ScalingService<RuntimeScalingSink>>,
    outputs: Arc<ScalingOutputs>,
}

impl RuntimeScaling {
    fn load(settings: Arc<SettingsStore>) -> Self {
        let dpi_enabled = match settings.get::<bool>(DPI_SCALING_SETTING) {
            Ok(Some(enabled)) => enabled,
            Ok(None) => true,
            Err(error) => {
                crate::diagnostics::report_proxy_failure(&error);
                true
            }
        };
        let last_game_scale = match settings.get::<f32>(LAST_UI_SCALE_SETTING) {
            Ok(Some(scale)) if scale.is_finite() && scale > 0.0 => scale,
            Ok(_) => 1.0,
            Err(error) => {
                crate::diagnostics::report_proxy_failure(&error);
                1.0
            }
        };
        let outputs = Arc::new(ScalingOutputs::new());
        let sink = RuntimeScalingSink {
            settings,
            outputs: Arc::clone(&outputs),
        };
        Self {
            service: Mutex::new(ScalingService::new(sink, dpi_enabled, last_game_scale)),
            outputs,
        }
    }

    fn advance(&self, width: u32, height: u32, ui_size: MumbleUiScale) -> ScalingSnapshot {
        let mut service = mutex_lock(&self.service);
        if let Err(error) = service.update_resolution(width as f32, height as f32) {
            crate::diagnostics::report_proxy_failure(&error);
        }
        if let Err(error) = service.update_game_ui(UiScale(ui_size.value())) {
            crate::diagnostics::report_proxy_failure(&error);
        }
        service.snapshot()
    }

    fn font_global_scale(&self) -> f32 {
        self.outputs.font_global_scale()
    }
}

fn write_identity(resource: &ResourceLease, identity: MumbleIdentity) {
    debug_assert_eq!(resource.len(), core::mem::size_of::<MumbleIdentity>());
    let source = (&raw const identity).cast::<u8>();
    let destination = resource.as_mut_ptr().cast::<u8>();
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
    for offset in 0..core::mem::size_of::<MumbleIdentity>() {
        // SAFETY: the source is one initialized identity and the retained
        // destination has the exact same byte length. Byte writes require no
        // alignment and match the legacy shared-resource synchronization model.
        unsafe {
            destination
                .add(offset)
                .write_volatile(source.add(offset).read())
        };
    }
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
}

fn mutex_lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn initialize() {
    let _ = SERVICES.get_or_init(|| match RuntimeServices::build() {
        Ok(services) => Some(services),
        Err(error) => {
            crate::diagnostics::report_proxy_failure(&error);
            None
        }
    });
}

pub(crate) fn ui_host() -> Arc<UiHost> {
    Arc::clone(UI_HOST.get_or_init(|| Arc::new(UiHost::default())))
}

pub(crate) fn texture_coordinator() -> Arc<crate::textures::RuntimeTextureCoordinator> {
    Arc::clone(
        TEXTURES.get_or_init(|| Arc::new(crate::textures::RuntimeTextureCoordinator::default())),
    )
}

pub(crate) fn font_coordinator() -> Arc<crate::fonts::RuntimeFontCoordinator> {
    SERVICES.get().and_then(Option::as_ref).map_or_else(
        || Arc::new(crate::fonts::RuntimeFontCoordinator::default()),
        |services| Arc::clone(&services.fonts),
    )
}

pub(crate) fn render_observer() -> Arc<dyn RenderSessionObserver> {
    SERVICES.get().and_then(Option::as_ref).map_or_else(
        || Arc::new(NoopRenderSessionObserver) as Arc<dyn RenderSessionObserver>,
        |services| Arc::clone(&services.render_observer),
    )
}

pub(crate) fn shutdown() {
    if let Some(services) = SERVICES.get().and_then(Option::as_ref) {
        services.shutdown();
    }
}

pub(crate) fn shutdown_game_input() {
    if let Some(services) = SERVICES.get().and_then(Option::as_ref) {
        services.shutdown_game_input();
    }
}

pub(crate) fn window_router() -> Arc<dyn WindowMessageRouter> {
    SERVICES.get().and_then(Option::as_ref).map_or_else(
        || Arc::new(NoopWindowMessageRouter) as Arc<dyn WindowMessageRouter>,
        |services| Arc::clone(&services.input) as Arc<dyn WindowMessageRouter>,
    )
}

pub(crate) fn advance_localization() -> LocalizationFrame {
    SERVICES
        .get()
        .and_then(Option::as_ref)
        .and_then(|services| services.localization.as_ref())
        .map_or_else(LocalizationFrame::default, RuntimeLocalization::advance)
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct UiServiceFrame {
    pub(crate) font_global_scale: f32,
}

impl Default for UiServiceFrame {
    fn default() -> Self {
        Self {
            font_global_scale: 1.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct QuickAccessLinkState {
    active_icons: usize,
    position: NexusLinkQuickAccessPosition,
    vertical_layout: bool,
}

fn quick_access_link_state<F>(
    snapshot: &UiQuickAccessSnapshot,
    mut has_handler: F,
) -> QuickAccessLinkState
where
    F: FnMut(&str) -> bool,
{
    let active_icons = snapshot
        .shortcuts
        .iter()
        .filter(|shortcut| shortcut.is_active(has_handler(shortcut.input_bind.as_ref())))
        .count();
    QuickAccessLinkState {
        active_icons,
        position: nexus_link_quick_access_position(snapshot.settings.position),
        vertical_layout: snapshot.settings.vertical_layout,
    }
}

const fn nexus_link_quick_access_position(
    position: UiQuickAccessPosition,
) -> NexusLinkQuickAccessPosition {
    match position {
        UiQuickAccessPosition::Extend => NexusLinkQuickAccessPosition::Extend,
        UiQuickAccessPosition::Under => NexusLinkQuickAccessPosition::Under,
        UiQuickAccessPosition::Bottom => NexusLinkQuickAccessPosition::Bottom,
        UiQuickAccessPosition::Custom => NexusLinkQuickAccessPosition::Custom,
    }
}

pub(crate) fn advance_ui_services(width: u32, height: u32) -> UiServiceFrame {
    let Some(services) = SERVICES.get().and_then(Option::as_ref) else {
        return UiServiceFrame::default();
    };
    let (telemetry, ui_size) = services.mumble.as_ref().map_or_else(
        || (DerivedTelemetry::default(), MumbleUiScale::NORMAL),
        MumbleRuntime::state,
    );
    let scaling = services.scaling.advance(width, height, ui_size);
    let fonts = services.fonts.selected_addresses(UiScale(ui_size.value()));
    if let Some(publisher) = &services.nexus_link {
        let quick_access = services
            .ui_host
            .quick_access()
            .snapshot(telemetry.is_gameplay, false);
        let quick_access = quick_access_link_state(&quick_access, |identifier| {
            services.input.has_handler(identifier)
        });
        if let Ok(render) = RenderSnapshot::new(
            width,
            height,
            scaling.cumulative,
            FontSnapshot::from_addresses(fonts.font, fonts.font_big, fonts.font_ui),
        ) && let Ok(quick_access) = NexusLinkQuickAccessSnapshot::new(
            quick_access.active_icons,
            quick_access.position,
            quick_access.vertical_layout,
        ) {
            publisher.publish(NexusLinkSnapshot::new(render, telemetry, quick_access));
        }
    }
    UiServiceFrame {
        font_global_scale: services.scaling.font_global_scale(),
    }
}

extern "system" fn module_anchor() {}

fn current_module_path() -> Result<PathBuf, ServiceInitError> {
    let mut module: HMODULE = ptr::null_mut();
    let flags =
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
    let address = module_anchor as *const () as *const u16;

    // SAFETY: `address` identifies code in this loaded module and `module`
    // remains writable for the synchronous call. UNCHANGED_REFCOUNT avoids
    // acquiring ownership that would need releasing during runtime teardown.
    let found = unsafe { GetModuleHandleExW(flags, address, &raw mut module) };
    if found == 0 || module.is_null() {
        return Err(ServiceInitError::ModuleLookup(io::Error::last_os_error()));
    }

    let mut buffer = vec![0_u16; MAX_EXTENDED_PATH_UNITS];
    // SAFETY: `buffer` contains the advertised number of writable UTF-16
    // units and `module` was returned by `GetModuleHandleExW` above.
    let length =
        unsafe { GetModuleFileNameW(module, buffer.as_mut_ptr(), MAX_EXTENDED_PATH_UNITS as u32) }
            as usize;
    if length == 0 || length >= buffer.len() {
        return Err(ServiceInitError::ModulePath);
    }
    buffer.truncate(length);
    Ok(PathBuf::from(OsString::from_wide(&buffer)))
}

fn system_directory() -> Result<PathBuf, ServiceInitError> {
    let mut buffer = vec![0_u16; MAX_EXTENDED_PATH_UNITS];
    // SAFETY: `buffer` contains the advertised number of writable UTF-16
    // units for the duration of the synchronous Windows API call.
    let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), MAX_EXTENDED_PATH_UNITS as u32) }
        as usize;
    if length == 0 {
        return Err(ServiceInitError::SystemDirectory(io::Error::last_os_error()));
    }
    if length >= buffer.len() {
        return Err(ServiceInitError::SystemDirectory(io::Error::new(
            io::ErrorKind::InvalidData,
            "system directory exceeds the supported Windows path limit",
        )));
    }
    buffer.truncate(length);
    Ok(PathBuf::from(OsString::from_wide(&buffer)))
}

fn documents_directory() -> Result<PathBuf, ServiceInitError> {
    let mut buffer = vec![0_u16; SHELL_PATH_UNITS];
    // SAFETY: the optional window and token handles are null by contract and
    // `buffer` provides the MAX_PATH-sized storage required by this API.
    let result = unsafe {
        SHGetFolderPathW(
            ptr::null_mut(),
            CSIDL_PERSONAL as i32,
            ptr::null_mut(),
            SHGFP_TYPE_CURRENT as u32,
            buffer.as_mut_ptr(),
        )
    };
    if result < 0 {
        return Err(ServiceInitError::DocumentsDirectory);
    }
    let Some(length) = buffer.iter().position(|unit| *unit == 0) else {
        return Err(ServiceInitError::DocumentsDirectory);
    };
    if length == 0 {
        return Err(ServiceInitError::DocumentsDirectory);
    }
    buffer.truncate(length);
    Ok(PathBuf::from(OsString::from_wide(&buffer)))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use nexus_ui_host::{
        ContextMenuItemSnapshot, OwnerGeneration, QuickAccessPosition, QuickAccessSettings,
        QuickAccessSnapshot, ShortcutSnapshot,
    };

    use super::{
        NexusLinkQuickAccessPosition, nexus_link_quick_access_position, quick_access_link_state,
        ui_host,
    };

    fn shortcut(identifier: &str, input_bind: &str) -> ShortcutSnapshot {
        ShortcutSnapshot {
            owner: OwnerGeneration::new(1, 1),
            id: Arc::from(identifier),
            texture: Arc::from("texture"),
            hover_texture: Arc::from("hover-texture"),
            input_bind: Arc::from(input_bind),
            tooltip: Arc::from("tooltip"),
            suppressed: false,
            context_items: Arc::from(Vec::<ContextMenuItemSnapshot>::new()),
            notifications: Arc::from(Vec::<Arc<str>>::new()),
        }
    }

    #[test]
    fn nexus_link_quick_access_uses_live_registry_count_and_layout() {
        let settings = QuickAccessSettings {
            vertical_layout: true,
            position: QuickAccessPosition::Custom,
            ..QuickAccessSettings::default()
        };
        let snapshot = QuickAccessSnapshot {
            revision: 7,
            settings,
            globally_visible: false,
            shortcuts: Arc::from(vec![
                shortcut("active", "bind.active"),
                shortcut("inactive", ""),
            ]),
        };

        let state = quick_access_link_state(&snapshot, |identifier| identifier == "bind.active");
        assert_eq!(state.active_icons, 1);
        assert_eq!(state.position, NexusLinkQuickAccessPosition::Custom);
        assert!(state.vertical_layout);
    }

    #[test]
    fn quick_access_position_mapping_is_closed_and_exhaustive() {
        assert_eq!(
            nexus_link_quick_access_position(QuickAccessPosition::Extend),
            NexusLinkQuickAccessPosition::Extend
        );
        assert_eq!(
            nexus_link_quick_access_position(QuickAccessPosition::Under),
            NexusLinkQuickAccessPosition::Under
        );
        assert_eq!(
            nexus_link_quick_access_position(QuickAccessPosition::Bottom),
            NexusLinkQuickAccessPosition::Bottom
        );
        assert_eq!(
            nexus_link_quick_access_position(QuickAccessPosition::Custom),
            NexusLinkQuickAccessPosition::Custom
        );
    }

    #[test]
    fn process_ui_host_is_shared() {
        let first = ui_host();
        let second = ui_host();
        assert!(Arc::ptr_eq(&first, &second));
    }
}
