use std::{
    marker::PhantomData,
    rc::Rc,
    sync::{
        Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

/// Admission gate and drain counter for detours and native addon callbacks.
pub struct CallbackGate {
    accepting: AtomicBool,
    in_flight: AtomicUsize,
    drain_mutex: Mutex<()>,
    drained: Condvar,
}

impl CallbackGate {
    /// Creates a gate that initially accepts callbacks.
    #[must_use]
    pub const fn open() -> Self {
        Self {
            accepting: AtomicBool::new(true),
            in_flight: AtomicUsize::new(0),
            drain_mutex: Mutex::new(()),
            drained: Condvar::new(),
        }
    }

    /// Admits one callback when shutdown has not closed the gate.
    ///
    /// The second admission check closes the race where shutdown begins after
    /// the first check but before the in-flight increment.
    #[must_use]
    pub fn try_enter(&self) -> Option<CallbackGuard<'_>> {
        if !self.accepting.load(Ordering::Acquire) {
            return None;
        }

        self.in_flight.fetch_add(1, Ordering::AcqRel);
        if self.accepting.load(Ordering::Acquire) {
            Some(CallbackGuard {
                gate: self,
                _thread_bound: PhantomData,
            })
        } else {
            self.leave();
            None
        }
    }

    /// Permanently rejects new callbacks and returns whether this call closed it.
    pub fn close(&self) -> bool {
        self.accepting.swap(false, Ordering::AcqRel)
    }

    /// Waits for accepted callbacks to leave, bounded by `timeout`.
    #[must_use]
    pub fn wait_for_drain(&self, timeout: Duration) -> bool {
        if self.in_flight.load(Ordering::Acquire) == 0 {
            return true;
        }

        let start = Instant::now();
        let mut guard = lock(&self.drain_mutex);
        loop {
            if self.in_flight.load(Ordering::Acquire) == 0 {
                return true;
            }
            let elapsed = start.elapsed();
            let Some(remaining) = timeout.checked_sub(elapsed) else {
                return false;
            };
            if remaining.is_zero() {
                return false;
            }

            let (next_guard, wait_result) = self
                .drained
                .wait_timeout(guard, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard = next_guard;
            if wait_result.timed_out() {
                return self.in_flight.load(Ordering::Acquire) == 0;
            }
        }
    }

    /// Returns whether new callbacks can still enter.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.accepting.load(Ordering::Acquire)
    }

    /// Returns the current callback count for diagnostics.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
    }

    fn leave(&self) {
        let previous = self.in_flight.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "callback gate counter underflow");
        if previous == 1 {
            // Pairing notification with this mutex prevents a waiter from
            // observing a nonzero count and then missing the final wakeup.
            let _guard = lock(&self.drain_mutex);
            self.drained.notify_all();
        }
    }
}

impl Default for CallbackGate {
    fn default() -> Self {
        Self::open()
    }
}

/// RAII proof that one callback was admitted through a [`CallbackGate`].
pub struct CallbackGuard<'a> {
    gate: &'a CallbackGate,
    // Hook callbacks are deliberately drained on their entry thread.
    _thread_bound: PhantomData<Rc<()>>,
}

impl Drop for CallbackGuard<'_> {
    fn drop(&mut self) {
        self.gate.leave();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, mpsc},
        thread,
        time::Duration,
    };

    use super::CallbackGate;

    #[test]
    fn close_rejects_new_callbacks() {
        let gate = CallbackGate::open();
        assert!(gate.try_enter().is_some());
        assert!(gate.close());
        assert!(!gate.close());
        assert!(gate.try_enter().is_none());
    }

    #[test]
    fn waits_for_in_flight_callback_to_leave() {
        let gate = Arc::new(CallbackGate::open());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker_gate = Arc::clone(&gate);
        let worker = thread::spawn(move || {
            let _guard = worker_gate
                .try_enter()
                .unwrap_or_else(|| panic!("gate closed before worker entered"));
            entered_tx
                .send(())
                .unwrap_or_else(|_| panic!("entry signal receiver disappeared"));
            release_rx
                .recv()
                .unwrap_or_else(|_| panic!("release signal sender disappeared"));
        });
        entered_rx
            .recv()
            .unwrap_or_else(|_| panic!("worker did not enter"));

        gate.close();
        assert!(!gate.wait_for_drain(Duration::from_millis(1)));
        release_tx
            .send(())
            .unwrap_or_else(|_| panic!("worker disappeared"));
        assert!(gate.wait_for_drain(Duration::from_secs(1)));
        worker.join().unwrap_or_else(|_| panic!("worker panicked"));
    }

    #[test]
    fn dropping_guard_decrements_exactly_once() {
        let gate = CallbackGate::open();
        let guard = gate
            .try_enter()
            .unwrap_or_else(|| panic!("open gate rejected callback"));
        assert_eq!(gate.in_flight(), 1);
        drop(guard);
        assert_eq!(gate.in_flight(), 0);
        assert!(gate.wait_for_drain(Duration::ZERO));
    }
}
