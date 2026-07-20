use core::ffi::c_char;
use core::fmt;
use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::sync::{Arc, Mutex, MutexGuard};

use nexus_core::OwnerToken;
use nexus_ui_services::{LocalizationHandle, LocalizationService};

use crate::{
    BackendFailure, BackendOperationError, CallBoundaryError, LocalizationBackend,
    NativeCallBoundary, NativeText, RequiredServiceResult,
};

/// Caller-attributed adapter for process-stable localization strings.
///
/// Native inputs are copied through [`NativeCallBoundary`] before the locale
/// service sees them. Results are copied again into an append-only string
/// table owned by this process-lifetime adapter, so a pointer remains valid
/// after later translations and owner override changes.
pub struct LocalizationApi {
    boundary: Arc<NativeCallBoundary>,
    service: Arc<Mutex<LocalizationService>>,
    handle: LocalizationHandle,
    retained: Mutex<BTreeMap<Vec<u8>, Box<CStr>>>,
}

impl LocalizationApi {
    /// Creates an adapter over the UI-thread localization service.
    #[must_use]
    pub fn new(
        boundary: Arc<NativeCallBoundary>,
        service: Arc<Mutex<LocalizationService>>,
    ) -> Self {
        let handle = mutex_lock(&service).handle();
        Self {
            boundary,
            service,
            handle,
            retained: Mutex::new(BTreeMap::new()),
        }
    }

    /// Translates one copied identifier using the active language.
    pub fn translate(&self, identifier: *const c_char) -> RequiredServiceResult<*const c_char> {
        self.with_current_owner(|_owner| {
            let identifier = self.boundary.snapshot_identifier(identifier)?;
            self.translate_copied(identifier, None)
        })
    }

    /// Translates one copied identifier using an explicit language.
    pub fn translate_to(
        &self,
        identifier: *const c_char,
        language: *const c_char,
    ) -> RequiredServiceResult<*const c_char> {
        self.with_current_owner(|_owner| {
            let identifier = self.boundary.snapshot_identifier(identifier)?;
            let language = self.boundary.snapshot_identifier(language)?;
            self.translate_copied(identifier, Some(language))
        })
    }

    /// Queues one copied, generation-owned translation override.
    pub fn set_translated_string(
        &self,
        identifier: *const c_char,
        language: *const c_char,
        value: *const c_char,
    ) -> RequiredServiceResult<()> {
        self.with_current_owner(|owner| {
            let identifier = self.boundary.snapshot_identifier(identifier)?;
            let language = self.boundary.snapshot_identifier(language)?;
            let value = self.boundary.snapshot_message(value)?;
            self.handle
                .set(
                    owner.into(),
                    identifier.as_str(),
                    language.as_str(),
                    value.as_str(),
                )
                .map_err(|_error| self.service_rejected())
        })
    }

    /// Queues cleanup for one exact retired add-on generation.
    ///
    /// The lifecycle coordinator must first close that generation's callback
    /// gate and wait for admitted API calls to drain. This ordering guarantees
    /// that no accepted call can enqueue an override after this cleanup marker.
    pub fn cleanup_owner(&self, owner: OwnerToken) -> RequiredServiceResult<()> {
        self.handle
            .cleanup_owner(owner.into())
            .map_err(|_error| self.service_rejected())
    }

    /// Returns the number of distinct native strings retained by this adapter.
    #[must_use]
    pub fn retained_strings(&self) -> usize {
        mutex_lock(&self.retained).len()
    }

    fn translate_copied(
        &self,
        identifier: NativeText,
        language: Option<NativeText>,
    ) -> RequiredServiceResult<*const c_char> {
        let identifier =
            CString::new(identifier.into_string()).map_err(|_error| self.service_rejected())?;
        let language = language
            .map(|language| CString::new(language.into_string()))
            .transpose()
            .map_err(|_error| self.service_rejected())?;
        let translated = {
            let service = mutex_lock(&self.service);
            service
                .translate(identifier.as_c_str(), language.as_deref())
                .to_bytes()
                .to_vec()
        };
        self.retain_translation(translated)
    }

    fn retain_translation(&self, bytes: Vec<u8>) -> RequiredServiceResult<*const c_char> {
        let mut retained = mutex_lock(&self.retained);
        if let Some(existing) = retained.get(bytes.as_slice()) {
            return Ok(existing.as_ptr());
        }

        let value = CString::new(bytes.clone()).map_err(|_error| self.service_rejected())?;
        let pointer = value.as_ptr();
        retained.insert(bytes, value.into_boxed_c_str());
        Ok(pointer)
    }

    fn with_current_owner<R>(
        &self,
        operation: impl FnOnce(OwnerToken) -> RequiredServiceResult<R>,
    ) -> RequiredServiceResult<R> {
        let owner = self.boundary.resolve_owner(None)?;
        let gate = self.boundary.callback_gate_for_current(owner)?;
        let Some(_admission) = gate.try_enter() else {
            return Err(self.caller_rejected());
        };
        operation(owner)
    }

