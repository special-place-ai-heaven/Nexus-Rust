use core::ffi::c_void;
use core::ptr::NonNull;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use nexus_addon_backend::{
    TextureFacadeError, TextureServiceFacade, TextureSourceFactory, TextureSourceFailurePolicy,
};
use nexus_control::{FailureCode, InternalFailure, RenderOperation};
use nexus_dxgi::RenderCallbackError;
use nexus_network::{BaseUrl, CachePolicy, ClientError, HttpClient, HttpClientConfig, SystemClock};
use nexus_network_winhttp::{WinHttpTimeouts, WinHttpTransport};
use nexus_overlay::{RenderSessionAttachment, RenderSessionObserver, RenderSessionResources};
use nexus_render::{RenderStage, SwapChainId};
use nexus_textures::{
    AdvanceReport, BackendFailure, DirectoryOverrides, DownloadTarget, Downloader, ImageRsDecoder,
    LoadOptions, OwnerGeneration, RequestOutcome, ServiceStats, TextureCallback, TextureConfig,
    TextureError, TextureHandle, TextureService, TextureSource, WindowsResourceProvider,
};
use nexus_textures_d3d11::D3d11GpuBackend;
use thiserror::Error;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::core::Interface;

const MAX_IN_FLIGHT_OPERATIONS: usize = 256;
const RETIRE_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

std::thread_local! {
    static ACTIVE_TEXTURE_OPERATIONS: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

/// Stable identity of the selected swap-chain resource generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TextureSessionIdentity {
    pub(crate) swap_chain_id: SwapChainId,
    pub(crate) generation: u64,
}

