use std::{collections::HashMap, sync::Arc, time::Duration};

use nexus_abi::AddonDefinitionFlags;
use nexus_core::{CallbackGate, OwnerToken};
use thiserror::Error;

use crate::{
    ApiTableCatalog, ApiTableError, ApiTableRef, DefinitionError, LiveAddonModule, MetadataLimits,
    OwnedAddonDefinition, validate_and_copy_definition,
};

/// Explicit coordination state for one add-on generation.
///
/// `Loaded` and `Unloaded` are acknowledgements from a future native-module
/// adapter. This crate itself performs neither operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleState {
    /// Metadata was validated and copied, but no load callback is acknowledged.
    Discovered,
    /// The platform layer acknowledged successful native activation.
    Loaded,
    /// Unload was requested and new callbacks are rejected.
    UnloadRequested,
    /// In-flight callbacks and registrations are being drained.
    Draining,
    /// The platform layer acknowledged that the native module was released.
    Unloaded,
    /// Activation failed before the generation became loaded.
    Failed,
}

/// Why a definition is being admitted into the host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoadMode {
    /// First activation during process startup.
    Initial,
    /// Runtime replacement of an earlier generation.
    HotReload,
}

/// Why an active generation is being asked to unload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnloadReason {
    /// User- or updater-requested runtime unload.
    Runtime,
    /// Process shutdown, when locked-lifetime add-ons may still be cleaned up.
    Shutdown,
}

/// Ordered cleanup groups run around the callback-gate drain boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CleanupPhase {
    /// Disable and remove hooks before waiting for native callback drain.
    HookRegistrations,
    /// Remove render, event, window, and input dispatch registrations.
    CallbackRegistrations,
    /// Release host resources only after all in-flight callbacks have left.
    OwnedResources,
}

impl CleanupPhase {
    /// Safety order used for every generation.
    pub const ORDER: [Self; 3] = [
        Self::HookRegistrations,
        Self::CallbackRegistrations,
        Self::OwnedResources,
    ];

    const PRE_DRAIN_COUNT: usize = 2;
}

/// A subsystem cleanup failure without a native pointer payload.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("{message}")]
pub struct CleanupError {
    message: Box<str>,
}

