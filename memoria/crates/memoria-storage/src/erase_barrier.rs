//! In-process gate that keeps an erase from overlapping accepted writes.
//!
//! `seal` rejects new work for one scope and waits until work that already
//! entered has finished. Snapshot creation is global: an account snapshot can
//! copy every scope, so it waits on and is rejected by any sealed erase.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

#[derive(Debug)]
struct ScopeSlot {
    sealed: bool,
    inflight: usize,
}

#[derive(Debug)]
struct BarrierState {
    scopes: HashMap<String, ScopeSlot>,
    snapshot_inflight: usize,
}

#[derive(Debug)]
pub struct EraseBarrier {
    state: Mutex<BarrierState>,
    notify: Notify,
}

impl Default for EraseBarrier {
    fn default() -> Self {
        Self::new()
    }
}

impl EraseBarrier {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(BarrierState {
                scopes: HashMap::new(),
                snapshot_inflight: 0,
            }),
            notify: Notify::new(),
        }
    }

    pub fn try_enter(self: &Arc<Self>, scope: &str) -> Option<WritePermit> {
        let mut state = self.state.lock().expect("erase barrier");
        let slot = state.scopes.entry(scope.to_string()).or_insert(ScopeSlot {
            sealed: false,
            inflight: 0,
        });
        if slot.sealed {
            return None;
        }
        slot.inflight += 1;
        Some(WritePermit {
            barrier: Arc::clone(self),
            kind: PermitKind::Scope(scope.to_string()),
        })
    }

    pub fn try_enter_snapshot(self: &Arc<Self>) -> Option<WritePermit> {
        let mut state = self.state.lock().expect("erase barrier");
        if state.scopes.values().any(|slot| slot.sealed) {
            return None;
        }
        state.snapshot_inflight += 1;
        Some(WritePermit {
            barrier: Arc::clone(self),
            kind: PermitKind::Snapshot,
        })
    }

    pub async fn seal(&self, scope: &str) {
        {
            let mut state = self.state.lock().expect("erase barrier");
            state
                .scopes
                .entry(scope.to_string())
                .or_insert(ScopeSlot {
                    sealed: false,
                    inflight: 0,
                })
                .sealed = true;
        }
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            let idle = {
                let state = self.state.lock().expect("erase barrier");
                let inflight = state
                    .scopes
                    .get(scope)
                    .map(|slot| slot.inflight)
                    .unwrap_or(0);
                inflight == 0 && state.snapshot_inflight == 0
            };
            if idle {
                return;
            }
            notified.await;
        }
    }

    pub fn unseal(&self, scope: &str) {
        let mut state = self.state.lock().expect("erase barrier");
        if let Some(slot) = state.scopes.get_mut(scope) {
            slot.sealed = false;
        }
        drop(state);
        self.notify.notify_waiters();
    }

    fn leave(&self, kind: &PermitKind) {
        let mut state = self.state.lock().expect("erase barrier");
        match kind {
            PermitKind::Scope(scope) => {
                if let Some(slot) = state.scopes.get_mut(scope) {
                    slot.inflight = slot.inflight.saturating_sub(1);
                }
            }
            PermitKind::Snapshot => {
                state.snapshot_inflight = state.snapshot_inflight.saturating_sub(1);
            }
        }
        drop(state);
        self.notify.notify_waiters();
    }
}

enum PermitKind {
    Scope(String),
    Snapshot,
}

pub struct WritePermit {
    barrier: Arc<EraseBarrier>,
    kind: PermitKind,
}

impl Drop for WritePermit {
    fn drop(&mut self) {
        self.barrier.leave(&self.kind);
    }
}

pub struct SealGuard {
    barrier: Arc<EraseBarrier>,
    scope: String,
}

impl SealGuard {
    /// Own the seal before its first cancellation point. The caller must hold
    /// the guard until all already-started erase statements have finished.
    pub async fn acquire(barrier: Arc<EraseBarrier>, scope: impl Into<String>) -> Self {
        let guard = Self {
            barrier,
            scope: scope.into(),
        };
        guard.barrier.seal(&guard.scope).await;
        guard
    }
}

impl Drop for SealGuard {
    fn drop(&mut self) {
        self.barrier.unseal(&self.scope);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn cancelled_seal_wait_reopens_writes_and_snapshots() {
        for snapshot in [false, true] {
            let barrier = Arc::new(EraseBarrier::new());
            let permit = if snapshot {
                barrier.try_enter_snapshot().unwrap()
            } else {
                barrier.try_enter("grp").unwrap()
            };
            let mut waiting = Box::pin(SealGuard::acquire(barrier.clone(), "grp"));
            // Poll to the pending drain without relying on scheduler timing.
            assert!(
                tokio::time::timeout(Duration::from_millis(10), &mut waiting)
                    .await
                    .is_err()
            );
            assert!(barrier.try_enter("grp").is_none());
            drop(waiting);
            drop(permit);
            assert!(barrier.try_enter("grp").is_some());
            assert!(barrier.try_enter_snapshot().is_some());
        }
    }

    #[tokio::test]
    async fn cancelled_caller_does_not_unseal_a_detached_statement() {
        let barrier = Arc::new(EraseBarrier::new());
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let caller_barrier = barrier.clone();
        let caller = tokio::spawn(async move {
            let seal = SealGuard::acquire(caller_barrier, "grp").await;
            let (tx, rx) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                let _seal = seal;
                ready_tx.send(()).unwrap();
                release_rx.await.unwrap();
                drop(_seal);
                let _ = tx.send(());
                done_tx.send(()).unwrap();
            });
            let _ = rx.await;
        });
        ready_rx.await.unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        assert!(barrier.try_enter("grp").is_none());
        assert!(barrier.try_enter_snapshot().is_none());
        release_tx.send(()).unwrap();
        done_rx.await.unwrap();
        assert!(barrier.try_enter("grp").is_some());
        assert!(barrier.try_enter_snapshot().is_some());
    }

    #[tokio::test]
    async fn seal_waits_for_an_accepted_write_then_rejects_the_next_one() {
        let barrier = Arc::new(EraseBarrier::new());
        let permit = barrier.try_enter("grp").expect("accepted");
        let waiting = {
            let barrier = Arc::clone(&barrier);
            tokio::spawn(async move { barrier.seal("grp").await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiting.is_finished());
        assert!(barrier.try_enter("grp").is_none());
        drop(permit);
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("seal timed out")
            .expect("seal task");
        assert!(barrier.try_enter("grp").is_none());
        barrier.unseal("grp");
        assert!(barrier.try_enter("grp").is_some());
    }

    #[tokio::test]
    async fn seal_waits_for_an_in_flight_snapshot() {
        let barrier = Arc::new(EraseBarrier::new());
        let snapshot = barrier.try_enter_snapshot().expect("snapshot");
        let waiting = {
            let barrier = Arc::clone(&barrier);
            tokio::spawn(async move { barrier.seal("grp").await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiting.is_finished());
        drop(snapshot);
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("seal timed out")
            .expect("seal task");
        assert!(barrier.try_enter_snapshot().is_none());
    }

    #[tokio::test]
    async fn a_sealed_scope_does_not_block_another_scope() {
        let barrier = Arc::new(EraseBarrier::new());
        barrier.seal("grp-a").await;
        assert!(barrier.try_enter("grp-b").is_some());
        assert!(barrier.try_enter("grp-a").is_none());
    }
}
