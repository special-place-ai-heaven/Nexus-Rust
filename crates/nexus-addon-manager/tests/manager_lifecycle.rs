//! End-to-end orchestration tests using only injected fake platform boundaries.

use std::{
    collections::HashSet,
    ffi::{CStr, c_char},
    mem::size_of,
    num::NonZeroUsize,
    path::PathBuf,
    ptr::{NonNull, null},
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicPtr, AtomicUsize, Ordering},
    },
    time::Duration,
};

use nexus_abi::{
    AddonApi, AddonDefinitionFlags, AddonDefinitionV1, GetAddonDefinitionV1, UpdateProvider,
    Version,
};
use nexus_addon_loader::{
    ADDON_DEFINITION_EXPORT, AbsoluteDllPath, LoaderPlatform, ModuleBounds, ModuleHandle,
    ModuleImage, PlatformError,
};
use nexus_addon_manager::{
    ActivationRequest, AddonConfigDocument, AddonDirectory, AddonManager, BinaryRevision,
    CleanupError, CleanupPhase, ConfigAccess, DirectoryChangeKind, DirectoryEvent,
    DirectoryRecommendation, DirectoryScanner, DiscoveredDll, DiscoveryError, EnableEffect,
    HostIssue, InspectOutcome, LoadedModuleAddressResolver, ManagerError, ManagerOptions,
    ManagerRuntime, ManagerState, ModuleAddressRange, ModuleAddressResolver, PolicyReason,
    RegistrationCleaner, UninstallTiming, UnloadReason, UpdateTiming,
};
use nexus_core::{AddressOwnershipIndex, CallbackGate, OwnerToken};
use nexus_host::{ApiTableCatalog, ApiTables, MetadataLimits, ModuleMemory, ModuleReadError};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeEvent {
    LoadLibrary,
    LoadCallback,
    UnloadCallback,
    FreeLibrary,
}

type SharedNativeEvents = Arc<Mutex<Vec<NativeEvent>>>;

static TEST_LOCK: Mutex<()> = Mutex::new(());
static EXPORTED_DEFINITION: AtomicPtr<AddonDefinitionV1> = AtomicPtr::new(std::ptr::null_mut());
static CALLBACK_EVENTS: OnceLock<Mutex<Option<SharedNativeEvents>>> = OnceLock::new();

unsafe extern "C" fn definition_export() -> *mut AddonDefinitionV1 {
    EXPORTED_DEFINITION.load(Ordering::Acquire)
}

unsafe extern "C" fn load_callback(_api: *mut AddonApi) {
    record_native(NativeEvent::LoadCallback);
}

unsafe extern "C" fn unload_callback() {
    record_native(NativeEvent::UnloadCallback);
}

fn callback_events() -> &'static Mutex<Option<SharedNativeEvents>> {
    CALLBACK_EVENTS.get_or_init(|| Mutex::new(None))
}

fn record_native(event: NativeEvent) {
    if let Some(events) = lock(callback_events()).clone() {
        lock(&events).push(event);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone, Copy)]
struct Segment {
    start: usize,
    bytes: &'static [u8],
}

#[derive(Clone)]
struct FakeMemory {
    segments: Arc<[Segment]>,
}

impl ModuleMemory for FakeMemory {
    fn read_bounded(
        &self,
        address: NonNull<c_char>,
        maximum_bytes: usize,
    ) -> Result<&[u8], ModuleReadError> {
        let address = address.as_ptr() as usize;
        let segment = self
            .segments
            .iter()
            .find(|segment| {
                address >= segment.start
                    && address < segment.start.saturating_add(segment.bytes.len())
            })
            .ok_or_else(|| ModuleReadError::new("fake memory range is unavailable"))?;
        let offset = address - segment.start;
        let length = maximum_bytes.min(segment.bytes.len() - offset);
        Ok(&segment.bytes[offset..offset + length])
    }
}

struct FakePlatform {
    events: SharedNativeEvents,
    bounds: ModuleBounds,
    memory: FakeMemory,
    executable: HashSet<usize>,
    free_failures: AtomicUsize,
}

