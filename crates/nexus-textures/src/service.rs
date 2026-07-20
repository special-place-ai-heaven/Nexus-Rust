use core::fmt;
use core::mem::{align_of, offset_of, size_of};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use nexus_abi::Texture;

use crate::io::read_file_bounded;
use crate::queue::BoundedQueue;
use crate::{
    ConfigError, DecodeLimits, DecodedImage, Downloader, GpuBackend, GpuTexture, ImageDecoder,
    ModuleHandle, OverrideProvider, QueueKind, ResourceProvider, TextureConfig, TextureError,
};

/// Addon identity plus one loader generation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct OwnerGeneration {
    /// Stable host-assigned addon identity.
    pub owner: u64,
    /// Monotonically increasing load generation for that addon.
    pub generation: u64,
}

impl From<nexus_core::OwnerToken> for OwnerGeneration {
    fn from(owner: nexus_core::OwnerToken) -> Self {
        Self {
            owner: u64::from(owner.signature),
            generation: owner.generation,
        }
    }
}

/// Owner of a request or callback.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RequestOwner {
    /// Host-owned work which survives addon cleanup.
    Host,
    /// Work owned by one exact addon generation.
    Addon(OwnerGeneration),
}

/// Per-request behavior flags.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoadOptions {
    /// Owner used for unload cleanup.
    pub owner: RequestOwner,
    /// Rename an existing registry entry before loading the replacement.
    pub shadow_existing: bool,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            owner: RequestOwner::Host,
            shadow_existing: false,
        }
    }
}

/// A URL whose value is deliberately redacted from `Debug` output.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct DownloadTarget(Arc<str>);

impl DownloadTarget {
    /// Create a target. The service applies its configured byte bound on submission.
    #[must_use]
    pub fn new(value: impl Into<Arc<str>>) -> Self {
        Self(value.into())
    }

    /// Borrow the target for an injected downloader.
    ///
    /// Implementations must not include this value in logs or error messages.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for DownloadTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DownloadTarget(<redacted>)")
    }
}

/// Owned source data for a texture request.
pub enum TextureSource {
    /// Read encoded bytes from a file on the decode worker.
    File(PathBuf),
    /// Copy a PNG resource from a loaded module before the request returns.
    Resource {
        /// Borrowed module handle.
        module: ModuleHandle,
        /// Integer PNG resource identifier.
        resource_id: u32,
    },
    /// Fetch encoded bytes with the injected downloader.
    Url(DownloadTarget),
    /// Already-owned encoded bytes. Addon adapters should copy raw input into this vector.
    Memory(Vec<u8>),
}

impl fmt::Debug for TextureSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self {
            Self::File(_) => "File(<redacted>)",
            Self::Resource { .. } => "Resource(<redacted>)",
            Self::Url(_) => "Url(<redacted>)",
            Self::Memory(_) => "Memory(<redacted>)",
        };
        formatter.write_str(kind)
    }
}

#[repr(C)]
struct TextureStorage {
    width: u32,
    height: u32,
    resource: usize,
}

const _: () = {
    assert!(size_of::<TextureStorage>() == size_of::<Texture>());
    assert!(align_of::<TextureStorage>() == align_of::<Texture>());
    assert!(offset_of!(TextureStorage, width) == offset_of!(Texture, width));
    assert!(offset_of!(TextureStorage, height) == offset_of!(Texture, height));
    assert!(offset_of!(TextureStorage, resource) == offset_of!(Texture, resource));
};

struct TextureEntry {
    abi: Box<TextureStorage>,
    _gpu: Box<dyn GpuTexture>,
}

/// Cloneable lease for one stable ABI texture record and its owned GPU SRV.
#[derive(Clone)]
pub struct TextureHandle(Arc<TextureEntry>);

impl TextureHandle {
    /// Texture width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.0.abi.width
    }

    /// Texture height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.0.abi.height
    }

    /// Address of the owned `ID3D11ShaderResourceView`.
    #[must_use]
    pub fn resource_address(&self) -> usize {
        self.0.abi.resource
    }

    /// Stable pointer matching [`nexus_abi::Texture`].
    ///
    /// The pointer remains valid while this handle or its registry entry lives.
    #[must_use]
    pub fn as_abi_ptr(&self) -> *mut Texture {
        let storage = self.0.abi.as_ref() as *const TextureStorage;
        storage.cast_mut().cast::<Texture>()
    }

    /// Test whether two handles name the same stable registry allocation.
    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl fmt::Debug for TextureHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TextureHandle")
            .field("width", &self.width())
            .field("height", &self.height())
            .field("resource", &"<redacted>")
            .finish()
    }
}

