use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use nexus_control::{FailureCode, InternalFailure, RenderOperation};
use nexus_dxgi::RenderCallbackError;
use nexus_input::{
    GameBindRegistry, GameInputError, GameInvoker, GameMessageSink, PersistenceError,
    PhysicalInputState,
};
use nexus_input_win32::{Win32GameInput, WindowAttachError};
use nexus_overlay::{RenderSessionAttachment, RenderSessionObserver, RenderSessionResources};
use nexus_render::SwapChainId;
use windows_sys::Win32::Foundation::HWND;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RenderSessionIdentity {
    swap_chain_id: SwapChainId,
    generation: u64,
}

impl RenderSessionIdentity {
    const fn new(swap_chain_id: SwapChainId, generation: u64) -> Self {
        Self {
            swap_chain_id,
            generation,
        }
    }
}

trait AttachedGameWindow: Send + 'static {}

impl<T> AttachedGameWindow for T where T: Send + 'static {}

struct ActiveGameWindow {
    identity: RenderSessionIdentity,
    attachment_id: u64,
    _window: Box<dyn AttachedGameWindow>,
}

#[derive(Default)]
struct GameWindowState {
    next_attachment_id: u64,
    stopped: bool,
    active: Option<ActiveGameWindow>,
}

#[derive(Default)]
struct GameWindowSessions {
    state: Mutex<GameWindowState>,
}

impl GameWindowSessions {
    fn attach(
        self: &Arc<Self>,
        identity: RenderSessionIdentity,
        window: Box<dyn AttachedGameWindow>,
    ) -> Result<GameInputSessionLease, SessionAttachError> {
        let mut pending = Some(window);
        let outcome = {
            let mut state = mutex_lock(&self.state);
            if state.stopped {
                Err(SessionAttachError::Stopped)
            } else {
                match state.next_attachment_id.checked_add(1) {
                    Some(attachment_id) => {
                        let window = pending
                            .take()
                            .expect("a pending game-window attachment must exist");
                        state.next_attachment_id = attachment_id;
                        let stale = state.active.replace(ActiveGameWindow {
                            identity,
                            attachment_id,
                            _window: window,
                        });
                        Ok((stale, attachment_id))
                    }
                    None => Err(SessionAttachError::LifecycleExhausted),
                }
            }
        };

        let (stale, attachment_id) = match outcome {
            Ok(attached) => attached,
            Err(error) => {
                drop(pending);
                return Err(error);
            }
        };
        drop(stale);

        Ok(GameInputSessionLease {
            sessions: Arc::clone(self),
            identity,
            attachment_id,
        })
    }

    fn stop(&self) -> Option<ActiveGameWindow> {
        let mut state = mutex_lock(&self.state);
        state.stopped = true;
        state.active.take()
    }

    fn detach(&self, identity: RenderSessionIdentity, attachment_id: u64) {
        let active = {
            let mut state = mutex_lock(&self.state);
            if state.active.as_ref().is_some_and(|active| {
                active.identity == identity && active.attachment_id == attachment_id
            }) {
                state.active.take()
            } else {
                None
            }
        };
        drop(active);
    }

    #[cfg(test)]
    fn active_identity(&self) -> Option<RenderSessionIdentity> {
        mutex_lock(&self.state)
            .active
            .as_ref()
            .map(|active| active.identity)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionAttachError {
    Stopped,
    LifecycleExhausted,
}

struct GameInputSessionLease {
    sessions: Arc<GameWindowSessions>,
    identity: RenderSessionIdentity,
    attachment_id: u64,
}

impl Drop for GameInputSessionLease {
    fn drop(&mut self) {
        self.sessions.detach(self.identity, self.attachment_id);
    }
}

pub(crate) struct RuntimeGameInput {
    path: PathBuf,
    adapter: Arc<Win32GameInput>,
    invoker: Arc<Mutex<Option<GameInvoker>>>,
    sessions: Arc<GameWindowSessions>,
}

impl RuntimeGameInput {
    /// Game-bind invoker lifecycle slot handed to the add-on API.
    #[allow(
        dead_code,
        reason = "called by the render-session install, landing next"
    )]
    pub(crate) fn invoker(&self) -> Arc<Mutex<Option<GameInvoker>>> {
        Arc::clone(&self.invoker)
    }