// SAFETY: every fake handle is a stable non-zero token, all borrowed memory is
// leaked for the test process, and release only records an event.
unsafe impl LoaderPlatform for FakePlatform {
    type Memory = FakeMemory;

    unsafe fn load_library(&self, _path: &AbsoluteDllPath) -> Result<ModuleHandle, PlatformError> {
        lock(&self.events).push(NativeEvent::LoadLibrary);
        Ok(ModuleHandle::from_non_zero(
            NonZeroUsize::new(1).expect("fixture handle is non-zero"),
        ))
    }

    unsafe fn resolve_definition_export(
        &self,
        _module: ModuleHandle,
        export: &'static CStr,
    ) -> Result<GetAddonDefinitionV1, PlatformError> {
        assert_eq!(export, ADDON_DEFINITION_EXPORT);
        Ok(definition_export)
    }

    unsafe fn module_image(
        &self,
        _module: ModuleHandle,
    ) -> Result<ModuleImage<Self::Memory>, PlatformError> {
        Ok(ModuleImage::new(self.bounds, self.memory.clone()))
    }

    unsafe fn is_executable_address(
        &self,
        _module: ModuleHandle,
        address: NonZeroUsize,
    ) -> Result<bool, PlatformError> {
        Ok(self.executable.contains(&address.get()))
    }

    unsafe fn free_library(&self, _module: ModuleHandle) -> Result<(), PlatformError> {
        lock(&self.events).push(NativeEvent::FreeLibrary);
        self.free_failures
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                remaining.checked_sub(1)
            })
            .map_or(Ok(()), |_previous| Err(PlatformError::FreeLibrary))
    }
}

#[derive(Clone)]
struct FakeScanner {
    entries: Arc<Mutex<Vec<DiscoveredDll>>>,
}

impl DirectoryScanner for FakeScanner {
    fn scan(&self, _directory: &AddonDirectory) -> Result<Vec<DiscoveredDll>, DiscoveryError> {
        Ok(lock(&self.entries).clone())
    }
}

#[derive(Clone, Copy)]
struct FixedResolver(ModuleAddressRange);

impl ModuleAddressResolver<FakePlatform> for FixedResolver {
    fn resolve(
        &self,
        _module: &nexus_addon_loader::AddonModule<FakePlatform>,
    ) -> Result<ModuleAddressRange, nexus_addon_manager::AddressResolutionError> {
        Ok(self.0)
    }
}

#[derive(Clone)]
struct RecordingCleaner {
    calls: Arc<Mutex<Vec<(OwnerToken, CleanupPhase)>>>,
}

impl RegistrationCleaner for RecordingCleaner {
    fn cleanup(&mut self, owner: OwnerToken, phase: CleanupPhase) -> Result<(), CleanupError> {
        lock(&self.calls).push((owner, phase));
        Ok(())
    }
}

struct Fixture {
    platform: Arc<FakePlatform>,
    scanner: FakeScanner,
    resolver: FixedResolver,
    candidate: DiscoveredDll,
    probe: NonZeroUsize,
    native_events: SharedNativeEvents,
    cleanup_events: Arc<Mutex<Vec<(OwnerToken, CleanupPhase)>>>,
}

impl Fixture {
    fn new(flags: AddonDefinitionFlags) -> Self {
        Self::with_free_failures(flags, 0)
    }

