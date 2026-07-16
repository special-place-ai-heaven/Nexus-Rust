//! Bounded alert queue and render-thread snapshots.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::error::validate_text;
use crate::{OwnerGeneration, OwnerHandle, UiRegistryError};

/// Legacy alert category and color selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum AlertKind {
    /// Ignored by the queue.
    None = 0,
    /// Informational alert.
    Info = 1,
    /// Error alert.
    Error = 2,
}

/// Capacity and animation limits for [`AlertQueue`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlertQueueConfig {
    /// Maximum queued alerts.
    pub maximum_alerts: usize,
    /// Maximum UTF-8 bytes in one message.
    pub maximum_message_bytes: usize,
    /// Fully opaque duration after first presentation.
    pub hold_millis: u64,
    /// Cubic fade duration.
    pub fade_millis: u64,
}

impl Default for AlertQueueConfig {
    fn default() -> Self {
        Self {
            maximum_alerts: 256,
            maximum_message_bytes: 4_096,
            hold_millis: 5_000,
            fade_millis: 2_500,
        }
    }
}

impl AlertQueueConfig {
    pub(crate) fn validate(self) -> Result<Self, UiRegistryError> {
        if self.maximum_alerts == 0 {
            return Err(UiRegistryError::InvalidConfiguration(
                "alert queue capacity must be non-zero",
            ));
        }
        if self.maximum_message_bytes == 0 {
            return Err(UiRegistryError::InvalidConfiguration(
                "alert message limit must be non-zero",
            ));
        }
        if self.fade_millis == 0 {
            return Err(UiRegistryError::InvalidConfiguration(
                "alert fade duration must be non-zero",
            ));
        }
        Ok(self)
    }
}

/// Result of submitting an alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyOutcome {
    /// `AlertKind::None` is ignored exactly like the legacy widget.
    IgnoredNone,
    /// A new alert was appended.
    Queued,
    /// The message matched the current front; its fade was reset without
    /// changing the existing alert's kind or owner.
    ResetFront,
}

#[derive(Debug)]
struct AlertEntry {
    owner: OwnerHandle,
    kind: AlertKind,
    message: Arc<str>,
    shown_at_millis: Option<u64>,
    fade_started_at_millis: Option<u64>,
    reset_revision: u64,
}

/// Immutable alert data suitable for a render-thread consumer.
#[derive(Debug, Clone, PartialEq)]
pub struct AlertSnapshot {
    /// Owner of the queued alert.
    pub owner: OwnerGeneration,
    /// Alert category.
    pub kind: AlertKind,
    /// Owned message text.
    pub message: Arc<str>,
    /// First time this alert was presented.
    pub shown_at_millis: u64,
    /// Current deterministic opacity.
    pub opacity: f32,
    /// Increments whenever a duplicate front message resets its fade.
    pub reset_revision: u64,
}

/// Result of advancing the alert queue for a render frame.
#[derive(Debug, Clone, PartialEq)]
pub struct AlertFrame {
    /// Front alert to draw, if one remains.
    pub alert: Option<AlertSnapshot>,
    /// Whether the previous front completed its fade on this call.
    pub expired: bool,
}

#[derive(Debug, Default)]
struct AlertState {
    queue: VecDeque<AlertEntry>,
}

/// Thread-safe bounded FIFO with legacy front-message deduplication.
#[derive(Debug)]
pub struct AlertQueue {
    state: Mutex<AlertState>,
    config: AlertQueueConfig,
}

impl Default for AlertQueue {
    fn default() -> Self {
        Self {
            state: Mutex::new(AlertState::default()),
            config: AlertQueueConfig::default(),
        }
    }
}

impl AlertQueue {
    /// Creates an alert queue with validated bounds.
    pub fn new(config: AlertQueueConfig) -> Result<Self, UiRegistryError> {
        Ok(Self {
            state: Mutex::new(AlertState::default()),
            config: config.validate()?,
        })
    }

    /// Queues an alert or resets the fade when its message matches the front.
    pub fn notify(
        &self,
        owner: &OwnerHandle,
        kind: AlertKind,
        message: &str,
    ) -> Result<NotifyOutcome, UiRegistryError> {
        if kind == AlertKind::None {
            return Ok(NotifyOutcome::IgnoredNone);
        }
        validate_text("alert message", message, self.config.maximum_message_bytes)?;
        let Some(_activity) = owner.try_enter() else {
            return Err(UiRegistryError::OwnerRetired(owner.identity()));
        };
        let mut state = self.lock();
        if let Some(front) = state.queue.front_mut()
            && front.message.as_ref() == message
        {
            front.fade_started_at_millis = None;
            front.reset_revision = front.reset_revision.saturating_add(1);
            return Ok(NotifyOutcome::ResetFront);
        }
        if state.queue.len() >= self.config.maximum_alerts {
            return Err(UiRegistryError::CapacityExceeded {
                registry: "alert queue",
                maximum: self.config.maximum_alerts,
            });
        }
        state.queue.push_back(AlertEntry {
            owner: owner.clone(),
            kind,
            message: Arc::from(message),
            shown_at_millis: None,
            fade_started_at_millis: None,
            reset_revision: 0,
        });
        Ok(NotifyOutcome::Queued)
    }

    /// Advances front-alert timing and returns an owned stable snapshot.
    #[must_use]
    pub fn advance(&self, now_millis: u64) -> AlertFrame {
        let mut state = self.lock();
        let Some(front) = state.queue.front_mut() else {
            return AlertFrame {
                alert: None,
                expired: false,
            };
        };

        let shown_at = *front.shown_at_millis.get_or_insert(now_millis);
        let held_for = now_millis.saturating_sub(shown_at);
        let opacity = if held_for <= self.config.hold_millis {
            1.0
        } else {
            let fade_started = *front.fade_started_at_millis.get_or_insert(now_millis);
            let faded_for = now_millis.saturating_sub(fade_started);
            if faded_for >= self.config.fade_millis {
                state.queue.pop_front();
                return AlertFrame {
                    alert: None,
                    expired: true,
                };
            }
            let progress = faded_for as f64 / self.config.fade_millis as f64;
            (1.0 - progress.powi(3)) as f32
        };

        AlertFrame {
            alert: Some(AlertSnapshot {
                owner: front.owner.identity(),
                kind: front.kind,
                message: Arc::clone(&front.message),
                shown_at_millis: shown_at,
                opacity,
                reset_revision: front.reset_revision,
            }),
            expired: false,
        }
    }

    /// Returns the current bounded queue length.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().queue.len()
    }

    /// Returns whether no alerts are queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn cleanup_owner_generation(&self, owner: OwnerGeneration) -> usize {
        let mut state = self.lock();
        let before = state.queue.len();
        state.queue.retain(|alert| alert.owner.identity() != owner);
        before - state.queue.len()
    }

    fn lock(&self) -> MutexGuard<'_, AlertState> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}
