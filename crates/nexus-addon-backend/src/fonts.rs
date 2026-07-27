use core::ffi::{c_char, c_void};
use core::fmt;
use core::mem::{offset_of, size_of};
use std::collections::{BTreeMap, VecDeque};
use std::ffi::{CStr, CString};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use nexus_abi::ReceiveFont;
use nexus_core::{CallbackGate, OwnerToken};
use nexus_imgui_compat::sys;
use nexus_ui_services::{
    FontConfig, FontGetResult, FontHandle, FontRegistration, OwnerId, ResourceFont, SubscriptionId,
};

use crate::{
    BackendFailure, BackendOperationError, FontBackend, NativeCallBoundary, RequiredServiceResult,
};

/// Maximum copied `ImFontConfig::GlyphRanges` length, including its terminator.
///
/// Dear ImGui represents coverage as compact start/end pairs, so 2,048 pairs
/// is far beyond the built-in tables while keeping hostile native scans bounded.
const MAX_GLYPH_RANGE_UNITS: usize = 4_097;

/// Maximum callback notifications retained while publication or dispatch is blocked.
const MAX_PENDING_FONT_CALLBACKS: usize = 64;

/// Thread-transferable callback accepted by the render-thread font service.
///
/// A production implementation may erase the `Send` bound when moving this
/// closure into [`nexus_ui_services::FontManager`] on the render thread.
pub type SendFontCallback = Box<dyn FnMut(&CStr, Option<FontHandle>) + Send + 'static>;

/// Minimal process-safe facade over the render-thread-bound font manager.
///
/// Every method must synchronously marshal its owned arguments to the render
/// thread and return only after the corresponding `FontManager` call finishes.
/// Calls already originating on that render thread must execute inline rather
/// than waiting on their own queue.
/// Errors must be atomic: a rejected call cannot leave a partial registration.
/// `cleanup_owner` is a barrier over all earlier calls for that owner and must
/// return only after its manager registrations and callbacks are removed.
pub trait RenderFontService: Send + Sync + 'static {
    /// Implements the existing manager's immediate `get` and subscription.
    fn get(
        &self,
        owner: OwnerId,
        identifier: String,
        callback: SendFontCallback,
    ) -> RequiredServiceResult<FontGetResult>;

    /// Releases one exact manager subscription.
    fn release(
        &self,
        identifier: String,
        subscription: SubscriptionId,
    ) -> RequiredServiceResult<bool>;

    /// Registers an owned file-backed font request.
    fn add_from_file(
        &self,
        owner: OwnerId,
        identifier: String,
        size: f32,
        filename: PathBuf,
        callback: Option<SendFontCallback>,
        config: FontConfig,
    ) -> RequiredServiceResult<FontRegistration>;

    /// Registers an owned resource-backed font request.
    fn add_from_resource(
        &self,
        owner: OwnerId,
        identifier: String,
        size: f32,
        resource: ResourceFont,
        callback: Option<SendFontCallback>,
        config: FontConfig,
    ) -> RequiredServiceResult<FontRegistration>;

    /// Registers an owned memory-backed font request.
    fn add_from_memory(
        &self,
        owner: OwnerId,
        identifier: String,
        size: f32,
        data: Vec<u8>,
        callback: Option<SendFontCallback>,
        config: FontConfig,
    ) -> RequiredServiceResult<FontRegistration>;

    /// Resizes one manager font.
    fn resize(&self, identifier: String, size: f32) -> RequiredServiceResult<bool>;

    /// Removes one exact add-on generation and drains its queued manager work.
    fn cleanup_owner(&self, owner: OwnerId) -> RequiredServiceResult<usize>;

    /// Removes only one exact generation's callback subscribers.
    ///
    /// This is the pre-drain phase barrier. Font resources must stay available
    /// to callbacks that are still draining, so implementations must not remove
    /// font entries or invalidate the atlas here.
    fn cleanup_owner_callbacks(&self, owner: OwnerId) -> RequiredServiceResult<usize>;

    /// Removes one exact generation's font resources after the gate drained.
    ///
    /// This is the post-drain phase barrier. It sweeps entries that only became
    /// unreferenced during [`Self::cleanup_owner_callbacks`].
    fn cleanup_owner_resources(&self, owner: OwnerId) -> RequiredServiceResult<usize>;
}

/// Caller-attributed implementation of the native font ABI.
pub struct FontApi {
    boundary: Arc<NativeCallBoundary>,
    service: Arc<dyn RenderFontService>,
    subscriptions: Mutex<SubscriptionMap>,
}

impl FontApi {
    /// Creates a font adapter over a render-thread service facade.
    #[must_use]
    pub fn new(boundary: Arc<NativeCallBoundary>, service: Arc<dyn RenderFontService>) -> Self {
        Self {
            boundary,
            service,
            subscriptions: Mutex::new(BTreeMap::new()),
        }
    }

    /// Gets a font and retains the callback only when the manager subscribes it.
    pub fn get(
        &self,
        identifier: *const c_char,
        callback: Option<ReceiveFont>,
    ) -> RequiredServiceResult<()> {
        let (owner, callback) = self.resolve_callback(callback)?;
        let identifier = self.boundary.snapshot_identifier(identifier)?.into_string();
        let Some(callback) = callback else {
            return Ok(());
        };
        let result = self.service_result(self.service.get(
            owner.into(),
            identifier.clone(),
            callback.service_callback(),
        ))?;
        self.finish_publication(owner, identifier, callback, result.subscription)
    }