    fn with_free_failures(flags: AddonDefinitionFlags, free_failures: usize) -> Self {
        let name = Box::leak(Box::new(*b"Fixture\0"));
        let author = Box::leak(Box::new(*b"Nexus\0"));
        let description = Box::leak(Box::new(*b"Managed fixture\0"));
        let definition = Box::leak(Box::new(AddonDefinitionV1 {
            signature: 0x1234_5678,
            api_version: 1,
            name: name.as_ptr().cast(),
            version: Version::new(1, 2, 3, 4),
            author: author.as_ptr().cast(),
            description: description.as_ptr().cast(),
            load: Some(load_callback),
            unload: Some(unload_callback),
            flags,
            provider: UpdateProvider::NONE,
            update_link: null(),
        }));
        EXPORTED_DEFINITION.store(definition, Ordering::Release);

        // SAFETY: the definition is leaked and the byte view exactly covers it.
        let definition_bytes = unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref(definition).cast::<u8>(),
                size_of::<AddonDefinitionV1>(),
            )
        };
        let segments: Arc<[Segment]> = vec![
            Segment {
                start: definition_bytes.as_ptr() as usize,
                bytes: definition_bytes,
            },
            Segment {
                start: name.as_ptr() as usize,
                bytes: name,
            },
            Segment {
                start: author.as_ptr() as usize,
                bytes: author,
            },
            Segment {
                start: description.as_ptr() as usize,
                bytes: description,
            },
        ]
        .into();
        let code = [
            definition_export as *const () as usize,
            load_callback as *const () as usize,
            unload_callback as *const () as usize,
        ];
        let lowest_data = segments
            .iter()
            .map(|segment| segment.start)
            .min()
            .expect("fixture has data");
        let highest_data = segments
            .iter()
            .map(|segment| segment.start + segment.bytes.len())
            .max()
            .expect("fixture has data");
        let lowest = code.iter().copied().fold(lowest_data, usize::min);
        let highest = code
            .iter()
            .copied()
            .map(|address| address + 1)
            .fold(highest_data, usize::max);
        let probe = NonZeroUsize::new(lowest).expect("fixture addresses are non-zero");
        let size = highest - lowest;
        let bounds = ModuleBounds::new(probe, size).expect("fixture bounds are valid");
        let range = ModuleAddressRange::new(probe, size).expect("manager bounds are valid");
        let native_events = Arc::new(Mutex::new(Vec::new()));
        *lock(callback_events()) = Some(Arc::clone(&native_events));
        let platform = Arc::new(FakePlatform {
            events: Arc::clone(&native_events),
            bounds,
            memory: FakeMemory { segments },
            executable: HashSet::from(code),
            free_failures: AtomicUsize::new(free_failures),
        });
        let candidate = DiscoveredDll::new(
            addon_path("fixture.dll"),
            BinaryRevision::from_bytes([1; 16]),
            128,
        )
        .expect("candidate should be valid");
        let entries = Arc::new(Mutex::new(vec![candidate.clone()]));
        Self {
            platform,
            scanner: FakeScanner { entries },
            resolver: FixedResolver(range),
            candidate,
            probe,
            native_events,
            cleanup_events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn manager(&self) -> AddonManager<FakePlatform, RecordingCleaner> {
        self.manager_with_api_tables(true)
    }

    fn manager_without_api_tables(&self) -> AddonManager<FakePlatform, RecordingCleaner> {
        self.manager_with_api_tables(false)
    }

    fn manager_with_api_tables(
        &self,
        install_api_tables: bool,
    ) -> AddonManager<FakePlatform, RecordingCleaner> {
        self.manager_with_resolver(install_api_tables, self.resolver)
    }

    fn manager_with_resolver(
        &self,
        install_api_tables: bool,
        resolver: impl ModuleAddressResolver<FakePlatform>,
    ) -> AddonManager<FakePlatform, RecordingCleaner> {
        self.manager_with_ownership(
            install_api_tables,
            resolver,
            Arc::new(AddressOwnershipIndex::new()),
        )
    }

    fn manager_with_ownership(
        &self,
        install_api_tables: bool,
        resolver: impl ModuleAddressResolver<FakePlatform>,
        ownership: Arc<AddressOwnershipIndex>,
    ) -> AddonManager<FakePlatform, RecordingCleaner> {
        let runtime = ManagerRuntime::new(
            self.scanner.clone(),
            Arc::clone(&self.platform),
            resolver,
            RecordingCleaner {
                calls: Arc::clone(&self.cleanup_events),
            },
        )
        .with_address_ownership_index(ownership);
        let runtime = if install_api_tables {
            runtime.with_api_tables(populated_test_catalog())
        } else {
            runtime
        };
        AddonManager::new(
            AddonDirectory::new(addon_root()).expect("root should be absolute"),
            runtime,
            AddonConfigDocument::new(),
            ConfigAccess::Writable,
            ManagerOptions {
                metadata_limits: MetadataLimits::default(),
                diagnostic_capacity: 64,
                game_build: 1_234,
            },
        )
        .expect("manager should construct")
    }
}

