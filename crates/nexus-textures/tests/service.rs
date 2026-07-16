//! Concurrency, lifetime, queue, override, and callback behavior tests.

use std::collections::HashMap;
use std::ffi::c_void;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};

use nexus_textures::{
    BackendFailure, DecodeLimits, DecodedImage, DirectoryOverrides, DownloadTarget, Downloader,
    GpuBackend, GpuTexture, ImageDecoder, LoadOptions, ModuleHandle, NoOverrides, OverrideProvider,
    OwnerGeneration, RequestOutcome, RequestOwner, ResourceProvider, TextureCallback,
    TextureConfig, TextureError, TextureService, TextureSource,
};

#[derive(Default)]
struct FakeDecoder {
    calls: AtomicUsize,
    seen: Mutex<Vec<Vec<u8>>>,
    invalid: AtomicBool,
}

impl ImageDecoder for FakeDecoder {
    fn decode(
        &self,
        encoded: &[u8],
        _limits: DecodeLimits,
    ) -> Result<DecodedImage, BackendFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen
            .lock()
            .expect("fake decoder observation lock should remain usable")
            .push(encoded.to_vec());
        if self.invalid.load(Ordering::SeqCst) {
            return Ok(DecodedImage {
                width: 0,
                height: 1,
                rgba8: Vec::new(),
            });
        }
        let marker = encoded.first().copied().unwrap_or_default();
        Ok(DecodedImage {
            width: 1,
            height: 1,
            rgba8: vec![marker, marker, marker, u8::MAX],
        })
    }
}

struct BlockingDecoder {
    calls: AtomicUsize,
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
}

impl ImageDecoder for BlockingDecoder {
    fn decode(
        &self,
        encoded: &[u8],
        _limits: DecodeLimits,
    ) -> Result<DecodedImage, BackendFailure> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.entered.wait();
            self.release.wait();
        }
        let marker = encoded.first().copied().unwrap_or_default();
        Ok(DecodedImage {
            width: 1,
            height: 1,
            rgba8: vec![marker, marker, marker, u8::MAX],
        })
    }
}

struct FakeGpuTexture {
    address: NonZeroUsize,
    drops: Arc<AtomicUsize>,
}

impl GpuTexture for FakeGpuTexture {
    fn srv_address(&self) -> NonZeroUsize {
        self.address
    }
}

impl Drop for FakeGpuTexture {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct FakeGpu {
    created: AtomicUsize,
    drops: Arc<AtomicUsize>,
    markers: Mutex<Vec<u8>>,
    fail: AtomicBool,
}

impl GpuBackend for FakeGpu {
    fn create_rgba8(&self, image: &DecodedImage) -> Result<Box<dyn GpuTexture>, BackendFailure> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(BackendFailure::Unavailable);
        }
        let sequence = self.created.fetch_add(1, Ordering::SeqCst) + 1;
        self.markers
            .lock()
            .expect("fake GPU observation lock should remain usable")
            .push(image.rgba8[0]);
        Ok(Box::new(FakeGpuTexture {
            address: NonZeroUsize::new(0x1000 + sequence)
                .expect("fake SRV address should be non-zero"),
            drops: Arc::clone(&self.drops),
        }))
    }
}

#[derive(Default)]
struct FakeDownloader {
    response: Mutex<Vec<u8>>,
    calls: AtomicUsize,
}

impl Downloader for FakeDownloader {
    fn fetch(
        &self,
        _target: &DownloadTarget,
        _max_bytes: usize,
    ) -> Result<Vec<u8>, BackendFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .response
            .lock()
            .expect("fake downloader response lock should remain usable")
            .clone())
    }
}

struct BlockingDownloader {
    calls: AtomicUsize,
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
}

impl Downloader for BlockingDownloader {
    fn fetch(
        &self,
        _target: &DownloadTarget,
        _max_bytes: usize,
    ) -> Result<Vec<u8>, BackendFailure> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.entered.wait();
            self.release.wait();
        }
        Ok(vec![7])
    }
}

#[derive(Default)]
struct FakeOverrides {
    entries: Mutex<HashMap<String, Vec<u8>>>,
}