/// Closed failures exposed by the future addon texture facade.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum RuntimeTextureError {
    #[error("no texture render session is available")]
    Unavailable,
    #[error("the bounded texture operation limit was reached")]
    Busy,
    #[error(transparent)]
    Texture(#[from] TextureError),
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
enum TextureAttachError {
    #[error("the render session did not expose a D3D11 device")]
    MissingDevice,
    #[error("the D3D11 device is incompatible with the texture service")]
    UnsupportedDevice,
    #[error("the bounded texture service could not be started")]
    ServiceUnavailable,
    #[error("the texture session generation was exhausted")]
    LifecycleExhausted,
}

#[derive(Default)]
struct CoordinatorState {
    next_attachment_id: u64,
    active: Option<Arc<ActiveTextureSession>>,
    /// Every texture record ever handed out, kept alive for the process.
    ///
    /// An add-on caches the `Texture*` it receives and dereferences it for the rest of
    /// the session, so the record behind it may never be freed and its identifier must
    /// keep resolving. Retiring a render session drops that session's `TextureService`
    /// and with it the service-side registry, so without this table the pointer an
    /// add-on holds would dangle on the first resize. See `CONFORMANCE.md` §2.6.
    ///
    /// The shader-resource view inside each record is created from the *device*, not the
    /// swap chain, so it stays valid across a resize or a render-target rebuild and the
    /// retained handle remains fully usable. Device loss is the separate case that must
    /// re-upload into the existing record.
    ///
    /// Growth is unbounded by design: the reference imposes no ceiling and frees no
    /// record.
    retained: HashMap<Arc<str>, TextureHandle>,
}

struct SessionLifecycle {
    accepting: bool,
    in_flight: usize,
}

struct ActiveTextureSession {
    attachment_id: u64,
    identity: TextureSessionIdentity,
    render_thread: ThreadId,
    service: TextureService,
    lifecycle: Mutex<SessionLifecycle>,
    drained: Condvar,
}

impl ActiveTextureSession {
    fn new(attachment_id: u64, identity: TextureSessionIdentity, service: TextureService) -> Self {
        Self {
            attachment_id,
            identity,
            render_thread: std::thread::current().id(),
            service,
            lifecycle: Mutex::new(SessionLifecycle {
                accepting: true,
                in_flight: 0,
            }),
            drained: Condvar::new(),
        }
    }

    fn try_enter(self: &Arc<Self>) -> Result<TextureOperationGuard, RuntimeTextureError> {
        let mut lifecycle = mutex_lock(&self.lifecycle);
        if !lifecycle.accepting {
            return Err(RuntimeTextureError::Unavailable);
        }
        if lifecycle.in_flight >= MAX_IN_FLIGHT_OPERATIONS {
            return Err(RuntimeTextureError::Busy);
        }
        lifecycle.in_flight += 1;
        drop(lifecycle);

        ACTIVE_TEXTURE_OPERATIONS.with(|operations| {
            operations.borrow_mut().push(self.attachment_id);
        });
        Ok(TextureOperationGuard {
            session: Arc::clone(self),
        })
    }

    fn retire(&self) {
        let local_depth = current_operation_depth(self.attachment_id);
        let deadline = Instant::now() + RETIRE_DRAIN_TIMEOUT;
        let mut lifecycle = mutex_lock(&self.lifecycle);
        lifecycle.accepting = false;

        while lifecycle.in_flight > local_depth {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self.drained.wait_timeout(lifecycle, remaining) {
                Ok((next, timeout)) => {
                    lifecycle = next;
                    if timeout.timed_out() {
                        break;
                    }
                }
                Err(poisoned) => {
                    let (next, _timeout) = poisoned.into_inner();
                    lifecycle = next;
                }
            }
        }
    }
}

struct TextureOperationGuard {
    session: Arc<ActiveTextureSession>,
}

impl Drop for TextureOperationGuard {
    fn drop(&mut self) {
        ACTIVE_TEXTURE_OPERATIONS.with(|operations| {
            let mut operations = operations.borrow_mut();
            if let Some(index) = operations
                .iter()
                .rposition(|attachment_id| *attachment_id == self.session.attachment_id)
            {
                operations.remove(index);
            }
        });

        let mut lifecycle = mutex_lock(&self.session.lifecycle);
        debug_assert!(lifecycle.in_flight > 0);
        lifecycle.in_flight = lifecycle.in_flight.saturating_sub(1);
        self.session.drained.notify_all();
    }
}

/// Process-owned facade over the currently selected render-session service.
///
/// The concrete service never escapes this coordinator. Future addon ABI calls
/// receive only owned texture handles or closed value results.
#[derive(Default)]
pub(crate) struct RuntimeTextureCoordinator {
    state: Mutex<CoordinatorState>,
}

impl RuntimeTextureCoordinator {
    fn attach_service(
        self: &Arc<Self>,
        identity: TextureSessionIdentity,
        service: TextureService,
    ) -> Result<TextureSessionLease, TextureAttachError> {
        let (attachment_id, previous) = {
            let mut state = mutex_lock(&self.state);
            let attachment_id = state
                .next_attachment_id
                .checked_add(1)
                .ok_or(TextureAttachError::LifecycleExhausted)?;
            state.next_attachment_id = attachment_id;
            let active = Arc::new(ActiveTextureSession::new(attachment_id, identity, service));
            let previous = state.active.replace(active);
            (attachment_id, previous)
        };

        retire_session(previous);
        Ok(TextureSessionLease {
            coordinator: Arc::clone(self),
            attachment_id,
        })
    }

    fn acquire(&self) -> Result<TextureOperationGuard, RuntimeTextureError> {
        let active = mutex_lock(&self.state)
            .active
            .as_ref()
            .cloned()
            .ok_or(RuntimeTextureError::Unavailable)?;
        active.try_enter()
    }

    /// Advances bounded completions only on the attached render thread and the
    /// fully enabled addon stage.
    pub(crate) fn advance(&self, stage: RenderStage) -> Option<AdvanceReport> {
        if stage != RenderStage::Addons {
            return None;
        }
        let operation = self.acquire().ok()?;
        if operation.session.render_thread != std::thread::current().id() {
            return None;
        }
        Some(operation.session.service.advance())
    }

    /// Returns the currently attached swap-chain generation without native pointers.
    #[must_use]
    pub(crate) fn active_identity(&self) -> Option<TextureSessionIdentity> {
        mutex_lock(&self.state)
            .active
            .as_ref()
            .map(|active| active.identity)
    }

    /// Returns an owned texture handle from the current generation.
    #[allow(dead_code)]
    pub(crate) fn get(
        &self,
        identifier: &str,
    ) -> Result<Option<TextureHandle>, RuntimeTextureError> {
        let operation = self.acquire()?;
        Ok(operation.session.service.get(identifier))
    }

    /// Submits bounded creation work against the current generation.
    #[allow(dead_code)]
    pub(crate) fn get_or_create(
        &self,
        identifier: &str,
        source: TextureSource,
        options: LoadOptions,
    ) -> Result<RequestOutcome, RuntimeTextureError> {
        let operation = self.acquire()?;
        operation
            .session
            .service
            .get_or_create(identifier, source, options)
            .map_err(RuntimeTextureError::from)
    }

    /// Submits bounded creation work and an optional owned callback.
    #[allow(dead_code)]
    pub(crate) fn load(
        &self,
        identifier: &str,
        source: TextureSource,
        options: LoadOptions,
        callback: Option<TextureCallback>,
    ) -> Result<RequestOutcome, RuntimeTextureError> {
        let operation = self.acquire()?;
        operation
            .session
            .service
            .load(identifier, source, options, callback)
            .map_err(RuntimeTextureError::from)
    }

    /// Removes callbacks and pending ownership for one exact addon generation.
    #[allow(dead_code)]
    pub(crate) fn cleanup_owner_generation(
        &self,
        owner: OwnerGeneration,
    ) -> Result<usize, RuntimeTextureError> {
        let operation = self.acquire()?;
        Ok(operation.session.service.cleanup_owner_generation(owner))
    }

    /// Returns closed queue and registry counters for the current generation.
    #[allow(dead_code)]
    pub(crate) fn stats(&self) -> Result<ServiceStats, RuntimeTextureError> {
        let operation = self.acquire()?;
        Ok(operation.session.service.stats())
    }

    /// Stops publication and retires the current service exactly once.
    pub(crate) fn shutdown(&self) {
        let active = mutex_lock(&self.state).active.take();
        retire_session(active);
    }

    /// Keeps one record alive for the process, so an add-on-held `Texture*` cannot dangle.
    ///
    /// The first handle seen for an identifier wins. Replacing it would move the record an
    /// add-on already cached, which is the very thing this table exists to prevent.
    fn retain(&self, identifier: &str, handle: &TextureHandle) {
        let mut state = mutex_lock(&self.state);
        if !state.retained.contains_key(identifier) {
            state.retained.insert(Arc::from(identifier), handle.clone());
        }
    }

    fn retained(&self, identifier: &str) -> Option<TextureHandle> {
        mutex_lock(&self.state).retained.get(identifier).cloned()
    }

    /// Number of records held for the process. Diagnostics only.
    #[cfg(test)]
    fn retained_count(&self) -> usize {
        mutex_lock(&self.state).retained.len()
    }

    fn detach(&self, attachment_id: u64) {
        let active = {
            let mut state = mutex_lock(&self.state);
            match state.active.as_ref() {
                Some(active) if active.attachment_id == attachment_id => state.active.take(),
                _ => None,
            }
        };
        retire_session(active);
    }
}

impl TextureServiceFacade for RuntimeTextureCoordinator {
    fn get(&self, identifier: &str) -> Result<Option<TextureHandle>, TextureFacadeError> {
        // The registry is host-wide in the reference, so a record that resolved once keeps
        // resolving even after the session that created it has been retired.
        let from_session = match self.acquire() {
            Ok(operation) => TextureServiceFacade::get(&operation.session.service, identifier)?,
            Err(_error) => None,
        };
        if let Some(handle) = from_session {
            self.retain(identifier, &handle);
            return Ok(Some(handle));
        }
        Ok(self.retained(identifier))
    }

    fn load_with_source(
        &self,
        identifier: &str,
        options: LoadOptions,
        callback: Option<TextureCallback>,
        source: TextureSourceFactory<'_>,
        failure_policy: TextureSourceFailurePolicy,
    ) -> Result<RequestOutcome, TextureFacadeError> {
        let operation = self
            .acquire()
            .map_err(|_error| TextureFacadeError::Rejected)?;
        let outcome = TextureServiceFacade::load_with_source(
            &operation.session.service,
            identifier,
            options,
            callback,
            source,
            failure_policy,
        )?;
        if let RequestOutcome::Cached(handle) = &outcome {
            self.retain(identifier, handle);
        }
        Ok(outcome)
    }

    fn cleanup_owner_generation(
        &self,
        owner: OwnerGeneration,
    ) -> Result<usize, TextureFacadeError> {
        let operation = self
            .acquire()
            .map_err(|_error| TextureFacadeError::Rejected)?;
        TextureServiceFacade::cleanup_owner_generation(&operation.session.service, owner)
    }
}

impl fmt::Debug for RuntimeTextureCoordinator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeTextureCoordinator")
            .field("active_identity", &self.active_identity())
            .finish_non_exhaustive()
    }
}