impl CleanupError {
    /// Creates a cleanup failure suitable for propagation through the host.
    #[must_use]
    pub fn new(message: impl Into<Box<str>>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Injected generation-aware registration cleanup.
pub trait RegistrationCleaner {
    /// Cleans one phase for exactly `owner`.
    ///
    /// Implementations must be idempotent for a retried phase. The host advances
    /// its phase cursor only after this method succeeds.
    fn cleanup(&mut self, owner: OwnerToken, phase: CleanupPhase) -> Result<(), CleanupError>;
}

/// A no-op cleaner for hosts that have not assembled registration services yet.
impl RegistrationCleaner for () {
    fn cleanup(&mut self, _owner: OwnerToken, _phase: CleanupPhase) -> Result<(), CleanupError> {
        Ok(())
    }
}

/// Read-only state retained for one add-on generation.
pub struct AddonRecord {
    owner: OwnerToken,
    definition: OwnedAddonDefinition,
    load_mode: LoadMode,
    state: LifecycleState,
    gate: Arc<CallbackGate>,
    cleanup_cursor: usize,
}

impl AddonRecord {
    /// Returns this exact signature and generation.
    #[must_use]
    pub const fn owner(&self) -> OwnerToken {
        self.owner
    }

    /// Returns the fully owned, pointer-free definition metadata.
    #[must_use]
    pub const fn definition(&self) -> &OwnedAddonDefinition {
        &self.definition
    }

    /// Returns whether this is an initial load or a hot reload.
    #[must_use]
    pub const fn load_mode(&self) -> LoadMode {
        self.load_mode
    }

    /// Returns the current explicit lifecycle state.
    #[must_use]
    pub const fn state(&self) -> LifecycleState {
        self.state
    }

    /// Returns the number of completed cleanup phases.
    #[must_use]
    pub const fn completed_cleanup_phases(&self) -> usize {
        self.cleanup_cursor
    }
}

/// A safe registry and state machine for native add-on generations.
pub struct AddonHost<C> {
    cleaner: C,
    api_tables: Arc<ApiTableCatalog>,
    records: HashMap<OwnerToken, AddonRecord>,
    active_by_signature: HashMap<u32, OwnerToken>,
    last_generation: HashMap<u32, u64>,
}

impl<C: RegistrationCleaner> AddonHost<C> {
    /// Creates a host with an injected registration cleaner.
    pub fn new(cleaner: C) -> Result<Self, HostError> {
        Ok(Self {
            cleaner,
            api_tables: Arc::new(ApiTableCatalog::new()?),
            records: HashMap::new(),
            active_by_signature: HashMap::new(),
            last_generation: HashMap::new(),
        })
    }

    /// Creates a host using a fully assembled render-session API catalog.
    #[must_use]
    pub fn with_api_tables(cleaner: C, api_tables: Arc<ApiTableCatalog>) -> Self {
        Self {
            cleaner,
            api_tables,
            records: HashMap::new(),
            active_by_signature: HashMap::new(),
            last_generation: HashMap::new(),
        }
    }

    /// Returns whether activation can expose callable API tables.
    #[must_use]
    pub fn api_tables_populated(&self) -> bool {
        self.api_tables.is_populated()
    }

    /// Validates and owns a definition without loading or calling native code.
    pub fn discover(
        &mut self,
        module: &impl LiveAddonModule,
        mode: LoadMode,
        limits: MetadataLimits,
    ) -> Result<OwnerToken, HostError> {
        let definition = validate_and_copy_definition(module, limits)?;
        let signature = definition.signature();

        if let Some(existing) = self.active_by_signature.get(&signature).copied() {
            return Err(HostError::DuplicateSignature {
                signature,
                existing,
            });
        }

        if mode == LoadMode::HotReload
            && (definition
                .flags()
                .contains(AddonDefinitionFlags::DISABLE_HOT_LOADING)
                || definition
                    .flags()
                    .contains(AddonDefinitionFlags::LAUNCH_ONLY))
        {
            return Err(HostError::HotReloadForbidden { signature });
        }

        let generation = self
            .last_generation
            .get(&signature)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(HostError::GenerationExhausted { signature })?;
        let owner = OwnerToken {
            signature,
            generation,
        };
        self.last_generation.insert(signature, generation);
        self.active_by_signature.insert(signature, owner);
        self.records.insert(
            owner,
            AddonRecord {
                owner,
                definition,
                load_mode: mode,
                state: LifecycleState::Discovered,
                gate: Arc::new(CallbackGate::open()),
                cleanup_cursor: 0,
            },
        );
        Ok(owner)
    }

    /// Acknowledges that an external platform adapter completed activation.
    ///
    /// This method does not invoke the add-on's load callback.
    pub fn acknowledge_loaded(&mut self, owner: OwnerToken) -> Result<(), HostError> {
        let record = self.record_mut(owner)?;
        ensure_state(record, &[LifecycleState::Discovered], "Discovered")?;
        record.state = LifecycleState::Loaded;
        Ok(())
    }

    /// Rejects new callbacks and records an unload request.
    ///
    /// This method does not invoke the add-on's unload callback.
    pub fn request_unload(
        &mut self,
        owner: OwnerToken,
        reason: UnloadReason,
    ) -> Result<(), HostError> {
        let record = self.record_mut(owner)?;
        ensure_state(record, &[LifecycleState::Loaded], "Loaded")?;
        if reason == UnloadReason::Runtime
            && record
                .definition
                .flags()
                .contains(AddonDefinitionFlags::DISABLE_HOT_LOADING)
        {
            return Err(HostError::RuntimeUnloadForbidden { owner });
        }

        record.gate.close();
        record.state = LifecycleState::UnloadRequested;
        Ok(())
    }

    /// Waits for callbacks and cleans registrations in the fixed safety order.
    ///
    /// The method may block for at most `timeout`; it never starts a thread. A
    /// timeout or failed phase leaves the record in `Draining` so the caller may
    /// retry without repeating earlier successful phases.
    pub fn drain_registrations(
        &mut self,
        owner: OwnerToken,
        timeout: Duration,
    ) -> Result<(), HostError> {
        let (records, cleaner) = (&mut self.records, &mut self.cleaner);
        let record = records
            .get_mut(&owner)
            .ok_or(HostError::UnknownOwner { owner })?;
        ensure_state(
            record,
            &[LifecycleState::UnloadRequested, LifecycleState::Draining],
            "UnloadRequested or Draining",
        )?;
        record.state = LifecycleState::Draining;

        while record.cleanup_cursor < CleanupPhase::PRE_DRAIN_COUNT {
            cleanup_next_phase(cleaner, record)?;
        }

        if !record.gate.wait_for_drain(timeout) {
            return Err(HostError::DrainTimedOut { owner });
        }

        while record.cleanup_cursor < CleanupPhase::ORDER.len() {
            cleanup_next_phase(cleaner, record)?;
        }
        Ok(())
    }

    /// Acknowledges that a future platform layer released the native module.
    ///
    /// This method does not call `FreeLibrary` or any equivalent operation.
    pub fn acknowledge_module_released(&mut self, owner: OwnerToken) -> Result<(), HostError> {
        {
            let record = self.record_mut(owner)?;
            ensure_state(record, &[LifecycleState::Draining], "Draining")?;
            if record.cleanup_cursor != CleanupPhase::ORDER.len() || record.gate.in_flight() != 0 {
                return Err(HostError::DrainIncomplete { owner });
            }
            record.state = LifecycleState::Unloaded;
        }

        if self.active_by_signature.get(&owner.signature) == Some(&owner) {
            self.active_by_signature.remove(&owner.signature);
        }
        Ok(())
    }

    /// Records activation failure before native loading was acknowledged.
    pub fn acknowledge_activation_failed(&mut self, owner: OwnerToken) -> Result<(), HostError> {
        {
            let record = self.record_mut(owner)?;
            ensure_state(record, &[LifecycleState::Discovered], "Discovered")?;
            record.gate.close();
            record.state = LifecycleState::Failed;
        }
        if self.active_by_signature.get(&owner.signature) == Some(&owner) {
            self.active_by_signature.remove(&owner.signature);
        }
        Ok(())
    }

    /// Cancels a discovered generation before its native load callback ran.
    ///
    /// The per-signature generation counter remains monotonic, while the
    /// inactive record and active-signature claim are removed atomically.
    pub fn cancel_discovery(&mut self, owner: OwnerToken) -> Result<(), HostError> {
        {
            let record = self.record_mut(owner)?;
            ensure_state(record, &[LifecycleState::Discovered], "Discovered")?;
            record.gate.close();
        }
        if self.active_by_signature.get(&owner.signature) == Some(&owner) {
            self.active_by_signature.remove(&owner.signature);
        }
        self.records.remove(&owner);
        Ok(())
    }

    /// Returns one generation record, including historical terminal records.
    #[must_use]
    pub fn record(&self, owner: OwnerToken) -> Option<&AddonRecord> {
        self.records.get(&owner)
    }

    /// Returns the currently active generation for a signature.
    #[must_use]
    pub fn active_owner(&self, signature: u32) -> Option<OwnerToken> {
        self.active_by_signature.get(&signature).copied()
    }

    /// Borrows the pinned API table selected by this generation.
    pub fn api_table(&self, owner: OwnerToken) -> Result<ApiTableRef<'_>, HostError> {
        let revision = self
            .record(owner)
            .ok_or(HostError::UnknownOwner { owner })?
            .definition
            .api_revision();
        Ok(self.api_tables.get(revision))
    }

    /// Clones the callback gate used by owner-aware registration wrappers.
    pub fn callback_gate(&self, owner: OwnerToken) -> Result<Arc<CallbackGate>, HostError> {
        self.record(owner)
            .map(|record| Arc::clone(&record.gate))
            .ok_or(HostError::UnknownOwner { owner })
    }

    /// Borrows the injected cleaner, primarily for diagnostics and tests.
    #[must_use]
    pub const fn cleaner(&self) -> &C {
        &self.cleaner
    }

    fn record_mut(&mut self, owner: OwnerToken) -> Result<&mut AddonRecord, HostError> {
        self.records
            .get_mut(&owner)
            .ok_or(HostError::UnknownOwner { owner })
    }
}

fn cleanup_next_phase<C: RegistrationCleaner>(
    cleaner: &mut C,
    record: &mut AddonRecord,
) -> Result<(), HostError> {
    let owner = record.owner;
    let phase = CleanupPhase::ORDER[record.cleanup_cursor];
    cleaner
        .cleanup(owner, phase)
        .map_err(|source| HostError::Cleanup {
            owner,
            phase,
            source,
        })?;
    record.cleanup_cursor += 1;
    Ok(())
}
fn ensure_state(
    record: &AddonRecord,
    expected: &[LifecycleState],
    expected_label: &'static str,
) -> Result<(), HostError> {
    if expected.contains(&record.state) {
        Ok(())
    } else {
        Err(HostError::InvalidTransition {
            owner: record.owner,
            expected: expected_label,
            actual: record.state,
        })
    }
}

/// Failure to validate or advance an add-on generation.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum HostError {
    /// Definition validation or metadata copying failed.
    #[error(transparent)]
    Definition(#[from] DefinitionError),
    /// API table allocation failed.
    #[error(transparent)]
    ApiTable(#[from] ApiTableError),
    /// Another non-terminal generation already owns the signature.
    #[error("add-on signature {signature:#010x} is already active as {existing:?}")]
    DuplicateSignature {
        /// Duplicated signature.
        signature: u32,
        /// Existing active owner.
        existing: OwnerToken,
    },
    /// A per-signature generation counter overflowed.
    #[error("add-on signature {signature:#010x} exhausted its generation counter")]
    GenerationExhausted {
        /// Signature whose counter overflowed.
        signature: u32,
    },
    /// The supplied owner token has never been discovered.
    #[error("unknown add-on owner {owner:?}")]
    UnknownOwner {
        /// Unknown owner token.
        owner: OwnerToken,
    },
    /// The operation is not valid in the current state.
    #[error("owner {owner:?} must be {expected}, but is {actual:?}")]
    InvalidTransition {
        /// Target generation.
        owner: OwnerToken,
        /// Human-readable accepted states.
        expected: &'static str,
        /// Current state.
        actual: LifecycleState,
    },
    /// Definition flags forbid a runtime replacement.
    #[error("add-on signature {signature:#010x} forbids hot reload")]
    HotReloadForbidden {
        /// Locked signature.
        signature: u32,
    },
    /// Definition flags forbid an ordinary runtime unload.
    #[error("owner {owner:?} disables runtime unloading")]
    RuntimeUnloadForbidden {
        /// Locked generation.
        owner: OwnerToken,
    },
    /// Callbacks remained in flight until the caller-provided deadline.
    #[error("owner {owner:?} did not drain callbacks before the deadline")]
    DrainTimedOut {
        /// Generation that timed out.
        owner: OwnerToken,
    },
    /// One ordered cleanup phase failed.
    #[error("owner {owner:?} failed cleanup phase {phase:?}: {source}")]
    Cleanup {
        /// Generation being cleaned.
        owner: OwnerToken,
        /// Phase that failed.
        phase: CleanupPhase,
        /// Subsystem failure.
        #[source]
        source: CleanupError,
    },
    /// Module release was acknowledged before callback and registration drain.
    #[error("owner {owner:?} cannot be released before its drain completes")]
    DrainIncomplete {
        /// Generation that is not ready.
        owner: OwnerToken,
    },
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, ffi::c_char, ptr::NonNull, time::Duration};

    use nexus_abi::{AddonApi, AddonDefinitionFlags, AddonDefinitionV1, UpdateProvider, Version};
    use nexus_core::OwnerToken;

    use super::{
        AddonHost, CleanupError, CleanupPhase, HostError, LifecycleState, LoadMode,
        RegistrationCleaner, UnloadReason,
    };
    use crate::{
        DefinitionLease, LiveAddonModule, MetadataLimits, ModuleAccessError, ModuleMemory,
        ModuleReadError,
    };

    unsafe extern "C" fn load_stub(_api: *mut AddonApi) {}
    unsafe extern "C" fn unload_stub() {}

    #[derive(Default)]
    struct RecordingCleaner {
        calls: Vec<(OwnerToken, CleanupPhase)>,
    }

    impl RegistrationCleaner for RecordingCleaner {
        fn cleanup(&mut self, owner: OwnerToken, phase: CleanupPhase) -> Result<(), CleanupError> {
            self.calls.push((owner, phase));
            Ok(())
        }
    }

    struct TestModule {
        definition: AddonDefinitionV1,
        memory: TestMemory,
    }

    impl TestModule {
        fn new(signature: u32, flags: AddonDefinitionFlags) -> Self {
            let mut memory = TestMemory::default();
            let name = memory.insert(b"Name\0");
            let author = memory.insert(b"Author\0");
            let description = memory.insert(b"Description\0");
            Self {
                definition: AddonDefinitionV1 {
                    signature,
                    api_version: 6,
                    name,
                    version: Version::new(1, 0, 0, 0),
                    author,
                    description,
                    load: Some(load_stub),
                    unload: Some(unload_stub),
                    flags,
                    provider: UpdateProvider::NONE,
                    update_link: std::ptr::null(),
                },
                memory,
            }
        }
    }

    impl LiveAddonModule for TestModule {
        fn definition(&self) -> Result<DefinitionLease<'_>, ModuleAccessError> {
            Ok(DefinitionLease::new(&self.definition, &self.memory))
        }
    }