    /// Releases every subscription created with this exact callback address.
    pub fn release(
        &self,
        identifier: *const c_char,
        callback: Option<ReceiveFont>,
    ) -> RequiredServiceResult<()> {
        let (owner, callback) = self.resolve_callback(callback)?;
        let identifier = self.boundary.snapshot_identifier(identifier)?.into_string();
        let Some(callback) = callback else {
            return Ok(());
        };
        let key = SubscriptionKey::new(owner.into(), identifier.clone(), callback.address());
        let receipts = mutex_lock(&self.subscriptions)
            .remove(&key)
            .unwrap_or_default();
        if receipts.subscriptions.is_empty() {
            return Ok(());
        }
        let Some(_admission) = callback.gate.try_enter() else {
            return Ok(());
        };
        let mut rejected = Vec::new();
        for subscription in receipts.subscriptions {
            if self
                .service
                .release(identifier.clone(), subscription)
                .is_err()
            {
                rejected.push(subscription);
            }
        }
        if rejected.is_empty() {
            return Ok(());
        }
        if !callback.gate.is_open() {
            return Err(self.service_rejected());
        }
        let mut subscriptions = mutex_lock(&self.subscriptions);
        let restored = subscriptions.entry(key).or_default();
        restored.subscriptions.extend(rejected);
        restored.publication = Some(Arc::downgrade(&callback));
        drop(subscriptions);
        Err(self.service_rejected())
    }

    /// Adds a file-backed font after copying every native argument.
    pub fn add_from_file(
        &self,
        identifier: *const c_char,
        size: f32,
        filename: *const c_char,
        callback: Option<ReceiveFont>,
        config: *mut c_void,
    ) -> RequiredServiceResult<()> {
        let (owner, callback) = self.resolve_callback(callback)?;
        let identifier = self.boundary.snapshot_identifier(identifier)?.into_string();
        let filename = PathBuf::from(self.boundary.snapshot_path(filename)?.into_string());
        let config = self.snapshot_config(config)?;
        let service_callback = callback.as_ref().map(NativeFontCallback::service_callback);
        let registration = self.service_result(self.service.add_from_file(
            owner.into(),
            identifier.clone(),
            size,
            filename,
            service_callback,
            config,
        ))?;
        self.finish_registration(owner, identifier, callback, registration)
    }

    /// Adds a resource-backed font without retaining its native module pointer.
    pub fn add_from_resource(
        &self,
        identifier: *const c_char,
        size: f32,
        resource_id: u32,
        module: *mut c_void,
        callback: Option<ReceiveFont>,
        config: *mut c_void,
    ) -> RequiredServiceResult<()> {
        let (owner, callback) = self.resolve_callback(callback)?;
        let identifier = self.boundary.snapshot_identifier(identifier)?.into_string();
        let config = self.snapshot_config(config)?;
        let service_callback = callback.as_ref().map(NativeFontCallback::service_callback);
        let registration = self.service_result(self.service.add_from_resource(
            owner.into(),
            identifier.clone(),
            size,
            ResourceFont {
                module: module as usize,
                resource_id,
            },
            service_callback,
            config,
        ))?;
        self.finish_registration(owner, identifier, callback, registration)
    }

    /// Adds a memory-backed font after copying its complete byte range.
    pub fn add_from_memory(
        &self,
        identifier: *const c_char,
        size: f32,
        data: *mut c_void,
        data_size: usize,
        callback: Option<ReceiveFont>,
        config: *mut c_void,
    ) -> RequiredServiceResult<()> {
        let (owner, callback) = self.resolve_callback(callback)?;
        let identifier = self.boundary.snapshot_identifier(identifier)?.into_string();
        let data = self
            .boundary
            .snapshot_buffer(data.cast_const(), data_size)?
            .into_vec();
        let config = self.snapshot_config(config)?;
        let service_callback = callback.as_ref().map(NativeFontCallback::service_callback);
        let registration = self.service_result(self.service.add_from_memory(
            owner.into(),
            identifier.clone(),
            size,
            data,
            service_callback,
            config,
        ))?;
        self.finish_registration(owner, identifier, callback, registration)
    }

    /// Resizes one font after authenticating and copying its identifier.
    pub fn resize(&self, identifier: *const c_char, size: f32) -> RequiredServiceResult<()> {
        self.boundary.resolve_owner(None)?;
        let identifier = self.boundary.snapshot_identifier(identifier)?.into_string();
        self.service_result(self.service.resize(identifier, size))?;
        Ok(())
    }

    /// Removes and drains all registrations for one exact add-on generation.
    ///
    /// Runtime cleanup should close and drain the generation's callback gate
    /// before calling this barrier.
    pub fn cleanup_owner(&self, owner: OwnerToken) -> RequiredServiceResult<usize> {
        let owner_id = OwnerId::from(owner);
        self.fence_owner_publications(owner_id);
        self.service_result(self.service.cleanup_owner(owner_id))
    }

    /// Fences one exact generation's callbacks while keeping its resources.
    ///
    /// Runtime cleanup calls this *before* draining the generation's callback
    /// gate. Font resources stay registered so callbacks still in flight remain
    /// valid; [`Self::cleanup_owner_resources`] releases them after the drain.
    pub fn cleanup_owner_callbacks(&self, owner: OwnerToken) -> RequiredServiceResult<usize> {
        let owner_id = OwnerId::from(owner);
        self.fence_owner_publications(owner_id);
        self.service_result(self.service.cleanup_owner_callbacks(owner_id))
    }

    /// Releases one exact generation's font resources after the gate drained.
    ///
    /// Runtime cleanup calls this *after* [`Self::cleanup_owner_callbacks`] and
    /// the callback-gate drain.
    pub fn cleanup_owner_resources(&self, owner: OwnerToken) -> RequiredServiceResult<usize> {
        let owner_id = OwnerId::from(owner);
        self.service_result(self.service.cleanup_owner_resources(owner_id))
    }