impl Drop for RuntimeTextureCoordinator {
    fn drop(&mut self) {
        let active = mutex_lock(&self.state).active.take();
        retire_session(active);
    }
}

struct TextureSessionLease {
    coordinator: Arc<RuntimeTextureCoordinator>,
    attachment_id: u64,
}

impl Drop for TextureSessionLease {
    fn drop(&mut self) {
        self.coordinator.detach(self.attachment_id);
    }
}

trait TextureServiceFactory: Send + Sync + 'static {
    fn create(&self, device: NonNull<c_void>) -> Result<TextureService, TextureAttachError>;
}

struct RuntimeTextureObserver<F> {
    coordinator: Arc<RuntimeTextureCoordinator>,
    factory: F,
}

impl<F> RenderSessionObserver for RuntimeTextureObserver<F>
where
    F: TextureServiceFactory,
{
    fn attach(
        &self,
        resources: RenderSessionResources<'_>,
    ) -> Result<Box<dyn RenderSessionAttachment>, RenderCallbackError> {
        let identity = TextureSessionIdentity {
            swap_chain_id: resources.swap_chain_id(),
            generation: resources.generation(),
        };
        let service = self
            .factory
            .create(resources.device())
            .map_err(map_attach_error)?;
        let lease = self
            .coordinator
            .attach_service(identity, service)
            .map_err(map_attach_error)?;
        Ok(Box::new(lease))
    }
}