/// Result delivered to an asynchronous texture callback.
#[derive(Clone)]
pub struct TextureCallbackEvent {
    /// Registry identifier supplied by the caller.
    pub identifier: Arc<str>,
    /// Stable texture handle or a closed failure.
    pub result: Result<TextureHandle, TextureError>,
}

impl fmt::Debug for TextureCallbackEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TextureCallbackEvent")
            .field("identifier", &"<redacted>")
            .field("succeeded", &self.result.is_ok())
            .finish()
    }
}

/// Panic-contained asynchronous callback invoked by [`TextureService::advance`].
pub type TextureCallback = Arc<dyn Fn(TextureCallbackEvent) + Send + Sync + 'static>;

/// Immediate disposition of a submitted load request.
#[derive(Clone, Debug)]
pub enum RequestOutcome {
    /// The registry already contained a texture. A supplied callback is still queued.
    Cached(TextureHandle),
    /// New work was accepted by a bounded worker queue.
    Queued,
    /// The identifier was already in flight and the request joined it.
    Joined,
}

/// Work performed by one render-thread advance.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AdvanceReport {
    /// Completion records removed from the bounded queue.
    pub completions: usize,
    /// Textures successfully uploaded and registered.
    pub created: usize,
    /// Requests completed with failure.
    pub failed: usize,
    /// Addon callbacks invoked successfully.
    pub callbacks: usize,
    /// Addon callback panics caught during this advance.
    pub callback_panics: usize,
}

/// Redaction-safe service counters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ServiceStats {
    /// Number of live registry keys, including shadow aliases.
    pub registry_entries: usize,
    /// Number of unique in-flight identifiers.
    pub pending_identifiers: usize,
    /// Waiting decode jobs.
    pub queued_work: usize,
    /// Waiting download jobs.
    pub queued_downloads: usize,
    /// Decoded results and ready callbacks waiting for `advance`.
    pub queued_completions: usize,
    /// Total callback panics caught since construction.
    pub callback_panics: u64,
}

struct CallbackRegistration {
    id: u64,
    owner: RequestOwner,
    callback: TextureCallback,
}

struct PendingRequest {
    request_id: u64,
    owners: HashSet<RequestOwner>,
    callbacks: Vec<CallbackRegistration>,
}

#[derive(Default)]
struct RegistryState {
    next_request_id: u64,
    next_callback_id: u64,
    registry: HashMap<Arc<str>, TextureHandle>,
    pending: HashMap<Arc<str>, PendingRequest>,
}

enum DecodeSource {
    File(PathBuf),
    Encoded(Vec<u8>),
}

struct DecodeJob {
    identifier: Arc<str>,
    request_id: u64,
    source: DecodeSource,
}

struct DownloadJob {
    identifier: Arc<str>,
    request_id: u64,
    target: DownloadTarget,
}

enum Completion {
    Decoded {
        identifier: Arc<str>,
        request_id: u64,
        result: Result<DecodedImage, TextureError>,
    },
    Callback {
        owner: RequestOwner,
        event: TextureCallbackEvent,
        callback: TextureCallback,
    },
}

struct Shared {
    config: TextureConfig,
    state: Mutex<RegistryState>,
    decoder: Arc<dyn ImageDecoder>,
    downloader: Arc<dyn Downloader>,
    work: BoundedQueue<DecodeJob>,
    downloads: BoundedQueue<DownloadJob>,
    completions: BoundedQueue<Completion>,
    stopping: AtomicBool,
    callback_panics: AtomicU64,
}

/// Bounded texture registry, acquisition pipeline, and render-thread uploader.
pub struct TextureService {
    shared: Arc<Shared>,
    gpu: Arc<dyn GpuBackend>,
    overrides: Arc<dyn OverrideProvider>,
    resources: Arc<dyn ResourceProvider>,
    workers: Vec<JoinHandle<()>>,
}

