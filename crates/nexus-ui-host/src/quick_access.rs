//! Quick Access shortcut, context-menu, and notification registry.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::callback::CallbackSlot;
use crate::error::validate_text;
use crate::{
    CallbackInvocation, OwnerGeneration, OwnerHandle, OwnerRetirement, UiCallback, UiRegistryError,
};

/// Identifier of the built-in Nexus menu shortcut.
pub const QA_MENU_ID: &str = "!Nexus";
/// Notification key used by the compatibility API.
pub const QA_GENERIC_KEY: &str = "Generic";

/// Quick Access layout anchor, matching the legacy enum values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum QuickAccessPosition {
    /// Extend from the game menu bar.
    Extend = 0,
    /// Place under the game menu bar.
    Under = 1,
    /// Place at the bottom of the screen.
    Bottom = 2,
    /// Use the configured custom offset.
    Custom = 3,
}

/// Global Quick Access visibility mode, matching the legacy predicates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum QuickAccessVisibility {
    /// Visible regardless of game state.
    AlwaysShow = 0,
    /// Visible whenever gameplay is active.
    Gameplay = 1,
    /// Visible during gameplay while out of combat.
    OutOfCombat = 2,
    /// Visible during gameplay while in combat.
    InCombat = 3,
    /// Always hidden.
    Hide = 4,
}

impl QuickAccessVisibility {
    /// Evaluates the exact legacy gameplay/combat visibility predicate.
    #[must_use]
    pub const fn is_visible(self, is_gameplay: bool, is_in_combat: bool) -> bool {
        match self {
            Self::AlwaysShow => true,
            Self::Gameplay => is_gameplay,
            Self::OutOfCombat => is_gameplay && !is_in_combat,
            Self::InCombat => is_gameplay && is_in_combat,
            Self::Hide => false,
        }
    }
}

/// Renderer-independent Quick Access layout settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuickAccessSettings {
    /// Whether icons are stacked vertically.
    pub vertical_layout: bool,
    /// Global visibility policy.
    pub visibility: QuickAccessVisibility,
    /// Layout anchor.
    pub position: QuickAccessPosition,
    /// Custom horizontal offset.
    pub offset_x: f32,
    /// Custom vertical offset.
    pub offset_y: f32,
    /// Whether opacity should increase only while hovered.
    pub only_show_on_hover: bool,
    /// Idle opacity used by a render consumer.
    pub opacity: f32,
}

impl Default for QuickAccessSettings {
    fn default() -> Self {
        Self {
            vertical_layout: false,
            visibility: QuickAccessVisibility::AlwaysShow,
            position: QuickAccessPosition::Extend,
            offset_x: 0.0,
            offset_y: 0.0,
            only_show_on_hover: false,
            opacity: 0.5,
        }
    }
}

/// Bounds and panic policy for [`QuickAccessRegistry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuickAccessConfig {
    /// Maximum shortcut records.
    pub maximum_shortcuts: usize,
    /// Maximum attached plus orphan context-menu records.
    pub maximum_context_items: usize,
    /// Maximum unique notification keys on one shortcut.
    pub maximum_notifications_per_shortcut: usize,
    /// Maximum persisted suppressed identifiers.
    pub maximum_suppressed_identifiers: usize,
    /// Maximum UTF-8 bytes in any identifier, texture key, bind, or tooltip.
    pub maximum_string_bytes: usize,
    /// Managed callback panics allowed before one item is disabled.
    pub maximum_panics_per_callback: u32,
}

impl Default for QuickAccessConfig {
    fn default() -> Self {
        Self {
            maximum_shortcuts: 2_048,
            maximum_context_items: 8_192,
            maximum_notifications_per_shortcut: 256,
            maximum_suppressed_identifiers: 2_048,
            maximum_string_bytes: 4_096,
            maximum_panics_per_callback: 3,
        }
    }
}

impl QuickAccessConfig {
    pub(crate) fn validate(self) -> Result<Self, UiRegistryError> {
        if self.maximum_shortcuts == 0
            || self.maximum_context_items == 0
            || self.maximum_notifications_per_shortcut == 0
            || self.maximum_suppressed_identifiers == 0
            || self.maximum_string_bytes == 0
            || self.maximum_panics_per_callback == 0
        {
            return Err(UiRegistryError::InvalidConfiguration(
                "Quick Access limits must all be non-zero",
            ));
        }
        Ok(self)
    }
}

