use core::ffi::{c_char, c_void};
use core::fmt;
use core::ptr;
use std::ffi::CString;
use std::path::PathBuf;
use std::sync::Arc;

use nexus_abi::{ReceiveTexture, Texture};
use nexus_core::OwnerToken;
use nexus_textures::{
    DownloadTarget, LoadOptions, ModuleHandle, OwnerGeneration, RequestOutcome, RequestOwner,
    TextureCallback, TextureHandle, TextureService, TextureSource,
};

use crate::{
    BackendFailure, BackendOperationError, NativeCallBoundary, NativeText, RequiredServiceResult,
    TextureBackend,
};

/// Lazily acquires one owned texture source during a facade submission.
pub type TextureSourceFactory<'a> = Box<dyn FnOnce() -> RequiredServiceResult<TextureSource> + 'a>;

/// Closed result from a process texture facade.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextureFacadeError {
    /// The caller-provided lazy source factory rejected the request.
    Source(BackendOperationError),
    /// The active texture service or its lifecycle gate rejected the request.
    Rejected,
}

/// Controls legacy callback publication for synchronous source failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextureSourceFailurePolicy {
    /// Do not publish failures produced by the lazy source factory.
    Suppress,
    /// Publish a null texture only for a typed service rejection.
    NotifyServiceRejected,
}

/// Process-safe access to the texture service selected for the active render session.
///
/// Implementations must keep the selected service alive for the complete method
/// call. The source factory is deliberately borrowed and non-`Send`: it runs
/// synchronously only after cache, pending, and override resolution require it.
pub trait TextureServiceFacade: Send + Sync + 'static {
    /// Gets one existing stable texture descriptor.
    fn get(&self, identifier: &str) -> Result<Option<TextureHandle>, TextureFacadeError>;

    /// Submits one lazy texture request against the active service.
    fn load_with_source(
        &self,
        identifier: &str,
        options: LoadOptions,
        callback: Option<TextureCallback>,
        source: TextureSourceFactory<'_>,
        failure_policy: TextureSourceFailurePolicy,
    ) -> Result<RequestOutcome, TextureFacadeError>;

    /// Removes callbacks and pending ownership for one exact add-on generation.
    fn cleanup_owner_generation(&self, owner: OwnerGeneration)
    -> Result<usize, TextureFacadeError>;
}

impl TextureServiceFacade for TextureService {
    fn get(&self, identifier: &str) -> Result<Option<TextureHandle>, TextureFacadeError> {
        Ok(TextureService::get(self, identifier))
    }

    fn load_with_source(
        &self,
        identifier: &str,
        options: LoadOptions,
        callback: Option<TextureCallback>,
        source: TextureSourceFactory<'_>,
        failure_policy: TextureSourceFailurePolicy,
    ) -> Result<RequestOutcome, TextureFacadeError> {
        TextureService::load_with_source(
            self,
            identifier,
            options,
            callback,
            move || source().map_err(TextureFacadeError::Source),
            move |error| {
                failure_policy == TextureSourceFailurePolicy::NotifyServiceRejected
                    && matches!(
                        error,
                        TextureFacadeError::Source(BackendOperationError::ServiceRejected)
                    )
            },
            |_error| TextureFacadeError::Rejected,
        )
    }

    fn cleanup_owner_generation(
        &self,
        owner: OwnerGeneration,
    ) -> Result<usize, TextureFacadeError> {
        Ok(TextureService::cleanup_owner_generation(self, owner))
    }
}

/// Caller-attributed adapter from the legacy texture ABI to a render-session facade.
///
/// Native strings and memory are copied before submission. The service owns
/// stable ABI descriptors, bounded worker queues, and render-thread callback
/// delivery; this adapter owns native caller attribution and callback gating.
pub struct TextureApi {
    boundary: Arc<NativeCallBoundary>,
    service: Arc<dyn TextureServiceFacade>,
}

impl TextureApi {
    /// Creates a texture adapter around the process texture service.
    #[must_use]
    pub fn new(boundary: Arc<NativeCallBoundary>, service: Arc<dyn TextureServiceFacade>) -> Self {
        Self { boundary, service }
    }

    /// Gets one existing stable texture descriptor.
    pub fn get(&self, identifier: *const c_char) -> RequiredServiceResult<*mut Texture> {
        let _owner = self.boundary.resolve_owner(None)?;
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        let texture = self
            .service
            .get(identifier.as_str())
            .map_err(|error| self.facade_error(error))?;
        Ok(texture.map_or(ptr::null_mut(), |texture| texture.as_abi_ptr()))
    }