    #[derive(Default)]
    struct TestMemory {
        entries: HashMap<usize, Box<[u8]>>,
    }

    impl TestMemory {
        fn insert(&mut self, value: &[u8]) -> *const c_char {
            let value: Box<[u8]> = value.into();
            let pointer = value.as_ptr();
            self.entries.insert(pointer as usize, value);
            pointer.cast()
        }
    }

    impl ModuleMemory for TestMemory {
        fn read_bounded(
            &self,
            address: NonNull<c_char>,
            maximum_bytes: usize,
        ) -> Result<&[u8], ModuleReadError> {
            let bytes = self
                .entries
                .get(&(address.as_ptr() as usize))
                .ok_or_else(|| ModuleReadError::new("unknown test address"))?;
            Ok(&bytes[..bytes.len().min(maximum_bytes)])
        }
    }

    #[test]
    fn lifecycle_drains_registrations_in_safety_order() {
        let module = TestModule::new(7, AddonDefinitionFlags::NONE);
        let mut host = AddonHost::new(RecordingCleaner::default()).expect("host should allocate");
        let owner = host
            .discover(&module, LoadMode::Initial, MetadataLimits::default())
            .expect("definition should be discovered");

        assert_eq!(
            host.record(owner).map(|record| record.state()),
            Some(LifecycleState::Discovered)
        );
        host.acknowledge_loaded(owner)
            .expect("activation acknowledgement should work");
        host.request_unload(owner, UnloadReason::Runtime)
            .expect("runtime unload should be accepted");
        assert_eq!(
            host.record(owner).map(|record| record.state()),
            Some(LifecycleState::UnloadRequested)
        );
        host.drain_registrations(owner, Duration::ZERO)
            .expect("empty gate should drain");
        assert_eq!(
            host.record(owner).map(|record| record.state()),
            Some(LifecycleState::Draining)
        );

        let phases: Vec<_> = host
            .cleaner()
            .calls
            .iter()
            .map(|(called_owner, phase)| {
                assert_eq!(*called_owner, owner);
                *phase
            })
            .collect();
        assert_eq!(phases, CleanupPhase::ORDER);

        host.acknowledge_module_released(owner)
            .expect("release acknowledgement should work after drain");
        assert_eq!(
            host.record(owner).map(|record| record.state()),
            Some(LifecycleState::Unloaded)
        );
        assert_eq!(host.active_owner(owner.signature), None);
    }