/// Duplicate-aware result of adding a shortcut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutRegistrationOutcome {
    /// A new shortcut was inserted.
    Registered,
    /// The identifier already existed and the original record was retained.
    Duplicate,
}

/// Shortcut mutation report, including legacy orphan reparenting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortcutMutation {
    /// Registration outcome.
    pub outcome: ShortcutRegistrationOutcome,
    /// Orphan context items attached during the unconditional parent pass.
    pub adopted_orphans: usize,
    /// Monotonic invalidation revision, incremented even for duplicates.
    pub revision: u64,
}

/// Placement result of adding a context-menu callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextRegistrationOutcome {
    /// Attached to an existing target shortcut.
    Attached,
    /// Stored in the orphanage until the target shortcut appears.
    Orphaned,
    /// That item identifier already existed at the selected location.
    Duplicate,
}

/// Result of changing a shortcut notification key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationOutcome {
    /// A unique key was appended.
    Added,
    /// The key already existed; the first owner remains authoritative.
    Duplicate,
    /// A matching key was removed.
    Removed,
    /// The shortcut does not exist.
    ShortcutMissing,
    /// The shortcut exists but the key does not.
    NotificationMissing,
}

/// Legacy notification badge display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationBadge {
    /// A numeric badge from one through nine.
    Count(u8),
    /// Ten or more notifications, rendered as `X` by the legacy UI.
    Overflow,
}

#[derive(Debug)]
struct ContextItem {
    id: Arc<str>,
    target: Arc<str>,
    callback: Arc<CallbackSlot>,
}

#[derive(Debug)]
struct Notification {
    owner: OwnerGeneration,
    key: Arc<str>,
}

#[derive(Debug)]
struct Shortcut {
    owner: OwnerHandle,
    id: Arc<str>,
    texture: Arc<str>,
    hover_texture: Arc<str>,
    input_bind: Arc<str>,
    tooltip: Arc<str>,
    suppressed: bool,
    context_items: BTreeMap<Arc<str>, ContextItem>,
    notifications: Vec<Notification>,
}

/// Stable snapshot of one context-menu callback.
#[derive(Debug, Clone)]
pub struct ContextMenuItemSnapshot {
    /// Globally unique item identifier within its current location.
    pub id: Arc<str>,
    /// Target shortcut identifier.
    pub target: Arc<str>,
    /// Addon generation that owns the callback.
    pub owner: OwnerGeneration,
    callback: Arc<CallbackSlot>,
}

impl ContextMenuItemSnapshot {
    /// Invokes this callback outside all Quick Access locks.
    #[must_use]
    pub fn invoke(&self) -> CallbackInvocation {
        self.callback.invoke()
    }
}

/// Stable snapshot of one shortcut in lexicographic identifier order.
#[derive(Debug, Clone)]
pub struct ShortcutSnapshot {
    /// Shortcut owner generation.
    pub owner: OwnerGeneration,
    /// Shortcut identifier.
    pub id: Arc<str>,
    /// Normal texture identifier.
    pub texture: Arc<str>,
    /// Hover texture identifier.
    pub hover_texture: Arc<str>,
    /// Input-bind identifier; empty means no bind.
    pub input_bind: Arc<str>,
    /// Tooltip text.
    pub tooltip: Arc<str>,
    /// Whether user suppression hides this shortcut.
    pub suppressed: bool,
    /// Context items in lexicographic item-ID order.
    pub context_items: Arc<[ContextMenuItemSnapshot]>,
    /// Unique notification keys in insertion order.
    pub notifications: Arc<[Arc<str>]>,
}

impl ShortcutSnapshot {
    /// Matches the legacy active predicate: context items or a bind handler.
    #[must_use]
    pub fn is_active(&self, has_input_bind_handler: bool) -> bool {
        !self.context_items.is_empty() || has_input_bind_handler
    }

    /// Returns whether the shortcut itself is active and not suppressed.
    #[must_use]
    pub fn is_renderable(&self, has_input_bind_handler: bool) -> bool {
        !self.suppressed && self.is_active(has_input_bind_handler)
    }

