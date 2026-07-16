//! Lifecycle and failure-ordering tests for the injected loader platform.

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
    ADDON_DEFINITION_EXPORT, AddonLoader, LoadError, LoaderPlatform, ModuleBounds, ModuleError,
    ModuleHandle, ModuleImage, ModuleState, PanicBoundary, PlatformError, PlatformOperation,
};
use nexus_host::{MetadataLimits, ModuleMemory, ModuleReadError, validate_and_copy_definition};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Event {
    LoadLibrary,
    InspectImage,
    ResolveDefinition,
    InspectCode,
    LoadCallback,
    UnloadCallback,
    HostCleanup,
    FreeLibrary,
}

type SharedEvents = Arc<Mutex<Vec<Event>>>;
type CallbackEventSlot = Mutex<Option<SharedEvents>>;

static TEST_LOCK: Mutex<()> = Mutex::new(());
static EXPORTED_DEFINITION: AtomicPtr<AddonDefinitionV1> = AtomicPtr::new(std::ptr::null_mut());
static CALLBACK_EVENTS: OnceLock<CallbackEventSlot> = OnceLock::new();
static PANIC_PAYLOAD_DROPS: AtomicUsize = AtomicUsize::new(0);

struct PanicOnDrop;

impl Drop for PanicOnDrop {
    fn drop(&mut self) {
        PANIC_PAYLOAD_DROPS.fetch_add(1, Ordering::SeqCst);
        panic!("panic payload destructor ran");
    }
}

fn panic_with_drop_payload() -> ! {
    std::panic::panic_any(PanicOnDrop)
}

unsafe extern "C" fn definition_export() -> *mut AddonDefinitionV1 {
    EXPORTED_DEFINITION.load(Ordering::Acquire)
}

unsafe extern "C" fn load_callback(_api: *mut AddonApi) {
    record_callback(Event::LoadCallback);
}

unsafe extern "C" fn unload_callback() {
    record_callback(Event::UnloadCallback);
}

fn callback_events() -> &'static CallbackEventSlot {
    CALLBACK_EVENTS.get_or_init(|| Mutex::new(None))
}

fn record_callback(event: Event) {
    let events = lock(callback_events()).clone();
    if let Some(events) = events {
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
    panic_payload_read: bool,
}

impl ModuleMemory for FakeMemory {
    fn read_bounded(
        &self,
        address: NonNull<c_char>,
        maximum_bytes: usize,
    ) -> Result<&[u8], ModuleReadError> {
        if self.panic_payload_read {
            panic_with_drop_payload();
        }
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
    events: Arc<Mutex<Vec<Event>>>,
    bounds: ModuleBounds,
    memory: FakeMemory,
    executable: HashSet<usize>,
    missing_export: bool,
    panic_resolve: bool,
    panic_free: bool,
    panic_payload_free: bool,
    free_failures: AtomicUsize,
}

// SAFETY: fake handles are stable non-zero tokens, all returned memory borrows
// point into leaked test allocations, and release is recorded exactly as the
// production trait requires.
unsafe impl LoaderPlatform for FakePlatform {
    type Memory = FakeMemory;

    unsafe fn load_library(
        &self,
        _path: &nexus_addon_loader::AbsoluteDllPath,
    ) -> Result<ModuleHandle, PlatformError> {
        lock(&self.events).push(Event::LoadLibrary);
        let raw = NonZeroUsize::new(1).ok_or(PlatformError::LoadLibrary)?;
        Ok(ModuleHandle::from_non_zero(raw))
    }

    unsafe fn resolve_definition_export(
        &self,
        _module: ModuleHandle,
        export: &'static CStr,
    ) -> Result<GetAddonDefinitionV1, PlatformError> {
        lock(&self.events).push(Event::ResolveDefinition);
        assert_eq!(export, ADDON_DEFINITION_EXPORT);
        assert!(!self.panic_resolve, "injected platform panic");
        if self.missing_export {
            Err(PlatformError::MissingDefinitionExport)
        } else {
            Ok(definition_export)
        }
    }

    unsafe fn module_image(
        &self,
        _module: ModuleHandle,
    ) -> Result<ModuleImage<Self::Memory>, PlatformError> {
        lock(&self.events).push(Event::InspectImage);
        Ok(ModuleImage::new(self.bounds, self.memory.clone()))
    }

    unsafe fn is_executable_address(
        &self,
        _module: ModuleHandle,
        address: NonZeroUsize,
    ) -> Result<bool, PlatformError> {
        lock(&self.events).push(Event::InspectCode);
        Ok(self.executable.contains(&address.get()))
    }

    unsafe fn free_library(&self, _module: ModuleHandle) -> Result<(), PlatformError> {
        lock(&self.events).push(Event::FreeLibrary);
        if self.panic_payload_free {
            panic_with_drop_payload();
        }
        assert!(!self.panic_free, "injected release panic");
        self.free_failures
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                remaining.checked_sub(1)
            })
            .map_or(Ok(()), |_previous| Err(PlatformError::FreeLibrary))
    }
}