    #[test]
    fn timeout_keeps_resources_and_successful_phases_safe_for_retry() {
        let module = TestModule::new(14, AddonDefinitionFlags::NONE);
        let mut host = AddonHost::new(RecordingCleaner::default()).expect("host should allocate");
        let owner = host
            .discover(&module, LoadMode::Initial, MetadataLimits::default())
            .expect("definition should be discovered");
        host.acknowledge_loaded(owner)
            .expect("activation acknowledgement should work");
        let gate = host.callback_gate(owner).expect("gate should exist");
        let guard = gate
            .try_enter()
            .expect("loaded gate should admit callbacks");

        host.request_unload(owner, UnloadReason::Runtime)
            .expect("unload should close ingress");
        assert_eq!(
            host.drain_registrations(owner, Duration::ZERO),
            Err(HostError::DrainTimedOut { owner })
        );
        let first_attempt: Vec<_> = host
            .cleaner()
            .calls
            .iter()
            .map(|(_, phase)| *phase)
            .collect();
        assert_eq!(
            first_attempt,
            [
                CleanupPhase::HookRegistrations,
                CleanupPhase::CallbackRegistrations
            ]
        );

        drop(guard);
        host.drain_registrations(owner, Duration::ZERO)
            .expect("retry should finish after the callback leaves");
        let completed: Vec<_> = host
            .cleaner()
            .calls
            .iter()
            .map(|(_, phase)| *phase)
            .collect();
        assert_eq!(completed, CleanupPhase::ORDER);
    }

