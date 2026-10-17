use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

type Registry<K> = Mutex<HashMap<K, Weak<KeyedGate<K>>>>;

/// Gates are shared by callers and live only while callers hold them.
/// Do not evict live gates: that would let a second initializer bypass waiters.
#[derive(Clone)]
pub(crate) struct KeyedGates<K: Clone + Eq + Hash> {
    registry: Arc<Registry<K>>,
}

impl<K: Clone + Eq + Hash> Default for KeyedGates<K> {
    fn default() -> Self {
        Self {
            registry: Arc::default(),
        }
    }
}

struct KeyedGate<K: Clone + Eq + Hash> {
    mutex: tokio::sync::Mutex<()>,
    key: K,
    registry: Weak<Registry<K>>,
}

impl<K: Clone + Eq + Hash> KeyedGates<K> {
    /// Bound the complete operation, including gate wait. Timing out drops the
    /// work future and releases the gate; the registry never evicts a live gate.
    pub(crate) async fn run<T>(
        &self,
        key: K,
        deadline: Duration,
        work: impl Future<Output = T>,
    ) -> Result<T, tokio::time::error::Elapsed> {
        tokio::time::timeout(deadline, async {
            let gate = self.get(key);
            let _guard = gate.mutex.lock().await;
            work.await
        })
        .await
    }

    fn get(&self, key: K) -> Arc<KeyedGate<K>> {
        let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = registry.get(&key).and_then(Weak::upgrade) {
            return entry;
        }
        let entry = Arc::new(KeyedGate {
            mutex: tokio::sync::Mutex::new(()),
            key: key.clone(),
            registry: Arc::downgrade(&self.registry),
        });
        registry.insert(key, Arc::downgrade(&entry));
        entry
    }
}

impl<K: Clone + Eq + Hash> Drop for KeyedGate<K> {
    fn drop(&mut self) {
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        let mut registry = registry.lock().unwrap_or_else(|e| e.into_inner());
        // A caller can replace an expired Weak while this destructor waits for
        // the registry lock. Never remove that caller's replacement gate.
        if registry
            .get(&self.key)
            .is_some_and(|entry| std::ptr::eq(entry.as_ptr(), self))
        {
            registry.remove(&self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{KeyedGate, KeyedGates};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn operation_timeout_releases_the_gate_and_allows_retry() {
        let gates = KeyedGates::default();
        assert!(gates
            .run(
                "key",
                Duration::from_millis(1),
                std::future::pending::<()>()
            )
            .await
            .is_err());
        assert!(gates.registry.lock().unwrap().is_empty());
        assert_eq!(
            gates
                .run("key", Duration::from_secs(1), async { 42 })
                .await
                .unwrap(),
            42
        );
    }

    #[tokio::test]
    async fn waiter_timeout_does_not_run_work_or_replace_an_active_gate() {
        let gates = KeyedGates::default();
        let gate = gates.get("key");
        let guard = gate.mutex.lock().await;
        let ran = std::sync::atomic::AtomicBool::new(false);
        let result = gates
            .run("key", Duration::from_millis(1), async {
                ran.store(true, std::sync::atomic::Ordering::Relaxed);
            })
            .await;
        assert!(result.is_err());
        assert!(!ran.load(std::sync::atomic::Ordering::Relaxed));
        assert!(Arc::ptr_eq(&gate, &gates.get("key")));
        assert!(gate.mutex.try_lock().is_err());
        drop(guard);
        drop(gate);
        assert!(gates.registry.lock().unwrap().is_empty());
    }

    #[test]
    fn clones_share_live_gates_and_reclaim_idle_entries() {
        let locks = KeyedGates::default();
        let first = locks.get("same-user".to_owned());
        let second = locks.clone().get("same-user".to_owned());
        assert!(Arc::ptr_eq(&first, &second));
        drop(first);
        assert_eq!(locks.registry.lock().unwrap().len(), 1);
        drop(second);
        assert!(locks.registry.lock().unwrap().is_empty());

        for index in 0..100 {
            drop(locks.get(format!("one-time-user-{index}")));
        }
        assert!(locks.registry.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn different_users_do_not_share_an_initialization_gate() {
        let locks = KeyedGates::default();
        let first = locks.get("first-user".to_owned());
        let _guard = first.mutex.lock().await;
        let second = locks.get("second-user".to_owned());
        assert!(second.mutex.try_lock().is_ok());
        assert!(locks.get("first-user".to_owned()).mutex.try_lock().is_err());
    }

    #[tokio::test]
    async fn cancelling_a_waiter_preserves_the_active_gate() {
        let locks = KeyedGates::default();
        let first = locks.get("same-user".to_owned());
        let guard = first.mutex.lock().await;
        let shared = locks.clone();
        let (started, waiting) = oneshot::channel();
        let waiter = tokio::spawn(async move {
            let entry = shared.get("same-user".to_owned());
            started.send(()).unwrap();
            let _guard = entry.mutex.lock().await;
        });
        waiting.await.unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert!(Arc::ptr_eq(&first, &locks.get("same-user".to_owned())));
        drop(guard);
        drop(first);
        assert!(locks.registry.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn cancelling_an_initializer_releases_its_waiters() {
        let locks = KeyedGates::default();
        let shared = locks.clone();
        let (entered, initialized) = oneshot::channel();
        let initializer = tokio::spawn(async move {
            let entry = shared.get("same-user".to_owned());
            let _guard = entry.mutex.lock().await;
            entered.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        initialized.await.unwrap();
        let shared = locks.clone();
        let (started, waiting) = oneshot::channel();
        let waiter = tokio::spawn(async move {
            let entry = shared.get("same-user".to_owned());
            started.send(()).unwrap();
            let _guard = entry.mutex.lock().await;
        });
        waiting.await.unwrap();
        initializer.abort();
        assert!(initializer.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("waiter must not be stranded")
            .unwrap();
        assert!(locks.registry.lock().unwrap().is_empty());
    }

    #[test]
    fn expired_gate_cleanup_does_not_remove_a_replacement() {
        let locks = KeyedGates::default();
        let old = locks.get("same-user".to_owned());
        // Model replacement after the final strong reference expires but before
        // its destructor obtains the registry mutex.
        let replacement = Arc::new(KeyedGate {
            mutex: tokio::sync::Mutex::new(()),
            key: "same-user".into(),
            registry: Arc::downgrade(&locks.registry),
        });
        locks
            .registry
            .lock()
            .unwrap()
            .insert("same-user".into(), Arc::downgrade(&replacement));
        drop(old);
        assert!(Arc::ptr_eq(
            &replacement,
            &locks.get("same-user".to_owned())
        ));
        drop(replacement);
        assert!(locks.registry.lock().unwrap().is_empty());
    }

    #[test]
    fn gate_can_outlive_the_registry() {
        let locks = KeyedGates::default();
        let entry = locks.get("same-user".to_owned());
        drop(locks);
        drop(entry);
    }
}