    /// Returns the legacy badge for the captured notification count.
    #[must_use]
    pub fn notification_badge(&self) -> Option<NotificationBadge> {
        match self.notifications.len() {
            0 => None,
            count @ 1..=9 => Some(NotificationBadge::Count(count as u8)),
            _ => Some(NotificationBadge::Overflow),
        }
    }
}

/// Immutable Quick Access state for a render-thread consumer.
#[derive(Debug, Clone)]
pub struct QuickAccessSnapshot {
    /// Invalidation revision captured with this state.
    pub revision: u64,
    /// Layout and opacity settings.
    pub settings: QuickAccessSettings,
    /// Result of the global gameplay/combat visibility predicate.
    pub globally_visible: bool,
    /// Shortcuts in lexicographic identifier order.
    pub shortcuts: Arc<[ShortcutSnapshot]>,
}

impl QuickAccessSnapshot {
    /// Filters renderable shortcuts while calling the bind resolver outside
    /// every registry lock.
    #[must_use]
    pub fn renderable_shortcuts<F>(&self, mut has_handler: F) -> Vec<&ShortcutSnapshot>
    where
        F: FnMut(&str) -> bool,
    {
        if !self.globally_visible {
            return Vec::new();
        }
        self.shortcuts
            .iter()
            .filter(|shortcut| shortcut.is_renderable(has_handler(shortcut.input_bind.as_ref())))
            .collect()
    }
}

/// Counts state removed for one owner-generation cleanup.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct QuickAccessCleanup {
    /// Shortcut records removed.
    pub shortcuts: usize,
    /// Attached context callbacks removed.
    pub context_items: usize,
    /// Orphan context callbacks removed.
    pub orphan_context_items: usize,
    /// Notification keys removed or discarded with an owned shortcut.
    pub notifications: usize,
}

#[derive(Debug, Default)]
struct QuickAccessState {
    shortcuts: BTreeMap<Arc<str>, Shortcut>,
    orphans: BTreeMap<Arc<str>, ContextItem>,
    suppressed: BTreeSet<Arc<str>>,
    settings: QuickAccessSettings,
    revision: u64,
}

/// Deterministic Quick Access state service with legacy duplicate rules.
#[derive(Debug)]
pub struct QuickAccessRegistry {
    state: Mutex<QuickAccessState>,
    config: QuickAccessConfig,
}

impl Default for QuickAccessRegistry {
    fn default() -> Self {
        Self {
            state: Mutex::new(QuickAccessState::default()),
            config: QuickAccessConfig::default(),
        }
    }
}

impl QuickAccessRegistry {
    /// Creates a registry with validated bounds.
    pub fn new(config: QuickAccessConfig) -> Result<Self, UiRegistryError> {
        Ok(Self {
            state: Mutex::new(QuickAccessState::default()),
            config: config.validate()?,
        })
    }

    /// Adds a shortcut. Duplicate IDs preserve the first record, but still
    /// run orphan reparenting and increment invalidation revision.
    #[allow(clippy::too_many_arguments)]
    pub fn add_shortcut(
        &self,
        owner: &OwnerHandle,
        id: &str,
        texture: &str,
        hover_texture: &str,
        input_bind: &str,
        tooltip: &str,
    ) -> Result<ShortcutMutation, UiRegistryError> {
        self.validate_string("shortcut identifier", id)?;
        self.validate_string("shortcut texture", texture)?;
        self.validate_string("shortcut hover texture", hover_texture)?;
        self.validate_string("shortcut input bind", input_bind)?;
        self.validate_string("shortcut tooltip", tooltip)?;
        let Some(_activity) = owner.try_enter() else {
            return Err(UiRegistryError::OwnerRetired(owner.identity()));
        };
        let mut state = self.lock();
        let outcome = if state.shortcuts.contains_key(id) {
            ShortcutRegistrationOutcome::Duplicate
        } else {
            if state.shortcuts.len() >= self.config.maximum_shortcuts {
                return Err(UiRegistryError::CapacityExceeded {
                    registry: "Quick Access shortcuts",
                    maximum: self.config.maximum_shortcuts,
                });
            }
            let suppressed = state.suppressed.contains(id);
            let id: Arc<str> = Arc::from(id);
            state.shortcuts.insert(
                Arc::clone(&id),
                Shortcut {
                    owner: owner.clone(),
                    id,
                    texture: Arc::from(texture),
                    hover_texture: Arc::from(hover_texture),
                    input_bind: Arc::from(input_bind),
                    tooltip: Arc::from(tooltip),
                    suppressed,
                    context_items: BTreeMap::new(),
                    notifications: Vec::new(),
                },
            );
            ShortcutRegistrationOutcome::Registered
        };
        let adopted_orphans = reparent_orphans(&mut state);
        state.revision = state.revision.saturating_add(1);
        Ok(ShortcutMutation {
            outcome,
            adopted_orphans,
            revision: state.revision,
        })
    }