impl TextureService {
    /// Construct the service and start one decode worker plus one download worker.
    pub fn new(
        config: TextureConfig,
        decoder: Arc<dyn ImageDecoder>,
        gpu: Arc<dyn GpuBackend>,
        downloader: Arc<dyn Downloader>,
        overrides: Arc<dyn OverrideProvider>,
        resources: Arc<dyn ResourceProvider>,
    ) -> Result<Self, ConfigError> {
        let config = config.validate()?;
        let shared = Arc::new(Shared {
            config,
            state: Mutex::new(RegistryState {
                next_request_id: 1,
                next_callback_id: 1,
                ..RegistryState::default()
            }),
            decoder,
            downloader,
            work: BoundedQueue::new(config.work_queue_capacity),
            downloads: BoundedQueue::new(config.download_queue_capacity),
            completions: BoundedQueue::new(config.completion_queue_capacity),
            stopping: AtomicBool::new(false),
            callback_panics: AtomicU64::new(0),
        });

        let mut workers = Vec::with_capacity(2);
        let decode_shared = Arc::clone(&shared);
        match std::thread::Builder::new()
            .name("nexus-texture-decode".to_owned())
            .spawn(move || decode_worker(decode_shared))
        {
            Ok(worker) => workers.push(worker),
            Err(_) => return Err(ConfigError::WorkerSpawnFailed),
        }

        let download_shared = Arc::clone(&shared);
        match std::thread::Builder::new()
            .name("nexus-texture-download".to_owned())
            .spawn(move || download_worker(download_shared))
        {
            Ok(worker) => workers.push(worker),
            Err(_) => {
                stop_shared(&shared);
                for worker in workers {
                    let _ = worker.join();
                }
                return Err(ConfigError::WorkerSpawnFailed);
            }
        }

        Ok(Self {
            shared,
            gpu,
            overrides,
            resources,
            workers,
        })
    }

    /// Return a cloneable handle without exposing a registry lock.
    #[must_use]
    pub fn get(&self, identifier: &str) -> Option<TextureHandle> {
        self.lock_state().registry.get(identifier).cloned()
    }

    /// Return an existing handle or synchronously submit creation work.
    ///
    /// As in the legacy addon API, a first miss returns `Queued` rather than a
    /// half-created texture. GPU creation completes during `advance`.
    pub fn get_or_create(
        &self,
        identifier: &str,
        source: TextureSource,
        options: LoadOptions,
    ) -> Result<RequestOutcome, TextureError> {
        self.load(identifier, source, options, None)
    }

    /// Submit creation work and optionally receive its result asynchronously.
    pub fn load(
        &self,
        identifier: &str,
        source: TextureSource,
        options: LoadOptions,
        callback: Option<TextureCallback>,
    ) -> Result<RequestOutcome, TextureError> {
        self.validate_identifier(identifier)?;
        if self.shared.stopping.load(Ordering::Acquire) {
            return Err(TextureError::ServiceStopped);
        }

        let identifier: Arc<str> = Arc::from(identifier);
        let reserved = self.reserve_request(&identifier, options, callback, true)?;
        let Reservation::New { request_id, .. } = reserved else {
            return self.finish_existing_reservation(reserved);
        };

        let source = match self
            .resolve_override(&identifier)
            .and_then(|override_source| {
                override_source.map_or_else(|| self.resolve_source(source), Ok)
            }) {
            Ok(source) => source,
            Err(error) => {
                self.fail_reserved(&identifier, request_id, error);
                return Err(error);
            }
        };

        if let Err(error) = self.queue_resolved(&identifier, request_id, source) {
            self.fail_reserved(&identifier, request_id, error);
            return Err(error);
        }
        Ok(RequestOutcome::Queued)
    }