impl OverrideProvider for FakeOverrides {
    fn load_override(
        &self,
        identifier: &str,
        _max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, BackendFailure> {
        Ok(self
            .entries
            .lock()
            .expect("fake override map lock should remain usable")
            .get(identifier)
            .cloned())
    }
}

struct FakeResources {
    bytes: Vec<u8>,
    calls: AtomicUsize,
}

impl ResourceProvider for FakeResources {
    fn load_png(
        &self,
        _module: ModuleHandle,
        _resource_id: u32,
        _max_bytes: usize,
    ) -> Result<Vec<u8>, BackendFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.bytes.clone())
    }
}

fn make_service(
    config: TextureConfig,
    decoder: Arc<dyn ImageDecoder>,
    gpu: Arc<dyn GpuBackend>,
    downloader: Arc<dyn Downloader>,
    overrides: Arc<dyn OverrideProvider>,
    resources: Arc<dyn ResourceProvider>,
) -> TextureService {
    TextureService::new(config, decoder, gpu, downloader, overrides, resources)
        .expect("test texture service should start")
}

fn pump_until(service: &TextureService, predicate: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let _ = service.advance();
        if predicate() {
            return;
        }
        std::thread::yield_now();
    }
    panic!("texture service did not reach the expected state before the test deadline");
}

#[test]
fn memory_load_creates_stable_abi_record_and_releases_gpu_once() {
    let decoder = Arc::new(FakeDecoder::default());
    let gpu = Arc::new(FakeGpu::default());
    let callbacks = Arc::new(AtomicUsize::new(0));
    let callback_count = Arc::clone(&callbacks);
    let callback: TextureCallback = Arc::new(move |event| {
        assert!(event.result.is_ok());
        callback_count.fetch_add(1, Ordering::SeqCst);
    });
    let service = make_service(
        TextureConfig::default(),
        decoder,
        gpu.clone(),
        Arc::new(FakeDownloader::default()),
        Arc::new(NoOverrides),
        Arc::new(FakeResources {
            bytes: vec![1],
            calls: AtomicUsize::new(0),
        }),
    );

    let outcome = service
        .load(
            "stable",
            TextureSource::Memory(vec![3]),
            LoadOptions::default(),
            Some(callback),
        )
        .expect("memory request should be accepted");
    assert!(matches!(outcome, RequestOutcome::Queued));
    pump_until(&service, || service.get("stable").is_some());

    let first = service.get("stable").expect("texture should be registered");
    let second = service
        .get("stable")
        .expect("texture should remain registered");
    assert!(first.ptr_eq(&second));
    assert_eq!(first.as_abi_ptr().addr(), second.as_abi_ptr().addr());
    assert_eq!(callbacks.load(Ordering::SeqCst), 1);
    drop(first);
    drop(second);
    assert_eq!(gpu.drops.load(Ordering::SeqCst), 0);
    drop(service);
    assert_eq!(gpu.drops.load(Ordering::SeqCst), 1);
}