    /// Sink delivering one message to the game only, handed to the add-on API.
    #[allow(
        dead_code,
        reason = "called by the render-session install, landing next"
    )]
    pub(crate) fn game_message_sink(&self) -> Arc<Win32GameInput> {
        Arc::clone(&self.adapter)
    }

    pub(crate) fn load(path: PathBuf) -> (Arc<Self>, Option<PersistenceError>) {
        let adapter = Arc::new(Win32GameInput::new());
        let mut registry = GameBindRegistry::with_defaults();
        let load_error = path
            .is_file()
            .then(|| registry.load_xml_file(&path))
            .transpose()
            .err();
        let sink_adapter = Arc::clone(&adapter);
        let sink: Arc<dyn GameMessageSink> = sink_adapter;
        let physical_adapter = Arc::clone(&adapter);
        let physical: Arc<dyn PhysicalInputState> = physical_adapter;
        let invoker = GameInvoker::new(registry, sink, physical);
        (
            Arc::new(Self {
                path,
                adapter,
                invoker: Arc::new(Mutex::new(Some(invoker))),
                sessions: Arc::new(GameWindowSessions::default()),
            }),
            load_error,
        )
    }

    fn attach_render_session(
        self: &Arc<Self>,
        identity: RenderSessionIdentity,
        hwnd: usize,
    ) -> Result<GameInputSessionLease, RuntimeGameInputAttachError> {
        let window = self
            .adapter
            .attach(hwnd as HWND)
            .map_err(RuntimeGameInputAttachError::Window)?;
        self.sessions
            .attach(identity, Box::new(window))
            .map_err(RuntimeGameInputAttachError::Session)
    }

    pub(crate) fn shutdown(&self) -> RuntimeGameInputShutdownReport {
        let active = self.sessions.stop();
        let mut invoker = {
            let mut slot = mutex_lock(self.invoker.as_ref());
            slot.take()
        };
        let release_error = invoker
            .as_mut()
            .and_then(|invoker| invoker.release_all().err());

        // Keep the exact native lease alive for release delivery, then detach
        // before persistence and before any other runtime service shuts down.
        drop(active);
        let persistence_error = invoker
            .as_ref()
            .and_then(|invoker| invoker.registry().save_xml_file(&self.path).err());
        drop(invoker);

        RuntimeGameInputShutdownReport {
            release_error,
            persistence_error,
        }
    }
}

impl Drop for RuntimeGameInput {
    fn drop(&mut self) {
        let _report = self.shutdown();
    }
}

pub(crate) struct RuntimeGameInputShutdownReport {
    pub(crate) release_error: Option<GameInputError>,
    pub(crate) persistence_error: Option<PersistenceError>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeGameInputAttachError {
    Window(WindowAttachError),
    Session(SessionAttachError),
}

struct RuntimeGameInputObserver {
    game_input: Arc<RuntimeGameInput>,
    downstream: Arc<dyn RenderSessionObserver>,
}

impl RenderSessionObserver for RuntimeGameInputObserver {
    fn attach(
        &self,
        resources: RenderSessionResources<'_>,
    ) -> Result<Box<dyn RenderSessionAttachment>, RenderCallbackError> {
        let game = self
            .game_input
            .attach_render_session(
                RenderSessionIdentity::new(resources.swap_chain_id(), resources.generation()),
                resources.hwnd(),
            )
            .map_err(map_attach_error)?;
        let downstream = self.downstream.attach(resources)?;
        Ok(Box::new(CombinedRenderSessionAttachment {
            game: Some(Box::new(game)),
            downstream: Some(downstream),
        }))
    }
}

struct CombinedRenderSessionAttachment {
    game: Option<Box<dyn RenderSessionAttachment>>,
    downstream: Option<Box<dyn RenderSessionAttachment>>,
}

impl Drop for CombinedRenderSessionAttachment {
    fn drop(&mut self) {
        drop(self.game.take());
        drop(self.downstream.take());
    }
}

pub(crate) fn production_observer(
    game_input: Arc<RuntimeGameInput>,
    downstream: Arc<dyn RenderSessionObserver>,
) -> Arc<dyn RenderSessionObserver> {
    Arc::new(RuntimeGameInputObserver {
        game_input,
        downstream,
    })
}

const fn map_attach_error(error: RuntimeGameInputAttachError) -> RenderCallbackError {
    let failure = match error {
        RuntimeGameInputAttachError::Window(WindowAttachError::InvalidWindow) => {
            InternalFailure::MissingWindow
        }
        RuntimeGameInputAttachError::Window(WindowAttachError::GenerationExhausted)
        | RuntimeGameInputAttachError::Session(
            SessionAttachError::Stopped | SessionAttachError::LifecycleExhausted,
        ) => InternalFailure::InvalidState,
    };
    RenderCallbackError::new(
        RenderOperation::PrepareTarget,
        FailureCode::Internal(failure),
    )
}

fn mutex_lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::sync::Weak;
    use std::sync::atomic::{AtomicU64, Ordering};

