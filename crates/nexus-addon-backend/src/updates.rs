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

        let request = AddonUpdateRequest {
            owner,
            signature,
            update_url: update_url.into_string(),
        };
        let mut state = mutex_lock(&self.state);
        if state.closed || state.requests.len() >= self.capacity.get() {
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
    use std::sync::Arc;

    use nexus_addon_ffi::{AddonCallerResolver, AddressOwnerResolver};
    use nexus_core::OwnerToken;
    use nexus_native_memory::NativeMemoryReader;

    use super::UpdateApi;
    use crate::{BackendFailures, UpdateBackend};

    const OWNER: OwnerToken = OwnerToken {
        signature: 0xA11D_0042,
        generation: 7,
    };

    struct CurrentOwner;

    impl AddressOwnerResolver for CurrentOwner {
        fn owner_for_address(&self, _address: NonZeroUsize) -> Option<OwnerToken> {
            None
        }

        fn is_current_owner(&self, owner: OwnerToken) -> bool {
            owner == OWNER
        }
    }

    struct Harness {
        api: UpdateApi,
        callers: Arc<AddonCallerResolver>,
        failures: Arc<BackendFailures>,
    }

    impl Harness {
        fn new(capacity: usize) -> Self {
            let callers = Arc::new(AddonCallerResolver::new(Arc::new(CurrentOwner)));
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
                failures,
            }
        }

        fn enter_owner(&self) -> nexus_addon_ffi::AddonOwnerScope {
            self.callers
                .enter_owner_scope(OWNER)
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
