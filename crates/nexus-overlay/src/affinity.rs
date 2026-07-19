use std::marker::PhantomData;
use std::rc::Rc;
#[cfg(test)]
use std::sync::Weak;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, ThreadId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AffinityStatus {
    Unclaimed,
    Owner,
    Foreign,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AffinityClaimError {
    Foreign,
    Busy,
    StaleProof,
    Exhausted,
    InvalidState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AffinityProof {
    generation: u64,
    present_sequence: u64,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ThreadAffinity {
    inner: Arc<AffinityInner>,
}

#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct WeakThreadAffinity {
    inner: Weak<AffinityInner>,
}

#[derive(Debug, Default)]
struct AffinityInner {
    state: Mutex<AffinityState>,
}

#[derive(Debug, Default)]
struct AffinityState {
    owner: Option<AffinityOwner>,
    next_token: u64,
    latest_proof: Option<AffinityProof>,
}

#[derive(Debug)]
struct AffinityOwner {
    thread: ThreadId,
    token: u64,
    claims: usize,
    leases: usize,
    guards: usize,
}

#[must_use = "dropping an uncommitted affinity claim releases its provisional ownership"]
pub(crate) struct ThreadAffinityClaim {
    inner: Arc<AffinityInner>,
    thread: ThreadId,
    token: u64,
    active: bool,
    _thread_bound: PhantomData<Rc<()>>,
}

#[must_use = "the affinity lease must be retained for the complete native-state lifetime"]
pub(crate) struct ThreadAffinityLease {
    inner: Arc<AffinityInner>,
    thread: ThreadId,
    token: u64,
    active: bool,
    _thread_bound: PhantomData<Rc<()>>,
}

#[must_use = "the affinity guard must span the complete native callback"]
pub(crate) struct ThreadAffinityGuard {
    inner: Arc<AffinityInner>,
    thread: ThreadId,
    token: u64,
    active: bool,
    _thread_bound: PhantomData<Rc<()>>,
}

impl AffinityProof {
    /// Identifies one classifier-approved Present in the selected chain's
    /// monotonic resource generation and callback sequence.
    pub(crate) const fn new(generation: u64, present_sequence: u64) -> Self {
        Self {
            generation,
            present_sequence,
        }
    }

    fn is_strictly_after(self, other: Self) -> bool {
        self.generation >= other.generation && self.present_sequence > other.present_sequence
    }
}

impl ThreadAffinity {
    #[cfg(test)]
    pub(crate) fn downgrade(&self) -> WeakThreadAffinity {
        WeakThreadAffinity {
            inner: Arc::downgrade(&self.inner),
        }
    }

    #[cfg(test)]
    pub(crate) fn begin_claim(&self) -> Result<ThreadAffinityClaim, AffinityClaimError> {
        self.begin_claim_with_proof(None)
    }

    pub(crate) fn begin_present_claim(
        &self,
        proof: AffinityProof,
    ) -> Result<ThreadAffinityClaim, AffinityClaimError> {
        // A foreign caller may rotate only a dormant epoch. Live claims and
        // guards are the cooperative acknowledgement that its owner is still
        // inside native work; leases alone deliberately permit handoff because
        // their TLS resources remain dormant and token-revoked.
        self.begin_claim_with_proof(Some(proof))
    }

    fn begin_claim_with_proof(
        &self,
        proof: Option<AffinityProof>,
    ) -> Result<ThreadAffinityClaim, AffinityClaimError> {
        let current = thread::current().id();
        let mut state = recover(&self.inner.state);
        if let Some(proof) = proof
            && state
                .latest_proof
                .is_some_and(|latest| !proof.is_strictly_after(latest))
        {
            return Err(AffinityClaimError::StaleProof);
        }
        let token = match state.owner.as_mut() {
            Some(owner) if owner.thread == current => {
                let claims = owner
                    .claims
                    .checked_add(1)
                    .ok_or(AffinityClaimError::Exhausted)?;
                owner.claims = claims;
                owner.token
            }
            Some(owner) => {
                if proof.is_none() {
                    return Err(AffinityClaimError::Foreign);
                }
                if owner.claims != 0 || owner.guards != 0 {
                    return Err(AffinityClaimError::Busy);
                }

                let token = next_token(&mut state.next_token)?;
                state.owner = Some(AffinityOwner {
                    thread: current,
                    token,
                    claims: 1,
                    leases: 0,
                    guards: 0,
                });
                token
            }
            None => {
                if proof.is_none() && state.latest_proof.is_some() {
                    return Err(AffinityClaimError::Foreign);
                }
                let token = next_token(&mut state.next_token)?;
                state.owner = Some(AffinityOwner {
                    thread: current,
                    token,
                    claims: 1,
                    leases: 0,
                    guards: 0,
                });
                token
            }
        };
        if let Some(proof) = proof {
            state.latest_proof = Some(proof);
        }
        drop(state);

        Ok(ThreadAffinityClaim {
            inner: Arc::clone(&self.inner),
            thread: current,
            token,
            active: true,
            _thread_bound: PhantomData,
        })
    }

    pub(crate) fn status(&self) -> AffinityStatus {
        let current = thread::current().id();
        match recover(&self.inner.state).owner.as_ref() {
            Some(owner) if owner.thread == current => AffinityStatus::Owner,
            Some(_) => AffinityStatus::Foreign,
            None => AffinityStatus::Unclaimed,
        }
    }
}

#[cfg(test)]
impl WeakThreadAffinity {
    pub(crate) fn upgrade(&self) -> Option<ThreadAffinity> {
        self.inner.upgrade().map(|inner| ThreadAffinity { inner })
    }

    pub(crate) fn is_alive(&self) -> bool {
        self.inner.strong_count() != 0
    }
}

impl ThreadAffinityClaim {
    #[cfg(test)]
    pub(crate) fn commit(mut self) -> Result<ThreadAffinityLease, AffinityClaimError> {
        let (lease, guard) = self.commit_inner(false)?;
        debug_assert!(guard.is_none(), "unguarded commit cannot create a guard");
        Ok(lease)
    }

    pub(crate) fn commit_guarded(
        mut self,
    ) -> Result<(ThreadAffinityLease, ThreadAffinityGuard), AffinityClaimError> {
        let (lease, guard) = self.commit_inner(true)?;
        let Some(guard) = guard else {
            return Err(AffinityClaimError::InvalidState);
        };
        Ok((lease, guard))
    }

    fn commit_inner(
        &mut self,
        guarded: bool,
    ) -> Result<(ThreadAffinityLease, Option<ThreadAffinityGuard>), AffinityClaimError> {
        let mut state = recover(&self.inner.state);
        let Some(owner) = state
            .owner
            .as_mut()
            .filter(|owner| owner.thread == self.thread && owner.token == self.token)
        else {
            self.active = false;
            return Err(AffinityClaimError::Foreign);
        };
        let Some(claims) = owner.claims.checked_sub(1) else {
            self.active = false;
            return Err(AffinityClaimError::InvalidState);
        };
        let Some(leases) = owner.leases.checked_add(1) else {
            drop(state);
            return Err(AffinityClaimError::Exhausted);
        };
        let guards = if guarded {
            Some(
                owner
                    .guards
                    .checked_add(1)
                    .ok_or(AffinityClaimError::Exhausted)?,
            )
        } else {
            None
        };
        owner.claims = claims;
        owner.leases = leases;
        if let Some(guards) = guards {
            owner.guards = guards;
        }
        self.active = false;
        drop(state);

        let lease = ThreadAffinityLease {
            inner: Arc::clone(&self.inner),
            thread: self.thread,
            token: self.token,
            active: true,
            _thread_bound: PhantomData,
        };
        let guard = guarded.then(|| ThreadAffinityGuard {
            inner: Arc::clone(&self.inner),
            thread: self.thread,
            token: self.token,
            active: true,
            _thread_bound: PhantomData,
        });
        Ok((lease, guard))
    }
}

impl ThreadAffinityLease {
    pub(crate) fn shares_epoch(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
            && self.thread == other.thread
            && self.token == other.token
    }
}

impl Drop for ThreadAffinityClaim {
    fn drop(&mut self) {
        if self.active {
            release_claim(&self.inner, self.thread, self.token);
            self.active = false;
        }
    }
}

impl Drop for ThreadAffinityLease {
    fn drop(&mut self) {
        if self.active {
            release_lease(&self.inner, self.thread, self.token);
            self.active = false;
        }
    }
}

impl Drop for ThreadAffinityGuard {
    fn drop(&mut self) {
        if self.active {
            release_guard(&self.inner, self.thread, self.token);
            self.active = false;
        }
    }
}

fn next_token(next: &mut u64) -> Result<u64, AffinityClaimError> {
    let token = next
        .checked_add(1)
        .filter(|token| *token != 0)
        .ok_or(AffinityClaimError::Exhausted)?;
    *next = token;
    Ok(token)
}

fn release_claim(inner: &AffinityInner, thread: ThreadId, token: u64) {
    let mut state = recover(&inner.state);
    let Some(owner) = state
        .owner
        .as_mut()
        .filter(|owner| owner.thread == thread && owner.token == token)
    else {
        return;
    };
    let Some(claims) = owner.claims.checked_sub(1) else {
        debug_assert!(false, "an active claim must be registered");
        return;
    };
    owner.claims = claims;
    clear_released_owner(&mut state);
}

fn release_lease(inner: &AffinityInner, thread: ThreadId, token: u64) {
    let mut state = recover(&inner.state);
    let Some(owner) = state
        .owner
        .as_mut()
        .filter(|owner| owner.thread == thread && owner.token == token)
    else {
        return;
    };
    let Some(leases) = owner.leases.checked_sub(1) else {
        debug_assert!(false, "an active lease must be registered");
        return;
    };
    owner.leases = leases;
    clear_released_owner(&mut state);
}

fn release_guard(inner: &AffinityInner, thread: ThreadId, token: u64) {
    let mut state = recover(&inner.state);
    let Some(owner) = state
        .owner
        .as_mut()
        .filter(|owner| owner.thread == thread && owner.token == token)
    else {
        return;
    };
    let Some(guards) = owner.guards.checked_sub(1) else {
        debug_assert!(false, "an active guard must be registered");
        return;
    };
    owner.guards = guards;
    clear_released_owner(&mut state);
}

fn clear_released_owner(state: &mut AffinityState) {
    if state
        .owner
        .as_ref()
        .is_some_and(|owner| owner.claims == 0 && owner.leases == 0 && owner.guards == 0)
    {
        state.owner = None;
    }
}

fn recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use super::{AffinityClaimError, AffinityProof, AffinityStatus, ThreadAffinity};

    #[test]
    fn token_exhaustion_fails_closed_without_aba_reuse() {
        let mut token = u64::MAX;
        assert_eq!(
            super::next_token(&mut token),
            Err(AffinityClaimError::Exhausted)
        );
        assert_eq!(token, u64::MAX);
    }

    #[test]
    fn committed_lease_owns_only_for_its_exact_lifetime() {
        let affinity = ThreadAffinity::default();
        assert_eq!(affinity.status(), AffinityStatus::Unclaimed);
        let lease = affinity
            .begin_claim()
            .expect("initial affinity claim should succeed")
            .commit()
            .expect("initial affinity claim should commit");
        assert_eq!(affinity.status(), AffinityStatus::Owner);

        drop(
            affinity
                .begin_claim()
                .expect("owner reentry should succeed"),
        );
        assert_eq!(affinity.status(), AffinityStatus::Owner);

        drop(lease);
        assert_eq!(affinity.status(), AffinityStatus::Unclaimed);
    }

    #[test]
    fn a_foreign_thread_cannot_take_ownership_while_lease_is_live() {
        let affinity = Arc::new(ThreadAffinity::default());
        let lease = affinity
            .begin_claim()
            .expect("initial affinity claim should succeed")
            .commit()
            .expect("initial affinity claim should commit");
        let foreign = Arc::clone(&affinity);
        let result = std::thread::spawn(move || {
            (
                foreign.status(),
                matches!(foreign.begin_claim(), Err(AffinityClaimError::Foreign)),
            )
        })
        .join()
        .expect("affinity test thread should complete");
        assert_eq!(result, (AffinityStatus::Foreign, true));
        assert_eq!(affinity.status(), AffinityStatus::Owner);
        drop(lease);
    }

    #[test]
    fn failed_initial_publication_releases_for_a_later_thread() {
        let affinity = Arc::new(ThreadAffinity::default());
        let provisional = affinity
            .begin_claim()
            .expect("initial provisional claim should succeed");
        assert_eq!(affinity.status(), AffinityStatus::Owner);

        let blocked = Arc::clone(&affinity);
        assert!(
            std::thread::spawn(move || {
                matches!(blocked.begin_claim(), Err(AffinityClaimError::Foreign))
            })
            .join()
            .expect("blocked affinity test thread should complete")
        );

        drop(provisional);
        assert_eq!(affinity.status(), AffinityStatus::Unclaimed);

        let later = Arc::clone(&affinity);
        let result = std::thread::spawn(move || {
            let claim = later
                .begin_claim()
                .expect("later thread should acquire the abandoned claim");
            let provisional_status = later.status();
            let lease = claim
                .commit()
                .expect("later thread should commit the abandoned claim");
            let committed_status = later.status();
            drop(lease);
            (provisional_status, committed_status, later.status())
        })
        .join()
        .expect("later affinity test thread should complete");
        assert_eq!(
            result,
            (
                AffinityStatus::Owner,
                AffinityStatus::Owner,
                AffinityStatus::Unclaimed
            )
        );
        assert_eq!(affinity.status(), AffinityStatus::Unclaimed);
    }

    #[test]
    fn nested_same_thread_publication_keeps_exact_leases_alive() {
        let affinity = ThreadAffinity::default();
        let outer = affinity
            .begin_claim()
            .expect("outer provisional claim should succeed");
        let inner = affinity
            .begin_claim()
            .expect("same-thread nested claim should succeed");
        let inner_lease = inner
            .commit()
            .expect("nested claim should commit an exact lease");

        drop(outer);
        assert_eq!(affinity.status(), AffinityStatus::Owner);
        let second_lease = affinity
            .begin_claim()
            .expect("committed owner reentry should succeed")
            .commit()
            .expect("committed owner reentry should create another exact lease");
        drop(inner_lease);
        assert_eq!(affinity.status(), AffinityStatus::Owner);
        drop(second_lease);
        assert_eq!(affinity.status(), AffinityStatus::Unclaimed);
    }

    #[test]
    fn replacement_claim_bridges_old_and_new_state_leases() {
        let affinity = ThreadAffinity::default();
        let old_state_lease = affinity
            .begin_claim()
            .expect("old state should claim affinity")
            .commit()
            .expect("old state should commit its lease");
        let replacement = affinity
            .begin_claim()
            .expect("replacement should claim the same owner epoch");

        drop(old_state_lease);
        assert_eq!(affinity.status(), AffinityStatus::Owner);
        let new_state_lease = replacement
            .commit()
            .expect("replacement claim should become the new state lease");
        assert_eq!(affinity.status(), AffinityStatus::Owner);
        drop(new_state_lease);
        assert_eq!(affinity.status(), AffinityStatus::Unclaimed);
    }

    #[test]
    fn lease_keeps_bookkeeping_alive_after_handle_drops_on_foreign_thread() {
        let affinity = ThreadAffinity::default();
        let weak = affinity.downgrade();
        let lease = affinity
            .begin_claim()
            .expect("state should claim affinity")
            .commit()
            .expect("state should commit its lease");

        std::thread::spawn(move || drop(affinity))
            .join()
            .expect("foreign handle drop should complete");
        assert!(weak.is_alive());
        assert!(weak.upgrade().is_some());
        drop(lease);
        assert!(!weak.is_alive());
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn short_lived_owner_thread_exit_allows_another_thread_to_reclaim() {
        let affinity = Arc::new(ThreadAffinity::default());
        let first = Arc::clone(&affinity);
        std::thread::spawn(move || {
            let _lease = first
                .begin_claim()
                .expect("short-lived owner should claim affinity")
                .commit()
                .expect("short-lived owner should commit its lease");
            assert_eq!(first.status(), AffinityStatus::Owner);
        })
        .join()
        .expect("short-lived owner should exit normally");
        assert_eq!(affinity.status(), AffinityStatus::Unclaimed);

        let second = Arc::clone(&affinity);
        let reclaimed = std::thread::spawn(move || {
            let lease = second
                .begin_claim()
                .expect("later thread should reclaim released affinity")
                .commit()
                .expect("later thread should commit reclaimed affinity");
            let status = second.status();
            drop(lease);
            status
        })
        .join()
        .expect("later owner should exit normally");
        assert_eq!(reclaimed, AffinityStatus::Owner);
        assert_eq!(affinity.status(), AffinityStatus::Unclaimed);
    }

    #[test]
    fn newer_present_rotates_a_quiescent_foreign_epoch() {
        let affinity = Arc::new(ThreadAffinity::default());
        let (old_lease, old_guard) = affinity
            .begin_present_claim(AffinityProof::new(3, 10))
            .expect("initial present should claim affinity")
            .commit_guarded()
            .expect("initial present should commit a guarded lease");
        drop(old_guard);

        let migrated = Arc::clone(&affinity);
        assert!(
            std::thread::spawn(move || {
                let (lease, guard) = migrated
                    .begin_present_claim(AffinityProof::new(3, 11))
                    .expect("newer foreign present should rotate a quiescent epoch")
                    .commit_guarded()
                    .expect("migrated present should commit a guarded lease");
                let owned = migrated.status() == AffinityStatus::Owner;
                drop(guard);
                drop(lease);
                owned
            })
            .join()
            .expect("migration thread should finish")
        );

        assert_eq!(affinity.status(), AffinityStatus::Unclaimed);
        let stale = Arc::clone(&affinity);
        assert!(
            std::thread::spawn(move || matches!(
                stale.begin_present_claim(AffinityProof::new(3, 11)),
                Err(AffinityClaimError::StaleProof)
            ))
            .join()
            .expect("stale callback thread should finish")
        );
        let replacement = affinity
            .begin_present_claim(AffinityProof::new(3, 12))
            .expect("later present should claim a fresh epoch")
            .commit()
            .expect("later present should commit its lease");
        assert!(!old_lease.shares_epoch(&replacement));
        drop(old_lease);
        assert_eq!(affinity.status(), AffinityStatus::Owner);
        drop(replacement);
        assert_eq!(affinity.status(), AffinityStatus::Unclaimed);
    }

    #[test]
    fn live_guard_defers_a_newer_foreign_present() {
        let affinity = Arc::new(ThreadAffinity::default());
        let (old_lease, old_guard) = affinity
            .begin_present_claim(AffinityProof::new(5, 20))
            .expect("initial present should claim affinity")
            .commit_guarded()
            .expect("initial present should commit a guarded lease");

        let blocked = Arc::clone(&affinity);
        assert!(
            std::thread::spawn(move || matches!(
                blocked.begin_present_claim(AffinityProof::new(5, 21)),
                Err(AffinityClaimError::Busy)
            ))
            .join()
            .expect("blocked migration thread should finish")
        );

        drop(old_guard);
        let migrated = Arc::clone(&affinity);
        assert!(
            std::thread::spawn(move || {
                let Ok((lease, guard)) = migrated
                    .begin_present_claim(AffinityProof::new(5, 22))
                    .and_then(|claim| claim.commit_guarded())
                else {
                    return false;
                };
                drop(guard);
                drop(lease);
                true
            })
            .join()
            .expect("later migration thread should finish")
        );
        drop(old_lease);
        assert_eq!(affinity.status(), AffinityStatus::Unclaimed);
    }

    #[test]
    fn live_provisional_claim_defers_handoff() {
        let affinity = Arc::new(ThreadAffinity::default());
        let provisional = affinity
            .begin_present_claim(AffinityProof::new(7, 30))
            .expect("initial present should claim affinity");

        let blocked = Arc::clone(&affinity);
        assert!(
            std::thread::spawn(move || matches!(
                blocked.begin_present_claim(AffinityProof::new(7, 31)),
                Err(AffinityClaimError::Busy)
            ))
            .join()
            .expect("blocked handoff thread should finish")
        );
        drop(provisional);
        assert_eq!(affinity.status(), AffinityStatus::Unclaimed);
    }

    #[test]
    fn stale_or_regressed_proof_cannot_rotate_an_epoch() {
        let affinity = Arc::new(ThreadAffinity::default());
        let lease = affinity
            .begin_present_claim(AffinityProof::new(9, 40))
            .expect("initial present should claim affinity")
            .commit()
            .expect("initial present should commit its lease");

        for proof in [
            AffinityProof::new(9, 40),
            AffinityProof::new(9, 39),
            AffinityProof::new(8, 41),
        ] {
            let stale = Arc::clone(&affinity);
            assert!(
                std::thread::spawn(move || matches!(
                    stale.begin_present_claim(proof),
                    Err(AffinityClaimError::StaleProof)
                ))
                .join()
                .expect("stale proof thread should finish")
            );
        }

        assert_eq!(affinity.status(), AffinityStatus::Owner);
        drop(lease);
    }

    #[test]
    fn concurrent_claims_have_one_exact_owner() {
        let affinity = Arc::new(ThreadAffinity::default());
        let start = Arc::new(Barrier::new(3));
        let finish = Arc::new(Barrier::new(3));
        let mut threads = Vec::new();
        for _ in 0..2 {
            let affinity = Arc::clone(&affinity);
            let start = Arc::clone(&start);
            let finish = Arc::clone(&finish);
            threads.push(std::thread::spawn(move || {
                start.wait();
                let lease = affinity
                    .begin_claim()
                    .ok()
                    .and_then(|claim| claim.commit().ok());
                finish.wait();
                lease.is_some()
            }));
        }

        start.wait();
        finish.wait();
        let winners = threads
            .into_iter()
            .map(|thread| thread.join().expect("claiming thread should complete"))
            .filter(|won| *won)
            .count();
        assert_eq!(winners, 1);
        assert_eq!(affinity.status(), AffinityStatus::Unclaimed);
    }
}
