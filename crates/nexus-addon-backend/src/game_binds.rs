use core::fmt;
use std::collections::{BTreeMap, VecDeque};
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use nexus_abi::GameBind;
use nexus_input::{GameBindId, GameInputError, GameInvoker, GamePressToken};
use nexus_platform::{CancellationToken, MinimalScheduler, TaskHandle, TaskOutcome, TaskPriority};

use crate::{
    BackendFailure, BackendFailures, BackendOperationError, GameBindBackend, RequiredServiceResult,
};

const ASYNC_QUEUE_CAPACITY: usize = 4_096;

/// Serialized adapter for synchronous and asynchronous GW2 game-bind calls.
///
/// Asynchronous calls use one FIFO command lane per canonical bind on the
/// process scheduler. This preserves same-bind submission order without making
/// a timed invocation block unrelated game input.
pub struct GameBindApi {
    lane: Arc<GameBindLane>,
}

impl GameBindApi {
    /// Creates an adapter around the runtime's exact game-invoker lifecycle slot.
    #[must_use]
    pub fn new(
        failures: Arc<BackendFailures>,
        invoker: Arc<Mutex<Option<GameInvoker>>>,
        scheduler: Option<Arc<MinimalScheduler>>,
    ) -> Self {
        Self {
            lane: Arc::new(GameBindLane {
                failures,
                invoker,
                scheduler,
                state: Mutex::new(GameBindLaneState::default()),
            }),
        }
    }

    /// Enqueues a game-bind press in FIFO order.
    pub fn press_async(&self, bind: GameBind) -> RequiredServiceResult<()> {
        self.lane
            .enqueue(GameBindCommand::Press(canonical_game_bind_id(bind)))
    }

    /// Enqueues a game-bind release in FIFO order.
    pub fn release_async(&self, bind: GameBind) -> RequiredServiceResult<()> {
        self.lane
            .enqueue(GameBindCommand::Release(canonical_game_bind_id(bind)))
    }

    /// Enqueues one cancellation-aware press and release operation.
    pub fn invoke_async(&self, bind: GameBind, duration: i32) -> RequiredServiceResult<()> {
        self.lane.enqueue(GameBindCommand::Invoke {
            bind: canonical_game_bind_id(bind),
            duration,
        })
    }

    /// Presses one game bind synchronously.
    pub fn press(&self, bind: GameBind) -> RequiredServiceResult<()> {
        self.lane.sync_operation(|invoker| {
            invoker
                .press_repeated(canonical_game_bind_id(bind))
                .map(|_| ())
        })
    }

    /// Releases one game bind synchronously.
    pub fn release(&self, bind: GameBind) -> RequiredServiceResult<()> {
        self.lane
            .sync_operation(|invoker| invoker.release(canonical_game_bind_id(bind)).map(|_| ()))
    }

    /// Returns exactly `0` or `1` for the canonical game binding state.
    pub fn is_bound(&self, bind: GameBind) -> RequiredServiceResult<u8> {
        self.lane.sync_operation(|invoker| {
            Ok(u8::from(
                invoker.registry().is_bound_exact(raw_game_bind_id(bind)),
            ))
        })
    }

    /// Closes the service, cancels queued work, and waits for the active command.
    ///
    /// A timed invocation that already pressed its bind always attempts its
    /// release before this method returns. Runtime shutdown must call this
    /// before taking the shared invoker slot or detaching the game window.
    /// When called from this scheduler's own worker, terminal task-handle
    /// observation is deferred so the worker never waits on itself; the closed
    /// lane and synchronous `release_all` still prevent later input effects.
    pub fn shutdown_and_drain(&self) {
        self.lane.shutdown_and_drain();
    }
}

impl GameBindBackend for GameBindApi {
    fn press_async(&self, bind: GameBind) -> RequiredServiceResult<()> {
        GameBindApi::press_async(self, bind)
    }

    fn release_async(&self, bind: GameBind) -> RequiredServiceResult<()> {
        GameBindApi::release_async(self, bind)
    }

    fn invoke_async(&self, bind: GameBind, duration: i32) -> RequiredServiceResult<()> {
        GameBindApi::invoke_async(self, bind, duration)
    }

    fn press(&self, bind: GameBind) -> RequiredServiceResult<()> {
        GameBindApi::press(self, bind)
    }

    fn release(&self, bind: GameBind) -> RequiredServiceResult<()> {
        GameBindApi::release(self, bind)
    }