    /// Submit work while acquiring its concrete source only when it is needed.
    ///
    /// Pending and cached reservations are resolved before the source provider
    /// is called, and an encoded override wins before native source acquisition.
    /// The provider runs synchronously without a service lock.
    /// attach_joined_callback retains the core service's callback fan-out when
    /// true; ABI compatibility adapters pass false because the legacy host
    /// ignored later callbacks for an identifier that was already queued.
    ///
    /// Service failures are converted with map_service_error. A provider error
    /// is returned unchanged, and the primary callback is removed before any
    /// joined callbacks receive a closed failure.
    pub fn load_with_source<E>(
        &self,
        identifier: &str,
        options: LoadOptions,
        callback: Option<TextureCallback>,
        attach_joined_callback: bool,
        source: impl FnOnce() -> Result<TextureSource, E>,
        map_service_error: impl Fn(TextureError) -> E,
    ) -> Result<RequestOutcome, E> {
        self.validate_identifier(identifier)
            .map_err(&map_service_error)?;
        if self.shared.stopping.load(Ordering::Acquire) {
            return Err(map_service_error(TextureError::ServiceStopped));
        }

        let identifier: Arc<str> = Arc::from(identifier);
        let reserved = self
            .reserve_request(&identifier, options, callback, attach_joined_callback)
            .map_err(&map_service_error)?;
        let Reservation::New {
            request_id,
            primary_callback_id,
        } = reserved
        else {
            return self
                .finish_existing_reservation(reserved)
                .map_err(map_service_error);
        };

        let resolved = match self.resolve_override(&identifier) {
            Ok(Some(source)) => source,
            Ok(None) => {
                let source = match source() {
                    Ok(source) => source,
                    Err(error) => {
                        self.fail_reserved_excluding(
                            &identifier,
                            request_id,
                            TextureError::DecodeFailed,
                            primary_callback_id,
                        );
                        return Err(error);
                    }
                };
                match self.resolve_source(source) {
                    Ok(source) => source,
                    Err(error) => {
                        self.fail_reserved_excluding(
                            &identifier,
                            request_id,
                            error,
                            primary_callback_id,
                        );
                        return Err(map_service_error(error));
                    }
                }
            }
            Err(error) => {
                self.fail_reserved_excluding(&identifier, request_id, error, primary_callback_id);
                return Err(map_service_error(error));
            }
        };

        if let Err(error) = self.queue_resolved(&identifier, request_id, resolved) {
            self.fail_reserved_excluding(&identifier, request_id, error, primary_callback_id);
            return Err(map_service_error(error));
        }
        Ok(RequestOutcome::Queued)
    }

    /// Process bounded decoded results, GPU uploads, and callbacks on the caller thread.
    pub fn advance(&self) -> AdvanceReport {
        let mut report = AdvanceReport::default();
        for _ in 0..self.shared.config.max_completions_per_advance {
            let Some(completion) = self.shared.completions.try_pop() else {
                break;
            };
            report.completions += 1;
            match completion {
                Completion::Callback {
                    event, callback, ..
                } => self.dispatch_callback(event, &callback, &mut report),
                Completion::Decoded {
                    identifier,
                    request_id,
                    result,
                } => self.finish_decode(identifier, request_id, result, &mut report),
            }
        }
        report
    }

    /// Remove all pending and ready callbacks owned by one exact addon generation.
    ///
    /// Requests with no remaining owners are cancelled logically; a worker may
    /// finish decoding, but its stale request id will never reach the GPU backend.
    pub fn cleanup_owner_generation(&self, owner: OwnerGeneration) -> usize {
        let target = RequestOwner::Addon(owner);
        let mut removed = 0;
        {
            let mut state = self.lock_state();
            state.pending.retain(|_, pending| {
                pending.callbacks.retain(|registration| {
                    let keep = registration.owner != target;
                    if !keep {
                        removed += 1;
                    }
                    keep
                });
                pending.owners.remove(&target);
                !pending.owners.is_empty() || !pending.callbacks.is_empty()
            });
        }
        self.shared
            .completions
            .retain(|completion| match completion {
                Completion::Callback { owner, .. } if *owner == target => {
                    removed += 1;
                    false
                }
                _ => true,
            });
        removed
    }

    /// Snapshot closed counters without exposing identifiers or source values.
    #[must_use]
    pub fn stats(&self) -> ServiceStats {
        let state = self.lock_state();
        ServiceStats {
            registry_entries: state.registry.len(),
            pending_identifiers: state.pending.len(),
            queued_work: self.shared.work.len(),
            queued_downloads: self.shared.downloads.len(),
            queued_completions: self.shared.completions.len(),
            callback_panics: self.shared.callback_panics.load(Ordering::Relaxed),
        }
    }