    #[test]
    fn disable_hot_loading_blocks_runtime_but_not_shutdown_cleanup() {
        let module = TestModule::new(8, AddonDefinitionFlags::DISABLE_HOT_LOADING);
        let mut host = AddonHost::new(()).expect("host should allocate");
        let owner = host
            .discover(&module, LoadMode::Initial, MetadataLimits::default())
            .expect("initial discovery should work");
        host.acknowledge_loaded(owner)
            .expect("activation acknowledgement should work");

        assert_eq!(
            host.request_unload(owner, UnloadReason::Runtime),
            Err(HostError::RuntimeUnloadForbidden { owner })
        );
        host.request_unload(owner, UnloadReason::Shutdown)
            .expect("shutdown cleanup must remain possible");
    }

    #[test]
    fn launch_only_and_locked_addons_cannot_hot_reload() {
        for (signature, flags) in [
            (9, AddonDefinitionFlags::LAUNCH_ONLY),
            (10, AddonDefinitionFlags::DISABLE_HOT_LOADING),
        ] {
            let module = TestModule::new(signature, flags);
            let mut host = AddonHost::new(()).expect("host should allocate");
            assert_eq!(
                host.discover(&module, LoadMode::HotReload, MetadataLimits::default()),
                Err(HostError::HotReloadForbidden { signature })
            );
        }
    }