fn populated_test_catalog() -> Arc<ApiTableCatalog> {
    // SAFETY: every field in all six API tables is a raw pointer, an optional
    // function pointer, or an integer newtype, and therefore accepts zero as a
    // valid inert test value. Production catalogs are assembled by the FFI crate.
    let tables = unsafe { std::mem::zeroed::<ApiTables>() };
    Arc::new(ApiTableCatalog::from_tables(tables).expect("test API tables should fit the ABI"))
}

fn addon_root() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        PathBuf::from(r"C:\NexusTests\addons")
    }
    #[cfg(not(target_os = "windows"))]
    {
        PathBuf::from("/nexus-tests/addons")
    }
}

fn addon_path(name: &str) -> PathBuf {
    addon_root().join(name)
}

fn ready_owner(
    manager: &mut AddonManager<FakePlatform, RecordingCleaner>,
    candidate: &DiscoveredDll,
) -> OwnerToken {
    manager.refresh_discovery().expect("inert scan should work");
    // SAFETY: the injected platform runs only controlled fixture callbacks and
    // all returned memory is backed by leaked test allocations.
    match unsafe { manager.inspect(candidate.path(), ActivationRequest::Startup) }
        .expect("fixture inspection should work")
    {
        InspectOutcome::Ready(owner) => owner,
        InspectOutcome::Blocked { reason, .. } => {
            panic!("fixture was unexpectedly blocked: {reason:?}")
        }
    }
}

#[test]
fn native_activation_fails_closed_without_a_populated_render_session_catalog() {
    let _serial = lock(&TEST_LOCK);
    let fixture = Fixture::new(AddonDefinitionFlags::NONE);
    let mut manager = fixture.manager_without_api_tables();
    let owner = ready_owner(&mut manager, &fixture.candidate);

    // SAFETY: the failure is required before the controlled native fixture
    // callback can be reached.
    let result = unsafe { manager.activate(owner) };

    assert!(matches!(
        result,
        Err(ManagerError::Host(HostIssue::ApiTable))
    ));
    assert!(!lock(&fixture.native_events).contains(&NativeEvent::LoadCallback));
}

#[test]
fn ownership_publication_failure_rolls_back_discovery_and_allows_clean_retry() {
    let _serial = lock(&TEST_LOCK);
    let fixture = Fixture::new(AddonDefinitionFlags::NONE);
    let ownership = Arc::new(AddressOwnershipIndex::new());
    let blocker = OwnerToken {
        signature: 0xDEAD_BEEF,
        generation: 1,
    };
    ownership
        .publish(
            blocker,
            fixture.resolver.0.start(),
            fixture.resolver.0.len(),
            Arc::new(CallbackGate::open()),
        )
        .expect("blocking fixture range should publish");
    let mut manager =
        fixture.manager_with_ownership(true, LoadedModuleAddressResolver, Arc::clone(&ownership));
    manager.refresh_discovery().expect("scan should succeed");

    // SAFETY: the injected platform exposes only leaked fixture memory.
    let first = unsafe { manager.inspect(fixture.candidate.path(), ActivationRequest::Startup) };
    assert!(matches!(first, Err(ManagerError::OwnershipIndex(_))));
    assert_eq!(ownership.mapped_count(), 1);
    assert_eq!(
        *lock(&fixture.native_events),
        [NativeEvent::LoadLibrary, NativeEvent::FreeLibrary]
    );

    assert!(ownership.retire(blocker));
    // SAFETY: the same controlled fixture is retried after the conflict left.
    let owner =
        match unsafe { manager.inspect(fixture.candidate.path(), ActivationRequest::Startup) }
            .expect("retry should inspect")
        {
            InspectOutcome::Ready(owner) => owner,
            InspectOutcome::Blocked { reason, .. } => panic!("unexpected policy block: {reason:?}"),
        };
    assert_eq!(owner.generation, 2);
    // SAFETY: fixture callbacks only record lifecycle events.
    unsafe { manager.activate(owner) }.expect("retry should activate");
    manager
        .request_unload(owner, UnloadReason::Shutdown)
        .expect("shutdown should close admission");
    manager
        .drain(owner, Duration::ZERO)
        .expect("fixture should drain");
    // SAFETY: fixture unload only records an event.
    unsafe { manager.invoke_native_unload(owner) }.expect("unload should run");
    manager.finish_unload(owner).expect("release should finish");
    assert_eq!(ownership.mapped_count(), 0);
}