    fn reserve_request(
        &self,
        identifier: &Arc<str>,
        options: LoadOptions,
        callback: Option<TextureCallback>,
        attach_joined_callback: bool,
    ) -> Result<Reservation, TextureError> {
        let mut state = self.lock_state();
        let registration = callback.map(|callback| {
            let id = state.next_callback_id;
            state.next_callback_id = state.next_callback_id.wrapping_add(1).max(1);
            CallbackRegistration {
                id,
                owner: options.owner,
                callback,
            }
        });
        if let Some(pending) = state.pending.get_mut(identifier.as_ref()) {
            if attach_joined_callback && let Some(registration) = registration {
                if pending.callbacks.len() >= self.shared.config.max_callbacks_per_texture {
                    return Err(TextureError::CallbackLimit);
                }
                pending.callbacks.push(registration);
            }
            pending.owners.insert(options.owner);
            return Ok(Reservation::Joined);
        }

        if !options.shadow_existing {
            if let Some(texture) = state.registry.get(identifier.as_ref()).cloned() {
                return Ok(Reservation::Cached {
                    texture,
                    owner: options.owner,
                    callback: registration.map(|registration| registration.callback),
                    identifier: Arc::clone(identifier),
                });
            }
        } else {
            shadow_texture(&mut state.registry, identifier);
        }

        let request_id = state.next_request_id;
        state.next_request_id = state.next_request_id.wrapping_add(1).max(1);
        let mut owners = HashSet::new();
        owners.insert(options.owner);
        let primary_callback_id = registration.as_ref().map(|registration| registration.id);
        let callbacks = registration.into_iter().collect();
        state.pending.insert(
            Arc::clone(identifier),
            PendingRequest {
                request_id,
                owners,
                callbacks,
            },
        );
        Ok(Reservation::New {
            request_id,
            primary_callback_id,
        })
    }

    fn finish_existing_reservation(
        &self,
        reservation: Reservation,
    ) -> Result<RequestOutcome, TextureError> {
        match reservation {
            Reservation::Joined => Ok(RequestOutcome::Joined),
            Reservation::Cached {
                texture,
                owner,
                callback,
                identifier,
            } => {
                if let Some(callback) = callback {
                    let completion = Completion::Callback {
                        owner,
                        event: TextureCallbackEvent {
                            identifier,
                            result: Ok(texture.clone()),
                        },
                        callback,
                    };
                    self.shared
                        .completions
                        .try_push(completion)
                        .map_err(|_| TextureError::QueueFull(QueueKind::Completion))?;
                }
                Ok(RequestOutcome::Cached(texture))
            }
            Reservation::New { .. } => unreachable!("new reservation is handled by load"),
        }
    }

    fn queue_resolved(
        &self,
        identifier: &Arc<str>,
        request_id: u64,
        source: ResolvedSource,
    ) -> Result<(), TextureError> {
        match source {
            ResolvedSource::Decode(source) => self
                .shared
                .work
                .try_push(DecodeJob {
                    identifier: Arc::clone(identifier),
                    request_id,
                    source,
                })
                .map_err(|_| TextureError::QueueFull(QueueKind::Work)),
            ResolvedSource::Download(target) => self
                .shared
                .downloads
                .try_push(DownloadJob {
                    identifier: Arc::clone(identifier),
                    request_id,
                    target,
                })
                .map_err(|_| TextureError::QueueFull(QueueKind::Download)),
        }
    }