    use nexus_input::{
        GameBindId, GameMessage, GameSinkError, GameSlot, InputBind, InputDevice, ModifierState,
    };

    use super::*;

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

    struct DropProbe {
        label: &'static str,
        locked_label: &'static str,
        events: Arc<Mutex<Vec<&'static str>>>,
        sessions: Weak<GameWindowSessions>,
    }

    impl Drop for DropProbe {
        fn drop(&mut self) {
            let lock_available = self
                .sessions
                .upgrade()
                .is_none_or(|sessions| sessions.state.try_lock().is_ok());
            mutex_lock(self.events.as_ref()).push(if lock_available {
                self.label
            } else {
                self.locked_label
            });
        }
    }

    struct AttachmentProbe {
        label: &'static str,
        events: Arc<Mutex<Vec<&'static str>>>,
        session: Option<GameInputSessionLease>,
    }

    impl Drop for AttachmentProbe {
        fn drop(&mut self) {
            drop(self.session.take());
            mutex_lock(self.events.as_ref()).push(self.label);
        }
    }

    struct RecordingSink {
        invoker: Weak<Mutex<Option<GameInvoker>>>,
        events: Arc<Mutex<Vec<&'static str>>>,
    }

    impl GameMessageSink for RecordingSink {
        fn send_batch(&self, _messages: &[GameMessage]) -> Result<(), GameSinkError> {
            let lock_available = self
                .invoker
                .upgrade()
                .is_none_or(|invoker| invoker.try_lock().is_ok());
            mutex_lock(self.events.as_ref()).push(if lock_available {
                "dispatch"
            } else {
                "dispatch-under-lock"
            });
            Ok(())
        }
    }

    struct FixedPhysical;

    impl PhysicalInputState for FixedPhysical {
        fn modifiers(&self) -> ModifierState {
            ModifierState::default()
        }
    }

    fn identity(swap_chain: u64, generation: u64) -> RenderSessionIdentity {
        RenderSessionIdentity::new(SwapChainId::new(swap_chain), generation)
    }

    fn probe(
        label: &'static str,
        locked_label: &'static str,
        events: &Arc<Mutex<Vec<&'static str>>>,
        sessions: &Arc<GameWindowSessions>,
    ) -> Box<dyn AttachedGameWindow> {
        Box::new(DropProbe {
            label,
            locked_label,
            events: Arc::clone(events),
            sessions: Arc::downgrade(sessions),
        })
    }