    /// Aborts staged publications for one generation and forgets its receipts.
    ///
    /// Aborting is what actually fences the generation. Closing admission alone
    /// only prevents *new* enqueues: work the manager already accepted remains
    /// dispatchable, so a queued publication could still reach native code
    /// after the barrier returned. Aborting clears that pending work and latches
    /// the queue closed.
    ///
    /// This runs before any service call, so a rejected service leaves the
    /// generation fenced rather than half-open. The publication handles are
    /// collected and the lock released before aborting, because `abort` takes
    /// each callback's own queue lock.
    fn fence_owner_publications(&self, owner_id: OwnerId) {
        let fenced = {
            let mut subscriptions = mutex_lock(&self.subscriptions);
            let fenced = subscriptions
                .iter()
                .filter(|(key, _)| key.owner == owner_id)
                .filter_map(|(_, receipts)| receipts.publication.clone())
                .collect::<Vec<_>>();
            subscriptions.retain(|key, _| key.owner != owner_id);
            fenced
        };
        for publication in fenced {
            if let Some(publication) = publication.upgrade() {
                publication.abort();
            }
        }
    }

    fn finish_registration(
        &self,
        owner: OwnerToken,
        identifier: String,
        callback: Option<Arc<NativeFontCallback>>,
        registration: FontRegistration,
    ) -> RequiredServiceResult<()> {
        if callback.is_some() != registration.subscription.is_some() {
            if let Some(callback) = callback {
                callback.abort();
            }
            self.rollback_owner(owner);
            return Err(self.service_rejected());
        }
        match callback {
            Some(callback) => {
                self.finish_publication(owner, identifier, callback, registration.subscription)
            }
            None => self.finish_without_callback(owner),
        }
    }

    fn finish_publication(
        &self,
        owner: OwnerToken,
        identifier: String,
        callback: Arc<NativeFontCallback>,
        subscription: Option<SubscriptionId>,
    ) -> RequiredServiceResult<()> {
        if let Some(subscription) = subscription {
            let key = SubscriptionKey::new(owner.into(), identifier, callback.address());
            let mut subscriptions = mutex_lock(&self.subscriptions);
            let receipts = subscriptions.entry(key).or_default();
            receipts.subscriptions.push(subscription);
            receipts.publication = Some(Arc::downgrade(&callback));
        }
        if let Err(error) = self.boundary.validate_current_owner(owner) {
            callback.abort();
            self.rollback_owner(owner);
            return Err(error.into());
        }
        callback.publish();
        Ok(())
    }

    fn finish_without_callback(&self, owner: OwnerToken) -> RequiredServiceResult<()> {
        if let Err(error) = self.boundary.validate_current_owner(owner) {
            self.rollback_owner(owner);
            return Err(error.into());
        }
        Ok(())
    }

    fn rollback_owner(&self, owner: OwnerToken) {
        let owner_id = OwnerId::from(owner);
        if self.service.cleanup_owner(owner_id).is_err() {
            self.boundary
                .failures()
                .record(BackendFailure::ServiceRejected);
        }
        mutex_lock(&self.subscriptions).retain(|key, _| key.owner != owner_id);
    }

    fn resolve_callback(
        &self,
        callback: Option<ReceiveFont>,
    ) -> RequiredServiceResult<(OwnerToken, Option<Arc<NativeFontCallback>>)> {
        let Some(callback) = callback else {
            return Ok((self.boundary.resolve_owner(None)?, None));
        };
        let owner = self
            .boundary
            .resolve_owner_for_address(callback as *const () as *const c_void)?;
        let gate = self.boundary.callback_gate_for_current(owner)?;
        Ok((
            owner,
            Some(Arc::new(NativeFontCallback::new(
                callback,
                gate,
                Arc::clone(&self.boundary),
            ))),
        ))
    }

    fn snapshot_config(&self, config: *mut c_void) -> RequiredServiceResult<FontConfig> {
        if config.is_null() {
            return Ok(FontConfig::default());
        }
        let bytes = self
            .boundary
            .snapshot_buffer(config.cast_const(), size_of::<sys::ImFontConfig>())?
            .into_vec();
        let pixel_snap_h = read_bool(&bytes, offset_of!(sys::ImFontConfig, PixelSnapH))
            .ok_or_else(|| self.service_rejected())?;
        let merge_mode = read_bool(&bytes, offset_of!(sys::ImFontConfig, MergeMode))
            .ok_or_else(|| self.service_rejected())?;
        let glyph_ranges_address = read_usize(&bytes, offset_of!(sys::ImFontConfig, GlyphRanges))
            .ok_or_else(|| self.service_rejected())?;
        Ok(FontConfig {
            font_no: read_i32(&bytes, offset_of!(sys::ImFontConfig, FontNo))
                .ok_or_else(|| self.service_rejected())?,
            oversample_h: read_i32(&bytes, offset_of!(sys::ImFontConfig, OversampleH))
                .ok_or_else(|| self.service_rejected())?,
            oversample_v: read_i32(&bytes, offset_of!(sys::ImFontConfig, OversampleV))
                .ok_or_else(|| self.service_rejected())?,
            pixel_snap_h,
            glyph_extra_spacing: [
                read_f32(&bytes, offset_of!(sys::ImFontConfig, GlyphExtraSpacing))
                    .ok_or_else(|| self.service_rejected())?,
                read_f32(
                    &bytes,
                    offset_of!(sys::ImFontConfig, GlyphExtraSpacing) + size_of::<f32>(),
                )
                .ok_or_else(|| self.service_rejected())?,
            ],
            glyph_offset: [
                read_f32(&bytes, offset_of!(sys::ImFontConfig, GlyphOffset))
                    .ok_or_else(|| self.service_rejected())?,
                read_f32(
                    &bytes,
                    offset_of!(sys::ImFontConfig, GlyphOffset) + size_of::<f32>(),
                )
                .ok_or_else(|| self.service_rejected())?,
            ],
            glyph_ranges: self.snapshot_glyph_ranges(glyph_ranges_address)?,
            glyph_min_advance_x: read_f32(&bytes, offset_of!(sys::ImFontConfig, GlyphMinAdvanceX))
                .ok_or_else(|| self.service_rejected())?,
            glyph_max_advance_x: read_f32(&bytes, offset_of!(sys::ImFontConfig, GlyphMaxAdvanceX))
                .ok_or_else(|| self.service_rejected())?,
            merge_mode,
            rasterizer_flags: read_u32(&bytes, offset_of!(sys::ImFontConfig, RasterizerFlags))
                .ok_or_else(|| self.service_rejected())?,
            rasterizer_multiply: read_f32(
                &bytes,
                offset_of!(sys::ImFontConfig, RasterizerMultiply),
            )
            .ok_or_else(|| self.service_rejected())?,
            ellipsis_char: read_u16(&bytes, offset_of!(sys::ImFontConfig, EllipsisChar))
                .ok_or_else(|| self.service_rejected())?,
        })
    }