#[test]
fn watcher_is_inert_and_full_lifecycle_drains_before_release() {
    let _serial = lock(&TEST_LOCK);
    let fixture = Fixture::new(AddonDefinitionFlags::NONE);
    let mut manager = fixture.manager_with_resolver(true, LoadedModuleAddressResolver);

    let impacts = manager
        .apply_directory_event(DirectoryEvent::Upsert(fixture.candidate.clone()))
        .expect("watcher event should apply");
    assert_eq!(impacts[0].kind(), DirectoryChangeKind::Added);
    assert!(
        lock(&fixture.native_events).is_empty(),
        "watcher must never load a DLL"
    );

    let owner = ready_owner(&mut manager, &fixture.candidate);
    let ownership = manager.address_ownership_index();
    assert_eq!(manager.owner_for_address(fixture.probe), Some(owner));
    assert!(ownership.is_current_owner(owner));
    // SAFETY: this fixture load callback ignores the injected stable API pointer.
    unsafe { manager.activate(owner) }.expect("activation should work");
    assert_eq!(
        manager
            .snapshot(fixture.candidate.path())
            .map(|s| s.state()),
        Some(ManagerState::Active)
    );

    let changed = DiscoveredDll::new(
        fixture.candidate.path().as_path(),
        BinaryRevision::from_bytes([2; 16]),
        129,
    )
    .expect("changed candidate should be valid");
    let impact = manager
        .apply_directory_event(DirectoryEvent::Upsert(changed))
        .expect("change should apply")
        .remove(0);
    assert_eq!(
        impact.recommendation(),
        DirectoryRecommendation::HotReload(owner)
    );

    let staged = AbsoluteDllPath::new(addon_path("fixture-update.dll"))
        .expect("staged path should be valid");
    assert_eq!(
        manager
            .plan_update(owner, staged)
            .expect("update should plan")
            .timing(),
        UpdateTiming::RuntimeHotReload
    );
    assert_eq!(
        manager
            .plan_uninstall(owner)
            .expect("uninstall should plan")
            .timing(),
        UninstallTiming::RuntimeAfterUnload
    );
    assert_eq!(
        manager
            .set_enabled(owner.signature, false)
            .expect("config should update"),
        EnableEffect::UnloadAvailable(owner)
    );

    let gate = manager
        .callback_gate(owner)
        .expect("host gate should exist");
    let guard = gate
        .try_enter()
        .expect("active gate should admit callbacks");
    manager
        .request_unload(owner, UnloadReason::Runtime)
        .expect("unload should close ingress");
    assert!(!ownership.is_current_owner(owner));
    assert_eq!(ownership.owner_for_address(fixture.probe), Some(owner));
    assert!(matches!(
        manager.drain(owner, Duration::ZERO),
        Err(ManagerError::Host(_))
    ));
    drop(guard);
    manager
        .drain(owner, Duration::ZERO)
        .expect("drain retry should complete");
    // SAFETY: the injected unload callback only records an event.
    unsafe { manager.invoke_native_unload(owner) }.expect("native unload should work");
    manager.finish_unload(owner).expect("release should work");

    assert_eq!(manager.owner_for_address(fixture.probe), None);
    assert_eq!(ownership.mapped_count(), 0);
    assert_eq!(
        manager
            .snapshot(fixture.candidate.path())
            .map(|s| s.state()),
        Some(ManagerState::Unloaded)
    );
    let phases: Vec<_> = lock(&fixture.cleanup_events)
        .iter()
        .map(|(called_owner, phase)| {
            assert_eq!(*called_owner, owner);
            *phase
        })
        .collect();
    assert_eq!(phases, CleanupPhase::ORDER);
    assert_eq!(
        *lock(&fixture.native_events),
        [
            NativeEvent::LoadLibrary,
            NativeEvent::LoadCallback,
            NativeEvent::UnloadCallback,
            NativeEvent::FreeLibrary,
        ]
    );
}