    fn is_bound(&self, bind: GameBind) -> RequiredServiceResult<u8> {
        GameBindApi::is_bound(self, bind)
    }
}

impl fmt::Debug for GameBindApi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = mutex_lock(&self.lane.state);
        formatter
            .debug_struct("GameBindApi")
            .field("accepting", &state.accepting)
            .field("queued", &state.pending)
            .field("active_lanes", &state.binds.len())
            .field("scheduler_available", &self.lane.scheduler.is_some())
            .finish_non_exhaustive()
    }
}

impl Drop for GameBindApi {
    fn drop(&mut self) {
        self.lane.shutdown_without_drain();
    }
}

#[derive(Clone, Copy, Debug)]
enum GameBindCommand {
    Press(GameBindId),
    Release(GameBindId),
    Invoke { bind: GameBindId, duration: i32 },
}

impl GameBindCommand {
    const fn bind(self) -> GameBindId {
        match self {
            Self::Press(bind) | Self::Release(bind) | Self::Invoke { bind, .. } => bind,
        }
    }
}

struct GameBindLane {
    failures: Arc<BackendFailures>,
    invoker: Arc<Mutex<Option<GameInvoker>>>,
    scheduler: Option<Arc<MinimalScheduler>>,
    state: Mutex<GameBindLaneState>,
}

struct GameBindLaneState {
    accepting: bool,
    pending: usize,
    binds: BTreeMap<GameBindId, BindCommandLane>,
}

#[derive(Default)]
struct BindCommandLane {
    queue: VecDeque<GameBindCommand>,
    runner: Option<TaskHandle>,
}

impl Default for GameBindLaneState {
    fn default() -> Self {
        Self {
            accepting: true,
            pending: 0,
            binds: BTreeMap::new(),
        }
    }
}

impl GameBindLane {
    fn enqueue(self: &Arc<Self>, command: GameBindCommand) -> RequiredServiceResult<()> {
        let Some(scheduler) = self.scheduler.as_ref() else {
            return Err(self.service_rejected());
        };
        let bind = command.bind();
        let mut state = mutex_lock(&self.state);
        Self::retire_finished_runner(&mut state, bind);
        if !state.accepting
            || state.pending >= ASYNC_QUEUE_CAPACITY
            || mutex_lock(self.invoker.as_ref()).is_none()
        {
            return Err(self.service_rejected());
        }

        let needs_runner = {
            let lane = state.binds.entry(bind).or_default();
            lane.queue.push_back(command);
            lane.runner.is_none()
        };
        state.pending += 1;
        if !needs_runner {
            return Ok(());
        }

        debug_assert_eq!(state.binds[&bind].queue.len(), 1);
        let lane = Arc::clone(self);
        match scheduler.submit(TaskPriority::Low, move |token| {
            let result = panic::catch_unwind(AssertUnwindSafe(|| lane.run_commands(bind, &token)));
            if result.is_err() {
                lane.abort_runner();
                lane.record_service_rejected();
            }
        }) {
            Ok(handle) => {
                let Some(bind_lane) = state.binds.get_mut(&bind) else {
                    state.accepting = false;
                    state.pending = 0;
                    state.binds.clear();
                    return Err(self.service_rejected());
                };
                bind_lane.runner = Some(handle);
                Ok(())
            }
            Err(_) => {
                state.accepting = false;
                state.pending = 0;
                state.binds.clear();
                Err(self.service_rejected())
            }
        }
    }

    fn retire_finished_runner(state: &mut GameBindLaneState, bind: GameBindId) {
        let finished = state
            .binds
            .get(&bind)
            .and_then(|lane| lane.runner.as_ref())
            .and_then(TaskHandle::outcome)
            .is_some();
        if finished {
            let queued = state.binds.remove(&bind).map_or(0, |lane| lane.queue.len());
            state.pending = state.pending.saturating_sub(queued);
            if queued > 0 {
                state.accepting = false;
                state.pending = 0;
                state.binds.clear();
            }
        }
    }

    fn run_commands(&self, bind: GameBindId, token: &CancellationToken) {
        loop {
            let command = {
                let mut state = mutex_lock(&self.state);
                if token.is_cancelled() || !state.accepting {
                    state.accepting = false;
                    state.pending = 0;
                    state.binds.clear();
                    return;
                }
                let command = state
                    .binds
                    .get_mut(&bind)
                    .and_then(|lane| lane.queue.pop_front());
                let Some(command) = command else {
                    state.binds.remove(&bind);
                    return;
                };
                state.pending -= 1;
                command
            };
            self.execute(command, token);
        }
    }