struct ProductionTextureFactory {
    override_directory: PathBuf,
    timeouts: WinHttpTimeouts,
}

impl TextureServiceFactory for ProductionTextureFactory {
    fn create(&self, device: NonNull<c_void>) -> Result<TextureService, TextureAttachError> {
        let raw = device.as_ptr();
        // SAFETY: the overlay lends a live ID3D11Device for this attach call.
        // `from_raw_borrowed` does not consume that reference, and `clone`
        // immediately AddRefs it into an owned COM value before the borrow ends.
        let borrowed = unsafe { ID3D11Device::from_raw_borrowed(&raw) }
            .ok_or(TextureAttachError::MissingDevice)?;
        let gpu = D3d11GpuBackend::new(borrowed.clone())
            .map_err(|_error| TextureAttachError::UnsupportedDevice)?;

        TextureService::new(
            TextureConfig::default(),
            Arc::new(ImageRsDecoder),
            Arc::new(gpu),
            Arc::new(WinHttpTextureDownloader {
                timeouts: self.timeouts,
            }),
            Arc::new(DirectoryOverrides::new(self.override_directory.clone())),
            Arc::new(WindowsResourceProvider),
        )
        .map_err(|_error| TextureAttachError::ServiceUnavailable)
    }
}

struct WinHttpTextureDownloader {
    timeouts: WinHttpTimeouts,
}