#[derive(Clone, Copy, Default)]
struct FakeOptions {
    missing_export: bool,
    panic_resolve: bool,
    panic_free: bool,
    panic_payload_free: bool,
    panic_payload_read: bool,
    null_definition: bool,
    invalid_load_callback: bool,
    free_failures: usize,
}

struct Fixture {
    platform: Arc<FakePlatform>,
    events: Arc<Mutex<Vec<Event>>>,
}

impl Fixture {
    fn new(options: FakeOptions) -> Self {
        let name = Box::leak(Box::new(*b"Fixture\0"));
        let author = Box::leak(Box::new(*b"Nexus\0"));
        let description = Box::leak(Box::new(*b"Safe fixture\0"));
        let definition = Box::leak(Box::new(AddonDefinitionV1 {
            signature: 0x1234_5678,
            api_version: 1,
            name: name.as_ptr().cast(),
            version: Version::new(1, 2, 3, 4),
            author: author.as_ptr().cast(),
            description: description.as_ptr().cast(),
            load: Some(load_callback),
            unload: Some(unload_callback),
            flags: AddonDefinitionFlags::NONE,
            provider: UpdateProvider::NONE,
            update_link: null(),
        }));
        EXPORTED_DEFINITION.store(
            if options.null_definition {
                std::ptr::null_mut()
            } else {
                definition
            },
            Ordering::Release,
        );

        // SAFETY: the definition allocation is leaked for the entire test
        // process and the byte view has exactly the ABI object's size.
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

        let code_addresses = [
            definition_export as *const () as usize,
            load_callback as *const () as usize,
            unload_callback as *const () as usize,
        ];
        let lowest_data = segments
            .iter()
            .map(|segment| segment.start)
            .min()
            .expect("fixture has data segments");
        let highest_data = segments
            .iter()
            .map(|segment| segment.start + segment.bytes.len())
            .max()
            .expect("fixture has data segments");
        let lowest = code_addresses.iter().copied().fold(lowest_data, usize::min);
        let highest = code_addresses
            .iter()
            .copied()
            .map(|address| address + 1)
            .fold(highest_data, usize::max);
        let base = NonZeroUsize::new(lowest).expect("test addresses are non-zero");
        let bounds = ModuleBounds::new(base, highest - lowest).expect("fixture bounds are valid");

        let mut executable = HashSet::from(code_addresses);
        if options.invalid_load_callback {
            executable.remove(&(load_callback as *const () as usize));
        }
        let events = Arc::new(Mutex::new(Vec::new()));
        *lock(callback_events()) = Some(Arc::clone(&events));
        let platform = Arc::new(FakePlatform {
            events: Arc::clone(&events),
            bounds,
            memory: FakeMemory {
                segments,
                panic_payload_read: options.panic_payload_read,
            },
            executable,
            missing_export: options.missing_export,
            panic_resolve: options.panic_resolve,
            panic_free: options.panic_free,
            panic_payload_free: options.panic_payload_free,
            free_failures: AtomicUsize::new(options.free_failures),
        });
        Self { platform, events }
    }