    fn snapshot_glyph_ranges(&self, address: usize) -> RequiredServiceResult<Vec<u16>> {
        if address == 0 {
            return Ok(Vec::new());
        }
        let mut ranges = Vec::new();
        for index in 0..MAX_GLYPH_RANGE_UNITS {
            let offset = index
                .checked_mul(size_of::<u16>())
                .and_then(|offset| address.checked_add(offset))
                .ok_or_else(|| self.service_rejected())?;
            let bytes = self
                .boundary
                .snapshot_buffer(offset as *const c_void, size_of::<u16>())?
                .into_vec();
            let unit = read_u16(&bytes, 0).ok_or_else(|| self.service_rejected())?;
            ranges.push(unit);
            if unit == 0 {
                return Ok(ranges);
            }
        }
        Err(self.service_rejected())
    }

    fn service_result<T>(&self, result: RequiredServiceResult<T>) -> RequiredServiceResult<T> {
        result.map_err(|_| self.service_rejected())
    }

    fn service_rejected(&self) -> BackendOperationError {
        self.boundary
            .failures()
            .record(BackendFailure::ServiceRejected);
        BackendOperationError::ServiceRejected
    }
}

impl FontBackend for FontApi {
    fn get(
        &self,
        identifier: *const c_char,
        callback: Option<ReceiveFont>,
    ) -> RequiredServiceResult<()> {
        FontApi::get(self, identifier, callback)
    }

    fn release(
        &self,
        identifier: *const c_char,
        callback: Option<ReceiveFont>,
    ) -> RequiredServiceResult<()> {
        FontApi::release(self, identifier, callback)
    }

    fn add_from_file(
        &self,
        identifier: *const c_char,
        size: f32,
        filename: *const c_char,
        callback: Option<ReceiveFont>,
        config: *mut c_void,
    ) -> RequiredServiceResult<()> {
        FontApi::add_from_file(self, identifier, size, filename, callback, config)
    }

    fn add_from_resource(
        &self,
        identifier: *const c_char,
        size: f32,
        resource_id: u32,
        module: *mut c_void,
        callback: Option<ReceiveFont>,
        config: *mut c_void,
    ) -> RequiredServiceResult<()> {
        FontApi::add_from_resource(
            self,
            identifier,
            size,
            resource_id,
            module,
            callback,
            config,
        )
    }

    fn add_from_memory(
        &self,
        identifier: *const c_char,
        size: f32,
        data: *mut c_void,
        data_size: usize,
        callback: Option<ReceiveFont>,
        config: *mut c_void,
    ) -> RequiredServiceResult<()> {
        FontApi::add_from_memory(self, identifier, size, data, data_size, callback, config)
    }

    fn resize(&self, identifier: *const c_char, size: f32) -> RequiredServiceResult<()> {
        FontApi::resize(self, identifier, size)
    }
}

impl fmt::Debug for FontApi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let subscriptions = mutex_lock(&self.subscriptions)
            .values()
            .map(|receipts| receipts.subscriptions.len())
            .sum::<usize>();
        formatter
            .debug_struct("FontApi")
            .field("subscriptions", &subscriptions)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct SubscriptionKey {
    owner: OwnerId,
    identifier: String,
    callback: usize,
}

/// Retained receipts for one exact `(owner, identifier, callback)` key.
///
/// The weak publication handle is what makes callback-phase cleanup able to
/// *abort* staged work rather than merely stop admitting new work. It is weak
/// on purpose: the manager's subscriber owns the callback, so cleanup must
/// never keep a native function pointer reachable past service teardown.
#[derive(Default, Debug)]
struct SubscriptionReceipts {
    subscriptions: Vec<SubscriptionId>,
    publication: Option<Weak<NativeFontCallback>>,
}

type SubscriptionMap = BTreeMap<SubscriptionKey, SubscriptionReceipts>;

impl SubscriptionKey {
    fn new(owner: OwnerId, identifier: String, callback: usize) -> Self {
        Self {
            owner,
            identifier,
            callback,
        }
    }
}

struct NativeFontCallback {
    callback: ReceiveFont,
    gate: Arc<CallbackGate>,
    boundary: Arc<NativeCallBoundary>,
    queue: Mutex<NativeCallbackQueue>,
}

impl NativeFontCallback {
    fn new(
        callback: ReceiveFont,
        gate: Arc<CallbackGate>,
        boundary: Arc<NativeCallBoundary>,
    ) -> Self {
        Self {
            callback,
            gate,
            boundary,
            queue: Mutex::new(NativeCallbackQueue::default()),
        }
    }

    fn address(&self) -> usize {
        self.callback as *const () as usize
    }