    /// Gets or asynchronously creates a texture from a copied filesystem path.
    pub fn get_or_create_from_file(
        &self,
        identifier: *const c_char,
        filename: *const c_char,
    ) -> RequiredServiceResult<*mut Texture> {
        let owner = self.boundary.resolve_owner(None)?;
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        if let Some(texture) = self.cached(owner, &identifier)? {
            return Ok(texture);
        }
        self.get_or_create(owner, &identifier, || {
            let filename = self.boundary.snapshot_path(filename)?;
            Ok(TextureSource::File(PathBuf::from(filename.as_str())))
        })
    }

    /// Gets or asynchronously creates a texture from a synchronously copied resource.
    pub fn get_or_create_from_resource(
        &self,
        identifier: *const c_char,
        resource_id: u32,
        module: *mut c_void,
    ) -> RequiredServiceResult<*mut Texture> {
        let owner = self.boundary.resolve_owner(None)?;
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        if let Some(texture) = self.cached(owner, &identifier)? {
            return Ok(texture);
        }
        self.get_or_create(owner, &identifier, || {
            self.resource_source(resource_id, module)
        })
    }

    /// Gets or asynchronously creates a texture from a copied URL pair.
    pub fn get_or_create_from_url(
        &self,
        identifier: *const c_char,
        remote: *const c_char,
        endpoint: *const c_char,
    ) -> RequiredServiceResult<*mut Texture> {
        let owner = self.boundary.resolve_owner(None)?;
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        if let Some(texture) = self.cached(owner, &identifier)? {
            return Ok(texture);
        }
        self.get_or_create(owner, &identifier, || self.url_source(remote, endpoint))
    }

    /// Gets or asynchronously creates a texture from a copied memory buffer.
    pub fn get_or_create_from_memory(
        &self,
        identifier: *const c_char,
        data: *mut c_void,
        size: usize,
    ) -> RequiredServiceResult<*mut Texture> {
        let owner = self.boundary.resolve_owner(None)?;
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        if let Some(texture) = self.cached(owner, &identifier)? {
            return Ok(texture);
        }
        self.get_or_create(owner, &identifier, || self.memory_source(data, size))
    }

    /// Starts an explicit, shadowing load from a copied filesystem path.
    pub fn load_from_file(
        &self,
        identifier: *const c_char,
        filename: *const c_char,
        callback: Option<ReceiveTexture>,
    ) -> RequiredServiceResult<()> {
        let (owner, identifier, callback) = self.prepare_load(identifier, callback)?;
        self.load(owner, &identifier, callback, || {
            let filename = self.boundary.snapshot_path(filename)?;
            Ok(TextureSource::File(PathBuf::from(filename.as_str())))
        })
    }

    /// Starts an explicit, shadowing load from a synchronously copied resource.
    pub fn load_from_resource(
        &self,
        identifier: *const c_char,
        resource_id: u32,
        module: *mut c_void,
        callback: Option<ReceiveTexture>,
    ) -> RequiredServiceResult<()> {
        let (owner, identifier, callback) = self.prepare_load(identifier, callback)?;
        self.load(owner, &identifier, callback, || {
            self.resource_source(resource_id, module)
        })
    }

    /// Starts an explicit, shadowing load from a copied URL pair.
    pub fn load_from_url(
        &self,
        identifier: *const c_char,
        remote: *const c_char,
        endpoint: *const c_char,
        callback: Option<ReceiveTexture>,
    ) -> RequiredServiceResult<()> {
        let (owner, identifier, callback) = self.prepare_load(identifier, callback)?;
        self.load(owner, &identifier, callback, || {
            self.url_source(remote, endpoint)
        })
    }

    /// Starts an explicit, shadowing load from a copied memory buffer.
    pub fn load_from_memory(
        &self,
        identifier: *const c_char,
        data: *mut c_void,
        size: usize,
        callback: Option<ReceiveTexture>,
    ) -> RequiredServiceResult<()> {
        let (owner, identifier, callback) = self.prepare_load(identifier, callback)?;
        self.load(owner, &identifier, callback, || {
            self.memory_source(data, size)
        })
    }

    fn get_or_create(
        &self,
        owner: OwnerToken,
        identifier: &NativeText,
        source: impl FnOnce() -> RequiredServiceResult<TextureSource>,
    ) -> RequiredServiceResult<*mut Texture> {
        let outcome = self
            .service
            .load_with_source(
                identifier.as_str(),
                load_options(owner, false),
                None,
                Box::new(source),
                TextureSourceFailurePolicy::Suppress,
            )
            .map_err(|error| self.facade_error(error))?;
        self.finish_submission(owner)?;
        Ok(outcome_pointer(outcome))
    }