    fn loader(&self) -> AddonLoader<FakePlatform> {
        AddonLoader::from_shared(Arc::clone(&self.platform))
    }

    fn events(&self) -> Vec<Event> {
        lock(&self.events).clone()
    }
}

fn addon_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        PathBuf::from(r"C:\NexusTests\fixture.dll")
    }
    #[cfg(not(target_os = "windows"))]
    {
        PathBuf::from("/nexus-tests/fixture.dll")
    }
}

fn load_fixture(fixture: &Fixture) -> nexus_addon_loader::AddonModule<FakePlatform> {
    // SAFETY: this injected platform executes only the controlled fixture
    // callbacks and returns memory backed by leaked test allocations.
    unsafe {
        fixture
            .loader()
            .load(addon_path(), MetadataLimits::default())
            .expect("fixture module should load")
    }
}

#[test]
fn validated_module_exposes_owned_metadata_and_live_definition() {
    let _serial = lock(&TEST_LOCK);
    let fixture = Fixture::new(FakeOptions::default());
    let module = load_fixture(&fixture);

    assert_eq!(module.state(), ModuleState::Inspected);
    assert_eq!(module.owned_definition().name(), "Fixture");
    assert_eq!(module.owned_definition().author(), "Nexus");
    assert!(validate_and_copy_definition(&module, MetadataLimits::default()).is_ok());
    assert!(module.image_size() >= size_of::<AddonDefinitionV1>());

    module.release().expect("unactivated module can release");
    assert_eq!(fixture.events().last(), Some(&Event::FreeLibrary));
}

#[test]
fn missing_export_releases_the_provisional_module_and_redacts_path() {
    let _serial = lock(&TEST_LOCK);
    let fixture = Fixture::new(FakeOptions {
        missing_export: true,
        ..FakeOptions::default()
    });
    // SAFETY: the fake platform is controlled and does not execute a DLL.
    let result = unsafe {
        fixture
            .loader()
            .load(addon_path(), MetadataLimits::default())
    };
    let Err(error) = result else {
        panic!("missing export should fail");
    };

    assert!(matches!(
        error,
        LoadError::Platform {
            operation: PlatformOperation::ResolveDefinitionExport,
            source: PlatformError::MissingDefinitionExport,
        }
    ));
    assert!(!error.to_string().contains("NexusTests"));
    assert_eq!(fixture.events().last(), Some(&Event::FreeLibrary));
}

#[test]
fn provisional_release_forgets_a_panicking_payload_destructor() {
    let _serial = lock(&TEST_LOCK);
    PANIC_PAYLOAD_DROPS.store(0, Ordering::SeqCst);
    let fixture = Fixture::new(FakeOptions {
        missing_export: true,
        panic_payload_free: true,
        ..FakeOptions::default()
    });

    // SAFETY: the fake platform is controlled and does not execute a DLL.
    let result = unsafe {
        fixture
            .loader()
            .load(addon_path(), MetadataLimits::default())
    };
    let Err(error) = result else {
        panic!("missing export should fail");
    };

    assert!(matches!(
        error,
        LoadError::Platform {
            operation: PlatformOperation::ResolveDefinitionExport,
            source: PlatformError::MissingDefinitionExport,
        }
    ));
    assert_eq!(PANIC_PAYLOAD_DROPS.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.events().last(), Some(&Event::FreeLibrary));
}