    /// Removes a shortcut and moves its surviving context items to orphans.
    pub fn remove_shortcut(&self, id: &str) -> Result<bool, UiRegistryError> {
        self.validate_string("shortcut identifier", id)?;
        let mut state = self.lock();
        let removed = state.shortcuts.remove(id);
        let did_remove = removed.is_some();
        if let Some(shortcut) = removed {
            for (item_id, item) in shortcut.context_items {
                if state.orphans.contains_key(item_id.as_ref()) {
                    item.callback.deactivate();
                } else {
                    state.orphans.insert(item_id, item);
                }
            }
        }
        state.revision = state.revision.saturating_add(1);
        Ok(did_remove)
    }

    /// Adds a context item to the built-in Nexus menu shortcut.
    pub fn add_simple_shortcut(
        &self,
        id: &str,
        callback: UiCallback,
    ) -> Result<ContextRegistrationOutcome, UiRegistryError> {
        self.add_context_item(id, QA_MENU_ID, callback)
    }

    /// Adds a context callback or places it in the orphanage.
    pub fn add_context_item(
        &self,
        id: &str,
        target: &str,
        callback: UiCallback,
    ) -> Result<ContextRegistrationOutcome, UiRegistryError> {
        self.validate_string("context item identifier", id)?;
        self.validate_string("context target identifier", target)?;
        let Some(_activity) = callback.try_enter_owner() else {
            return Err(UiRegistryError::OwnerRetired(callback.owner()));
        };
        let mut state = self.lock();
        let duplicate = state.shortcuts.get(target).map_or_else(
            || state.orphans.contains_key(id),
            |shortcut| shortcut.context_items.contains_key(id),
        );
        if duplicate {
            state.revision = state.revision.saturating_add(1);
            return Ok(ContextRegistrationOutcome::Duplicate);
        }
        if context_count(&state) >= self.config.maximum_context_items {
            return Err(UiRegistryError::CapacityExceeded {
                registry: "Quick Access context items",
                maximum: self.config.maximum_context_items,
            });
        }
        let id: Arc<str> = Arc::from(id);
        let item = ContextItem {
            id: Arc::clone(&id),
            target: Arc::from(target),
            callback: CallbackSlot::new(callback, self.config.maximum_panics_per_callback),
        };
        let outcome = if let Some(shortcut) = state.shortcuts.get_mut(target) {
            shortcut.context_items.insert(id, item);
            ContextRegistrationOutcome::Attached
        } else {
            state.orphans.insert(id, item);
            ContextRegistrationOutcome::Orphaned
        };
        state.revision = state.revision.saturating_add(1);
        Ok(outcome)
    }

    /// Removes an item ID from every shortcut and from the orphanage.
    pub fn remove_context_item(&self, id: &str) -> Result<usize, UiRegistryError> {
        self.validate_string("context item identifier", id)?;
        let mut state = self.lock();
        let mut removed = 0;
        for shortcut in state.shortcuts.values_mut() {
            if let Some(item) = shortcut.context_items.remove(id) {
                item.callback.deactivate();
                removed += 1;
            }
        }
        if let Some(item) = state.orphans.remove(id) {
            item.callback.deactivate();
            removed += 1;
        }
        state.revision = state.revision.saturating_add(1);
        Ok(removed)
    }