    fn cached(
        &self,
        owner: OwnerToken,
        identifier: &NativeText,
    ) -> RequiredServiceResult<Option<*mut Texture>> {
        let Some(texture) = self
            .service
            .get(identifier.as_str())
            .map_err(|error| self.facade_error(error))?
        else {
            return Ok(None);
        };
        self.finish_submission(owner)?;
        Ok(Some(texture.as_abi_ptr()))
    }

    fn load(
        &self,
        owner: OwnerToken,
        identifier: &NativeText,
        callback: Option<TextureCallback>,
        source: impl FnOnce() -> RequiredServiceResult<TextureSource>,
    ) -> RequiredServiceResult<()> {
        let _outcome = self
            .service
            .load_with_source(
                identifier.as_str(),
                load_options(owner, true),
                callback,
                Box::new(source),
                TextureSourceFailurePolicy::NotifyServiceRejected,
            )
            .map_err(|error| self.facade_error(error))?;
        self.finish_submission(owner)
    }

    fn prepare_load(
        &self,
        identifier: *const c_char,
        callback: Option<ReceiveTexture>,
    ) -> RequiredServiceResult<(OwnerToken, NativeText, Option<TextureCallback>)> {
        let owner = match callback {
            Some(callback) => self
                .boundary
                .resolve_owner_for_registered_address(callback_address(callback))?,
            None => self.boundary.resolve_owner(None)?,
        };
        let identifier = self.boundary.snapshot_identifier(identifier)?;
        let callback = callback
            .map(|callback| self.wrap_callback(owner, &identifier, callback))
            .transpose()?;
        Ok((owner, identifier, callback))
    }

    fn wrap_callback(
        &self,
        owner: OwnerToken,
        identifier: &NativeText,
        callback: ReceiveTexture,
    ) -> RequiredServiceResult<TextureCallback> {
        let gate = self.boundary.callback_gate_for_current(owner)?;
        let identifier = CString::new(identifier.as_str()).map_err(|_| self.service_rejected())?;
        Ok(Arc::new(move |event| {
            let Some(_guard) = gate.try_enter() else {
                return;
            };
            let texture = event
                .result
                .as_ref()
                .map_or(ptr::null_mut(), TextureHandle::as_abi_ptr);
            unsafe {
                // SAFETY: native address attribution binds `callback` to this
                // exact owner generation. `_guard` keeps that generation loaded
                // for the complete foreign call, and both arguments reference
                // service-owned storage valid for the duration of the call.
                callback(identifier.as_ptr(), texture);
            }
        }))
    }

    fn resource_source(
        &self,
        resource_id: u32,
        module: *mut c_void,
    ) -> RequiredServiceResult<TextureSource> {
        let module = unsafe {
            // SAFETY: the native texture API requires a live HMODULE for the
            // duration of this call. TextureService's resource provider obtains
            // its own temporary reference and copies the bytes synchronously.
            ModuleHandle::from_hmodule(module)
        }
        .ok_or_else(|| self.service_rejected())?;
        Ok(TextureSource::Resource {
            module,
            resource_id,
        })
    }

    fn url_source(
        &self,
        remote: *const c_char,
        endpoint: *const c_char,
    ) -> RequiredServiceResult<TextureSource> {
        let remote = self.boundary.snapshot_url(remote)?;
        let endpoint = self.boundary.snapshot_url(endpoint)?;
        let mut target = remote.as_str().to_owned();
        target.push_str(endpoint.as_str());
        Ok(TextureSource::Url(DownloadTarget::new(target)))
    }

    fn memory_source(
        &self,
        data: *mut c_void,
        size: usize,
    ) -> RequiredServiceResult<TextureSource> {
        let data = self.boundary.snapshot_buffer(data.cast_const(), size)?;
        Ok(TextureSource::Memory(data.into_vec()))
    }

    fn finish_submission(&self, owner: OwnerToken) -> RequiredServiceResult<()> {
        if let Err(error) = self.boundary.validate_current_owner(owner) {
            let _cleanup = self
                .service
                .cleanup_owner_generation(nexus_textures::OwnerGeneration::from(owner));
            return Err(error.into());
        }
        Ok(())
    }

    fn service_rejected(&self) -> BackendOperationError {
        self.boundary
            .failures()
            .record(BackendFailure::ServiceRejected);
        BackendOperationError::ServiceRejected
    }

    fn facade_error(&self, error: TextureFacadeError) -> BackendOperationError {
        match error {
            TextureFacadeError::Source(error) => error,
            TextureFacadeError::Rejected => self.service_rejected(),
        }
    }
}

impl TextureBackend for TextureApi {
    fn get(&self, identifier: *const c_char) -> RequiredServiceResult<*mut Texture> {
        self.get(identifier)
    }