    fn service_callback(self: &Arc<Self>) -> SendFontCallback {
        let callback = Arc::clone(self);
        Box::new(move |identifier, font| callback.enqueue(identifier, font))
    }

    fn publish(&self) {
        let should_drain = {
            let mut queue = mutex_lock(&self.queue);
            if queue.aborted {
                return;
            }
            queue.published = true;
            if queue.dispatching || queue.pending.is_empty() {
                false
            } else {
                queue.dispatching = true;
                true
            }
        };
        if should_drain {
            self.drain();
        }
    }

    fn abort(&self) {
        let mut queue = mutex_lock(&self.queue);
        queue.aborted = true;
        queue.pending.clear();
    }

    fn enqueue(&self, identifier: &CStr, font: Option<FontHandle>) {
        let pending = PendingFontCallback {
            identifier: identifier.to_owned(),
            font: font.map_or(0, |font| font.as_ptr() as usize),
        };
        let (should_drain, overflowed) = {
            let mut queue = mutex_lock(&self.queue);
            if queue.aborted {
                return;
            }
            let overflowed = queue.pending.len() >= MAX_PENDING_FONT_CALLBACKS;
            if overflowed {
                let _dropped = queue.pending.pop_front();
            }
            queue.pending.push_back(pending);
            if queue.published && !queue.dispatching {
                queue.dispatching = true;
                (true, overflowed)
            } else {
                (false, overflowed)
            }
        };
        if overflowed {
            self.boundary
                .failures()
                .record(BackendFailure::ServiceRejected);
        }
        if should_drain {
            self.drain();
        }
    }

    fn drain(&self) {
        loop {
            let pending = {
                let mut queue = mutex_lock(&self.queue);
                if queue.aborted {
                    queue.pending.clear();
                    queue.dispatching = false;
                    return;
                }
                let Some(pending) = queue.pending.pop_front() else {
                    queue.dispatching = false;
                    return;
                };
                pending
            };
            let Some(_guard) = self.gate.try_enter() else {
                continue;
            };
            // SAFETY: the identifier is an owned CString for this call, the
            // font is an opaque manager pointer, and the exact owner gate is
            // held for the complete foreign invocation.
            unsafe {
                (self.callback)(pending.identifier.as_ptr(), pending.font as *mut c_void);
            }
        }
    }
}

#[derive(Default)]
struct NativeCallbackQueue {
    published: bool,
    aborted: bool,
    dispatching: bool,
    pending: VecDeque<PendingFontCallback>,
}

struct PendingFontCallback {
    identifier: CString,
    font: usize,
}

fn read_bytes<const N: usize>(bytes: &[u8], offset: usize) -> Option<[u8; N]> {
    let end = offset.checked_add(N)?;
    bytes.get(offset..end)?.try_into().ok()
}

fn read_bool(bytes: &[u8], offset: usize) -> Option<bool> {
    match *bytes.get(offset)? {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

fn read_i32(bytes: &[u8], offset: usize) -> Option<i32> {
    Some(i32::from_ne_bytes(read_bytes(bytes, offset)?))
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_ne_bytes(read_bytes(bytes, offset)?))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(read_bytes(bytes, offset)?))
}

fn read_usize(bytes: &[u8], offset: usize) -> Option<usize> {
    Some(usize::from_ne_bytes(read_bytes(bytes, offset)?))
}

fn read_f32(bytes: &[u8], offset: usize) -> Option<f32> {
    Some(f32::from_ne_bytes(read_bytes(bytes, offset)?))
}