#[test]
fn relative_path_is_rejected_before_the_platform_is_called() {
    let _serial = lock(&TEST_LOCK);
    let fixture = Fixture::new(FakeOptions::default());
    // SAFETY: path validation rejects the request before any native operation.
    let result = unsafe {
        fixture
            .loader()
            .load("relative.dll", MetadataLimits::default())
    };
    let Err(error) = result else {
        panic!("relative path should fail");
    };

    assert!(matches!(
        error,
        LoadError::Path(nexus_addon_loader::PathPolicyError::NotAbsolute)
    ));
    assert!(fixture.events().is_empty());
}

#[test]
fn null_definition_releases_the_provisional_module() {
    let _serial = lock(&TEST_LOCK);
    let fixture = Fixture::new(FakeOptions {
        null_definition: true,
        ..FakeOptions::default()
    });
    // SAFETY: the controlled null export result is the behavior under test.
    let result = unsafe {
        fixture
            .loader()
            .load(addon_path(), MetadataLimits::default())
    };
    let Err(error) = result else {
        panic!("null definition should fail");
    };

    assert!(matches!(error, LoadError::NullDefinition));
    assert_eq!(fixture.events().last(), Some(&Event::FreeLibrary));
}

#[test]
fn definition_memory_boundary_forgets_a_panicking_payload_destructor() {
    let _serial = lock(&TEST_LOCK);
    PANIC_PAYLOAD_DROPS.store(0, Ordering::SeqCst);
    let fixture = Fixture::new(FakeOptions {
        panic_payload_read: true,
        ..FakeOptions::default()
    });

    // SAFETY: the fake memory adapter panic is the behavior under test.
    let result = unsafe {
        fixture
            .loader()
            .load(addon_path(), MetadataLimits::default())
    };
    let Err(error) = result else {
        panic!("definition memory panic should fail");
    };

    assert!(matches!(
        error,
        LoadError::RustPanic(PanicBoundary::DefinitionValidation)
    ));
    assert_eq!(PANIC_PAYLOAD_DROPS.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.events().last(), Some(&Event::FreeLibrary));
}

#[test]
fn platform_panic_is_contained_and_the_provisional_module_is_released() {
    let _serial = lock(&TEST_LOCK);
    let fixture = Fixture::new(FakeOptions {
        panic_resolve: true,
        ..FakeOptions::default()
    });
    // SAFETY: the fake platform panic is the behavior under test.
    let result = unsafe {
        fixture
            .loader()
            .load(addon_path(), MetadataLimits::default())
    };
    let Err(error) = result else {
        panic!("resolver panic should fail");
    };

    assert!(matches!(
        error,
        LoadError::RustPanic(PanicBoundary::ResolveDefinitionExport)
    ));
    assert_eq!(fixture.events().last(), Some(&Event::FreeLibrary));
}

#[test]
fn invalid_callback_address_is_rejected_before_activation() {
    let _serial = lock(&TEST_LOCK);
    let fixture = Fixture::new(FakeOptions {
        invalid_load_callback: true,
        ..FakeOptions::default()
    });
    // SAFETY: the fake code-address result is the behavior under test.
    let result = unsafe {
        fixture
            .loader()
            .load(addon_path(), MetadataLimits::default())
    };
    let Err(error) = result else {
        panic!("invalid callback should fail");
    };

    assert!(matches!(error, LoadError::LoadCallbackNotExecutable));
    assert_eq!(fixture.events().last(), Some(&Event::FreeLibrary));
}