    /// Appends a globally unique notification key to an existing shortcut.
    pub fn push_notification(
        &self,
        owner: &OwnerHandle,
        shortcut_id: &str,
        key: &str,
    ) -> Result<NotificationOutcome, UiRegistryError> {
        self.validate_string("shortcut identifier", shortcut_id)?;
        self.validate_string("notification key", key)?;
        let Some(_activity) = owner.try_enter() else {
            return Err(UiRegistryError::OwnerRetired(owner.identity()));
        };
        let mut state = self.lock();
        let Some(shortcut) = state.shortcuts.get_mut(shortcut_id) else {
            return Ok(NotificationOutcome::ShortcutMissing);
        };
        if shortcut
            .notifications
            .iter()
            .any(|notification| notification.key.as_ref() == key)
        {
            return Ok(NotificationOutcome::Duplicate);
        }
        if shortcut.notifications.len() >= self.config.maximum_notifications_per_shortcut {
            return Err(UiRegistryError::CapacityExceeded {
                registry: "Quick Access shortcut notifications",
                maximum: self.config.maximum_notifications_per_shortcut,
            });
        }
        shortcut.notifications.push(Notification {
            owner: owner.identity(),
            key: Arc::from(key),
        });
        state.revision = state.revision.saturating_add(1);
        Ok(NotificationOutcome::Added)
    }

    /// Removes the first matching notification key, independent of owner.
    pub fn pop_notification(
        &self,
        shortcut_id: &str,
        key: &str,
    ) -> Result<NotificationOutcome, UiRegistryError> {
        self.validate_string("shortcut identifier", shortcut_id)?;
        self.validate_string("notification key", key)?;
        let mut state = self.lock();
        let Some(shortcut) = state.shortcuts.get_mut(shortcut_id) else {
            return Ok(NotificationOutcome::ShortcutMissing);
        };
        let Some(index) = shortcut
            .notifications
            .iter()
            .position(|notification| notification.key.as_ref() == key)
        else {
            return Ok(NotificationOutcome::NotificationMissing);
        };
        shortcut.notifications.remove(index);
        state.revision = state.revision.saturating_add(1);
        Ok(NotificationOutcome::Removed)
    }

    /// Compatibility helper for the legacy `Generic` notification key.
    pub fn set_generic_notification(
        &self,
        owner: &OwnerHandle,
        shortcut_id: &str,
        state: bool,
    ) -> Result<NotificationOutcome, UiRegistryError> {
        if state {
            self.push_notification(owner, shortcut_id, QA_GENERIC_KEY)
        } else {
            self.pop_notification(shortcut_id, QA_GENERIC_KEY)
        }
    }

    /// Updates one persisted suppression ID and the live shortcut, if any.
    pub fn set_suppressed(&self, id: &str, suppressed: bool) -> Result<(), UiRegistryError> {
        self.validate_string("suppressed shortcut identifier", id)?;
        let mut state = self.lock();
        if suppressed {
            if !state.suppressed.contains(id)
                && state.suppressed.len() >= self.config.maximum_suppressed_identifiers
            {
                return Err(UiRegistryError::CapacityExceeded {
                    registry: "Quick Access suppressed identifiers",
                    maximum: self.config.maximum_suppressed_identifiers,
                });
            }
            state.suppressed.insert(Arc::from(id));
        } else {
            state.suppressed.remove(id);
        }
        if let Some(shortcut) = state.shortcuts.get_mut(id) {
            shortcut.suppressed = suppressed;
        }
        state.revision = state.revision.saturating_add(1);
        Ok(())
    }

    /// Replaces renderer-independent layout settings.
    pub fn set_settings(&self, settings: QuickAccessSettings) {
        let mut state = self.lock();
        state.settings = settings;
        state.revision = state.revision.saturating_add(1);
    }

    /// Captures lexicographically ordered state for one render frame.
    #[must_use]
    pub fn snapshot(&self, is_gameplay: bool, is_in_combat: bool) -> QuickAccessSnapshot {
        let state = self.lock();
        let shortcuts = state
            .shortcuts
            .values()
            .map(shortcut_snapshot)
            .collect::<Vec<_>>();
        QuickAccessSnapshot {
            revision: state.revision,
            settings: state.settings,
            globally_visible: state
                .settings
                .visibility
                .is_visible(is_gameplay, is_in_combat),
            shortcuts: Arc::from(shortcuts),
        }
    }