fn mutex_lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use core::ffi::{c_char, c_void};
    use core::num::NonZeroUsize;
    use core::sync::atomic::{AtomicBool, Ordering};
    use std::collections::VecDeque;
    use std::ffi::{CStr, CString};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, MutexGuard};

    use nexus_addon_ffi::{AddonCallerResolver, AddressOwnerResolver};
    use nexus_core::{CallbackGate, OwnerToken};
    use nexus_imgui_compat::sys;
    use nexus_native_memory::NativeMemoryReader;
    use nexus_ui_services::{
        FontAtlasBackend, FontBackendError, FontBuildRequest, FontConfig, FontGetResult,
        FontHandle, FontManager, FontRegistration, OwnerId, ResourceFont, SubscriptionId,
    };

    use super::{
        FontApi, MAX_PENDING_FONT_CALLBACKS, NativeFontCallback, RenderFontService,
        SendFontCallback, SubscriptionKey, SubscriptionReceipts,
    };
    use crate::{
        BackendFailures, BackendOperationError, CallBoundaryError, FontBackend, NativeCallBoundary,
        RequiredServiceResult,
    };

    const OWNER: OwnerToken = OwnerToken {
        signature: 0xF017,
        generation: 9,
    };
    const OTHER_OWNER: OwnerToken = OwnerToken {
        signature: 0xF018,
        generation: 2,
    };

    static CALLBACKS: Mutex<Vec<(String, usize)>> = Mutex::new(Vec::new());
    static CALLBACK_TEST_LOCK: Mutex<()> = Mutex::new(());

    unsafe extern "C" fn record_font(identifier: *const c_char, font: *mut c_void) {
        // SAFETY: the adapter retains a terminated identifier for this call.
        let identifier = unsafe { CStr::from_ptr(identifier) };
        lock(&CALLBACKS).push((identifier.to_string_lossy().into_owned(), font as usize));
    }

    unsafe extern "C" fn foreign_font(_identifier: *const c_char, _font: *mut c_void) {}

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
            match address.get() {
                address if address == record_font as *const () as usize => Some(OWNER),
                address if address == foreign_font as *const () as usize => Some(OTHER_OWNER),
                _ => None,
            }
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

    #[derive(Clone, Debug, PartialEq)]
    struct MemoryCall {
        owner: OwnerId,
        identifier: String,
        size: f32,
        data: Vec<u8>,
        config: FontConfig,
    }

    #[derive(Clone, Debug, PartialEq)]
    struct FileCall {
        owner: OwnerId,
        identifier: String,
        size: f32,
        filename: PathBuf,
        config: FontConfig,
    }

    #[derive(Clone, Debug, PartialEq)]
    struct ResourceCall {
        owner: OwnerId,
        identifier: String,
        size: f32,
        resource_id: u32,
        module: usize,
        config: FontConfig,
    }

    struct RecordingService {
        gate: Arc<CallbackGate>,
        release_observed_gate: AtomicBool,
        reject_and_close_release: AtomicBool,
        tokens: Mutex<VecDeque<SubscriptionId>>,
        callbacks: Mutex<Vec<SendFontCallback>>,
        memory: Mutex<Vec<MemoryCall>>,
        files: Mutex<Vec<FileCall>>,
        resources: Mutex<Vec<ResourceCall>>,
        releases: Mutex<Vec<(String, SubscriptionId)>>,
        cleanups: Mutex<Vec<OwnerId>>,
        callback_cleanups: Mutex<Vec<OwnerId>>,
        resource_cleanups: Mutex<Vec<OwnerId>>,
        resizes: Mutex<Vec<(String, f32)>>,
    }

    impl RecordingService {
        fn new(gate: Arc<CallbackGate>) -> Self {
            Self {
                gate,
                release_observed_gate: AtomicBool::new(false),
                reject_and_close_release: AtomicBool::new(false),
                tokens: Mutex::new(subscription_tokens(16).into()),
                callbacks: Mutex::new(Vec::new()),
                memory: Mutex::new(Vec::new()),
                files: Mutex::new(Vec::new()),
                resources: Mutex::new(Vec::new()),
                releases: Mutex::new(Vec::new()),
                cleanups: Mutex::new(Vec::new()),
                callback_cleanups: Mutex::new(Vec::new()),
                resource_cleanups: Mutex::new(Vec::new()),
                resizes: Mutex::new(Vec::new()),
            }
        }

        fn register_callback(&self, callback: Option<SendFontCallback>) -> Option<SubscriptionId> {
            callback.map(|callback| {
                lock(&self.callbacks).push(callback);
                lock(&self.tokens)
                    .pop_front()
                    .expect("test token pool should not be exhausted")
            })
        }

        fn notify(&self, identifier: &CStr, font: Option<FontHandle>) {
            for callback in &mut *lock(&self.callbacks) {
                callback(identifier, font);
            }
        }
    }

    impl RenderFontService for RecordingService {
        fn get(
            &self,
            _owner: OwnerId,
            identifier: String,
            mut callback: SendFontCallback,
        ) -> RequiredServiceResult<FontGetResult> {
            let identifier =
                CString::new(identifier).expect("test identifier should be terminated");
            callback(&identifier, None);
            let subscription = self.register_callback(Some(callback));
            Ok(FontGetResult {
                subscription,
                callback_panicked: false,
            })
        }

        fn release(
            &self,
            identifier: String,
            subscription: SubscriptionId,
        ) -> RequiredServiceResult<bool> {
            self.release_observed_gate
                .store(self.gate.in_flight() > 0, Ordering::Release);
            lock(&self.releases).push((identifier, subscription));
            if self.reject_and_close_release.swap(false, Ordering::AcqRel) {
                self.gate.close();
                return Err(BackendOperationError::ServiceRejected);
            }
            Ok(true)
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
            lock(&self.files).push(FileCall {
                owner,
                identifier,
                size,
                filename,
                config,
            });
            Ok(FontRegistration {
                subscription: self.register_callback(callback),
                created: true,
            })
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
            lock(&self.resources).push(ResourceCall {
                owner,
                identifier,
                size,
                resource_id: resource.resource_id,
                module: resource.module,
                config,
            });
            Ok(FontRegistration {
                subscription: self.register_callback(callback),
                created: true,
            })
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
            lock(&self.memory).push(MemoryCall {
                owner,
                identifier,
                size,
                data,
                config,
            });
            Ok(FontRegistration {
                subscription: self.register_callback(callback),
                created: true,
            })
        }

        fn resize(&self, identifier: String, size: f32) -> RequiredServiceResult<bool> {
            lock(&self.resizes).push((identifier, size));
            Ok(true)
        }

        fn cleanup_owner(&self, owner: OwnerId) -> RequiredServiceResult<usize> {
            lock(&self.cleanups).push(owner);
            let removed = lock(&self.callbacks).len();
            lock(&self.callbacks).clear();
            Ok(removed)
        }

        fn cleanup_owner_callbacks(&self, owner: OwnerId) -> RequiredServiceResult<usize> {
            lock(&self.callback_cleanups).push(owner);
            let removed = lock(&self.callbacks).len();
            lock(&self.callbacks).clear();
            Ok(removed)
        }

        fn cleanup_owner_resources(&self, owner: OwnerId) -> RequiredServiceResult<usize> {
            lock(&self.resource_cleanups).push(owner);
            Ok(0)
        }
    }

    struct Harness {
        api: FontApi,
        service: Arc<RecordingService>,
        callers: Arc<AddonCallerResolver>,
        owners: Arc<TestOwners>,
        gate: Arc<CallbackGate>,
        failures: Arc<BackendFailures>,
    }

    impl Harness {
        fn new() -> Self {
            let gate = Arc::new(CallbackGate::open());
            let owners = Arc::new(TestOwners::new(Arc::clone(&gate)));
            let callers = Arc::new(AddonCallerResolver::new(owners.clone()));
            let failures = Arc::new(BackendFailures::new());
            let boundary = Arc::new(NativeCallBoundary::new(
                Arc::clone(&callers),
                NativeMemoryReader::default(),
                Arc::clone(&failures),
            ));
            let service = Arc::new(RecordingService::new(Arc::clone(&gate)));
            let api = FontApi::new(boundary, service.clone());
            Self {
                api,
                service,
                callers,
                owners,
                gate,
                failures,
            }
        }

        fn enter_owner(&self) -> nexus_addon_ffi::AddonOwnerScope {
            self.callers
                .enter_owner_scope(OWNER)
                .expect("test owner should be current")
        }
    }

    #[derive(Clone, Copy)]
    struct NoopAtlas;

    impl FontAtlasBackend for NoopAtlas {
        fn rebuild(
            &mut self,
            fonts: &[FontBuildRequest<'_>],
            _localized_texts: &[&CStr],
        ) -> Result<Vec<Option<FontHandle>>, FontBackendError> {
            Ok(vec![None; fonts.len()])
        }
    }

    fn subscription_tokens(count: usize) -> Vec<SubscriptionId> {
        let mut manager = FontManager::new(NoopAtlas);
        (0..count)
            .map(|index| {
                manager
                    .register_memory(
                        OwnerId::HOST,
                        &format!("seed-{index}"),
                        12.0,
                        &[1],
                        FontConfig::default(),
                        Some(Box::new(|_, _| {})),
                    )
                    .expect("seed registration should succeed")
                    .subscription
                    .expect("seed callback should have a subscription")
            })
            .collect()
    }

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn c_string(value: &str) -> CString {
        CString::new(value).expect("test string contains no NUL")
    }

    #[test]
    fn implements_the_complete_font_backend_contract() {
        fn assert_backend<T: FontBackend>() {}
        assert_backend::<FontApi>();
    }

    #[test]
    fn callback_queue_is_bounded_before_publication_and_keeps_the_latest_work() {
        let _test_guard = lock(&CALLBACK_TEST_LOCK);
        lock(&CALLBACKS).clear();
        let harness = Harness::new();
        let callback = NativeFontCallback::new(
            record_font,
            Arc::clone(&harness.gate),
            Arc::clone(&harness.api.boundary),
        );

        for index in 0..MAX_PENDING_FONT_CALLBACKS + 3 {
            let identifier = c_string(&format!("bounded-font-{index}"));
            callback.enqueue(&identifier, None);
        }
        assert!(lock(&CALLBACKS).is_empty());
        callback.publish();

        let callbacks = lock(&CALLBACKS);
        assert_eq!(callbacks.len(), MAX_PENDING_FONT_CALLBACKS);
        assert_eq!(
            callbacks.first().map(|entry| entry.0.as_str()),
            Some("bounded-font-3")
        );
        assert_eq!(
            callbacks.last().map(|entry| entry.0.as_str()),
            Some("bounded-font-66")
        );
        drop(callbacks);
        assert_eq!(harness.failures.snapshot().service_rejected, 3);
    }

    #[test]
    fn registration_mismatch_uses_owner_rollback_and_purges_old_receipts() {
        let harness = Harness::new();
        let identifier = "mismatched-font".to_owned();
        let callback = Arc::new(NativeFontCallback::new(
            record_font,
            Arc::clone(&harness.gate),
            Arc::clone(&harness.api.boundary),
        ));
        let old_subscription = subscription_tokens(1)[0];
        lock(&harness.api.subscriptions).insert(
            SubscriptionKey::new(OwnerId::from(OWNER), "old-font".to_owned(), 17),
            SubscriptionReceipts {
                subscriptions: vec![old_subscription],
                publication: None,
            },
        );

        assert_eq!(
            harness.api.finish_registration(
                OWNER,
                identifier,
                Some(callback),
                FontRegistration {
                    subscription: None,
                    created: true,
                },
            ),
            Err(BackendOperationError::ServiceRejected)
        );
        assert!(lock(&harness.api.subscriptions).is_empty());
        assert_eq!(&*lock(&harness.service.cleanups), &[OwnerId::from(OWNER)]);
        assert_eq!(harness.failures.snapshot().service_rejected, 1);
    }

    #[test]
    fn native_memory_and_imgui_configuration_are_deep_copied() {
        let harness = Harness::new();
        let _scope = harness.enter_owner();
        let identifier = c_string("owned-font");
        let mut data = vec![1_u8, 2, 3, 4];
        let mut ranges = [0x20_u16, 0x7E, 0];
        let mut config = sys::ImFontConfig {
            FontNo: 2,
            OversampleH: 4,
            OversampleV: 2,
            PixelSnapH: true,
            GlyphExtraSpacing: sys::ImVec2 { x: 1.5, y: 2.5 },
            GlyphOffset: sys::ImVec2 { x: 3.5, y: 4.5 },
            GlyphRanges: ranges.as_ptr(),
            GlyphMinAdvanceX: 5.5,
            GlyphMaxAdvanceX: 6.5,
            MergeMode: true,
            RasterizerFlags: 7,
            RasterizerMultiply: 1.25,
            EllipsisChar: 0x2026,
            ..sys::ImFontConfig::default()
        };

        harness
            .api
            .add_from_memory(
                identifier.as_ptr(),
                18.0,
                data.as_mut_ptr().cast(),
                data.len(),
                None,
                (&mut config as *mut sys::ImFontConfig).cast(),
            )
            .expect("owned memory registration should succeed");
        data.fill(0xFF);
        ranges.fill(0);
        config.OversampleH = 99;
        assert_eq!(config.OversampleH, 99);

        let calls = lock(&harness.service.memory);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].owner, OwnerId::from(OWNER));
        assert_eq!(calls[0].identifier, "owned-font");
        assert_eq!(calls[0].size, 18.0);
        assert_eq!(calls[0].data, [1, 2, 3, 4]);
        assert_eq!(calls[0].config.font_no, 2);
        assert_eq!(calls[0].config.oversample_h, 4);
        assert_eq!(calls[0].config.oversample_v, 2);
        assert!(calls[0].config.pixel_snap_h);
        assert_eq!(calls[0].config.glyph_extra_spacing, [1.5, 2.5]);
        assert_eq!(calls[0].config.glyph_offset, [3.5, 4.5]);
        assert_eq!(calls[0].config.glyph_ranges, [0x20, 0x7E, 0]);
        assert_eq!(calls[0].config.glyph_min_advance_x, 5.5);
        assert_eq!(calls[0].config.glyph_max_advance_x, 6.5);
        assert!(calls[0].config.merge_mode);
        assert_eq!(calls[0].config.rasterizer_flags, 7);
        assert_eq!(calls[0].config.rasterizer_multiply, 1.25);
        assert_eq!(calls[0].config.ellipsis_char, 0x2026);
    }

    #[test]
    fn callback_publication_release_and_gate_are_generation_exact() {
        let _test_guard = lock(&CALLBACK_TEST_LOCK);
        lock(&CALLBACKS).clear();
        let harness = Harness::new();
        let _scope = harness.enter_owner();
        let identifier = c_string("callback-font");

        harness
            .api
            .get(identifier.as_ptr(), Some(record_font))
            .expect("get should publish its callback");
        assert_eq!(&*lock(&CALLBACKS), &[("callback-font".to_owned(), 0)]);

        harness
            .api
            .release(identifier.as_ptr(), Some(record_font))
            .expect("release should remove the exact subscription");
        assert_eq!(lock(&harness.service.releases).len(), 1);
        assert!(
            harness
                .service
                .release_observed_gate
                .load(Ordering::Acquire)
        );

        harness.gate.close();
        harness.service.notify(&identifier, None);
        assert_eq!(lock(&CALLBACKS).len(), 1);

        assert_eq!(
            harness.api.get(identifier.as_ptr(), Some(foreign_font)),
            Err(BackendOperationError::Boundary(
                CallBoundaryError::CallerAttribution
            ))
        );
    }

    #[test]
    fn file_resource_resize_and_null_callback_arguments_are_preserved() {
        let harness = Harness::new();
        let _scope = harness.enter_owner();
        let identifier = c_string("asset-font");
        let filename = c_string("fonts\\asset.ttf");
        let module = 0x1234_usize as *mut c_void;

        harness
            .api
            .add_from_file(
                identifier.as_ptr(),
                15.0,
                filename.as_ptr(),
                None,
                core::ptr::null_mut(),
            )
            .expect("file registration should succeed");
        harness
            .api
            .add_from_resource(
                identifier.as_ptr(),
                16.0,
                77,
                module,
                None,
                core::ptr::null_mut(),
            )
            .expect("resource registration should succeed");
        harness
            .api
            .resize(identifier.as_ptr(), 17.0)
            .expect("resize should succeed");

        assert_eq!(
            lock(&harness.service.files)[0].filename,
            PathBuf::from("fonts\\asset.ttf")
        );
        assert_eq!(lock(&harness.service.resources)[0].resource_id, 77);
        assert_eq!(lock(&harness.service.resources)[0].module, module as usize);
        assert_eq!(
            &*lock(&harness.service.resizes),
            &[("asset-font".to_owned(), 17.0)]
        );
    }

    #[test]
    fn close_race_aborts_callback_and_rolls_back_the_exact_owner_generation() {
        let _test_guard = lock(&CALLBACK_TEST_LOCK);
        lock(&CALLBACKS).clear();
        let harness = Harness::new();
        let _scope = harness.enter_owner();
        harness.owners.close_on_gate.store(true, Ordering::Release);
        let identifier = c_string("closing-font");

        assert_eq!(
            harness.api.get(identifier.as_ptr(), Some(record_font)),
            Err(BackendOperationError::Boundary(
                CallBoundaryError::CallerAttribution
            ))
        );
        assert!(lock(&CALLBACKS).is_empty());
        assert_eq!(&*lock(&harness.service.cleanups), &[OwnerId::from(OWNER)]);
    }

    #[test]
    fn owner_cleanup_is_a_service_barrier_and_forgets_release_receipts() {
        let _test_guard = lock(&CALLBACK_TEST_LOCK);
        lock(&CALLBACKS).clear();
        let harness = Harness::new();
        let _scope = harness.enter_owner();
        let identifier = c_string("cleanup-font");
        harness
            .api
            .get(identifier.as_ptr(), Some(record_font))
            .expect("get should subscribe");
        harness.gate.close();
        harness
            .api
            .cleanup_owner(OWNER)
            .expect("cleanup barrier should succeed");
        harness
            .api
            .release(identifier.as_ptr(), Some(record_font))
            .expect("stale release should be an exact no-op");
        assert!(lock(&harness.service.releases).is_empty());
    }

    #[test]
    fn failed_release_does_not_restore_receipts_after_generation_close() {
        let _test_guard = lock(&CALLBACK_TEST_LOCK);
        lock(&CALLBACKS).clear();
        let harness = Harness::new();
        let _scope = harness.enter_owner();
        let identifier = c_string("closing-release-font");
        harness
            .api
            .get(identifier.as_ptr(), Some(record_font))
            .expect("get should subscribe");
        harness
            .service
            .reject_and_close_release
            .store(true, Ordering::Release);

        assert_eq!(
            harness.api.release(identifier.as_ptr(), Some(record_font)),
            Err(BackendOperationError::ServiceRejected)
        );
        assert!(!harness.gate.is_open());
        assert!(lock(&harness.api.subscriptions).is_empty());
    }
}
