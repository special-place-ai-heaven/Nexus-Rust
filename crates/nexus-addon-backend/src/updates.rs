//! Authenticated, bounded add-on update requests.

use core::ffi::c_char;
use core::fmt;
use core::num::NonZeroUsize;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use nexus_core::OwnerToken;

use crate::{
    BackendFailure, BackendOperationError, NativeCallBoundary, RequiredServiceResult, UpdateBackend,
};

/// Maximum number of update requests retained by a default [`UpdateApi`].
pub const DEFAULT_UPDATE_QUEUE_CAPACITY: usize = 256;

/// One authenticated, fully owned add-on update request.
#[derive(Clone, Eq, PartialEq)]
pub struct AddonUpdateRequest {
    owner: OwnerToken,
    signature: i32,
    update_url: String,
}

impl AddonUpdateRequest {
    /// Returns the exact add-on generation that submitted this request.
    #[must_use]
    pub const fn owner(&self) -> OwnerToken {
        self.owner
    }

    /// Returns the legacy signed signature supplied by the add-on.
    #[must_use]
    pub const fn signature(&self) -> i32 {
        self.signature
    }

    /// Returns the copied update URL.
    #[must_use]
    pub fn update_url(&self) -> &str {
        &self.update_url
    }
}

impl fmt::Debug for AddonUpdateRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AddonUpdateRequest")
            .field("owner", &self.owner)
            .field("signature", &self.signature)
            .field("update_url_bytes", &self.update_url.len())
            .finish()
    }
}

struct UpdateQueueState {
    closed: bool,
    requests: VecDeque<AddonUpdateRequest>,
}

/// Production update boundary backed by a bounded owned-request queue.
///
/// A runtime update coordinator consumes requests with [`Self::try_dequeue`].
/// Closing the queue rejects new native calls while leaving already accepted
/// owned requests available to the coordinator.
pub struct UpdateApi {
    boundary: Arc<NativeCallBoundary>,
    capacity: NonZeroUsize,
    state: Mutex<UpdateQueueState>,
}

impl UpdateApi {
    /// Creates an update queue with [`DEFAULT_UPDATE_QUEUE_CAPACITY`].
    #[must_use]
    pub fn new(boundary: Arc<NativeCallBoundary>) -> Self {
        let capacity = NonZeroUsize::new(DEFAULT_UPDATE_QUEUE_CAPACITY)
            .expect("the default update queue capacity is nonzero");
        Self::with_capacity(boundary, capacity)
    }

    /// Creates an update queue with one explicit nonzero capacity.
    #[must_use]
    pub fn with_capacity(boundary: Arc<NativeCallBoundary>, capacity: NonZeroUsize) -> Self {
        Self {
            boundary,
            capacity,
            state: Mutex::new(UpdateQueueState {
                closed: false,
                requests: VecDeque::with_capacity(capacity.get()),
            }),
        }
    }

    /// Returns the number of accepted requests waiting for a coordinator.
    #[must_use]
    pub fn pending_len(&self) -> usize {
        mutex_lock(&self.state).requests.len()
    }

    /// Removes the oldest accepted request, if one is available.
    #[must_use]
    pub fn try_dequeue(&self) -> Option<AddonUpdateRequest> {
        mutex_lock(&self.state).requests.pop_front()
    }

    /// Prevents any later native request from entering the queue.
    pub fn close(&self) {
        mutex_lock(&self.state).closed = true;
    }

    /// Cancels unclaimed requests for one exact add-on generation.
    ///
    /// The caller must close the owner's callback gate before invoking this
    /// barrier. Requests already claimed with [`Self::try_dequeue`] remain the
    /// coordinator's responsibility.
    pub fn cancel_owner(&self, owner: OwnerToken) -> usize {
        let mut state = mutex_lock(&self.state);
        let before = state.requests.len();
        state.requests.retain(|request| request.owner != owner);
        before.saturating_sub(state.requests.len())
    }

    /// Closes admission and transfers every unclaimed request in FIFO order.
    ///
    /// Repeated calls return an empty collection. Requests are converted after
    /// releasing the queue lock so consumer destruction cannot run inside it.
    #[must_use]
    pub fn close_and_drain(&self) -> Vec<AddonUpdateRequest> {
        let requests = {
            let mut state = mutex_lock(&self.state);
            state.closed = true;
            std::mem::take(&mut state.requests)
        };
        requests.into_iter().collect()
    }

