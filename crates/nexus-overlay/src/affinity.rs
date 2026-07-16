use std::sync::{Mutex, MutexGuard};
use std::thread::{self, ThreadId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AffinityStatus {
    Unclaimed,
    Owner,
    Foreign,
}

#[derive(Debug, Default)]
pub(crate) struct ThreadAffinity {
    owner: Mutex<Option<ThreadId>>,
}

impl ThreadAffinity {
    pub(crate) fn claim(&self) -> AffinityStatus {
        let current = thread::current().id();
        let mut owner = recover(&self.owner);
        match owner.as_ref() {
            Some(existing) if *existing == current => AffinityStatus::Owner,
            Some(_) => AffinityStatus::Foreign,
            None => {
                *owner = Some(current);
                AffinityStatus::Owner
            }
        }
    }

    pub(crate) fn status(&self) -> AffinityStatus {
        let current = thread::current().id();
        match recover(&self.owner).as_ref() {
            Some(existing) if *existing == current => AffinityStatus::Owner,
            Some(_) => AffinityStatus::Foreign,
            None => AffinityStatus::Unclaimed,
        }
    }
}

fn recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{AffinityStatus, ThreadAffinity};

    #[test]
    fn first_thread_claims_and_reentry_is_stable() {
        let affinity = ThreadAffinity::default();
        assert_eq!(affinity.status(), AffinityStatus::Unclaimed);
        assert_eq!(affinity.claim(), AffinityStatus::Owner);
        assert_eq!(affinity.claim(), AffinityStatus::Owner);
    }

    #[test]
    fn a_foreign_thread_cannot_take_ownership() {
        let affinity = Arc::new(ThreadAffinity::default());
        assert_eq!(affinity.claim(), AffinityStatus::Owner);
        let foreign = Arc::clone(&affinity);
        let result = std::thread::spawn(move || (foreign.status(), foreign.claim()))
            .join()
            .expect("affinity test thread should complete");
        assert_eq!(result, (AffinityStatus::Foreign, AffinityStatus::Foreign));
        assert_eq!(affinity.status(), AffinityStatus::Owner);
    }
}