#[test]
fn hot_reload_advances_generation_only_after_old_release() {
    let _serial = lock(&TEST_LOCK);
    let fixture = Fixture::new(AddonDefinitionFlags::NONE);
    let mut manager = fixture.manager();
    let first = ready_owner(&mut manager, &fixture.candidate);
    // SAFETY: controlled fixture callback.
    unsafe { manager.activate(first) }.expect("first activation should work");

    // SAFETY: both generations use only the injected fake platform and callbacks.
    let second =
        unsafe { manager.hot_reload(first, Duration::ZERO) }.expect("hot reload should complete");
    assert_eq!(second.signature, first.signature);
    assert_eq!(second.generation, first.generation + 1);
    assert_eq!(manager.active_owner(second.signature), Some(second));
    assert_eq!(manager.owner_for_address(fixture.probe), Some(second));
    assert_eq!(
        lock(&fixture.native_events)
            .iter()
            .filter(|event| **event == NativeEvent::LoadLibrary)
            .count(),
        2
    );

    manager
        .request_unload(second, UnloadReason::Runtime)
        .expect("second generation should unload");
    manager
        .drain(second, Duration::ZERO)
        .expect("second generation should drain");
    // SAFETY: controlled fixture callback.
    unsafe { manager.invoke_native_unload(second) }.expect("unload should work");
    manager.finish_unload(second).expect("release should work");
}

#[test]
fn failed_release_retains_address_ownership_and_retries_explicitly() {
    let _serial = lock(&TEST_LOCK);
    let fixture = Fixture::with_free_failures(AddonDefinitionFlags::NONE, 1);
    let mut manager = fixture.manager();
    let owner = ready_owner(&mut manager, &fixture.candidate);
    // SAFETY: controlled fixture callback.
    unsafe { manager.activate(owner) }.expect("activation should work");
    manager
        .request_unload(owner, UnloadReason::Runtime)
        .expect("unload should be accepted");
    manager
        .drain(owner, Duration::ZERO)
        .expect("callbacks should drain");
    // SAFETY: controlled fixture callback.
    unsafe { manager.invoke_native_unload(owner) }.expect("unload callback should work");

    assert!(matches!(
        manager.finish_unload(owner),
        Err(ManagerError::ReleaseFailed { retryable: true })
    ));
    assert_eq!(
        manager
            .snapshot(fixture.candidate.path())
            .map(|snapshot| snapshot.state()),
        Some(ManagerState::ReleaseFailed)
    );
    assert_eq!(manager.owner_for_address(fixture.probe), Some(owner));

    manager
        .retry_release(fixture.candidate.path())
        .expect("ordinary platform failure should be retryable");
    assert_eq!(manager.owner_for_address(fixture.probe), None);
    assert_eq!(manager.active_owner(owner.signature), None);
    assert_eq!(
        lock(&fixture.native_events)
            .iter()
            .filter(|event| **event == NativeEvent::FreeLibrary)
            .count(),
        2
    );
}