    fn request_update(
        &self,
        signature: i32,
        update_url: *const c_char,
    ) -> RequiredServiceResult<()> {
        let owner = self.boundary.resolve_owner(None)?;
        let update_url = self.boundary.snapshot_url(update_url)?;
        let signature_bits = u32::from_ne_bytes(signature.to_ne_bytes());
        if signature == 0 || signature_bits != owner.signature || update_url.is_empty() {
            return self.service_rejected();
        }
        let gate = self.boundary.callback_gate_for_current(owner)?;

        let request = AddonUpdateRequest {
            owner,
            signature,
            update_url: update_url.into_string(),
        };
        let mut state = mutex_lock(&self.state);
        if state.closed || !gate.is_open() || state.requests.len() >= self.capacity.get() {
            drop(state);
            return self.service_rejected();
        }
        state.requests.push_back(request);
        Ok(())
    }

    fn service_rejected<T>(&self) -> RequiredServiceResult<T> {
        self.boundary
            .failures()
            .record(BackendFailure::ServiceRejected);
        Err(BackendOperationError::ServiceRejected)
    }
}

impl UpdateBackend for UpdateApi {
    fn request_update(
        &self,
        signature: i32,
        update_url: *const c_char,
    ) -> RequiredServiceResult<()> {
        self.request_update(signature, update_url)
    }
}

impl fmt::Debug for UpdateApi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = mutex_lock(&self.state);
        formatter
            .debug_struct("UpdateApi")
            .field("capacity", &self.capacity)
            .field("closed", &state.closed)
            .field("pending", &state.requests.len())
            .finish_non_exhaustive()
    }
}