impl Downloader for WinHttpTextureDownloader {
    fn fetch(&self, target: &DownloadTarget, max_bytes: usize) -> Result<Vec<u8>, BackendFailure> {
        if max_bytes == 0 {
            return Err(BackendFailure::Rejected);
        }
        let (base, endpoint) =
            BaseUrl::split_absolute(target.as_str()).map_err(|_error| BackendFailure::Rejected)?;
        let transport = WinHttpTransport::with_timeouts(self.timeouts)
            .map_err(|_error| BackendFailure::Unavailable)?;
        let mut client = HttpClient::new(base.as_str(), transport, SystemClock)
            .map_err(|_error| BackendFailure::Rejected)?
            .with_config(HttpClientConfig {
                max_response_bytes: max_bytes,
            });
        let response = client
            .get(&endpoint, "", CachePolicy::Default)
            .map_err(map_client_error)?;
        if !response.is_success() {
            return Err(BackendFailure::Rejected);
        }
        Ok(response.body().to_vec())
    }
}

/// Builds the observer installed into the process overlay adapter.
pub(crate) fn production_observer(
    coordinator: Arc<RuntimeTextureCoordinator>,
    override_directory: PathBuf,
) -> Arc<dyn RenderSessionObserver> {
    Arc::new(RuntimeTextureObserver {
        coordinator,
        factory: ProductionTextureFactory {
            override_directory,
            timeouts: WinHttpTimeouts::default(),
        },
    })
}

const fn map_client_error(error: ClientError) -> BackendFailure {
    match error {
        ClientError::Transport => BackendFailure::Unavailable,
        ClientError::InvalidRequest
        | ClientError::BodyTooLarge
        | ClientError::ContentLengthMismatch
        | ClientError::InvalidStatus => BackendFailure::Rejected,
    }
}

const fn map_attach_error(error: TextureAttachError) -> RenderCallbackError {
    let failure = match error {
        TextureAttachError::MissingDevice => InternalFailure::MissingDevice,
        TextureAttachError::UnsupportedDevice => InternalFailure::UnsupportedInterface,
        TextureAttachError::ServiceUnavailable | TextureAttachError::LifecycleExhausted => {
            InternalFailure::InvalidState
        }
    };
    RenderCallbackError::new(
        RenderOperation::PrepareTarget,
        FailureCode::Internal(failure),
    )
}

fn retire_session(session: Option<Arc<ActiveTextureSession>>) {
    if let Some(session) = session {
        session.retire();
        drop(session);
    }
}

fn current_operation_depth(attachment_id: u64) -> usize {
    ACTIVE_TEXTURE_OPERATIONS.with(|operations| {
        operations
            .borrow()
            .iter()
            .filter(|active_id| **active_id == attachment_id)
            .count()
    })
}