#[test]
fn release_occurs_after_unload_callback_and_host_cleanup() {
    let _serial = lock(&TEST_LOCK);
    let fixture = Fixture::new(FakeOptions::default());
    let mut module = load_fixture(&fixture);

    // SAFETY: the fixture ignores the controlled dangling opaque API pointer.
    unsafe {
        module
            .activate(NonNull::dangling())
            .expect("load callback should return");
    }
    module.request_shutdown().expect("shutdown should start");
    module
        .wait_for_callbacks(Duration::ZERO)
        .expect("no callbacks remain");
    // SAFETY: no external callbacks exist in this fixture.
    unsafe {
        module
            .invoke_unload()
            .expect("unload callback should return");
    }
    module
        .complete_host_cleanup(|| {
            lock(&fixture.events).push(Event::HostCleanup);
            Ok::<(), ()>(())
        })
        .expect("host cleanup should complete");
    module.release().expect("clean module should release");

    let events = fixture.events();
    let unload = events
        .iter()
        .position(|event| *event == Event::UnloadCallback)
        .expect("unload callback event exists");
    let cleanup = events
        .iter()
        .position(|event| *event == Event::HostCleanup)
        .expect("cleanup event exists");
    let release = events
        .iter()
        .position(|event| *event == Event::FreeLibrary)
        .expect("release event exists");
    assert!(unload < cleanup && cleanup < release);
}

#[test]
fn callback_guard_blocks_drain_and_release_until_it_leaves() {
    let _serial = lock(&TEST_LOCK);
    let fixture = Fixture::new(FakeOptions::default());
    let mut module = load_fixture(&fixture);
    // SAFETY: the fixture ignores the controlled dangling opaque API pointer.
    unsafe {
        module
            .activate(NonNull::dangling())
            .expect("activation should succeed");
    }
    let gate = module.callback_gate();
    let callback = gate.try_enter().expect("callback admission is open");
    module.request_shutdown().expect("shutdown should start");
    assert!(matches!(
        module.wait_for_callbacks(Duration::ZERO),
        Err(ModuleError::DrainTimedOut)
    ));
    assert!(!fixture.events().contains(&Event::FreeLibrary));

    drop(callback);
    module
        .wait_for_callbacks(Duration::ZERO)
        .expect("callback should now be drained");
    // SAFETY: every callback guard has left.
    unsafe {
        module.invoke_unload().expect("unload should return");
    }
    module
        .complete_host_cleanup(|| Ok::<(), ()>(()))
        .expect("cleanup should complete");
    module.release().expect("release should succeed");
}

#[test]
fn unactivated_release_closes_and_checks_callback_admission() {
    let _serial = lock(&TEST_LOCK);
    let fixture = Fixture::new(FakeOptions::default());
    let module = load_fixture(&fixture);
    let gate = module.callback_gate();
    let callback = gate.try_enter().expect("callback admission is open");

    let failure = module
        .release()
        .expect_err("an admitted callback must block release");
    assert!(matches!(
        failure.error(),
        ModuleError::CallbackDrainIncomplete
    ));
    assert!(failure.is_retryable());
    assert!(gate.try_enter().is_none());
    assert!(!fixture.events().contains(&Event::FreeLibrary));

    drop(callback);
    let (module, _error) = failure.into_parts();
    module.release().expect("drained release should succeed");
}

#[test]
fn cleanup_failure_and_panic_are_redacted_and_retryable() {
    let _serial = lock(&TEST_LOCK);
    let fixture = Fixture::new(FakeOptions::default());
    let mut module = load_fixture(&fixture);
    // SAFETY: the controlled fixture callbacks have no external registrations.
    unsafe {
        module
            .activate(NonNull::dangling())
            .expect("activation should succeed");
    }
    module.request_shutdown().expect("shutdown should start");
    module
        .wait_for_callbacks(Duration::ZERO)
        .expect("callbacks should drain");
    // SAFETY: every controlled callback has left.
    unsafe {
        module.invoke_unload().expect("unload should return");
    }

    let failure = module.complete_host_cleanup(|| Err::<(), _>("user-controlled detail"));
    assert!(matches!(failure, Err(ModuleError::HostCleanupFailed)));
    assert_eq!(module.state(), ModuleState::UnloadCallbackComplete);
    PANIC_PAYLOAD_DROPS.store(0, Ordering::SeqCst);
    let panic = module.complete_host_cleanup(|| -> Result<(), ()> {
        panic_with_drop_payload();
    });
    assert!(matches!(
        panic,
        Err(ModuleError::RustPanic(PanicBoundary::HostCleanup))
    ));
    assert_eq!(PANIC_PAYLOAD_DROPS.load(Ordering::SeqCst), 0);
    assert_eq!(module.state(), ModuleState::UnloadCallbackComplete);

    let destructor_panic = module.complete_host_cleanup(|| Err::<(), _>(PanicOnDrop));
    assert!(matches!(
        destructor_panic,
        Err(ModuleError::HostCleanupFailed)
    ));
    assert_eq!(PANIC_PAYLOAD_DROPS.load(Ordering::SeqCst), 1);
    assert_eq!(module.state(), ModuleState::UnloadCallbackComplete);

    module
        .complete_host_cleanup(|| Ok::<(), ()>(()))
        .expect("cleanup retry should succeed");
    module.release().expect("release should succeed");
}