    fn resolve_override(&self, identifier: &str) -> Result<Option<ResolvedSource>, TextureError> {
        let override_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.overrides
                .load_override(identifier, self.shared.config.max_encoded_bytes)
        }))
        .map_err(|_| TextureError::OverrideUnavailable)?
        .map_err(|_| TextureError::OverrideUnavailable)?;
        if let Some(encoded) = override_result {
            self.validate_encoded(&encoded)?;
            return Ok(Some(ResolvedSource::Decode(DecodeSource::Encoded(encoded))));
        }
        Ok(None)
    }

    fn resolve_source(&self, source: TextureSource) -> Result<ResolvedSource, TextureError> {
        match source {
            TextureSource::File(path) => Ok(ResolvedSource::Decode(DecodeSource::File(path))),
            TextureSource::Memory(encoded) => {
                self.validate_encoded(&encoded)?;
                Ok(ResolvedSource::Decode(DecodeSource::Encoded(encoded)))
            }
            TextureSource::Url(target) => {
                if target.as_str().is_empty()
                    || target.as_str().len() > self.shared.config.max_url_bytes
                {
                    return Err(TextureError::UrlTooLong);
                }
                Ok(ResolvedSource::Download(target))
            }
            TextureSource::Resource {
                module,
                resource_id,
            } => {
                let encoded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    self.resources.load_png(
                        module,
                        resource_id,
                        self.shared.config.max_encoded_bytes,
                    )
                }))
                .map_err(|_| TextureError::ResourceUnavailable)?
                .map_err(|_| TextureError::ResourceUnavailable)?;
                self.validate_encoded(&encoded)?;
                Ok(ResolvedSource::Decode(DecodeSource::Encoded(encoded)))
            }
        }
    }

    fn finish_decode(
        &self,
        identifier: Arc<str>,
        request_id: u64,
        result: Result<DecodedImage, TextureError>,
        report: &mut AdvanceReport,
    ) {
        if !self.pending_matches(&identifier, request_id) {
            return;
        }
        let result = result.and_then(|image| {
            validate_decoded(&image, self.shared.config)?;
            let (gpu, resource) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.gpu.create_rgba8(&image).map(|gpu| {
                    let resource = gpu.srv_address().get();
                    (gpu, resource)
                })
            }))
            .map_err(|_| TextureError::GpuUploadFailed)?
            .map_err(|_| TextureError::GpuUploadFailed)?;
            Ok(TextureHandle(Arc::new(TextureEntry {
                abi: Box::new(TextureStorage {
                    width: image.width,
                    height: image.height,
                    resource,
                }),
                _gpu: gpu,
            })))
        });

        let callbacks = {
            let mut state = self.lock_state();
            let Some(pending) = state.pending.get(identifier.as_ref()) else {
                return;
            };
            if pending.request_id != request_id {
                return;
            }
            let Some(pending) = state.pending.remove(identifier.as_ref()) else {
                return;
            };
            if let Ok(texture) = &result {
                state
                    .registry
                    .insert(Arc::clone(&identifier), texture.clone());
            }
            pending.callbacks
        };

        match &result {
            Ok(_) => report.created += 1,
            Err(_) => report.failed += 1,
        }
        self.dispatch_registrations(identifier, result, callbacks, report);
    }

    fn fail_reserved(&self, identifier: &Arc<str>, request_id: u64, error: TextureError) {
        self.fail_reserved_excluding(identifier, request_id, error, None);
    }

    fn fail_reserved_excluding(
        &self,
        identifier: &Arc<str>,
        request_id: u64,
        error: TextureError,
        excluded_callback_id: Option<u64>,
    ) {
        let callbacks = {
            let mut state = self.lock_state();
            match state.pending.get(identifier.as_ref()) {
                Some(pending) if pending.request_id == request_id => state
                    .pending
                    .remove(identifier.as_ref())
                    .map(|pending| {
                        pending
                            .callbacks
                            .into_iter()
                            .filter(|registration| Some(registration.id) != excluded_callback_id)
                            .collect()
                    })
                    .unwrap_or_default(),
                _ => Vec::new(),
            }
        };
        let mut ignored = AdvanceReport::default();
        self.dispatch_registrations(Arc::clone(identifier), Err(error), callbacks, &mut ignored);
    }

    fn dispatch_registrations(
        &self,
        identifier: Arc<str>,
        result: Result<TextureHandle, TextureError>,
        callbacks: Vec<CallbackRegistration>,
        report: &mut AdvanceReport,
    ) {
        for registration in callbacks {
            self.dispatch_callback(
                TextureCallbackEvent {
                    identifier: Arc::clone(&identifier),
                    result: result.clone(),
                },
                &registration.callback,
                report,
            );
        }
    }

    fn dispatch_callback(
        &self,
        event: TextureCallbackEvent,
        callback: &TextureCallback,
        report: &mut AdvanceReport,
    ) {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(event))).is_err() {
            self.shared.callback_panics.fetch_add(1, Ordering::Relaxed);
            report.callback_panics += 1;
        } else {
            report.callbacks += 1;
        }
    }

    fn pending_matches(&self, identifier: &str, request_id: u64) -> bool {
        self.lock_state()
            .pending
            .get(identifier)
            .is_some_and(|pending| pending.request_id == request_id)
    }

    fn validate_identifier(&self, identifier: &str) -> Result<(), TextureError> {
        if identifier.is_empty()
            || identifier.len() > self.shared.config.max_identifier_bytes
            || identifier.contains('\0')
        {
            return Err(TextureError::InvalidIdentifier);
        }
        Ok(())
    }

    fn validate_encoded(&self, encoded: &[u8]) -> Result<(), TextureError> {
        if encoded.is_empty() {
            return Err(TextureError::DecodeFailed);
        }
        if encoded.len() > self.shared.config.max_encoded_bytes {
            return Err(TextureError::EncodedTooLarge);
        }
        Ok(())
    }

    fn lock_state(&self) -> MutexGuard<'_, RegistryState> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for TextureService {
    fn drop(&mut self) {
        stop_shared(&self.shared);
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

enum Reservation {
    New {
        request_id: u64,
        primary_callback_id: Option<u64>,
    },
    Cached {
        texture: TextureHandle,
        owner: RequestOwner,
        callback: Option<TextureCallback>,
        identifier: Arc<str>,
    },
    Joined,
}

enum ResolvedSource {
    Decode(DecodeSource),
    Download(DownloadTarget),
}

fn decode_worker(shared: Arc<Shared>) {
    while let Some(job) = shared.work.pop_wait(&shared.stopping) {
        let result = match job.source {
            DecodeSource::File(path) => read_file_bounded(&path, shared.config.max_encoded_bytes)
                .map_err(|_| TextureError::FileUnavailable)
                .and_then(|encoded| decode_bytes(&shared, &encoded)),
            DecodeSource::Encoded(encoded) => decode_bytes(&shared, &encoded),
        };
        let completion = Completion::Decoded {
            identifier: job.identifier,
            request_id: job.request_id,
            result,
        };
        if shared
            .completions
            .push_wait(completion, &shared.stopping)
            .is_err()
        {
            return;
        }
    }
}

fn download_worker(shared: Arc<Shared>) {
    while let Some(job) = shared.downloads.pop_wait(&shared.stopping) {
        let downloaded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            shared
                .downloader
                .fetch(&job.target, shared.config.max_encoded_bytes)
        }))
        .map_err(|_| TextureError::DownloadFailed)
        .and_then(|result| result.map_err(|_| TextureError::DownloadFailed))
        .and_then(|encoded| {
            if encoded.is_empty() || encoded.len() > shared.config.max_encoded_bytes {
                return Err(TextureError::DownloadFailed);
            }
            decode_bytes(&shared, &encoded)
        });
        let completion = Completion::Decoded {
            identifier: job.identifier,
            request_id: job.request_id,
            result: downloaded,
        };
        if shared
            .completions
            .push_wait(completion, &shared.stopping)
            .is_err()
        {
            return;
        }
    }
}