    fn execute(&self, command: GameBindCommand, token: &CancellationToken) {
        if token.is_cancelled() {
            return;
        }
        match command {
            GameBindCommand::Press(bind) => {
                if matches!(
                    self.execute_operation(token, |invoker| {
                        invoker.press_repeated(bind).map(|_| ())
                    }),
                    Err(())
                ) {
                    self.record_service_rejected();
                }
            }
            GameBindCommand::Release(bind) => {
                if matches!(
                    self.execute_operation(token, |invoker| invoker.release(bind).map(|_| ())),
                    Err(())
                ) {
                    self.record_service_rejected();
                }
            }
            GameBindCommand::Invoke { bind, duration } => {
                self.execute_invoke(bind, duration, token);
            }
        }
    }

    fn execute_invoke(&self, bind: GameBindId, duration: i32, token: &CancellationToken) {
        if duration <= 0 {
            if matches!(
                self.execute_operation(token, |invoker| {
                    let (_dispatch, press) = invoker.press_repeated(bind)?;
                    invoker.release_tracked(bind, &press).map(|_| ())
                }),
                Err(())
            ) {
                self.record_service_rejected();
            }
            return;
        }

        let press = match self.execute_operation(token, |invoker| {
            invoker.press_repeated(bind).map(|(_dispatch, token)| token)
        }) {
            Ok(Some(press)) => press,
            Ok(None) => return,
            Err(()) => {
                self.record_service_rejected();
                return;
            }
        };
        let _cancelled = token.wait_cancelled_timeout(Duration::from_millis(duration as u64));
        if !self.release_tracked(bind, &press) {
            self.record_service_rejected();
        }
    }

    fn execute_operation<T>(
        &self,
        token: &CancellationToken,
        operation: impl FnOnce(&mut GameInvoker) -> Result<T, GameInputError>,
    ) -> Result<Option<T>, ()> {
        let state = mutex_lock(&self.state);
        if !state.accepting || token.is_cancelled() {
            return Ok(None);
        }
        let mut slot = mutex_lock(self.invoker.as_ref());
        let Some(invoker) = slot.as_mut() else {
            return Err(());
        };
        operation(invoker).map(Some).map_err(|_| ())
    }

    fn release_tracked(&self, bind: GameBindId, press: &GamePressToken) -> bool {
        let mut slot = mutex_lock(self.invoker.as_ref());
        let Some(invoker) = slot.as_mut() else {
            return false;
        };
        invoker.release_tracked(bind, press).is_ok()
    }

    fn sync_operation<T>(
        &self,
        operation: impl FnOnce(&mut GameInvoker) -> Result<T, GameInputError>,
    ) -> RequiredServiceResult<T> {
        let state = mutex_lock(&self.state);
        if !state.accepting {
            return Err(self.service_rejected());
        }
        let mut slot = mutex_lock(self.invoker.as_ref());
        let Some(invoker) = slot.as_mut() else {
            return Err(self.service_rejected());
        };
        operation(invoker).map_err(|_| self.service_rejected())
    }

    fn shutdown_and_drain(&self) {
        let runners = self.request_shutdown();
        for runner in &runners {
            runner.cancel();
        }
        let called_from_worker = self
            .scheduler
            .as_ref()
            .is_some_and(|scheduler| scheduler.is_worker_thread());
        if !called_from_worker {
            for runner in runners {
                if matches!(runner.wait(), TaskOutcome::Panicked) {
                    self.record_service_rejected();
                }
            }
        }
        self.release_all();
    }

    fn shutdown_without_drain(&self) {
        for runner in self.request_shutdown() {
            runner.cancel();
        }
        self.release_all();
    }

    fn request_shutdown(&self) -> Vec<TaskHandle> {
        let mut state = mutex_lock(&self.state);
        state.accepting = false;
        state.pending = 0;
        let runners = state
            .binds
            .values()
            .filter_map(|lane| lane.runner.clone())
            .collect();
        state.binds.clear();
        runners
    }

    fn release_all(&self) {
        let mut slot = mutex_lock(self.invoker.as_ref());
        if slot
            .as_mut()
            .is_some_and(|invoker| invoker.release_all().is_err())
        {
            self.record_service_rejected();
        }
    }

