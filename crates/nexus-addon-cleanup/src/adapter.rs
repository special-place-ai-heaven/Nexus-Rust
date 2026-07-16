use crate::domain::CleanupDomain;
use nexus_core::OwnerToken;
use std::fmt;
use std::marker::PhantomData;

/// Successful effect reported by one cleanup adapter invocation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CleanupEffect {
    removed: usize,
}

impl CleanupEffect {
    /// Reports a completed cleanup and the number of registrations affected.
    #[must_use]
    pub const fn complete(removed: usize) -> Self {
        Self { removed }
    }

    /// Number of registrations or resources removed by this invocation.
    #[must_use]
    pub const fn removed(self) -> usize {
        self.removed
    }
}

/// Redaction-safe category for a recoverable adapter failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdapterFailureKind {
    /// Cleanup cannot complete while owner activity remains in flight.
    Busy,
    /// The concrete service rejected its cleanup operation.
    Rejected,
    /// Work was accepted but synchronous completion was not proven.
    CompletionUnverified,
    /// A required upstream service is temporarily unavailable.
    Unavailable,
}

/// Structured, redaction-safe adapter failure.
///
/// No free-form source text is retained because upstream errors can contain
/// paths, addresses, or user-controlled values. Counts are preserved so a
/// retry report remains useful after partial progress.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct AdapterError {
    kind: AdapterFailureKind,
    removed: usize,
    remaining: Option<usize>,
}

impl AdapterError {
    /// Creates an adapter failure with no observed partial effect.
    #[must_use]
    pub const fn new(kind: AdapterFailureKind) -> Self {
        Self {
            kind,
            removed: 0,
            remaining: None,
        }
    }

    /// Adds the number of registrations removed before failure.
    #[must_use]
    pub const fn with_removed(mut self, removed: usize) -> Self {
        self.removed = removed;
        self
    }

    /// Adds the number of exact-owner registrations known to remain.
    #[must_use]
    pub const fn with_remaining(mut self, remaining: usize) -> Self {
        self.remaining = Some(remaining);
        self
    }

    /// Failure category.
    #[must_use]
    pub const fn kind(self) -> AdapterFailureKind {
        self.kind
    }

    /// Partial effect observed before the failure.
    #[must_use]
    pub const fn removed(self) -> usize {
        self.removed
    }

    /// Exact-owner registrations known to remain, when the service reports it.
    #[must_use]
    pub const fn remaining(self) -> Option<usize> {
        self.remaining
    }
}

impl fmt::Debug for AdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdapterError")
            .field("kind", &self.kind)
            .field("removed", &self.removed)
            .field("remaining", &self.remaining)
            .finish()
    }
}

/// Result returned by a cleanup adapter.
pub type AdapterResult = Result<CleanupEffect, AdapterError>;

/// Why a required adapter slot has no callable safe implementation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GapReason {
    /// The embedding runtime did not bind this required service.
    NotConfigured,
    /// The concrete owner-aware service is not available at this layer.
    ConcreteServiceUnavailable,
    /// Cleanup requires an unavailable thread-affinity bridge.
    ThreadAffinityBridgeUnavailable,
    /// Upstream only queues work and exposes no completion acknowledgement.
    CompletionAcknowledgementUnavailable,
    /// Upstream lacks a phase-safe owner cleanup operation.
    UpstreamApiMissing,
    /// Upstream cleanup cannot yet satisfy the no-drop-under-lock invariant.
    UpstreamLockSafetyUnproven,
}

type AdapterCallback = dyn FnMut(OwnerToken) -> AdapterResult + 'static;

pub(crate) enum ErasedAdapter {
    Callback(Box<AdapterCallback>),
    Gap(GapReason),
}

impl ErasedAdapter {
    pub(crate) fn invoke(&mut self, owner: OwnerToken) -> Option<AdapterResult> {
        match self {
            Self::Callback(callback) => Some(callback(owner)),
            Self::Gap(_) => None,
        }
    }

    pub(crate) const fn gap(&self) -> Option<GapReason> {
        match self {
            Self::Callback(_) => None,
            Self::Gap(reason) => Some(*reason),
        }
    }
}

/// Type-tagged adapter for exactly one cleanup domain.
///
/// `D` is a sealed marker such as [`crate::InlineHooks`] or
/// [`crate::LocalizationOverrides`]. The marker prevents accidentally wiring
/// a texture cleanup closure into the event slot.
pub struct TypedAdapter<D: CleanupDomain> {
    inner: ErasedAdapter,
    domain: PhantomData<fn() -> D>,
}

impl<D: CleanupDomain> TypedAdapter<D> {
    /// Injects a synchronous exact-owner cleanup operation.
    pub fn new<F>(callback: F) -> Self
    where
        F: FnMut(OwnerToken) -> AdapterResult + 'static,
    {
        Self {
            inner: ErasedAdapter::Callback(Box::new(callback)),
            domain: PhantomData,
        }
    }

    /// Installs an explicit fail-closed gap for an unavailable operation.
    #[must_use]
    pub const fn gap(reason: GapReason) -> Self {
        Self {
            inner: ErasedAdapter::Gap(reason),
            domain: PhantomData,
        }
    }

    pub(crate) fn erase(self) -> ErasedAdapter {
        self.inner
    }
}

impl<D: CleanupDomain> fmt::Debug for TypedAdapter<D> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TypedAdapter")
            .field("service", &D::SERVICE)
            .field("gap", &self.inner.gap())
            .finish_non_exhaustive()
    }
}