#[test]
fn concurrent_requests_deduplicate_decode_and_fan_out_callbacks() {
    let decoder = Arc::new(FakeDecoder::default());
    let gpu = Arc::new(FakeGpu::default());
    let service = Arc::new(make_service(
        TextureConfig::default(),
        decoder.clone(),
        gpu,
        Arc::new(FakeDownloader::default()),
        Arc::new(NoOverrides),
        Arc::new(FakeResources {
            bytes: vec![1],
            calls: AtomicUsize::new(0),
        }),
    ));
    let starts = Arc::new(Barrier::new(9));
    let callbacks = Arc::new(AtomicUsize::new(0));
    let mut threads = Vec::new();
    for _ in 0..8 {
        let service = Arc::clone(&service);
        let starts = Arc::clone(&starts);
        let callbacks = Arc::clone(&callbacks);
        threads.push(std::thread::spawn(move || {
            starts.wait();
            service
                .load(
                    "shared",
                    TextureSource::Memory(vec![4]),
                    LoadOptions::default(),
                    Some(Arc::new(move |_| {
                        callbacks.fetch_add(1, Ordering::SeqCst);
                    })),
                )
                .expect("concurrent request should queue or join")
        }));
    }
    starts.wait();
    for thread in threads {
        let outcome = thread.join().expect("request thread should not panic");
        assert!(matches!(
            outcome,
            RequestOutcome::Queued | RequestOutcome::Joined
        ));
    }
    pump_until(&service, || callbacks.load(Ordering::SeqCst) == 8);
    assert_eq!(decoder.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn work_queue_rejects_excess_without_unbounded_growth() {
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let decoder = Arc::new(BlockingDecoder {
        calls: AtomicUsize::new(0),
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    });
    let config = TextureConfig {
        work_queue_capacity: 1,
        ..TextureConfig::default()
    };
    let service = make_service(
        config,
        decoder,
        Arc::new(FakeGpu::default()),
        Arc::new(FakeDownloader::default()),
        Arc::new(NoOverrides),
        Arc::new(FakeResources {
            bytes: vec![1],
            calls: AtomicUsize::new(0),
        }),
    );
    service
        .get_or_create(
            "first",
            TextureSource::Memory(vec![1]),
            LoadOptions::default(),
        )
        .expect("first request should start");
    entered.wait();
    service
        .get_or_create(
            "second",
            TextureSource::Memory(vec![2]),
            LoadOptions::default(),
        )
        .expect("second request should fill the queue");
    let error = service
        .get_or_create(
            "third",
            TextureSource::Memory(vec![3]),
            LoadOptions::default(),
        )
        .expect_err("third request should exceed the queue bound");
    assert_eq!(
        error,
        TextureError::QueueFull(nexus_textures::QueueKind::Work)
    );
    release.wait();
    pump_until(&service, || {
        service.get("first").is_some() && service.get("second").is_some()
    });
}

#[test]
fn download_queue_is_separate_bounded_and_offline_testable() {
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let downloader = Arc::new(BlockingDownloader {
        calls: AtomicUsize::new(0),
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    });
    let config = TextureConfig {
        download_queue_capacity: 1,
        ..TextureConfig::default()
    };
    let service = make_service(
        config,
        Arc::new(FakeDecoder::default()),
        Arc::new(FakeGpu::default()),
        downloader,
        Arc::new(NoOverrides),
        Arc::new(FakeResources {
            bytes: vec![1],
            calls: AtomicUsize::new(0),
        }),
    );
    let url = || TextureSource::Url(DownloadTarget::new("https://private.invalid/a?mode=test"));
    service
        .get_or_create("one", url(), LoadOptions::default())
        .expect("first download should start");
    entered.wait();
    service
        .get_or_create("two", url(), LoadOptions::default())
        .expect("second download should fill the queue");
    assert_eq!(
        service
            .get_or_create("three", url(), LoadOptions::default())
            .expect_err("third download should exceed the queue bound"),
        TextureError::QueueFull(nexus_textures::QueueKind::Download)
    );
    release.wait();
    pump_until(&service, || {
        service.get("one").is_some() && service.get("two").is_some()
    });
}

#[test]
fn override_wins_and_shadow_preserves_previous_stable_entry() {
    let overrides = Arc::new(FakeOverrides::default());
    overrides
        .entries
        .lock()
        .expect("fake override map lock should remain usable")
        .insert("replace".to_owned(), vec![9]);
    let gpu = Arc::new(FakeGpu::default());
    let service = make_service(
        TextureConfig::default(),
        Arc::new(FakeDecoder::default()),
        gpu.clone(),
        Arc::new(FakeDownloader::default()),
        overrides.clone(),
        Arc::new(FakeResources {
            bytes: vec![1],
            calls: AtomicUsize::new(0),
        }),
    );
    service
        .get_or_create(
            "replace",
            TextureSource::Memory(vec![1]),
            LoadOptions::default(),
        )
        .expect("override request should queue");
    pump_until(&service, || service.get("replace").is_some());
    let old = service
        .get("replace")
        .expect("overridden texture should be registered");
    assert_eq!(
        gpu.markers
            .lock()
            .expect("fake GPU observation lock should remain usable")[0],
        9
    );

    overrides
        .entries
        .lock()
        .expect("fake override map lock should remain usable")
        .insert("replace".to_owned(), vec![8]);
    service
        .get_or_create(
            "replace",
            TextureSource::Memory(vec![2]),
            LoadOptions {
                shadow_existing: true,
                ..LoadOptions::default()
            },
        )
        .expect("shadow replacement should queue");
    pump_until(&service, || {
        service
            .get("replace")
            .is_some_and(|current| !current.ptr_eq(&old))
    });
    let shadow = service
        .get("replace_1")
        .expect("old texture should be available under a shadow key");
    assert!(shadow.ptr_eq(&old));
}

#[test]
fn cleanup_removes_exact_generation_but_shared_request_survives() {
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let service = make_service(
        TextureConfig::default(),
        Arc::new(BlockingDecoder {
            calls: AtomicUsize::new(0),
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        }),
        Arc::new(FakeGpu::default()),
        Arc::new(FakeDownloader::default()),
        Arc::new(NoOverrides),
        Arc::new(FakeResources {
            bytes: vec![1],
            calls: AtomicUsize::new(0),
        }),
    );
    let first_owner = OwnerGeneration {
        owner: 41,
        generation: 1,
    };
    let second_owner = OwnerGeneration {
        owner: 41,
        generation: 2,
    };
    let first_callbacks = Arc::new(AtomicUsize::new(0));
    let second_callbacks = Arc::new(AtomicUsize::new(0));
    let first_count = Arc::clone(&first_callbacks);
    service
        .load(
            "generation",
            TextureSource::Memory(vec![5]),
            LoadOptions {
                owner: RequestOwner::Addon(first_owner),
                shadow_existing: false,
            },
            Some(Arc::new(move |_| {
                first_count.fetch_add(1, Ordering::SeqCst);
            })),
        )
        .expect("first generation request should start");
    entered.wait();
    let second_count = Arc::clone(&second_callbacks);
    service
        .load(
            "generation",
            TextureSource::Memory(vec![6]),
            LoadOptions {
                owner: RequestOwner::Addon(second_owner),
                shadow_existing: false,
            },
            Some(Arc::new(move |_| {
                second_count.fetch_add(1, Ordering::SeqCst);
            })),
        )
        .expect("second generation should join");
    assert_eq!(service.cleanup_owner_generation(first_owner), 1);
    release.wait();
    pump_until(&service, || second_callbacks.load(Ordering::SeqCst) == 1);
    assert_eq!(first_callbacks.load(Ordering::SeqCst), 0);
    assert!(service.get("generation").is_some());
}

#[test]
fn callback_panic_is_contained_and_other_callbacks_run() {
    let service = make_service(
        TextureConfig::default(),
        Arc::new(FakeDecoder::default()),
        Arc::new(FakeGpu::default()),
        Arc::new(FakeDownloader::default()),
        Arc::new(NoOverrides),
        Arc::new(FakeResources {
            bytes: vec![1],
            calls: AtomicUsize::new(0),
        }),
    );
    let successful = Arc::new(AtomicUsize::new(0));
    service
        .load(
            "panic",
            TextureSource::Memory(vec![1]),
            LoadOptions::default(),
            Some(Arc::new(|_| panic!("intentional addon callback panic"))),
        )
        .expect("first callback request should queue");
    let successful_count = Arc::clone(&successful);
    service
        .load(
            "panic",
            TextureSource::Memory(vec![2]),
            LoadOptions::default(),
            Some(Arc::new(move |_| {
                successful_count.fetch_add(1, Ordering::SeqCst);
            })),
        )
        .expect("second callback should join");
    pump_until(&service, || successful.load(Ordering::SeqCst) == 1);
    assert_eq!(service.stats().callback_panics, 1);
}

#[test]
fn invalid_decoder_output_never_reaches_gpu() {
    let decoder = Arc::new(FakeDecoder::default());
    decoder.invalid.store(true, Ordering::SeqCst);
    let gpu = Arc::new(FakeGpu::default());
    let callback_error = Arc::new(Mutex::new(None));
    let observed = Arc::clone(&callback_error);
    let service = make_service(
        TextureConfig::default(),
        decoder,
        gpu.clone(),
        Arc::new(FakeDownloader::default()),
        Arc::new(NoOverrides),
        Arc::new(FakeResources {
            bytes: vec![1],
            calls: AtomicUsize::new(0),
        }),
    );
    service
        .load(
            "invalid",
            TextureSource::Memory(vec![1]),
            LoadOptions::default(),
            Some(Arc::new(move |event| {
                *observed
                    .lock()
                    .expect("callback result lock should remain usable") = event.result.err();
            })),
        )
        .expect("invalid image request should reach the decoder");
    pump_until(&service, || {
        callback_error
            .lock()
            .expect("callback result lock should remain usable")
            .is_some()
    });
    assert_eq!(
        *callback_error
            .lock()
            .expect("callback result lock should remain usable"),
        Some(TextureError::InvalidDecodedImage)
    );
    assert_eq!(gpu.created.load(Ordering::SeqCst), 0);
    assert!(service.get("invalid").is_none());
}

#[test]
fn resource_bytes_are_copied_before_async_decode() {
    let resources = Arc::new(FakeResources {
        bytes: vec![6],
        calls: AtomicUsize::new(0),
    });
    let gpu = Arc::new(FakeGpu::default());
    let service = make_service(
        TextureConfig::default(),
        Arc::new(FakeDecoder::default()),
        gpu.clone(),
        Arc::new(FakeDownloader::default()),
        Arc::new(NoOverrides),
        resources.clone(),
    );
    // SAFETY: the fake resource provider never dereferences the sentinel handle.
    let module = unsafe { ModuleHandle::from_hmodule(std::ptr::dangling_mut::<c_void>()) }
        .expect("non-zero fake module handle should be accepted");
    service
        .get_or_create(
            "resource",
            TextureSource::Resource {
                module,
                resource_id: 12,
            },
            LoadOptions::default(),
        )
        .expect("resource request should copy and queue");
    assert_eq!(resources.calls.load(Ordering::SeqCst), 1);
    pump_until(&service, || service.get("resource").is_some());
    assert_eq!(
        *gpu.markers
            .lock()
            .expect("fake GPU observation lock should remain usable"),
        vec![6]
    );
}

#[test]
fn debug_and_error_text_never_expose_source_values() {
    let sensitive = "https://private.invalid/image?mode=redacted-test";
    let target = DownloadTarget::new(sensitive);
    let source = TextureSource::Url(target.clone());
    assert!(!format!("{target:?}").contains(sensitive));
    assert!(!format!("{source:?}").contains(sensitive));
    assert!(!TextureError::DownloadFailed.to_string().contains(sensitive));
    assert!(
        !TextureError::FileUnavailable
            .to_string()
            .contains("C:\\private")
    );
}

#[test]
fn file_source_is_decoded_off_thread_and_path_is_never_reported() {
    let path =
        std::env::temp_dir().join(format!("nexus-textures-{}-encoded.bin", std::process::id()));
    std::fs::write(&path, [11]).expect("temporary encoded input should be writable");
    let gpu = Arc::new(FakeGpu::default());
    let service = make_service(
        TextureConfig::default(),
        Arc::new(FakeDecoder::default()),
        gpu.clone(),
        Arc::new(FakeDownloader::default()),
        Arc::new(NoOverrides),
        Arc::new(FakeResources {
            bytes: vec![1],
            calls: AtomicUsize::new(0),
        }),
    );
    service
        .get_or_create(
            "file",
            TextureSource::File(path.clone()),
            LoadOptions::default(),
        )
        .expect("file request should queue");
    pump_until(&service, || service.get("file").is_some());
    assert_eq!(
        *gpu.markers
            .lock()
            .expect("fake GPU observation lock should remain usable"),
        vec![11]
    );
    assert!(
        !format!("{:?}", TextureSource::File(path.clone())).contains(
            path.to_str()
                .expect("temporary path should be representable as UTF-8")
        )
    );
    std::fs::remove_file(path).expect("temporary encoded input should be removable");
}

#[test]
fn sole_owner_cleanup_cancels_gpu_upload_and_cached_callback_cleanup_is_async() {
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let gpu = Arc::new(FakeGpu::default());
    let service = make_service(
        TextureConfig::default(),
        Arc::new(BlockingDecoder {
            calls: AtomicUsize::new(0),
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        }),
        gpu.clone(),
        Arc::new(FakeDownloader::default()),
        Arc::new(NoOverrides),
        Arc::new(FakeResources {
            bytes: vec![1],
            calls: AtomicUsize::new(0),
        }),
    );
    let cancelled_owner = OwnerGeneration {
        owner: 77,
        generation: 3,
    };
    service
        .get_or_create(
            "cancelled",
            TextureSource::Memory(vec![1]),
            LoadOptions {
                owner: RequestOwner::Addon(cancelled_owner),
                shadow_existing: false,
            },
        )
        .expect("owned request should start");
    entered.wait();
    assert_eq!(service.cleanup_owner_generation(cancelled_owner), 0);
    release.wait();
    let deadline = Instant::now() + Duration::from_secs(3);
    while service.stats().queued_completions == 0 && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(service.stats().queued_completions, 1);
    assert_eq!(service.advance().completions, 1);
    assert!(service.get("cancelled").is_none());
    assert_eq!(gpu.created.load(Ordering::SeqCst), 0);

    service
        .get_or_create(
            "cached",
            TextureSource::Memory(vec![2]),
            LoadOptions::default(),
        )
        .expect("host texture should queue");
    pump_until(&service, || service.get("cached").is_some());
    let callback_count = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&callback_count);
    let cached_owner = OwnerGeneration {
        owner: 88,
        generation: 1,
    };
    let outcome = service
        .load(
            "cached",
            TextureSource::Memory(vec![9]),
            LoadOptions {
                owner: RequestOwner::Addon(cached_owner),
                shadow_existing: false,
            },
            Some(Arc::new(move |_| {
                observed.fetch_add(1, Ordering::SeqCst);
            })),
        )
        .expect("cached callback should be accepted");
    assert!(matches!(outcome, RequestOutcome::Cached(_)));
    assert_eq!(callback_count.load(Ordering::SeqCst), 0);
    assert_eq!(service.cleanup_owner_generation(cached_owner), 1);
    let _ = service.advance();
    assert_eq!(callback_count.load(Ordering::SeqCst), 0);
}

#[test]
fn gpu_failure_is_closed_and_override_paths_cannot_escape_directory() {
    let gpu = Arc::new(FakeGpu::default());
    gpu.fail.store(true, Ordering::SeqCst);
    let observed = Arc::new(Mutex::new(None));
    let callback_result = Arc::clone(&observed);
    let service = make_service(
        TextureConfig::default(),
        Arc::new(FakeDecoder::default()),
        gpu.clone(),
        Arc::new(FakeDownloader::default()),
        Arc::new(NoOverrides),
        Arc::new(FakeResources {
            bytes: vec![1],
            calls: AtomicUsize::new(0),
        }),
    );
    service
        .load(
            "gpu-failure",
            TextureSource::Memory(vec![1]),
            LoadOptions::default(),
            Some(Arc::new(move |event| {
                *callback_result
                    .lock()
                    .expect("GPU callback observation lock should remain usable") =
                    event.result.err();
            })),
        )
        .expect("GPU failure request should reach the worker");
    pump_until(&service, || {
        observed
            .lock()
            .expect("GPU callback observation lock should remain usable")
            .is_some()
    });
    assert_eq!(
        *observed
            .lock()
            .expect("GPU callback observation lock should remain usable"),
        Some(TextureError::GpuUploadFailed)
    );
    assert!(service.get("gpu-failure").is_none());

    let overrides = DirectoryOverrides::new(std::env::temp_dir());
    assert_eq!(
        overrides
            .load_override("../outside", 1024)
            .expect("unsafe override identifier should be ignored"),
        None
    );
    assert_eq!(
        overrides
            .load_override("..\\outside", 1024)
            .expect("Windows traversal identifier should be ignored"),
        None
    );
}