    fn abort_runner(&self) {
        let mut state = mutex_lock(&self.state);
        state.accepting = false;
        state.pending = 0;
        state.binds.clear();
    }

    fn service_rejected(&self) -> BackendOperationError {
        self.record_service_rejected();
        BackendOperationError::ServiceRejected
    }

    fn record_service_rejected(&self) {
        self.failures.record(BackendFailure::ServiceRejected);
    }
}

const fn raw_game_bind_id(bind: GameBind) -> GameBindId {
    GameBindId(bind.0)
}

const fn canonical_game_bind_id(bind: GameBind) -> GameBindId {
    GameBindId(bind.0).canonical()
}

fn mutex_lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::sync::{Arc, Condvar, Mutex, MutexGuard};
    use std::time::{Duration, Instant};

    use nexus_abi::GameBind;
    use nexus_input::{
        GameBindId, GameBindRegistry, GameMessage, GameMessageSink, GameSinkError, ModifierState,
        PhysicalInputState,
    };
    use nexus_platform::MinimalScheduler;

    use super::{GameBindApi, GameInvoker, mutex_lock};
    use crate::{BackendFailures, BackendOperationError, GameBindBackend};

    #[derive(Default)]
    struct RecordingSink {
        batches: Mutex<Vec<Vec<GameMessage>>>,
        changed: Condvar,
    }

    impl RecordingSink {
        fn wait_for_batches(&self, expected: usize, timeout: Duration) -> bool {
            let deadline = Instant::now() + timeout;
            let mut batches = lock(&self.batches);
            while batches.len() < expected {
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    return false;
                };
                let result = self
                    .changed
                    .wait_timeout_while(batches, remaining, |batches| batches.len() < expected);
                let (next, timed_out) = match result {
                    Ok(result) => result,
                    Err(poisoned) => poisoned.into_inner(),
                };
                batches = next;
                if timed_out.timed_out() && batches.len() < expected {
                    return false;
                }
            }
            true
        }

        fn snapshot(&self) -> Vec<Vec<GameMessage>> {
            lock(&self.batches).clone()
        }
    }

    impl GameMessageSink for RecordingSink {
        fn send_batch(&self, messages: &[GameMessage]) -> Result<(), GameSinkError> {
            lock(&self.batches).push(messages.to_vec());
            self.changed.notify_all();
            Ok(())
        }
    }

    struct FixedPhysical;

    impl PhysicalInputState for FixedPhysical {
        fn modifiers(&self) -> ModifierState {
            ModifierState::default()
        }
    }

    struct TestRig {
        api: GameBindApi,
        invoker: Arc<Mutex<Option<GameInvoker>>>,
        sink: Arc<RecordingSink>,
        failures: Arc<BackendFailures>,
        scheduler: Option<Arc<MinimalScheduler>>,
    }

    fn test_rig(worker_count: Option<usize>) -> TestRig {
        let sink = Arc::new(RecordingSink::default());
        let message_sink: Arc<dyn GameMessageSink> = sink.clone();
        let physical: Arc<dyn PhysicalInputState> = Arc::new(FixedPhysical);
        let invoker = Arc::new(Mutex::new(Some(GameInvoker::new(
            GameBindRegistry::with_defaults(),
            message_sink,
            physical,
        ))));
        let failures = Arc::new(BackendFailures::new());
        let scheduler = worker_count.map(|workers| {
            Arc::new(
                MinimalScheduler::with_worker_count(workers).expect("test scheduler should start"),
            )
        });
        let api = GameBindApi::new(
            Arc::clone(&failures),
            Arc::clone(&invoker),
            scheduler.clone(),
        );
        TestRig {
            api,
            invoker,
            sink,
            failures,
            scheduler,
        }
    }

    fn bind(id: GameBindId) -> GameBind {
        GameBind(id.0)
    }

    fn keyboard_states(batches: &[Vec<GameMessage>]) -> Vec<bool> {
        batches
            .iter()
            .flat_map(|batch| batch.iter())
            .filter_map(|message| match message {
                GameMessage::Keyboard { pressed, .. } => Some(*pressed),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn sync_calls_preserve_open_ids_and_match_legacy_bound_query() {
        let TestRig {
            api,
            invoker,
            failures,
            ..
        } = test_rig(None);

        assert_eq!(api.is_bound(bind(GameBindId::LEGACY_MOVE_SWIM_UP)), Ok(0));
        assert_eq!(
            api.is_bound(bind(GameBindId::MOVE_JUMP_SWIM_UP_FLY_UP)),
            Ok(1)
        );
        assert_eq!(api.is_bound(GameBind(u32::MAX)), Ok(0));
        assert_eq!(api.press(bind(GameBindId::LEGACY_MOVE_SWIM_UP)), Ok(()));
        assert!(
            mutex_lock(invoker.as_ref())
                .as_ref()
                .is_some_and(|invoker| invoker.is_pressed(GameBindId::MOVE_JUMP_SWIM_UP_FLY_UP))
        );
        assert_eq!(
            api.release(bind(GameBindId::MOVE_JUMP_SWIM_UP_FLY_UP)),
            Ok(())
        );
        assert_eq!(failures.snapshot().service_rejected, 0);
    }

    #[test]
    fn repeated_press_matches_native_behavior_and_failures_stay_closed() {
        let TestRig {
            api,
            invoker,
            sink,
            failures,
            ..
        } = test_rig(None);

        assert_eq!(
            api.press(bind(GameBindId::MOVE_WALK)),
            Err(BackendOperationError::ServiceRejected)
        );
        assert_eq!(api.press(bind(GameBindId::MOVE_FORWARD)), Ok(()));
        assert_eq!(api.press(bind(GameBindId::MOVE_FORWARD)), Ok(()));
        assert_eq!(api.release(bind(GameBindId::MOVE_FORWARD)), Ok(()));
        assert_eq!(keyboard_states(&sink.snapshot()), [true, true, false]);
        *mutex_lock(invoker.as_ref()) = None;
        assert_eq!(
            api.is_bound(bind(GameBindId::MOVE_FORWARD)),
            Err(BackendOperationError::ServiceRejected)
        );
        assert_eq!(failures.snapshot().service_rejected, 2);
    }

    #[test]
    fn async_requires_a_scheduler_but_sync_remains_available() {
        let TestRig { api, failures, .. } = test_rig(None);

        assert_eq!(api.press(bind(GameBindId::MOVE_FORWARD)), Ok(()));
        assert_eq!(api.release(bind(GameBindId::MOVE_FORWARD)), Ok(()));
        assert_eq!(
            api.press_async(bind(GameBindId::MOVE_FORWARD)),
            Err(BackendOperationError::ServiceRejected)
        );
        assert_eq!(failures.snapshot().service_rejected, 1);
    }

    #[test]
    fn multiworker_async_lane_preserves_fifo_submission_order() {
        let TestRig {
            api,
            invoker,
            sink,
            failures,
            scheduler,
        } = test_rig(Some(4));
        let pairs = 100;

        for _ in 0..pairs {
            assert_eq!(api.press_async(bind(GameBindId::MOVE_FORWARD)), Ok(()));
            assert_eq!(api.release_async(bind(GameBindId::MOVE_FORWARD)), Ok(()));
        }
        assert!(sink.wait_for_batches(pairs * 2, Duration::from_secs(5)));
        let expected = (0..pairs * 2)
            .map(|index| index % 2 == 0)
            .collect::<Vec<_>>();
        assert_eq!(keyboard_states(&sink.snapshot()), expected);
        assert!(
            mutex_lock(invoker.as_ref())
                .as_ref()
                .is_some_and(|invoker| !invoker.is_pressed(GameBindId::MOVE_FORWARD))
        );
        assert_eq!(failures.snapshot().service_rejected, 0);

        api.shutdown_and_drain();
        scheduler
            .expect("test scheduler should exist")
            .shutdown_and_drain()
            .expect("test scheduler should drain");
    }

    #[test]
    fn timed_invoke_releases_promptly_when_the_lane_is_cancelled() {
        let TestRig {
            api,
            invoker,
            sink,
            failures,
            scheduler,
        } = test_rig(Some(1));

        assert_eq!(
            api.invoke_async(bind(GameBindId::MOVE_FORWARD), 60_000),
            Ok(())
        );
        assert!(sink.wait_for_batches(1, Duration::from_secs(5)));
        api.shutdown_and_drain();
        assert!(sink.wait_for_batches(2, Duration::from_secs(5)));
        assert_eq!(keyboard_states(&sink.snapshot()), [true, false]);
        assert!(
            mutex_lock(invoker.as_ref())
                .as_ref()
                .is_some_and(|invoker| !invoker.is_pressed(GameBindId::MOVE_FORWARD))
        );
        assert_eq!(
            api.press(bind(GameBindId::MOVE_FORWARD)),
            Err(BackendOperationError::ServiceRejected)
        );
        assert_eq!(failures.snapshot().service_rejected, 1);

        scheduler
            .expect("test scheduler should exist")
            .shutdown_and_drain()
            .expect("test scheduler should drain");
    }

    #[test]
    fn timed_invoke_does_not_block_an_unrelated_bind_lane() {
        let TestRig {
            api,
            sink,
            failures,
            scheduler,
            ..
        } = test_rig(Some(2));

        assert_eq!(
            api.invoke_async(bind(GameBindId::MOVE_FORWARD), 60_000),
            Ok(())
        );
        assert!(sink.wait_for_batches(1, Duration::from_secs(5)));
        assert_eq!(api.press_async(bind(GameBindId::MOVE_BACKWARD)), Ok(()));
        assert!(sink.wait_for_batches(2, Duration::from_secs(5)));
        assert_eq!(api.release_async(bind(GameBindId::MOVE_BACKWARD)), Ok(()));
        assert!(sink.wait_for_batches(3, Duration::from_secs(5)));
        assert_eq!(failures.snapshot().service_rejected, 0);

        api.shutdown_and_drain();
        assert!(sink.wait_for_batches(4, Duration::from_secs(5)));
        scheduler
            .expect("test scheduler should exist")
            .shutdown_and_drain()
            .expect("test scheduler should drain");
    }

    #[test]
    fn nonpositive_invoke_dispatches_adjacent_press_and_release() {
        for duration in [-1, 0] {
            let TestRig {
                api,
                sink,
                failures,
                scheduler,
                ..
            } = test_rig(Some(2));

            assert_eq!(
                GameBindBackend::invoke_async(&api, bind(GameBindId::MOVE_FORWARD), duration),
                Ok(())
            );
            assert!(sink.wait_for_batches(2, Duration::from_secs(5)));
            assert_eq!(keyboard_states(&sink.snapshot()), [true, false]);
            assert_eq!(failures.snapshot().service_rejected, 0);

            api.shutdown_and_drain();
            scheduler
                .expect("test scheduler should exist")
                .shutdown_and_drain()
                .expect("test scheduler should drain");
        }
    }

    #[test]
    fn dropping_from_the_only_scheduler_worker_never_waits_on_its_runner() {
        let TestRig {
            api,
            invoker,
            scheduler,
            ..
        } = test_rig(Some(1));
        let scheduler = scheduler.expect("test scheduler should exist");
        let (finished_tx, finished_rx) = mpsc::channel();

        let owner = scheduler
            .submit(nexus_platform::TaskPriority::High, move |_| {
                assert_eq!(
                    api.invoke_async(bind(GameBindId::MOVE_FORWARD), 60_000),
                    Ok(())
                );
                drop(api);
                finished_tx
                    .send(())
                    .expect("test completion receiver should remain alive");
            })
            .expect("owner task should submit");
        finished_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("dropping the API must not deadlock its scheduler worker");
        assert_eq!(owner.wait(), nexus_platform::TaskOutcome::Completed);
        assert!(
            mutex_lock(invoker.as_ref())
                .as_ref()
                .is_some_and(|invoker| !invoker.is_pressed(GameBindId::MOVE_FORWARD))
        );
        scheduler
            .shutdown_and_drain()
            .expect("test scheduler should drain");
    }

    #[test]
    fn shutdown_from_the_only_scheduler_worker_never_waits_on_its_runner() {
        let TestRig {
            api,
            invoker,
            scheduler,
            ..
        } = test_rig(Some(1));
        let scheduler = scheduler.expect("test scheduler should exist");
        let (finished_tx, finished_rx) = mpsc::channel();

        let owner = scheduler
            .submit(nexus_platform::TaskPriority::High, move |_| {
                assert_eq!(api.press_async(bind(GameBindId::MOVE_FORWARD)), Ok(()));
                api.shutdown_and_drain();
                finished_tx
                    .send(())
                    .expect("test completion receiver should remain alive");
            })
            .expect("owner task should submit");
        finished_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("shutdown must not deadlock its scheduler worker");
        assert_eq!(owner.wait(), nexus_platform::TaskOutcome::Completed);
        assert!(
            mutex_lock(invoker.as_ref())
                .as_ref()
                .is_some_and(|invoker| !invoker.is_pressed(GameBindId::MOVE_FORWARD))
        );
        scheduler
            .shutdown_and_drain()
            .expect("test scheduler should drain");
    }

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