    #[test]
    fn generations_are_isolated_after_release() {
        let module = TestModule::new(11, AddonDefinitionFlags::NONE);
        let mut host = AddonHost::new(RecordingCleaner::default()).expect("host should allocate");
        let first = host
            .discover(&module, LoadMode::Initial, MetadataLimits::default())
            .expect("first generation should discover");
        host.acknowledge_loaded(first)
            .expect("first should activate");
        host.request_unload(first, UnloadReason::Runtime)
            .expect("first should request unload");
        host.drain_registrations(first, Duration::ZERO)
            .expect("first should drain");
        host.acknowledge_module_released(first)
            .expect("first should finish");

        let second = host
            .discover(&module, LoadMode::HotReload, MetadataLimits::default())
            .expect("second generation should discover");
        assert_eq!(second.signature, first.signature);
        assert_eq!(second.generation, first.generation + 1);
        assert_eq!(host.active_owner(second.signature), Some(second));
        assert_eq!(
            host.record(first).map(|record| record.state()),
            Some(LifecycleState::Unloaded)
        );
        assert_eq!(
            host.record(second).map(|record| record.state()),
            Some(LifecycleState::Discovered)
        );
        assert!(matches!(
            host.acknowledge_loaded(first),
            Err(HostError::InvalidTransition { owner, .. }) if owner == first
        ));
        assert!(
            host.cleaner()
                .calls
                .iter()
                .all(|(cleaned_owner, _)| *cleaned_owner == first)
        );
    }

    #[test]
    fn failed_discovery_acknowledgement_releases_the_signature_only() {
        let module = TestModule::new(12, AddonDefinitionFlags::NONE);
        let mut host = AddonHost::new(()).expect("host should allocate");
        let failed = host
            .discover(&module, LoadMode::Initial, MetadataLimits::default())
            .expect("generation should discover");
        host.acknowledge_activation_failed(failed)
            .expect("pre-load failure should be accepted");
        assert_eq!(
            host.record(failed).map(|record| record.state()),
            Some(LifecycleState::Failed)
        );

        let next = host
            .discover(&module, LoadMode::Initial, MetadataLimits::default())
            .expect("signature should be available after acknowledged failure");
        assert_eq!(next.generation, failed.generation + 1);
        host.cancel_discovery(next)
            .expect("an unactivated discovery can be cancelled");
        assert!(host.record(next).is_none());

        let after_cancel = host
            .discover(&module, LoadMode::Initial, MetadataLimits::default())
            .expect("signature should be available after cancelled discovery");
        assert_eq!(after_cancel.generation, next.generation + 1);
    }

    #[test]
    fn api_table_is_selected_without_per_generation_allocation() {
        let module = TestModule::new(13, AddonDefinitionFlags::NONE);
        let mut host = AddonHost::new(()).expect("host should allocate");
        let owner = host
            .discover(&module, LoadMode::Initial, MetadataLimits::default())
            .expect("generation should discover");
        let first = host.api_table(owner).expect("table should exist");
        let second = host
            .api_table(owner)
            .expect("table should remain available");

        assert_eq!(first.revision(), crate::ApiRevision::V6);
        assert_eq!(first.as_opaque_ptr(), second.as_opaque_ptr());
    }
}
