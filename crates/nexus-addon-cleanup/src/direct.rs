//! Faithful typed adapters for currently public concrete services.
//!
//! These helpers add no cleaner-owned lock. Consult
//! [`crate::CLEANUP_API_INVENTORY`] before selecting them: several upstream
//! registries currently remove callback-capable values while holding their own
//! locks. An embedding runtime can instead inject a corrected bridge with
//! [`crate::TypedAdapter::new`].

use crate::{
    AdapterError, AdapterFailureKind, CleanupEffect, EventCallbacks, FontCallbacks, FontResources,
    InlineHooks, LocalizationOverrides, ManagedInputCallbacks, RawWndProcCallbacks,
    TextureCallbacks, TypedAdapter, UiHostCallbacks,
};
use nexus_data_services::EventService;
use nexus_inline_hooks::InlineHookService;
use nexus_input::{ManagedInputBinds, RawWndProcRegistry};
use nexus_textures::TextureService;
use nexus_ui_host::{UiHost, UiHostCleanup};
use nexus_ui_services::{FontAtlasBackend, FontManager, LocalizationService};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

/// Adapts the owner-aware inline-hook service.
#[must_use]
pub fn inline_hooks(service: Arc<InlineHookService>) -> TypedAdapter<InlineHooks> {
    TypedAdapter::new(move |owner| match service.cleanup_owner(owner) {
        Ok(report) => Ok(CleanupEffect::complete(report.retired())),
        Err(error) => Err(AdapterError::new(AdapterFailureKind::Rejected)
            .with_removed(error.retired())
            .with_remaining(error.remaining())),
    })
}

/// Adapts `UiHost`'s public render/window/UI callback composite.
#[must_use]
pub fn ui_host(service: Arc<UiHost>) -> TypedAdapter<UiHostCallbacks> {
    TypedAdapter::new(move |owner| {
        let report = service.cleanup_owner_generation(owner.into());
        let removed = ui_host_removed(&report);
        if report.retirement.quiescent {
            Ok(CleanupEffect::complete(removed))
        } else {
            Err(AdapterError::new(AdapterFailureKind::Busy)
                .with_removed(removed)
                .with_remaining(report.retirement.in_flight))
        }
    })
}

/// Adapts the raw WndProc registry.
#[must_use]
pub fn raw_wndproc(service: Arc<RawWndProcRegistry>) -> TypedAdapter<RawWndProcCallbacks> {
    TypedAdapter::new(move |owner| {
        Ok(CleanupEffect::complete(
            service.cleanup_owner_generation(owner.into()),
        ))
    })
}

/// Adapts the managed input registry.
#[must_use]
pub fn managed_input(service: Arc<ManagedInputBinds>) -> TypedAdapter<ManagedInputCallbacks> {
    TypedAdapter::new(move |owner| {
        Ok(CleanupEffect::complete(
            service.cleanup_owner_generation(owner.into()),
        ))
    })
}

/// Adapts the event subscription service.
#[must_use]
pub fn events(service: Arc<EventService>) -> TypedAdapter<EventCallbacks> {
    TypedAdapter::new(move |owner| {
        let report = service.cleanup_owner(owner);
        if report.quiescent() {
            Ok(CleanupEffect::complete(report.retired()))
        } else {
            Err(AdapterError::new(AdapterFailureKind::Busy)
                .with_removed(report.retired())
                .with_remaining(report.in_flight()))
        }
    })
}

/// Adapts the texture callback/request registry.
#[must_use]
pub fn textures(service: Arc<TextureService>) -> TypedAdapter<TextureCallbacks> {
    TypedAdapter::new(move |owner| {
        Ok(CleanupEffect::complete(
            service.cleanup_owner_generation(owner.into()),
        ))
    })
}

/// Adapts a shared, thread-bound font manager for pre-drain callback cleanup.
///
/// This removes only the exact generation's subscribers, so font resources stay
/// available to callbacks that are still draining. Pair it with
/// [`font_resources`], which runs after the callback gate has drained.
///
/// An outstanding `RefCell` borrow is reported as [`AdapterFailureKind::Busy`]
/// and can be retried after the borrow ends.
#[must_use]
pub fn font_callbacks<B>(service: Rc<RefCell<FontManager<B>>>) -> TypedAdapter<FontCallbacks>
where
    B: FontAtlasBackend + 'static,
{
    TypedAdapter::new(move |owner| {
        let mut service = service
            .try_borrow_mut()
            .map_err(|_borrowed| AdapterError::new(AdapterFailureKind::Busy))?;
        let removed = service.cleanup_owner_callbacks(owner.into());
        Ok(CleanupEffect::complete(removed))
    })
}

/// Adapts a shared, thread-bound font manager for post-drain cleanup.
///
/// This removes the exact generation's claims and sweeps entries that are now
/// unreferenced, including entries released by [`font_callbacks`].
///
/// An outstanding `RefCell` borrow is reported as [`AdapterFailureKind::Busy`]
/// and can be retried after the borrow ends.
#[must_use]
pub fn font_resources<B>(service: Rc<RefCell<FontManager<B>>>) -> TypedAdapter<FontResources>
where
    B: FontAtlasBackend + 'static,
{
    TypedAdapter::new(move |owner| {
        let mut service = service
            .try_borrow_mut()
            .map_err(|_borrowed| AdapterError::new(AdapterFailureKind::Busy))?;
        let removed = service.cleanup_owner_resources(owner.into());
        Ok(CleanupEffect::complete(removed))
    })
}

/// Adapts a shared localization service and proves completion by advancing its
/// queue synchronously after the exact-owner cleanup command is accepted.
///
/// This adapter must run on the service's owning UI thread. Using only a
/// `LocalizationHandle` is insufficient because queue acceptance does not
/// prove cleanup has completed.
#[must_use]
pub fn localization_overrides(
    service: Rc<RefCell<LocalizationService>>,
) -> TypedAdapter<LocalizationOverrides> {
    TypedAdapter::new(move |owner| {
        let mut service = service
            .try_borrow_mut()
            .map_err(|_borrowed| AdapterError::new(AdapterFailureKind::Busy))?;
        let handle = service.handle();
        handle
            .cleanup_owner(owner.into())
            .map_err(|_error| AdapterError::new(AdapterFailureKind::Rejected))?;
        let report = service.advance();
        Ok(CleanupEffect::complete(report.removed_overrides))
    })
}

fn ui_host_removed(report: &UiHostCleanup) -> usize {
    report
        .render_callbacks
        .saturating_add(report.escape_windows)
        .saturating_add(report.alerts)
        .saturating_add(report.quick_access.shortcuts)
        .saturating_add(report.quick_access.context_items)
        .saturating_add(report.quick_access.orphan_context_items)
        .saturating_add(report.quick_access.notifications)
}