fn mutex_lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroUsize;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use nexus_textures::{
        DecodeLimits, DecodedImage, GpuBackend, GpuTexture, ImageDecoder, NoDownloader,
        NoOverrides, NoResources,
    };

    use super::*;

    struct TestGpu {
        drops: Arc<AtomicUsize>,
    }

    impl GpuBackend for TestGpu {
        fn create_rgba8(
            &self,
            _image: &DecodedImage,
        ) -> Result<Box<dyn GpuTexture>, BackendFailure> {
            let Some(address) = NonZeroUsize::new(0x1000) else {
                return Err(BackendFailure::Unavailable);
            };
            Ok(Box::new(TestGpuTexture(address)))
        }
    }

    impl Drop for TestGpu {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct TestGpuTexture(NonZeroUsize);

    impl GpuTexture for TestGpuTexture {
        fn srv_address(&self) -> NonZeroUsize {
            self.0
        }
    }

    fn service(drops: &Arc<AtomicUsize>) -> TextureService {
        TextureService::new(
            TextureConfig::default(),
            Arc::new(ImageRsDecoder),
            Arc::new(TestGpu {
                drops: Arc::clone(drops),
            }),
            Arc::new(NoDownloader),
            Arc::new(NoOverrides),
            Arc::new(NoResources),
        )
        .expect("offline test texture service should start")
    }

    /// Always yields one opaque pixel, so a load completes without real image bytes.
    struct StubDecoder;

    impl ImageDecoder for StubDecoder {
        fn decode(
            &self,
            _encoded: &[u8],
            _limits: DecodeLimits,
        ) -> Result<DecodedImage, BackendFailure> {
            Ok(DecodedImage {
                width: 1,
                height: 1,
                rgba8: vec![u8::MAX; 4],
            })
        }
    }

    fn decoding_service(drops: &Arc<AtomicUsize>) -> TextureService {
        TextureService::new(
            TextureConfig::default(),
            Arc::new(StubDecoder),
            Arc::new(TestGpu {
                drops: Arc::clone(drops),
            }),
            Arc::new(NoDownloader),
            Arc::new(NoOverrides),
            Arc::new(NoResources),
        )
        .expect("offline test texture service should start")
    }

    /// Registers one texture and returns its handle, driving the load to completion.
    fn register(coordinator: &RuntimeTextureCoordinator, identifier: &str) -> TextureHandle {
        let outcome = TextureServiceFacade::load_with_source(
            coordinator,
            identifier,
            LoadOptions::default(),
            None,
            Box::new(|| Ok(TextureSource::Memory(vec![1]))),
            TextureSourceFailurePolicy::Suppress,
        )
        .expect("the load should be accepted");
        assert!(matches!(outcome, RequestOutcome::Queued));

        // Decoding and upload run on a worker thread, so this is a genuine asynchronous
        // completion and polling is correct. The bound is generous rather than tight: a
        // small one fails under parallel test load, which is a flaky test rather than a
        // real defect.
        for _ in 0..100_000 {
            coordinator.advance(RenderStage::Addons);
            if let Ok(Some(handle)) = TextureServiceFacade::get(coordinator, identifier) {
                return handle;
            }
            std::thread::yield_now();
        }
        panic!("the texture never completed");
    }

    /// An add-on caches the `Texture*` it is handed and dereferences it for the whole
    /// session. A resize retires the render session and drops that session's
    /// `TextureService`, so without process-scoped retention the pointer would dangle and
    /// the identifier would stop resolving. See `CONFORMANCE.md` §2.6.
    #[test]
    fn an_addon_held_texture_survives_a_session_reattach() {
        let coordinator = Arc::new(RuntimeTextureCoordinator::default());
        let drops = Arc::new(AtomicUsize::new(0));
        let first = coordinator
            .attach_service(identity(1, 1), decoding_service(&drops))
            .expect("first service should attach");

        let handle = register(&coordinator, "icon");
        let addon_pointer = handle.as_abi_ptr();

        // A surface-affecting change advances the generation and re-attaches, which
        // retires the previous service exactly as a resize does.
        drop(first);
        let _second = coordinator
            .attach_service(identity(1, 2), decoding_service(&drops))
            .expect("second service should attach");
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "the first session's GPU backend must have been dropped, so this is a real              re-attach and not a no-op"
        );

        let after = TextureServiceFacade::get(&*coordinator, "icon")
            .expect("the lookup must not be rejected")
            .expect("the identifier must still resolve after a re-attach");
        assert!(
            after.ptr_eq(&handle),
            "the same registry allocation must be returned"
        );
        assert_eq!(
            after.as_abi_ptr(),
            addon_pointer,
            "the address an add-on cached must not move"
        );
        // The retained handle keeps the record alive on its own.
        drop(handle);
        assert_eq!(after.width(), 1);
        assert_eq!(coordinator.retained_count(), 1);
    }

    fn identity(sequence: u64, generation: u64) -> TextureSessionIdentity {
        TextureSessionIdentity {
            swap_chain_id: SwapChainId::new(sequence),
            generation,
        }
    }

    #[test]
    fn attach_publishes_identity_and_advances_only_the_addon_render_thread() {
        let coordinator = Arc::new(RuntimeTextureCoordinator::default());
        let drops = Arc::new(AtomicUsize::new(0));
        let lease = coordinator
            .attach_service(identity(7, 3), service(&drops))
            .expect("test service should attach");

        assert_eq!(coordinator.active_identity(), Some(identity(7, 3)));
        assert!(coordinator.advance(RenderStage::Addons).is_some());
        assert!(coordinator.advance(RenderStage::CoreUi).is_none());

        let foreign = Arc::clone(&coordinator);
        let foreign_result = std::thread::spawn(move || foreign.advance(RenderStage::Addons))
            .join()
            .expect("foreign-thread probe should not panic");
        assert!(foreign_result.is_none());

        drop(lease);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(coordinator.active_identity(), None);
    }

    #[test]
    fn reselection_retires_old_service_and_stale_lease_cannot_detach_new() {
        let coordinator = Arc::new(RuntimeTextureCoordinator::default());
        let drops = Arc::new(AtomicUsize::new(0));
        let first = coordinator
            .attach_service(identity(1, 1), service(&drops))
            .expect("first test service should attach");
        let second = coordinator
            .attach_service(identity(2, 1), service(&drops))
            .expect("second test service should attach");

        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(coordinator.active_identity(), Some(identity(2, 1)));
        drop(first);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(coordinator.active_identity(), Some(identity(2, 1)));

        drop(second);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
        assert_eq!(coordinator.active_identity(), None);
    }

    #[test]
    fn shutdown_retires_once_and_unattached_access_fails_closed() {
        let coordinator = Arc::new(RuntimeTextureCoordinator::default());
        let drops = Arc::new(AtomicUsize::new(0));
        let lease = coordinator
            .attach_service(identity(4, 9), service(&drops))
            .expect("test service should attach");

        coordinator.shutdown();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(coordinator.active_identity(), None);
        assert!(matches!(
            coordinator.get("missing"),
            Err(RuntimeTextureError::Unavailable)
        ));
        assert!(matches!(
            coordinator.cleanup_owner_generation(OwnerGeneration {
                owner: 5,
                generation: 2,
            }),
            Err(RuntimeTextureError::Unavailable)
        ));
        let source_calls = AtomicUsize::new(0);
        assert!(matches!(
            TextureServiceFacade::load_with_source(
                coordinator.as_ref(),
                "unavailable",
                LoadOptions {
                    owner: nexus_textures::RequestOwner::Host,
                    shadow_existing: false,
                },
                None,
                Box::new(|| {
                    source_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(TextureSource::Memory(vec![1]))
                }),
                TextureSourceFailurePolicy::Suppress,
            ),
            Err(TextureFacadeError::Rejected)
        ));
        assert_eq!(source_calls.load(Ordering::SeqCst), 0);

        drop(lease);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn facade_submission_holds_the_session_guard_through_lazy_source_acquisition() {
        let coordinator = Arc::new(RuntimeTextureCoordinator::default());
        let drops = Arc::new(AtomicUsize::new(0));
        let lease = coordinator
            .attach_service(identity(6, 2), service(&drops))
            .expect("test service should attach");
        let outcome = TextureServiceFacade::load_with_source(
            coordinator.as_ref(),
            "reentrant-retirement",
            LoadOptions {
                owner: nexus_textures::RequestOwner::Host,
                shadow_existing: false,
            },
            None,
            Box::new(|| {
                coordinator.shutdown();
                assert_eq!(coordinator.active_identity(), None);
                assert_eq!(drops.load(Ordering::SeqCst), 0);
                Ok(TextureSource::Memory(vec![1]))
            }),
            TextureSourceFailurePolicy::Suppress,
        )
        .expect("an admitted facade operation should finish against its selected service");
        assert!(matches!(outcome, RequestOutcome::Queued));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        drop(lease);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn offline_service_attach_never_constructs_a_network_transport() {
        let coordinator = Arc::new(RuntimeTextureCoordinator::default());
        let drops = Arc::new(AtomicUsize::new(0));
        let lease = coordinator
            .attach_service(identity(8, 1), service(&drops))
            .expect("NoDownloader service should attach without network access");

        assert_eq!(
            map_client_error(ClientError::Transport),
            BackendFailure::Unavailable
        );
        assert_eq!(
            map_client_error(ClientError::BodyTooLarge),
            BackendFailure::Rejected
        );
        drop(lease);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}
