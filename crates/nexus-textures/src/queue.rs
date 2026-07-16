use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};

pub(crate) struct BoundedQueue<T> {
    capacity: usize,
    state: Mutex<QueueState<T>>,
    available: Condvar,
    space: Condvar,
}

struct QueueState<T> {
    closed: bool,
    items: VecDeque<T>,
}

impl<T> BoundedQueue<T> {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(QueueState {
                closed: false,
                items: VecDeque::with_capacity(capacity),
            }),
            available: Condvar::new(),
            space: Condvar::new(),
        }
    }

    pub(crate) fn try_push(&self, item: T) -> Result<(), T> {
        let mut state = self.lock();
        if state.closed || state.items.len() >= self.capacity {
            return Err(item);
        }
        state.items.push_back(item);
        self.available.notify_one();
        Ok(())
    }

    pub(crate) fn push_wait(&self, item: T, stopping: &AtomicBool) -> Result<(), T> {
        let mut state = self.lock();
        while !state.closed && !stopping.load(Ordering::Acquire) {
            if state.items.len() < self.capacity {
                state.items.push_back(item);
                self.available.notify_one();
                return Ok(());
            }
            state = self
                .space
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        Err(item)
    }

    pub(crate) fn pop_wait(&self, stopping: &AtomicBool) -> Option<T> {
        let mut state = self.lock();
        loop {
            if stopping.load(Ordering::Acquire) || state.closed {
                return None;
            }
            if let Some(item) = state.items.pop_front() {
                self.space.notify_one();
                return Some(item);
            }
            state = self
                .available
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    pub(crate) fn try_pop(&self) -> Option<T> {
        let mut state = self.lock();
        let item = state.items.pop_front();
        if item.is_some() {
            self.space.notify_one();
        }
        item
    }

    pub(crate) fn retain(&self, mut keep: impl FnMut(&T) -> bool) {
        let mut state = self.lock();
        let before = state.items.len();
        state.items.retain(|item| keep(item));
        if state.items.len() != before {
            self.space.notify_all();
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.lock().items.len()
    }

    pub(crate) fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        self.available.notify_all();
        self.space.notify_all();
    }

    fn lock(&self) -> MutexGuard<'_, QueueState<T>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