    fn temp_game_binds_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "nexus-runtime-game-binds-{}-{}.xml",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn production_load_and_shutdown_round_trip_the_legacy_game_binds_path() {
        let path = temp_game_binds_path();
        let _ = std::fs::remove_file(&path);
        let expected = InputBind::new(true, false, true, InputDevice::KEYBOARD, 0x2E);
        let mut registry = GameBindRegistry::with_defaults();
        assert!(registry.set(GameBindId::MOVE_FORWARD, GameSlot::Primary, expected));
        assert!(registry.save_xml_file(&path).is_ok());

        let (runtime, load_error) = RuntimeGameInput::load(path.clone());
        assert!(load_error.is_none());
        let loaded = mutex_lock(runtime.invoker.as_ref())
            .as_ref()
            .and_then(|invoker| invoker.registry().get(GameBindId::MOVE_FORWARD))
            .expect("the loaded registry should retain the known game action");
        assert_eq!(loaded.primary, expected);

        let report = runtime.shutdown();
        assert!(report.release_error.is_none());
        assert!(report.persistence_error.is_none());
        drop(runtime);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reselection_tracks_exact_generation_and_duplicate_identity_leases_stay_stale_safe() {
        let sessions = Arc::new(GameWindowSessions::default());
        let events = Arc::new(Mutex::new(Vec::new()));
        let first = sessions
            .attach(
                identity(1, 8),
                probe("first", "first-under-lock", &events, &sessions),
            )
            .expect("first test game window should attach");
        let second = sessions
            .attach(
                identity(1, 8),
                probe("second", "second-under-lock", &events, &sessions),
            )
            .expect("replacement test game window should attach");
        let third = sessions
            .attach(
                identity(1, 9),
                probe("third", "third-under-lock", &events, &sessions),
            )
            .expect("new generation test game window should attach");

        assert_eq!(mutex_lock(events.as_ref()).as_slice(), ["first", "second"]);
        assert_eq!(sessions.active_identity(), Some(identity(1, 9)));
        drop(first);
        drop(second);
        assert_eq!(sessions.active_identity(), Some(identity(1, 9)));
        assert_eq!(mutex_lock(events.as_ref()).as_slice(), ["first", "second"]);

        drop(third);
        assert_eq!(sessions.active_identity(), None);
        assert_eq!(
            mutex_lock(events.as_ref()).as_slice(),
            ["first", "second", "third"]
        );
    }

    #[test]
    fn shutdown_detaches_once_and_late_attach_rolls_back_outside_lock() {
        let sessions = Arc::new(GameWindowSessions::default());
        let events = Arc::new(Mutex::new(Vec::new()));
        let stale = sessions
            .attach(
                identity(4, 3),
                probe("active", "active-under-lock", &events, &sessions),
            )
            .expect("test game window should attach");

        let active = sessions.stop();
        drop(active);
        assert_eq!(mutex_lock(events.as_ref()).as_slice(), ["active"]);
        let late = sessions.attach(
            identity(4, 4),
            probe("late", "late-under-lock", &events, &sessions),
        );
        assert_eq!(late.err(), Some(SessionAttachError::Stopped));
        assert_eq!(mutex_lock(events.as_ref()).as_slice(), ["active", "late"]);

        drop(stale);
        assert_eq!(mutex_lock(events.as_ref()).as_slice(), ["active", "late"]);
    }

    #[test]
    fn stale_combined_attachment_drops_game_before_downstream_without_detaching_reselection() {
        let sessions = Arc::new(GameWindowSessions::default());
        let events = Arc::new(Mutex::new(Vec::new()));
        let first = sessions
            .attach(
                identity(6, 4),
                probe("first", "first-under-lock", &events, &sessions),
            )
            .expect("first test game window should attach");
        let second = sessions
            .attach(
                identity(6, 4),
                probe("second", "second-under-lock", &events, &sessions),
            )
            .expect("replacement test game window should attach");
        let lease = CombinedRenderSessionAttachment {
            game: Some(Box::new(AttachmentProbe {
                label: "game",
                events: Arc::clone(&events),
                session: Some(first),
            })),
            downstream: Some(Box::new(AttachmentProbe {
                label: "downstream",
                events: Arc::clone(&events),
                session: None,
            })),
        };

        drop(lease);
        assert_eq!(
            mutex_lock(events.as_ref()).as_slice(),
            ["first", "game", "downstream"]
        );
        assert_eq!(sessions.active_identity(), Some(identity(6, 4)));
        drop(second);
        assert_eq!(
            mutex_lock(events.as_ref()).as_slice(),
            ["first", "game", "downstream", "second"]
        );
    }

    #[test]
    fn runtime_shutdown_releases_then_detaches_and_saves_outside_locks() {
        let path = temp_game_binds_path();
        let _ = std::fs::remove_file(&path);
        let events = Arc::new(Mutex::new(Vec::new()));
        let invoker = Arc::new(Mutex::new(None));
        let sink: Arc<dyn GameMessageSink> = Arc::new(RecordingSink {
            invoker: Arc::downgrade(&invoker),
            events: Arc::clone(&events),
        });
        let physical: Arc<dyn PhysicalInputState> = Arc::new(FixedPhysical);
        let mut game_invoker = GameInvoker::new(GameBindRegistry::with_defaults(), sink, physical);
        assert!(game_invoker.press(GameBindId::MOVE_FORWARD).is_ok());
        mutex_lock(events.as_ref()).clear();
        *mutex_lock(invoker.as_ref()) = Some(game_invoker);

        let sessions = Arc::new(GameWindowSessions::default());
        let stale = sessions
            .attach(
                identity(9, 2),
                probe("window", "window-under-lock", &events, &sessions),
            )
            .expect("test game window should attach");
        let runtime = RuntimeGameInput {
            path: path.clone(),
            adapter: Arc::new(Win32GameInput::new()),
            invoker,
            sessions,
        };

        let report = runtime.shutdown();
        assert!(report.release_error.is_none());
        assert!(report.persistence_error.is_none());
        assert_eq!(
            mutex_lock(events.as_ref()).as_slice(),
            ["dispatch", "window"]
        );
        assert!(path.is_file());
        let mut reloaded = GameBindRegistry::with_defaults();
        assert!(reloaded.load_xml_file(&path).is_ok());

        drop(stale);
        drop(runtime);
        let _ = std::fs::remove_file(path);
    }
}