    pub(crate) fn cleanup_owner_generation(
        &self,
        owner: OwnerGeneration,
    ) -> (QuickAccessCleanup, OwnerRetirement) {
        let mut state = self.lock();
        let mut report = QuickAccessCleanup::default();
        let mut removed_callbacks = Vec::new();

        for shortcut in state.shortcuts.values_mut() {
            let before = shortcut.context_items.len();
            shortcut.context_items.retain(|_, item| {
                if item.callback.owner() == owner {
                    item.callback.deactivate();
                    removed_callbacks.push(item.callback.owner_handle());
                    false
                } else {
                    true
                }
            });
            report.context_items += before - shortcut.context_items.len();

            let before = shortcut.notifications.len();
            shortcut
                .notifications
                .retain(|notification| notification.owner != owner);
            report.notifications += before - shortcut.notifications.len();
        }

        let orphan_before = state.orphans.len();
        state.orphans.retain(|_, item| {
            if item.callback.owner() == owner {
                item.callback.deactivate();
                removed_callbacks.push(item.callback.owner_handle());
                false
            } else {
                true
            }
        });
        report.orphan_context_items = orphan_before - state.orphans.len();

        let owned_ids = state
            .shortcuts
            .iter()
            .filter(|(_, shortcut)| shortcut.owner.identity() == owner)
            .map(|(id, _)| Arc::clone(id))
            .collect::<Vec<_>>();
        for id in owned_ids {
            if let Some(shortcut) = state.shortcuts.remove(id.as_ref()) {
                report.shortcuts += 1;
                report.notifications += shortcut.notifications.len();
                for (item_id, item) in shortcut.context_items {
                    if state.orphans.contains_key(item_id.as_ref()) {
                        item.callback.deactivate();
                    } else {
                        state.orphans.insert(item_id, item);
                    }
                }
            }
        }

        if report != QuickAccessCleanup::default() {
            state.revision = state.revision.saturating_add(1);
        }
        drop(state);
        let retirement = removed_callbacks.iter().fold(
            OwnerRetirement::already_quiescent(),
            |retirement, handle| retirement.merge(handle.retire_and_drain()),
        );
        (report, retirement)
    }

    fn validate_string(&self, field: &'static str, value: &str) -> Result<(), UiRegistryError> {
        validate_text(field, value, self.config.maximum_string_bytes)
    }

    fn lock(&self) -> MutexGuard<'_, QuickAccessState> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

fn context_count(state: &QuickAccessState) -> usize {
    state.orphans.len()
        + state
            .shortcuts
            .values()
            .map(|shortcut| shortcut.context_items.len())
            .sum::<usize>()
}

fn reparent_orphans(state: &mut QuickAccessState) -> usize {
    let candidates = state
        .orphans
        .iter()
        .filter(|(_, item)| state.shortcuts.contains_key(item.target.as_ref()))
        .map(|(id, _)| Arc::clone(id))
        .collect::<Vec<_>>();
    let mut adopted = 0;
    for id in candidates {
        let Some(item) = state.orphans.remove(id.as_ref()) else {
            continue;
        };
        let Some(parent) = state.shortcuts.get_mut(item.target.as_ref()) else {
            continue;
        };
        if parent.context_items.contains_key(item.id.as_ref()) {
            item.callback.deactivate();
        } else {
            parent.context_items.insert(Arc::clone(&item.id), item);
            adopted += 1;
        }
    }
    adopted
}

fn shortcut_snapshot(shortcut: &Shortcut) -> ShortcutSnapshot {
    let context_items = shortcut
        .context_items
        .values()
        .map(|item| ContextMenuItemSnapshot {
            id: Arc::clone(&item.id),
            target: Arc::clone(&item.target),
            owner: item.callback.owner(),
            callback: Arc::clone(&item.callback),
        })
        .collect::<Vec<_>>();
    let notifications = shortcut
        .notifications
        .iter()
        .map(|notification| Arc::clone(&notification.key))
        .collect::<Vec<_>>();
    ShortcutSnapshot {
        owner: shortcut.owner.identity(),
        id: Arc::clone(&shortcut.id),
        texture: Arc::clone(&shortcut.texture),
        hover_texture: Arc::clone(&shortcut.hover_texture),
        input_bind: Arc::clone(&shortcut.input_bind),
        tooltip: Arc::clone(&shortcut.tooltip),
        suppressed: shortcut.suppressed,
        context_items: Arc::from(context_items),
        notifications: Arc::from(notifications),
    }
}