fn decode_bytes(shared: &Shared, encoded: &[u8]) -> Result<DecodedImage, TextureError> {
    if encoded.is_empty() {
        return Err(TextureError::DecodeFailed);
    }
    if encoded.len() > shared.config.max_encoded_bytes {
        return Err(TextureError::EncodedTooLarge);
    }
    let limits = DecodeLimits {
        max_width: shared.config.max_width,
        max_height: shared.config.max_height,
        max_pixels: shared.config.max_pixels,
        max_allocation_bytes: shared.config.max_decode_allocation_bytes,
    };
    let image = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        shared.decoder.decode(encoded, limits)
    }))
    .map_err(|_| TextureError::DecodeFailed)?
    .map_err(|_| TextureError::DecodeFailed)?;
    validate_decoded(&image, shared.config)?;
    Ok(image)
}

fn validate_decoded(image: &DecodedImage, config: TextureConfig) -> Result<(), TextureError> {
    if image.width == 0 || image.height == 0 {
        return Err(TextureError::InvalidDecodedImage);
    }
    if image.width > config.max_width || image.height > config.max_height {
        return Err(TextureError::InvalidDecodedImage);
    }
    let pixels = u64::from(image.width)
        .checked_mul(u64::from(image.height))
        .ok_or(TextureError::InvalidDecodedImage)?;
    if pixels > config.max_pixels {
        return Err(TextureError::InvalidDecodedImage);
    }
    let bytes = pixels
        .checked_mul(4)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or(TextureError::InvalidDecodedImage)?;
    if image.rgba8.len() != bytes {
        return Err(TextureError::InvalidDecodedImage);
    }
    Ok(())
}

fn shadow_texture(registry: &mut HashMap<Arc<str>, TextureHandle>, identifier: &Arc<str>) {
    let Some(texture) = registry.remove(identifier.as_ref()) else {
        return;
    };
    let mut suffix = 1_u64;
    loop {
        let shadow: Arc<str> = Arc::from(format!("{identifier}_{suffix}"));
        if let std::collections::hash_map::Entry::Vacant(entry) = registry.entry(shadow) {
            entry.insert(texture);
            return;
        }
        suffix = suffix.saturating_add(1);
    }
}

fn stop_shared(shared: &Shared) {
    shared.stopping.store(true, Ordering::Release);
    shared.work.close();
    shared.downloads.close();
    shared.completions.close();
}