fn mutex_lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroUsize;
    use std::ffi::CString;
    use std::sync::{Arc, Barrier};
    use std::thread;

    use nexus_addon_ffi::{AddonCallerResolver, AddressOwnerResolver};
    use nexus_core::{CallbackGate, OwnerToken};
    use nexus_native_memory::NativeMemoryReader;

    use super::{AddonUpdateRequest, UpdateApi};
    use crate::{BackendFailures, UpdateBackend};

    const OWNER: OwnerToken = OwnerToken {
        signature: 0xA11D_0042,
        generation: 7,
    };
    const RELOADED_OWNER: OwnerToken = OwnerToken {
        signature: OWNER.signature,
        generation: OWNER.generation + 1,
    };
    const OTHER_OWNER: OwnerToken = OwnerToken {
        signature: OWNER.signature + 1,
        generation: 3,
    };

    struct CurrentOwners {
        owner: Arc<CallbackGate>,
        reloaded: Arc<CallbackGate>,
        other: Arc<CallbackGate>,
    }

    impl CurrentOwners {
        fn new() -> Self {
            Self {
                owner: Arc::new(CallbackGate::open()),
                reloaded: Arc::new(CallbackGate::open()),
                other: Arc::new(CallbackGate::open()),
            }
        }

        fn gate(&self, owner: OwnerToken) -> Option<Arc<CallbackGate>> {
            match owner {
                OWNER => Some(Arc::clone(&self.owner)),
                RELOADED_OWNER => Some(Arc::clone(&self.reloaded)),
                OTHER_OWNER => Some(Arc::clone(&self.other)),
                _ => None,
            }
        }
    }

    impl AddressOwnerResolver for CurrentOwners {
        fn owner_for_address(&self, _address: NonZeroUsize) -> Option<OwnerToken> {
            None
        }

        fn is_current_owner(&self, owner: OwnerToken) -> bool {
            self.gate(owner).is_some()
        }

        fn callback_gate_for_current(&self, owner: OwnerToken) -> Option<Arc<CallbackGate>> {
            self.gate(owner)
        }
    }

    struct Harness {
        api: UpdateApi,
        callers: Arc<AddonCallerResolver>,
        owners: Arc<CurrentOwners>,
        failures: Arc<BackendFailures>,
    }

    impl Harness {
        fn new(capacity: usize) -> Self {
            let owners = Arc::new(CurrentOwners::new());
            let callers = Arc::new(AddonCallerResolver::new(owners.clone()));
            let failures = Arc::new(BackendFailures::new());
            let boundary = Arc::new(crate::NativeCallBoundary::new(
                Arc::clone(&callers),
                NativeMemoryReader::default(),
                Arc::clone(&failures),
            ));
            let capacity = NonZeroUsize::new(capacity).expect("test capacity must be nonzero");
            Self {
                api: UpdateApi::with_capacity(boundary, capacity),
                callers,
                owners,
                failures,
            }
        }

        fn enter_owner(&self) -> nexus_addon_ffi::AddonOwnerScope {
            self.enter_exact_owner(OWNER)
        }

        fn enter_exact_owner(&self, owner: OwnerToken) -> nexus_addon_ffi::AddonOwnerScope {
            self.callers
                .enter_owner_scope(owner)
                .expect("test owner should be current")
        }
    }

    fn signature() -> i32 {
        i32::from_ne_bytes(OWNER.signature.to_ne_bytes())
    }

    #[test]
    fn implements_the_complete_update_backend_contract() {
        fn assert_backend<T: UpdateBackend>() {}
        assert_backend::<UpdateApi>();
    }

    #[test]
    fn accepted_requests_are_attributed_copied_and_fifo() {
        let harness = Harness::new(2);
        let mut first = b"https://updates.invalid/first\0".to_vec();
        let second = CString::new("https://updates.invalid/second").expect("valid test URL");
        let _scope = harness.enter_owner();

        harness
            .api
            .request_update(signature(), first.as_ptr().cast())
            .expect("first request should be accepted");
        first.fill(b'x');
        harness
            .api
            .request_update(signature(), second.as_ptr())
            .expect("second request should be accepted");

        let first = harness.api.try_dequeue().expect("first request queued");
        let second = harness.api.try_dequeue().expect("second request queued");
        assert_eq!(first.owner(), OWNER);
        assert_eq!(first.signature(), signature());
        assert_eq!(first.update_url(), "https://updates.invalid/first");
        assert_eq!(second.update_url(), "https://updates.invalid/second");
        assert!(harness.api.try_dequeue().is_none());
    }

    #[test]
    fn signature_spoofing_and_empty_urls_fail_closed() {
        let harness = Harness::new(2);
        let url = CString::new("https://updates.invalid/addon").expect("valid test URL");
        let empty = CString::new("").expect("valid empty C string");
        let _scope = harness.enter_owner();

        assert!(
            harness
                .api
                .request_update(signature().wrapping_add(1), url.as_ptr())
                .is_err()
        );
        assert!(
            harness
                .api
                .request_update(signature(), empty.as_ptr())
                .is_err()
        );
        assert_eq!(harness.api.pending_len(), 0);
        assert_eq!(harness.failures.snapshot().service_rejected, 2);
    }

    #[test]
    fn queue_capacity_and_close_are_explicit_and_preserve_accepted_work() {
        let harness = Harness::new(1);
        let first = CString::new("https://updates.invalid/first").expect("valid test URL");
        let second = CString::new("https://updates.invalid/second").expect("valid test URL");
        let _scope = harness.enter_owner();

        harness
            .api
            .request_update(signature(), first.as_ptr())
            .expect("first request should be accepted");
        assert!(
            harness
                .api
                .request_update(signature(), second.as_ptr())
                .is_err()
        );
        harness.api.close();
        assert!(
            harness
                .api
                .request_update(signature(), second.as_ptr())
                .is_err()
        );

        assert_eq!(harness.api.pending_len(), 1);
        assert_eq!(
            harness
                .api
                .try_dequeue()
                .expect("accepted request remains")
                .update_url(),
            "https://updates.invalid/first"
        );
        assert_eq!(harness.failures.snapshot().service_rejected, 2);
    }

    #[test]
    fn owner_cancellation_is_generation_exact_and_preserves_survivor_fifo() {
        let harness = Harness::new(4);
        let owner_first = CString::new("https://updates.invalid/owner-first").expect("valid URL");
        let reloaded = CString::new("https://updates.invalid/reloaded").expect("valid URL");
        let other = CString::new("https://updates.invalid/other").expect("valid URL");
        let owner_last = CString::new("https://updates.invalid/owner-last").expect("valid URL");

        {
            let _scope = harness.enter_exact_owner(OWNER);
            harness
                .api
                .request_update(signature(), owner_first.as_ptr())
                .expect("first owner request should queue");
        }
        {
            let _scope = harness.enter_exact_owner(RELOADED_OWNER);
            harness
                .api
                .request_update(signature(), reloaded.as_ptr())
                .expect("reloaded generation request should queue");
        }
        {
            let _scope = harness.enter_exact_owner(OTHER_OWNER);
            harness
                .api
                .request_update(
                    i32::from_ne_bytes(OTHER_OWNER.signature.to_ne_bytes()),
                    other.as_ptr(),
                )
                .expect("other owner request should queue");
        }
        {
            let _scope = harness.enter_exact_owner(OWNER);
            harness
                .api
                .request_update(signature(), owner_last.as_ptr())
                .expect("last owner request should queue");
        }

        assert_eq!(harness.api.cancel_owner(OWNER), 2);
        assert_eq!(harness.api.cancel_owner(OWNER), 0);
        assert_eq!(harness.api.pending_len(), 2);
        let reloaded = harness
            .api
            .try_dequeue()
            .expect("reloaded request survives");
        let other = harness.api.try_dequeue().expect("other request survives");
        assert_eq!(reloaded.owner(), RELOADED_OWNER);
        assert_eq!(reloaded.update_url(), "https://updates.invalid/reloaded");
        assert_eq!(other.owner(), OTHER_OWNER);
        assert_eq!(other.update_url(), "https://updates.invalid/other");
        assert_eq!(harness.failures.snapshot().service_rejected, 0);
    }

    #[test]
    fn closed_owner_cannot_publish_after_cleanup_barrier() {
        let harness = Harness::new(2);
        let first = CString::new("https://updates.invalid/first").expect("valid URL");
        let late = CString::new("https://updates.invalid/late").expect("valid URL");
        let _scope = harness.enter_owner();

        harness
            .api
            .request_update(signature(), first.as_ptr())
            .expect("request before cleanup should queue");
        assert!(harness.owners.owner.close());
        assert_eq!(harness.api.cancel_owner(OWNER), 1);
        assert!(
            harness
                .api
                .request_update(signature(), late.as_ptr())
                .is_err()
        );
        assert_eq!(harness.api.pending_len(), 0);
        assert_eq!(harness.failures.snapshot().service_rejected, 1);
    }

    #[test]
    fn concurrent_publication_and_cleanup_leave_no_late_request() {
        let harness = Arc::new(Harness::new(1));
        let start = Arc::new(Barrier::new(3));

        let publication_harness = Arc::clone(&harness);
        let publication_start = Arc::clone(&start);
        let publication = thread::spawn(move || {
            let url = CString::new("https://updates.invalid/racing").expect("valid URL");
            let _scope = publication_harness.enter_owner();
            publication_start.wait();
            publication_harness
                .api
                .request_update(signature(), url.as_ptr())
        });

        let cleanup_harness = Arc::clone(&harness);
        let cleanup_start = Arc::clone(&start);
        let cleanup = thread::spawn(move || {
            cleanup_start.wait();
            let _closed = cleanup_harness.owners.owner.close();
            cleanup_harness.api.cancel_owner(OWNER)
        });

        start.wait();
        let publication = publication
            .join()
            .expect("publication thread should not panic");
        let cancelled = cleanup.join().expect("cleanup thread should not panic");
        match publication {
            Ok(()) => assert_eq!(cancelled, 1),
            Err(_) => assert_eq!(cancelled, 0),
        }
        assert_eq!(harness.api.pending_len(), 0);
    }

    #[test]
    fn close_and_drain_transfers_each_request_once_in_fifo_order() {
        let harness = Harness::new(3);
        let first = CString::new("https://updates.invalid/first").expect("valid URL");
        let second = CString::new("https://updates.invalid/second").expect("valid URL");
        let late = CString::new("https://updates.invalid/late").expect("valid URL");
        let _scope = harness.enter_owner();

        harness
            .api
            .request_update(signature(), first.as_ptr())
            .expect("first request should queue");
        harness
            .api
            .request_update(signature(), second.as_ptr())
            .expect("second request should queue");

        let drained = harness.api.close_and_drain();
        assert_eq!(
            drained
                .iter()
                .map(AddonUpdateRequest::update_url)
                .collect::<Vec<_>>(),
            [
                "https://updates.invalid/first",
                "https://updates.invalid/second"
            ]
        );
        assert!(harness.api.close_and_drain().is_empty());
        assert!(
            harness
                .api
                .request_update(signature(), late.as_ptr())
                .is_err()
        );
        assert_eq!(harness.api.pending_len(), 0);
        assert_eq!(harness.failures.snapshot().service_rejected, 1);
    }

    #[test]
    fn unattributed_calls_never_enter_the_queue() {
        let harness = Harness::new(1);
        let url = CString::new("https://updates.invalid/addon").expect("valid test URL");

        assert!(
            harness
                .api
                .request_update(signature(), url.as_ptr())
                .is_err()
        );
        assert_eq!(harness.api.pending_len(), 0);
        assert_eq!(harness.failures.snapshot().caller_attribution, 1);
        assert_eq!(harness.failures.snapshot().service_rejected, 0);
    }

    #[test]
    fn debug_output_never_contains_the_update_url() {
        let harness = Harness::new(1);
        let url = CString::new("https://updates.invalid/private-token").expect("valid test URL");
        let _scope = harness.enter_owner();
        harness
            .api
            .request_update(signature(), url.as_ptr())
            .expect("request should be accepted");

        let request = harness.api.try_dequeue().expect("request queued");
        let output = format!("{request:?}");
        assert!(!output.contains("private-token"));
        assert!(output.contains("update_url_bytes"));
    }
}