    fn caller_rejected(&self) -> BackendOperationError {
        self.boundary
            .failures()
            .record(BackendFailure::CallerAttribution);
        CallBoundaryError::CallerAttribution.into()
    }

    fn service_rejected(&self) -> BackendOperationError {
        self.boundary
            .failures()
            .record(BackendFailure::ServiceRejected);
        BackendOperationError::ServiceRejected
    }
}

impl LocalizationBackend for LocalizationApi {
    fn translate(&self, identifier: *const c_char) -> RequiredServiceResult<*const c_char> {
        LocalizationApi::translate(self, identifier)
    }

    fn translate_to(
        &self,
        identifier: *const c_char,
        language: *const c_char,
    ) -> RequiredServiceResult<*const c_char> {
        LocalizationApi::translate_to(self, identifier, language)
    }

    fn set_translated_string(
        &self,
        identifier: *const c_char,
        language: *const c_char,
        value: *const c_char,
    ) -> RequiredServiceResult<()> {
        LocalizationApi::set_translated_string(self, identifier, language, value)
    }
}

impl fmt::Debug for LocalizationApi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalizationApi")
            .field("retained_strings", &self.retained_strings())
            .finish_non_exhaustive()
    }
}

fn mutex_lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroUsize;
    use std::ffi::{CStr, CString};
    use std::sync::Arc;

    use nexus_addon_ffi::{AddonCallerResolver, AddonOwnerScope, AddressOwnerResolver};
    use nexus_core::{CallbackGate, OwnerToken};
    use nexus_native_memory::NativeMemoryReader;
    use nexus_ui_services::{LocaleAsset, LocaleSource, LocaleSourceError, LocalizationService};

    use super::LocalizationApi;
    use crate::{
        BackendFailureSnapshot, BackendFailures, BackendOperationError, CallBoundaryError,
        LocalizationBackend, NativeCallBoundary,
    };

    const OWNER: OwnerToken = OwnerToken {
        signature: 7,
        generation: 1,
    };
    const OTHER_OWNER: OwnerToken = OwnerToken {
        signature: 7,
        generation: 2,
    };

    struct MemorySource(Vec<LocaleAsset>);

    impl LocaleSource for MemorySource {
        fn load(&mut self) -> Result<Vec<LocaleAsset>, LocaleSourceError> {
            Ok(core::mem::take(&mut self.0))
        }
    }

    struct TestOwners {
        gate: Arc<CallbackGate>,
    }

    impl AddressOwnerResolver for TestOwners {
        fn owner_for_address(&self, _address: NonZeroUsize) -> Option<OwnerToken> {
            None
        }

        fn is_current_owner(&self, owner: OwnerToken) -> bool {
            owner == OWNER || owner == OTHER_OWNER
        }

        fn callback_gate_for_current(&self, owner: OwnerToken) -> Option<Arc<CallbackGate>> {
            self.is_current_owner(owner).then(|| Arc::clone(&self.gate))
        }
    }

    struct Harness {
        api: LocalizationApi,
        service: Arc<std::sync::Mutex<LocalizationService>>,
        callers: Arc<AddonCallerResolver>,
        gate: Arc<CallbackGate>,
        failures: Arc<BackendFailures>,
    }

    impl Harness {
        fn new(queue_capacity: usize) -> Self {
            let mut service =
                LocalizationService::new("en", queue_capacity).expect("test localization");
            service
                .reload(&mut MemorySource(vec![
                    LocaleAsset::new(
                        br#"{"Identifier":"en","DisplayName":"English","Texts":{"hello":"Hello"}}"#,
                    ),
                    LocaleAsset::new(
                        br#"{"Identifier":"de","DisplayName":"Deutsch","Texts":{"hello":"Hallo"}}"#,
                    ),
                ]))
                .expect("test locale documents");
            let service = Arc::new(std::sync::Mutex::new(service));
            let gate = Arc::new(CallbackGate::open());
            let owners = Arc::new(TestOwners {
                gate: Arc::clone(&gate),
            });
            let callers = Arc::new(AddonCallerResolver::new(owners));
            let failures = Arc::new(BackendFailures::new());
            let boundary = Arc::new(NativeCallBoundary::new(
                Arc::clone(&callers),
                NativeMemoryReader::default(),
                Arc::clone(&failures),
            ));
            let api = LocalizationApi::new(boundary, Arc::clone(&service));
            Self {
                api,
                service,
                callers,
                gate,
                failures,
            }
        }

        fn enter_owner(&self, owner: OwnerToken) -> AddonOwnerScope {
            self.callers
                .enter_owner_scope(owner)
                .expect("test owner should be current")
        }

        fn advance(&self) {
            let _report = super::mutex_lock(&self.service).advance();
        }
    }

    fn c_string(value: &str) -> CString {
        CString::new(value).expect("test CString")
    }

    fn text_at(pointer: *const core::ffi::c_char) -> String {
        assert!(!pointer.is_null());
        // SAFETY: every pointer comes from the live adapter's append-only
        // retained string table and is read only while the adapter is alive.
        unsafe { CStr::from_ptr(pointer) }
            .to_str()
            .expect("adapter output should be UTF-8")
            .to_owned()
    }

    #[test]
    fn implements_the_complete_localization_backend_contract() {
        fn assert_backend<T: LocalizationBackend>() {}
        assert_backend::<LocalizationApi>();
    }

    #[test]
    fn translations_are_copied_and_remain_stable_across_later_calls() {
        let harness = Harness::new(8);
        let hello = c_string("hello");
        let missing_pointer;
        let hello_pointer;
        {
            let _scope = harness.enter_owner(OWNER);
            hello_pointer = harness.api.translate(hello.as_ptr()).expect("translation");
            let missing = c_string("missing");
            missing_pointer = harness.api.translate(missing.as_ptr()).expect("fallback");
        }
        drop(hello);

        assert_eq!(text_at(hello_pointer), "Hello");
        assert_eq!(text_at(missing_pointer), "missing");

        let repeated = c_string("hello");
        let german = c_string("de");
        let _scope = harness.enter_owner(OWNER);
        assert_eq!(
            harness.api.translate(repeated.as_ptr()).expect("repeat"),
            hello_pointer
        );
        let german_pointer = harness
            .api
            .translate_to(repeated.as_ptr(), german.as_ptr())
            .expect("explicit language");
        assert_eq!(text_at(german_pointer), "Hallo");
        assert_eq!(text_at(hello_pointer), "Hello");
        assert_eq!(harness.api.retained_strings(), 3);
    }

    #[test]
    fn overrides_and_cleanup_are_exact_to_one_owner_generation() {
        let harness = Harness::new(8);
        let identifier = c_string("hello");
        let language = c_string("en");
        let first = c_string("Generation one");
        let second = c_string("Generation two");

        {
            let _scope = harness.enter_owner(OWNER);
            harness
                .api
                .set_translated_string(identifier.as_ptr(), language.as_ptr(), first.as_ptr())
                .expect("first override");
        }
        {
            let _scope = harness.enter_owner(OTHER_OWNER);
            harness
                .api
                .set_translated_string(identifier.as_ptr(), language.as_ptr(), second.as_ptr())
                .expect("second override");
        }
        harness.advance();

        {
            let _scope = harness.enter_owner(OWNER);
            let pointer = harness
                .api
                .translate(identifier.as_ptr())
                .expect("latest override");
            assert_eq!(text_at(pointer), "Generation two");
        }

        harness.api.cleanup_owner(OWNER).expect("first cleanup");
        harness.advance();
        {
            let _scope = harness.enter_owner(OWNER);
            let pointer = harness
                .api
                .translate(identifier.as_ptr())
                .expect("remaining override");
            assert_eq!(text_at(pointer), "Generation two");
        }

        harness
            .api
            .cleanup_owner(OTHER_OWNER)
            .expect("second cleanup");
        harness.advance();
        let _scope = harness.enter_owner(OWNER);
        let pointer = harness
            .api
            .translate(identifier.as_ptr())
            .expect("base translation");
        assert_eq!(text_at(pointer), "Hello");
    }

    #[test]
    fn invalid_memory_full_queues_and_closed_admission_fail_closed() {
        let harness = Harness::new(1);
        let identifier = c_string("hello");
        let language = c_string("en");
        let value = c_string("override");
        let _scope = harness.enter_owner(OWNER);

        harness
            .api
            .set_translated_string(identifier.as_ptr(), language.as_ptr(), value.as_ptr())
            .expect("first queued override");
        assert_eq!(
            harness.api.set_translated_string(
                identifier.as_ptr(),
                language.as_ptr(),
                value.as_ptr(),
            ),
            Err(BackendOperationError::ServiceRejected)
        );
        assert_eq!(
            harness.api.translate(core::ptr::null()),
            Err(BackendOperationError::Boundary(
                CallBoundaryError::NativeMemory
            ))
        );

        assert!(harness.gate.close());
        assert_eq!(
            harness.api.translate(identifier.as_ptr()),
            Err(BackendOperationError::Boundary(
                CallBoundaryError::CallerAttribution
            ))
        );
        assert_eq!(
            harness.failures.snapshot(),
            BackendFailureSnapshot {
                caller_attribution: 1,
                native_memory: 1,
                service_rejected: 1,
                ..BackendFailureSnapshot::default()
            }
        );
    }
}