#[test]
fn locked_policy_plans_restart_and_still_allows_shutdown_cleanup() {
    let _serial = lock(&TEST_LOCK);
    let fixture = Fixture::new(AddonDefinitionFlags::DISABLE_HOT_LOADING);
    let mut manager = fixture.manager();
    let owner = ready_owner(&mut manager, &fixture.candidate);
    // SAFETY: controlled fixture callback.
    unsafe { manager.activate(owner) }.expect("activation should work");

    let changed = DiscoveredDll::new(
        fixture.candidate.path().as_path(),
        BinaryRevision::from_bytes([3; 16]),
        130,
    )
    .expect("changed candidate should be valid");
    let impact = manager
        .apply_directory_event(DirectoryEvent::Upsert(changed))
        .expect("change should apply")
        .remove(0);
    assert_eq!(
        impact.recommendation(),
        DirectoryRecommendation::RestartRequired(owner)
    );
    assert!(matches!(
        manager.request_unload(owner, UnloadReason::Runtime),
        Err(ManagerError::Host(_))
    ));
    assert_eq!(
        manager
            .plan_update(
                owner,
                AbsoluteDllPath::new(addon_path("locked-update.dll"))
                    .expect("staged path should be valid"),
            )
            .expect("update should plan")
            .timing(),
        UpdateTiming::RestartRequired
    );
    assert_eq!(
        manager
            .plan_uninstall(owner)
            .expect("uninstall should plan")
            .timing(),
        UninstallTiming::RestartRequired
    );
    assert!(matches!(
        // SAFETY: policy rejects before executing either native transition.
        unsafe { manager.hot_reload(owner, Duration::ZERO) },
        Err(ManagerError::Policy(PolicyReason::HotLoadingLocked))
    ));

    manager
        .request_unload(owner, UnloadReason::Shutdown)
        .expect("shutdown must override runtime lock");
    manager
        .drain(owner, Duration::ZERO)
        .expect("shutdown cleanup should drain");
    // SAFETY: controlled fixture callback.
    unsafe { manager.invoke_native_unload(owner) }.expect("shutdown unload should work");
    manager
        .finish_unload(owner)
        .expect("shutdown release should work");
}

#[test]
fn launch_only_second_attempt_is_blocked_and_released() {
    let _serial = lock(&TEST_LOCK);
    let fixture = Fixture::new(AddonDefinitionFlags::LAUNCH_ONLY);
    let mut manager = fixture.manager();
    let owner = ready_owner(&mut manager, &fixture.candidate);
    // SAFETY: controlled fixture callback.
    unsafe { manager.activate(owner) }.expect("initial launch-only activation should work");
    manager
        .request_unload(owner, UnloadReason::Runtime)
        .expect("launch-only does not itself forbid runtime unload");
    manager
        .drain(owner, Duration::ZERO)
        .expect("launch-only generation should drain");
    // SAFETY: controlled fixture callback.
    unsafe { manager.invoke_native_unload(owner) }.expect("unload should work");
    manager.finish_unload(owner).expect("release should work");

    // SAFETY: inspection uses the fake platform; policy releases before activation.
    let outcome = unsafe { manager.inspect(fixture.candidate.path(), ActivationRequest::Runtime) }
        .expect("blocked inspection should remain a successful policy outcome");
    assert_eq!(
        outcome,
        InspectOutcome::Blocked {
            signature: owner.signature,
            reason: PolicyReason::LaunchOnly,
        }
    );
    assert_eq!(
        manager
            .snapshot(fixture.candidate.path())
            .map(|s| s.state()),
        Some(ManagerState::PolicyBlocked)
    );
    assert_eq!(
        lock(&fixture.native_events).last(),
        Some(&NativeEvent::FreeLibrary)
    );
}

#[test]
fn redacted_debug_surfaces_never_include_real_paths_or_addresses() {
    let _serial = lock(&TEST_LOCK);
    let fixture = Fixture::new(AddonDefinitionFlags::NONE);
    let mut manager = fixture.manager();
    manager.refresh_discovery().expect("inert scan should work");
    let snapshot = manager
        .snapshot(fixture.candidate.path())
        .expect("snapshot should exist");
    let rendered = format!("{snapshot:?} {:?}", fixture.resolver.0);
    assert!(!rendered.contains("NexusTests"));
    assert!(!rendered.contains("nexus-tests"));
    assert!(!rendered.contains(&fixture.probe.get().to_string()));
    assert!(rendered.contains("<redacted>"));
}
