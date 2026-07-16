use core::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::{NativeRenderCallback, OwnerGeneration, OwnerHandle};

static NEXT_MANAGED_CALLBACK: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallbackKey {
    Managed(u64),
    Native(usize),
}

enum CallbackAction {
    Managed(Arc<dyn Fn() + Send + Sync + 'static>),
    Native(NativeRenderCallback),
}

impl fmt::Debug for CallbackAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Managed(_) => formatter.write_str("Managed(..)"),
            Self::Native(_) => formatter.write_str("Native(..)"),
        }
    }
}

/// Cloneable callback identity used by render and context-menu registries.
///
/// Cloning preserves identity, allowing the legacy duplicate-by-function
/// behavior without exposing a raw function address for managed callbacks.
#[derive(Clone, Debug)]
pub struct UiCallback {
    owner: OwnerHandle,
    key: CallbackKey,
    action: Arc<CallbackAction>,
}

impl UiCallback {
    /// Creates a panic-contained managed callback with a fresh identity.
    #[must_use]
    pub fn managed<F>(owner: OwnerHandle, callback: F) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        Self {
            owner,
            key: CallbackKey::Managed(next_managed_callback()),
            action: Arc::new(CallbackAction::Managed(Arc::new(callback))),
        }
    }

    /// Creates a callback backed by the native addon ABI.
    #[must_use]
    pub fn native(callback: NativeRenderCallback) -> Self {
        Self {
            owner: callback.owner().clone(),
            key: CallbackKey::Native(callback.address()),
            action: Arc::new(CallbackAction::Native(callback)),
        }
    }

    /// Returns the callback's owner generation.
    #[must_use]
    pub const fn owner(&self) -> OwnerGeneration {
        self.owner.identity()
    }

    pub(crate) fn same_identity(&self, other: &Self) -> bool {
        self.owner() == other.owner() && self.key == other.key
    }

    pub(crate) fn try_enter_owner(&self) -> Option<crate::owner::OwnerActivity> {
        self.owner.try_enter()
    }
}

fn next_managed_callback() -> u64 {
    loop {
        let current = NEXT_MANAGED_CALLBACK.load(Ordering::Relaxed);
        let next = if current == u64::MAX { 1 } else { current + 1 };
        if NEXT_MANAGED_CALLBACK
            .compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return current;
        }
    }
}

/// Result of one callback invocation attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallbackInvocation {
    /// The callback returned normally.
    Invoked,
    /// The managed callback panicked; `disabled` reports whether its budget
    /// was exhausted by this panic.
    Panicked {
        /// Whether future invocations are disabled.
        disabled: bool,
    },
    /// The registration was removed after its snapshot was produced.
    SkippedInactive,
    /// The owning addon generation has begun cleanup.
    SkippedOwnerRetired,
    /// The registration exhausted its configured panic budget.
    SkippedPanicDisabled,
}

#[derive(Debug)]
pub(crate) struct CallbackSlot {
    callback: UiCallback,
    active: AtomicBool,
    disabled: AtomicBool,
    panics: AtomicU32,
    maximum_panics: u32,
}

impl CallbackSlot {
    pub(crate) fn new(callback: UiCallback, maximum_panics: u32) -> Arc<Self> {
        Arc::new(Self {
            callback,
            active: AtomicBool::new(true),
            disabled: AtomicBool::new(false),
            panics: AtomicU32::new(0),
            maximum_panics,
        })
    }

    pub(crate) fn callback(&self) -> &UiCallback {
        &self.callback
    }

    pub(crate) fn owner(&self) -> OwnerGeneration {
        self.callback.owner()
    }

    pub(crate) fn owner_handle(&self) -> OwnerHandle {
        self.callback.owner.clone()
    }

    pub(crate) fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
    }

    pub(crate) fn invoke(&self) -> CallbackInvocation {
        if !self.active.load(Ordering::Acquire) {
            return CallbackInvocation::SkippedInactive;
        }
        if self.disabled.load(Ordering::Acquire) {
            return CallbackInvocation::SkippedPanicDisabled;
        }
        let Some(_activity) = self.callback.owner.try_enter() else {
            return CallbackInvocation::SkippedOwnerRetired;
        };
        if !self.active.load(Ordering::Acquire) {
            return CallbackInvocation::SkippedInactive;
        }

        let result = catch_unwind(AssertUnwindSafe(|| match self.callback.action.as_ref() {
            CallbackAction::Managed(callback) => callback(),
            CallbackAction::Native(callback) => {
                callback.invoke();
            }
        }));
        if result.is_ok() {
            return CallbackInvocation::Invoked;
        }

        let previous = self.panics.fetch_add(1, Ordering::AcqRel);
        let disabled = previous.saturating_add(1) >= self.maximum_panics;
        if disabled {
            self.disabled.store(true, Ordering::Release);
        }
        CallbackInvocation::Panicked { disabled }
    }
}