    fn get_or_create_from_file(
        &self,
        identifier: *const c_char,
        filename: *const c_char,
    ) -> RequiredServiceResult<*mut Texture> {
        self.get_or_create_from_file(identifier, filename)
    }

    fn get_or_create_from_resource(
        &self,
        identifier: *const c_char,
        resource_id: u32,
        module: *mut c_void,
    ) -> RequiredServiceResult<*mut Texture> {
        self.get_or_create_from_resource(identifier, resource_id, module)
    }

    fn get_or_create_from_url(
        &self,
        identifier: *const c_char,
        remote: *const c_char,
        endpoint: *const c_char,
    ) -> RequiredServiceResult<*mut Texture> {
        self.get_or_create_from_url(identifier, remote, endpoint)
    }

    fn get_or_create_from_memory(
        &self,
        identifier: *const c_char,
        data: *mut c_void,
        size: usize,
    ) -> RequiredServiceResult<*mut Texture> {
        self.get_or_create_from_memory(identifier, data, size)
    }

    fn load_from_file(
        &self,
        identifier: *const c_char,
        filename: *const c_char,
        callback: Option<ReceiveTexture>,
    ) -> RequiredServiceResult<()> {
        self.load_from_file(identifier, filename, callback)
    }

    fn load_from_resource(
        &self,
        identifier: *const c_char,
        resource_id: u32,
        module: *mut c_void,
        callback: Option<ReceiveTexture>,
    ) -> RequiredServiceResult<()> {
        self.load_from_resource(identifier, resource_id, module, callback)
    }

    fn load_from_url(
        &self,
        identifier: *const c_char,
        remote: *const c_char,
        endpoint: *const c_char,
        callback: Option<ReceiveTexture>,
    ) -> RequiredServiceResult<()> {
        self.load_from_url(identifier, remote, endpoint, callback)
    }

    fn load_from_memory(
        &self,
        identifier: *const c_char,
        data: *mut c_void,
        size: usize,
        callback: Option<ReceiveTexture>,
    ) -> RequiredServiceResult<()> {
        self.load_from_memory(identifier, data, size, callback)
    }
}

impl fmt::Debug for TextureApi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TextureApi")
            .field("boundary", &self.boundary)
            .finish_non_exhaustive()
    }
}

fn load_options(owner: OwnerToken, shadow_existing: bool) -> LoadOptions {
    LoadOptions {
        owner: RequestOwner::Addon(owner.into()),
        shadow_existing,
    }
}

fn outcome_pointer(outcome: RequestOutcome) -> *mut Texture {
    match outcome {
        RequestOutcome::Cached(texture) => texture.as_abi_ptr(),
        RequestOutcome::Queued | RequestOutcome::Joined => ptr::null_mut(),
    }
}

fn callback_address(callback: ReceiveTexture) -> *const c_void {
    callback as *const () as *const c_void
}

#[cfg(test)]
mod tests {
    use core::ffi::{c_char, c_void};
    use core::num::NonZeroUsize;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::ffi::{CStr, CString};
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::time::{Duration, Instant};

    use nexus_addon_ffi::{AddonCallerResolver, AddressOwnerResolver};
    use nexus_core::{CallbackGate, OwnerToken};
    use nexus_native_memory::NativeMemoryReader;
    use nexus_textures::{
        BackendFailure as TextureBackendFailure, DecodeLimits, DecodedImage, DownloadTarget,
        Downloader, GpuBackend, GpuTexture, ImageDecoder, ModuleHandle, NoOverrides,
        OverrideProvider, ResourceProvider, TextureConfig, TextureService,
    };

    use super::TextureApi;
    use crate::{
        BackendFailureSnapshot, BackendFailures, BackendOperationError, CallBoundaryError,
        NativeCallBoundary, TextureBackend,
    };

    const OWNER: OwnerToken = OwnerToken {
        signature: 0x7E57,
        generation: 5,
    };
    const OTHER_OWNER: OwnerToken = OwnerToken {
        signature: 0x7E58,
        generation: 2,
    };

    static TEST_LOCK: Mutex<()> = Mutex::new(());
    static RECEIVED: Mutex<Vec<(String, usize)>> = Mutex::new(Vec::new());
    static CALLBACK_GATE: Mutex<Option<Arc<CallbackGate>>> = Mutex::new(None);
    static OBSERVED_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
    static GATED_CALLS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn receive_texture(
        identifier: *const c_char,
        texture: *mut nexus_abi::Texture,
    ) {
        let identifier = unsafe {
            // SAFETY: the adapter retains a valid CString for this call.
            CStr::from_ptr(identifier)
        };
        let gate = lock(&CALLBACK_GATE).clone();
        if let Some(gate) = gate {
            OBSERVED_IN_FLIGHT.store(gate.in_flight(), Ordering::Release);
        }
        lock(&RECEIVED).push((identifier.to_string_lossy().into_owned(), texture.addr()));
    }