#[test]
fn failed_free_library_returns_the_live_module_for_retry() {
    let _serial = lock(&TEST_LOCK);
    let fixture = Fixture::new(FakeOptions {
        free_failures: 1,
        ..FakeOptions::default()
    });
    let module = load_fixture(&fixture);
    let failure = module.release().expect_err("first release should fail");
    assert!(matches!(
        failure.error(),
        ModuleError::Platform {
            operation: PlatformOperation::FreeLibrary,
            source: PlatformError::FreeLibrary,
        }
    ));
    let (module, _error) = failure.into_parts();
    module.release().expect("release retry should succeed");
    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| **event == Event::FreeLibrary)
            .count(),
        2
    );
}

#[test]
fn dropping_a_release_failure_never_retries_implicitly() {
    let _serial = lock(&TEST_LOCK);
    let fixture = Fixture::new(FakeOptions {
        free_failures: 1,
        ..FakeOptions::default()
    });
    let module = load_fixture(&fixture);
    let failure = module.release().expect_err("release should fail once");
    drop(failure);

    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| **event == Event::FreeLibrary)
            .count(),
        1
    );
}

#[test]
fn release_panic_pins_an_uncertain_token_instead_of_retrying() {
    let _serial = lock(&TEST_LOCK);
    let fixture = Fixture::new(FakeOptions {
        panic_free: true,
        ..FakeOptions::default()
    });
    let module = load_fixture(&fixture);
    let failure = module
        .release()
        .expect_err("release panic should be caught");

    assert!(matches!(
        failure.error(),
        ModuleError::RustPanic(PanicBoundary::FreeLibrary)
    ));
    assert!(!failure.is_retryable());
    let (module, _error) = failure.into_parts();
    assert_eq!(module.state(), ModuleState::ReleaseUncertain);
    drop(module);
    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| **event == Event::FreeLibrary)
            .count(),
        1
    );
}

#[test]
fn implicit_module_release_forgets_a_panicking_payload_destructor() {
    let _serial = lock(&TEST_LOCK);
    PANIC_PAYLOAD_DROPS.store(0, Ordering::SeqCst);
    let fixture = Fixture::new(FakeOptions {
        panic_payload_free: true,
        ..FakeOptions::default()
    });
    let module = load_fixture(&fixture);

    drop(module);

    assert_eq!(PANIC_PAYLOAD_DROPS.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| **event == Event::FreeLibrary)
            .count(),
        1
    );
}

#[test]
fn dropping_an_active_module_pins_it_instead_of_freeing_code() {
    let _serial = lock(&TEST_LOCK);
    let fixture = Fixture::new(FakeOptions::default());
    let mut module = load_fixture(&fixture);
    // SAFETY: the fixture callback is controlled and returns immediately.
    unsafe {
        module
            .activate(NonNull::dangling())
            .expect("activation should succeed");
    }
    drop(module);

    assert!(!fixture.events().contains(&Event::FreeLibrary));
}