    unsafe extern "C" fn gated_texture(
        _identifier: *const c_char,
        _texture: *mut nexus_abi::Texture,
    ) {
        GATED_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    unsafe extern "C" fn foreign_texture(
        _identifier: *const c_char,
        _texture: *mut nexus_abi::Texture,
    ) {
    }

    struct TestOwners {
        current: AtomicBool,
        close_on_gate: AtomicBool,
        gate: Arc<CallbackGate>,
    }

    impl TestOwners {
        fn new(gate: Arc<CallbackGate>) -> Self {
            Self {
                current: AtomicBool::new(true),
                close_on_gate: AtomicBool::new(false),
                gate,
            }
        }
    }

    impl AddressOwnerResolver for TestOwners {
        fn owner_for_address(&self, address: NonZeroUsize) -> Option<OwnerToken> {
            let address = address.get();
            if address == foreign_texture as *const () as usize {
                return Some(OTHER_OWNER);
            }
            [
                receive_texture as *const () as usize,
                gated_texture as *const () as usize,
            ]
            .contains(&address)
            .then_some(OWNER)
        }

        fn is_current_owner(&self, owner: OwnerToken) -> bool {
            (owner == OWNER || owner == OTHER_OWNER) && self.current.load(Ordering::Acquire)
        }

        fn callback_gate_for_current(&self, owner: OwnerToken) -> Option<Arc<CallbackGate>> {
            if owner != OWNER || !self.current.load(Ordering::Acquire) {
                return None;
            }
            if self.close_on_gate.swap(false, Ordering::AcqRel) {
                self.gate.close();
                self.current.store(false, Ordering::Release);
            }
            Some(Arc::clone(&self.gate))
        }
    }

    #[derive(Default)]
    struct FakeDecoder {
        encoded: Mutex<Vec<Vec<u8>>>,
    }

    impl ImageDecoder for FakeDecoder {
        fn decode(
            &self,
            encoded: &[u8],
            _limits: DecodeLimits,
        ) -> Result<DecodedImage, TextureBackendFailure> {
            let marker = *encoded.first().ok_or(TextureBackendFailure::Rejected)?;
            lock(&self.encoded).push(encoded.to_vec());
            Ok(DecodedImage {
                width: 1,
                height: 1,
                rgba8: vec![marker; 4],
            })
        }
    }

    struct FakeGpu {
        next: AtomicUsize,
    }

    impl Default for FakeGpu {
        fn default() -> Self {
            Self {
                next: AtomicUsize::new(0x1000),
            }
        }
    }

    struct FakeGpuTexture(NonZeroUsize);

    impl GpuTexture for FakeGpuTexture {
        fn srv_address(&self) -> NonZeroUsize {
            self.0
        }
    }

    impl GpuBackend for FakeGpu {
        fn create_rgba8(
            &self,
            _image: &DecodedImage,
        ) -> Result<Box<dyn GpuTexture>, TextureBackendFailure> {
            let address = self.next.fetch_add(0x10, Ordering::Relaxed);
            let address = NonZeroUsize::new(address).ok_or(TextureBackendFailure::Unavailable)?;
            Ok(Box::new(FakeGpuTexture(address)))
        }
    }

    #[derive(Default)]
    struct FakeDownloader {
        targets: Mutex<Vec<String>>,
    }

    impl Downloader for FakeDownloader {
        fn fetch(
            &self,
            target: &DownloadTarget,
            _max_bytes: usize,
        ) -> Result<Vec<u8>, TextureBackendFailure> {
            lock(&self.targets).push(target.as_str().to_owned());
            Ok(vec![0x22])
        }
    }

    #[derive(Default)]
    struct FakeResources {
        requests: Mutex<Vec<(ModuleHandle, u32)>>,
    }

    impl ResourceProvider for FakeResources {
        fn load_png(
            &self,
            module: ModuleHandle,
            resource_id: u32,
            _max_bytes: usize,
        ) -> Result<Vec<u8>, TextureBackendFailure> {
            lock(&self.requests).push((module, resource_id));
            Ok(vec![resource_id as u8])
        }
    }

    struct FixedOverrides;

    impl OverrideProvider for FixedOverrides {
        fn load_override(
            &self,
            identifier: &str,
            _max_bytes: usize,
        ) -> Result<Option<Vec<u8>>, TextureBackendFailure> {
            Ok((identifier == "overridden").then(|| vec![0x77]))
        }
    }

    struct Harness {
        api: TextureApi,
        service: Arc<TextureService>,
        callers: Arc<AddonCallerResolver>,
        owners: Arc<TestOwners>,
        gate: Arc<CallbackGate>,
        failures: Arc<BackendFailures>,
        decoder: Arc<FakeDecoder>,
        downloader: Arc<FakeDownloader>,
        resources: Arc<FakeResources>,
    }

    impl Harness {
        fn new() -> Self {
            Self::with_overrides(Arc::new(NoOverrides))
        }

        fn with_overrides(overrides: Arc<dyn OverrideProvider>) -> Self {
            let gate = Arc::new(CallbackGate::open());
            let owners = Arc::new(TestOwners::new(Arc::clone(&gate)));
            let callers = Arc::new(AddonCallerResolver::new(owners.clone()));
            let failures = Arc::new(BackendFailures::new());
            let boundary = Arc::new(NativeCallBoundary::new(
                Arc::clone(&callers),
                NativeMemoryReader::default(),
                Arc::clone(&failures),
            ));
            let decoder = Arc::new(FakeDecoder::default());
            let downloader = Arc::new(FakeDownloader::default());
            let resources = Arc::new(FakeResources::default());
            let service = Arc::new(
                TextureService::new(
                    TextureConfig::default(),
                    decoder.clone(),
                    Arc::new(FakeGpu::default()),
                    downloader.clone(),
                    overrides,
                    resources.clone(),
                )
                .expect("test texture service should start"),
            );
            let api = TextureApi::new(boundary, service.clone());
            Self {
                api,
                service,
                callers,
                owners,
                gate,
                failures,
                decoder,
                downloader,
                resources,
            }
        }

        fn enter_owner(&self) -> nexus_addon_ffi::AddonOwnerScope {
            self.callers
                .enter_owner_scope(OWNER)
                .expect("test owner should be current")
        }
    }

    #[test]
    fn implements_the_complete_texture_backend_contract() {
        fn assert_backend<T: TextureBackend>() {}
        assert_backend::<TextureApi>();
    }

    #[test]
    fn copied_memory_and_identifier_reach_a_generation_gated_callback() {
        let _serial = lock(&TEST_LOCK);
        lock(&RECEIVED).clear();
        OBSERVED_IN_FLIGHT.store(0, Ordering::Relaxed);
        let harness = Harness::new();
        *lock(&CALLBACK_GATE) = Some(Arc::clone(&harness.gate));
        let _scope = harness.enter_owner();
        let identifier = CString::new("copied-texture").expect("identifier should be valid");
        let mut encoded = vec![0x11];
        harness
            .api
            .load_from_memory(
                identifier.as_ptr(),
                encoded.as_mut_ptr().cast(),
                encoded.len(),
                Some(receive_texture),
            )
            .expect("memory load should queue");
        encoded[0] = 0xFE;
        drop(identifier);

        pump_until(&harness.service, || !lock(&RECEIVED).is_empty());
        assert_eq!(*lock(&harness.decoder.encoded), [vec![0x11]]);
        let received = lock(&RECEIVED);
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].0, "copied-texture");
        assert_ne!(received[0].1, 0);
        assert_eq!(OBSERVED_IN_FLIGHT.load(Ordering::Acquire), 1);
        drop(received);
        assert_eq!(harness.gate.in_flight(), 0);
        *lock(&CALLBACK_GATE) = None;
    }

    #[test]
    fn pending_and_override_paths_do_not_read_unused_native_sources() {
        let _serial = lock(&TEST_LOCK);
        lock(&RECEIVED).clear();
        GATED_CALLS.store(0, Ordering::Relaxed);

        let pending = Harness::new();
        let _scope = pending.enter_owner();
        let identifier = CString::new("pending").expect("identifier should be valid");
        let mut encoded = vec![0x31];
        pending
            .api
            .load_from_memory(
                identifier.as_ptr(),
                encoded.as_mut_ptr().cast(),
                encoded.len(),
                Some(receive_texture),
            )
            .expect("primary request should queue");
        wait_until(|| pending.service.stats().queued_completions > 0);

        pending
            .api
            .load_from_memory(
                identifier.as_ptr(),
                std::ptr::null_mut(),
                1,
                Some(gated_texture),
            )
            .expect("legacy pending request should ignore source and later callback");
        let _report = pending.service.advance();
        assert_eq!(lock(&RECEIVED).len(), 1);
        assert_eq!(GATED_CALLS.load(Ordering::Relaxed), 0);

        let rejected = CString::new("rejected").expect("identifier should be valid");
        assert_eq!(
            pending.api.load_from_memory(
                rejected.as_ptr(),
                std::ptr::null_mut(),
                1,
                Some(gated_texture),
            ),
            Err(BackendOperationError::Boundary(
                CallBoundaryError::NativeMemory
            ))
        );
        let _report = pending.service.advance();
        assert_eq!(GATED_CALLS.load(Ordering::Relaxed), 0);

        let overridden = Harness::with_overrides(Arc::new(FixedOverrides));
        let _override_scope = overridden.enter_owner();
        let identifier = CString::new("overridden").expect("identifier should be valid");
        assert!(
            overridden
                .api
                .get_or_create_from_memory(identifier.as_ptr(), std::ptr::null_mut(), 1)
                .expect("override should bypass the unused source")
                .is_null()
        );
        pump_until(&overridden.service, || {
            overridden.service.get("overridden").is_some()
        });
        assert_eq!(*lock(&overridden.decoder.encoded), [vec![0x77]]);
    }

    #[test]
    fn first_miss_is_null_cached_get_is_stable_and_load_shadows() {
        let _serial = lock(&TEST_LOCK);
        let harness = Harness::new();
        let _scope = harness.enter_owner();
        let identifier = CString::new("replaceable").expect("identifier should be valid");
        let shadow = CString::new("replaceable_1").expect("identifier should be valid");
        let mut first_bytes = vec![1];
        let first_miss = harness
            .api
            .get_or_create_from_memory(
                identifier.as_ptr(),
                first_bytes.as_mut_ptr().cast(),
                first_bytes.len(),
            )
            .expect("first request should queue");
        assert!(first_miss.is_null());
        pump_until(&harness.service, || {
            harness.service.get("replaceable").is_some()
        });
        let first = harness
            .api
            .get(identifier.as_ptr())
            .expect("registered texture should be readable");
        assert!(!first.is_null());

        let mut ignored_bytes = vec![2];
        let cached = harness
            .api
            .get_or_create_from_memory(
                identifier.as_ptr(),
                ignored_bytes.as_mut_ptr().cast(),
                ignored_bytes.len(),
            )
            .expect("cached request should succeed");
        assert_eq!(cached, first);
        let cached_without_a_source = harness
            .api
            .get_or_create_from_memory(identifier.as_ptr(), std::ptr::null_mut(), 1)
            .expect("a cache hit must not inspect the source pointer");
        assert_eq!(cached_without_a_source, first);

        let mut replacement_bytes = vec![3];
        harness
            .api
            .load_from_memory(
                identifier.as_ptr(),
                replacement_bytes.as_mut_ptr().cast(),
                replacement_bytes.len(),
                None,
            )
            .expect("explicit load should shadow and queue");
        assert!(
            harness
                .api
                .get(identifier.as_ptr())
                .expect("shadowed identifier lookup should succeed")
                .is_null()
        );
        assert_eq!(
            harness
                .api
                .get(shadow.as_ptr())
                .expect("shadow lookup should succeed"),
            first
        );
        pump_until(&harness.service, || {
            harness.service.get("replaceable").is_some()
        });
        let replacement = harness
            .api
            .get(identifier.as_ptr())
            .expect("replacement should be readable");
        assert!(!replacement.is_null());
        assert_ne!(replacement, first);
    }

    #[test]
    fn url_pair_and_resource_arguments_preserve_legacy_mapping() {
        let _serial = lock(&TEST_LOCK);
        let harness = Harness::new();
        let _scope = harness.enter_owner();
        let url_identifier = CString::new("remote").expect("identifier should be valid");
        let remote = CString::new("https://example.invalid").expect("remote should be valid");
        let endpoint = CString::new("/sprite.png?q=1").expect("endpoint should be valid");
        assert!(harness
            .api
            .get_or_create_from_url(
                url_identifier.as_ptr(),
                remote.as_ptr(),
                endpoint.as_ptr(),
            )
            .expect("URL request should queue")
            .is_null());
        wait_until(|| !lock(&harness.downloader.targets).is_empty());
        assert_eq!(
            *lock(&harness.downloader.targets),
            ["https://example.invalid/sprite.png?q=1"]
        );
        pump_until(&harness.service, || harness.service.get("remote").is_some());

        let resource_identifier = CString::new("resource").expect("identifier should be valid");
        let module_pointer = std::ptr::dangling_mut::<c_void>();
        let expected_module = unsafe {
            // SAFETY: the fake provider treats the non-null sentinel as opaque.
            ModuleHandle::from_hmodule(module_pointer)
        }
        .expect("sentinel module should be non-null");
        assert!(
            harness
                .api
                .get_or_create_from_resource(resource_identifier.as_ptr(), 37, module_pointer)
                .expect("resource request should queue")
                .is_null()
        );
        assert_eq!(*lock(&harness.resources.requests), [(expected_module, 37)]);
        pump_until(&harness.service, || {
            harness.service.get("resource").is_some()
        });
    }

    #[test]
    fn closed_gate_and_closing_generation_never_reach_foreign_code() {
        let _serial = lock(&TEST_LOCK);
        GATED_CALLS.store(0, Ordering::Relaxed);
        let harness = Harness::new();
        let _scope = harness.enter_owner();
        let identifier = CString::new("gated").expect("identifier should be valid");
        let mut encoded = vec![4];
        harness
            .api
            .load_from_memory(
                identifier.as_ptr(),
                encoded.as_mut_ptr().cast(),
                encoded.len(),
                Some(gated_texture),
            )
            .expect("callback request should queue");
        wait_until(|| harness.service.stats().queued_completions > 0);
        harness.gate.close();
        let _report = harness.service.advance();
        assert_eq!(GATED_CALLS.load(Ordering::Relaxed), 0);

        let closing = Harness::new();
        let _closing_scope = closing.enter_owner();
        closing.owners.close_on_gate.store(true, Ordering::Release);
        let identifier = CString::new("closing").expect("identifier should be valid");
        let mut encoded = vec![5];
        assert_eq!(
            closing.api.load_from_memory(
                identifier.as_ptr(),
                encoded.as_mut_ptr().cast(),
                encoded.len(),
                Some(gated_texture),
            ),
            Err(BackendOperationError::Boundary(
                CallBoundaryError::CallerAttribution
            ))
        );
        wait_until(|| closing.service.stats().queued_completions > 0);
        let _report = closing.service.advance();
        assert!(closing.service.get("closing").is_none());
        assert_eq!(GATED_CALLS.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn synchronous_load_failure_reports_null_through_the_generation_gate() {
        let _serial = lock(&TEST_LOCK);
        lock(&RECEIVED).clear();
        let harness = Harness::new();
        let _scope = harness.enter_owner();
        let identifier = CString::new("missing-resource").expect("identifier should be valid");

        assert_eq!(
            harness.api.load_from_resource(
                identifier.as_ptr(),
                41,
                std::ptr::null_mut(),
                Some(receive_texture),
            ),
            Err(BackendOperationError::ServiceRejected)
        );
        assert_eq!(*lock(&RECEIVED), [("missing-resource".to_owned(), 0)]);
        assert!(harness.service.get("missing-resource").is_none());

        lock(&RECEIVED).clear();
        let closed = Harness::new();
        let _closed_scope = closed.enter_owner();
        closed.gate.close();
        assert_eq!(
            closed.api.load_from_resource(
                identifier.as_ptr(),
                42,
                std::ptr::null_mut(),
                Some(receive_texture),
            ),
            Err(BackendOperationError::ServiceRejected)
        );
        assert!(lock(&RECEIVED).is_empty());
    }

    #[test]
    fn invalid_service_input_and_foreign_callbacks_fail_closed() {
        let _serial = lock(&TEST_LOCK);
        let harness = Harness::new();
        let _scope = harness.enter_owner();
        let empty = CString::new("").expect("empty C string should be valid");
        let mut encoded = vec![1];
        assert_eq!(
            harness.api.get_or_create_from_memory(
                empty.as_ptr(),
                encoded.as_mut_ptr().cast(),
                encoded.len(),
            ),
            Err(BackendOperationError::ServiceRejected)
        );
        assert_eq!(
            harness.failures.snapshot(),
            BackendFailureSnapshot {
                service_rejected: 1,
                ..BackendFailureSnapshot::default()
            }
        );

        let identifier = CString::new("foreign").expect("identifier should be valid");
        assert_eq!(
            harness.api.load_from_memory(
                identifier.as_ptr(),
                encoded.as_mut_ptr().cast(),
                encoded.len(),
                Some(foreign_texture),
            ),
            Err(BackendOperationError::Boundary(
                CallBoundaryError::CallerAttribution
            ))
        );
        assert_eq!(
            harness.failures.snapshot(),
            BackendFailureSnapshot {
                caller_attribution: 1,
                service_rejected: 1,
                ..BackendFailureSnapshot::default()
            }
        );
    }

    fn pump_until(service: &TextureService, predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let _report = service.advance();
            if predicate() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "texture service did not reach the expected state"
            );
            std::thread::yield_now();
        }
    }

    fn wait_until(predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !predicate() {
            assert!(
                Instant::now() < deadline,
                "texture worker did not reach the expected state"
            );
            std::thread::yield_now();
        }
    }

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
