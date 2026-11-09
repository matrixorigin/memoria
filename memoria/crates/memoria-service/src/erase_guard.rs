//! Operation-scoped lock for a physical memory erase.
//!
//! A failed renewal stops later destructive steps. The statement already sent
//! keeps the lock row until it returns, so a second erase cannot acquire that
//! lock while the statement is still running. Renewal and release both require
//! this operation's holder.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[cfg(test)]
use std::sync::atomic::AtomicUsize;
#[cfg(test)]
use std::sync::Mutex;
#[cfg(test)]
use std::time::Instant;

use memoria_core::MemoriaError;
use memoria_storage::{EraseStatementHold, MemoryEraseReport, SqlMemoryStore};
#[cfg(test)]
use memoria_storage::EraseStepGate;
use tokio::task::JoinHandle;

const LOCK_TTL: Duration = Duration::from_secs(30);
const RENEW_EVERY: Duration = Duration::from_secs(10);

pub fn lock_lost_error() -> MemoriaError {
    MemoriaError::Internal("memory erase lost its lock before completion".into())
}

pub trait EraseLock: Send + Sync + 'static {
    fn try_acquire(
        &self,
        key: &str,
        holder: &str,
        ttl: Duration,
    ) -> impl Future<Output = Result<bool, MemoriaError>> + Send;
    fn renew(
        &self,
        key: &str,
        holder: &str,
        ttl: Duration,
    ) -> impl Future<Output = Result<bool, MemoriaError>> + Send;
    fn release(
        &self,
        key: &str,
        holder: &str,
    ) -> impl Future<Output = Result<bool, MemoriaError>> + Send;
    fn held_by(
        &self,
        key: &str,
        holder: &str,
    ) -> impl Future<Output = Result<bool, MemoriaError>> + Send;
    /// Poll `action` only while `holder` still owns `key`, and keep that
    /// ownership until `action` returns.
    fn run_exclusive<'a, F>(
        &'a self,
        key: &'a str,
        holder: &'a str,
        ttl: Duration,
        action: F,
    ) -> impl Future<Output = Result<u64, MemoriaError>> + Send + 'a
    where
        F: Future<Output = Result<u64, MemoriaError>> + Send + 'a;
}

impl EraseLock for SqlMemoryStore {
    async fn try_acquire(
        &self,
        key: &str,
        holder: &str,
        ttl: Duration,
    ) -> Result<bool, MemoriaError> {
        self.try_acquire_lock_holder(key, holder, ttl_secs(ttl)).await
    }

    async fn renew(&self, key: &str, holder: &str, ttl: Duration) -> Result<bool, MemoriaError> {
        self.renew_lock_holder(key, holder, ttl_secs(ttl)).await
    }

    async fn release(&self, key: &str, holder: &str) -> Result<bool, MemoriaError> {
        self.release_lock_holder(key, holder).await
    }

    async fn held_by(&self, key: &str, holder: &str) -> Result<bool, MemoriaError> {
        self.lock_held_by(key, holder).await
    }

    async fn run_exclusive<'a, F>(
        &'a self,
        key: &'a str,
        holder: &'a str,
        ttl: Duration,
        action: F,
    ) -> Result<u64, MemoriaError>
    where
        F: Future<Output = Result<u64, MemoriaError>> + Send + 'a,
    {
        self.run_with_holder_lock(key, holder, ttl_secs(ttl), action)
            .await
    }
}

fn ttl_secs(ttl: Duration) -> i64 {
    ttl.as_secs().max(1) as i64
}

pub struct GateState<L: EraseLock> {
    backend: Arc<L>,
    key: String,
    holder: String,
    stop_steps: Arc<AtomicBool>,
    renew_stop: Arc<AtomicBool>,
    lost: Arc<AtomicBool>,
    released: Arc<AtomicBool>,
    ttl: Duration,
}

impl<L: EraseLock> Clone for GateState<L> {
    fn clone(&self) -> Self {
        Self {
            backend: Arc::clone(&self.backend),
            key: self.key.clone(),
            holder: self.holder.clone(),
            stop_steps: Arc::clone(&self.stop_steps),
            renew_stop: Arc::clone(&self.renew_stop),
            lost: Arc::clone(&self.lost),
            released: Arc::clone(&self.released),
            ttl: self.ttl,
        }
    }
}

impl<L: EraseLock> EraseStatementHold for GateState<L> {
    fn run_step<F>(
        &self,
        action: F,
    ) -> impl Future<Output = Result<u64, MemoriaError>> + Send
    where
        F: Future<Output = Result<u64, MemoriaError>> + Send + 'static,
    {
        locked_step(self.clone(), action)
    }
}

async fn locked_step<L: EraseLock>(
    gate: GateState<L>,
    action: impl Future<Output = Result<u64, MemoriaError>> + Send + 'static,
) -> Result<u64, MemoriaError> {
    if gate.stop_steps.load(Ordering::Relaxed) || gate.lost.load(Ordering::Relaxed) {
        gate.lost.store(true, Ordering::Relaxed);
        return Err(lock_lost_error());
    }
    match gate
        .backend
        .run_exclusive(&gate.key, &gate.holder, gate.ttl, action)
        .await
    {
        Err(error) if error.to_string().contains("lost its lock") => {
            gate.lost.store(true, Ordering::Relaxed);
            Err(error)
        }
        other => other,
    }
}

impl<L: EraseLock> GateState<L> {
    pub fn holder(&self) -> &str {
        &self.holder
    }

    pub fn lost(&self) -> bool {
        self.lost.load(Ordering::Relaxed)
    }

    /// Refuse another destructive step when this operation no longer owns the lock.
    pub async fn checkpoint(&self) -> Result<(), MemoriaError> {
        if self.stop_steps.load(Ordering::Relaxed) || self.lost.load(Ordering::Relaxed) {
            self.lost.store(true, Ordering::Relaxed);
            return Err(lock_lost_error());
        }
        match self.backend.held_by(&self.key, &self.holder).await {
            Ok(true) => Ok(()),
            Ok(false) => {
                self.lost.store(true, Ordering::Relaxed);
                Err(lock_lost_error())
            }
            Err(error) => {
                tracing::error!(%error, "memory erase could not confirm its lock");
                self.lost.store(true, Ordering::Relaxed);
                Err(lock_lost_error())
            }
        }
    }

    /// Stop renewal and release only this holder. The released flag is set after
    /// the release call returns, so cancelling this future still lets the lease
    /// drop release the same holder.
    pub async fn finish(&self) -> Result<bool, MemoriaError> {
        self.stop_steps.store(true, Ordering::Relaxed);
        self.renew_stop.store(true, Ordering::Relaxed);
        if self.released.load(Ordering::Relaxed) {
            return Ok(true);
        }
        let released = self.backend.release(&self.key, &self.holder).await?;
        self.released.store(true, Ordering::Relaxed);
        Ok(released)
    }
}

pub struct RenewLease<L: EraseLock> {
    renew: JoinHandle<()>,
    renew_stop: Arc<AtomicBool>,
    released: Arc<AtomicBool>,
    backend: Arc<L>,
    key: String,
    holder: String,
}

impl<L: EraseLock> RenewLease<L> {
    pub fn shutdown_renew(&self) {
        self.renew_stop.store(true, Ordering::Relaxed);
        self.renew.abort();
    }
}

impl<L: EraseLock> Drop for RenewLease<L> {
    fn drop(&mut self) {
        self.shutdown_renew();
        if self.released.swap(true, Ordering::Relaxed) {
            return;
        }
        let backend = Arc::clone(&self.backend);
        let key = self.key.clone();
        let holder = self.holder.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Err(error) = backend.release(&key, &holder).await {
                    tracing::error!(%error, "failed to release memory erase lock");
                }
            });
        }
    }
}

/// Sets the step-stop flag when the caller leaves, including cancellation.
/// It does not abort renewal: the detached erase still owns the lock until the
/// statement already sent to the database returns and the lease is dropped.
pub struct CancelGuard {
    stop_steps: Arc<AtomicBool>,
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        self.stop_steps.store(true, Ordering::Relaxed);
    }
}

pub struct EraseSession<L: EraseLock> {
    gate: GateState<L>,
    lease: Option<RenewLease<L>>,
}

impl<L: EraseLock> Drop for EraseSession<L> {
    fn drop(&mut self) {
        if self.lease.is_some() {
            self.gate.stop_steps.store(true, Ordering::Relaxed);
            self.gate.renew_stop.store(true, Ordering::Relaxed);
        }
    }
}

impl<L: EraseLock> EraseSession<L> {
    pub async fn start(backend: Arc<L>, key: String) -> Result<Self, MemoriaError> {
        Self::start_with(backend, key, LOCK_TTL, RENEW_EVERY).await
    }

    pub async fn start_with(
        backend: Arc<L>,
        key: String,
        ttl: Duration,
        renew_every: Duration,
    ) -> Result<Self, MemoriaError> {
        let holder = uuid::Uuid::new_v4().simple().to_string();
        if !backend.try_acquire(&key, &holder, ttl).await? {
            return Err(MemoriaError::Blocked(
                "memory erase is already running for this scope".into(),
            ));
        }
        let stop_steps = Arc::new(AtomicBool::new(false));
        let renew_stop = Arc::new(AtomicBool::new(false));
        let lost = Arc::new(AtomicBool::new(false));
        let released = Arc::new(AtomicBool::new(false));
        let renew = spawn_renew(
            Arc::clone(&backend),
            key.clone(),
            holder.clone(),
            ttl,
            renew_every,
            Arc::clone(&renew_stop),
            Arc::clone(&lost),
        );
        let gate = GateState {
            backend: Arc::clone(&backend),
            key: key.clone(),
            holder: holder.clone(),
            stop_steps,
            renew_stop: Arc::clone(&renew_stop),
            lost,
            released: Arc::clone(&released),
            ttl,
        };
        Ok(Self {
            gate,
            lease: Some(RenewLease {
                renew,
                renew_stop,
                released,
                backend,
                key,
                holder,
            }),
        })
    }

    pub fn gate(&self) -> GateState<L> {
        self.gate.clone()
    }

    pub async fn finish(&self) -> Result<bool, MemoriaError> {
        self.gate.finish().await
    }

    /// Move renewal into the caller that waits for the in-flight statement.
    /// The returned guard only stops later steps when the parent is dropped.
    pub fn handoff(mut self) -> (CancelGuard, RenewLease<L>, GateState<L>) {
        let lease = self.lease.take().expect("erase lock lease");
        let guard = CancelGuard {
            stop_steps: Arc::clone(&self.gate.stop_steps),
        };
        let gate = self.gate.clone();
        (guard, lease, gate)
    }
}

fn spawn_renew<L: EraseLock>(
    backend: Arc<L>,
    key: String,
    holder: String,
    ttl: Duration,
    renew_every: Duration,
    renew_stop: Arc<AtomicBool>,
    lost: Arc<AtomicBool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(renew_every);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if renew_stop.load(Ordering::Relaxed) {
                break;
            }
            match backend.renew(&key, &holder, ttl).await {
                Ok(true) => {}
                Ok(false) => {
                    lost.store(true, Ordering::Relaxed);
                    break;
                }
                Err(error) => {
                    tracing::error!(%error, "memory erase lock renewal failed");
                    lost.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
    })
}

/// Release this holder after the erase attempt. A lost lock never becomes success.
pub async fn conclude_erase<L: EraseLock>(
    result: Result<MemoryEraseReport, MemoriaError>,
    gate: &GateState<L>,
) -> Result<MemoryEraseReport, MemoriaError> {
    let lost_during = gate.lost();
    let release_failed = match gate.finish().await {
        Ok(true) => false,
        Ok(false) => true,
        Err(error) => {
            tracing::error!(%error, "failed to release memory erase lock");
            true
        }
    };
    match result {
        Ok(_) if lost_during || gate.lost() || release_failed => Err(lock_lost_error()),
        Ok(report) => Ok(report),
        Err(_) if lost_during || gate.lost() => Err(lock_lost_error()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
enum RenewBehavior {
    Extend,
    Reject,
    Fail,
}

#[cfg(test)]
struct LockRow {
    holder: String,
    expires: Instant,
    statement_open: bool,
}

#[cfg(test)]
struct MemoryLock {
    rows: Mutex<std::collections::HashMap<String, LockRow>>,
    renew: Mutex<RenewBehavior>,
    renew_calls: AtomicUsize,
}

#[cfg(test)]
impl MemoryLock {
    fn new(renew: RenewBehavior) -> Self {
        Self {
            rows: Mutex::new(std::collections::HashMap::new()),
            renew: Mutex::new(renew),
            renew_calls: AtomicUsize::new(0),
        }
    }

    fn renew_calls(&self) -> usize {
        self.renew_calls.load(Ordering::SeqCst)
    }

    fn expire(&self, key: &str) {
        if let Some(row) = self.rows.lock().expect("lock").get_mut(key) {
            row.expires = Instant::now() - Duration::from_secs(1);
        }
    }

    fn set_renew(&self, behavior: RenewBehavior) {
        *self.renew.lock().expect("lock") = behavior;
    }
}

#[cfg(test)]
impl EraseLock for MemoryLock {
    async fn try_acquire(
        &self,
        key: &str,
        holder: &str,
        ttl: Duration,
    ) -> Result<bool, MemoriaError> {
        let mut rows = self.rows.lock().expect("lock");
        if rows
            .get(key)
            .is_some_and(|row| row.statement_open || row.expires > Instant::now())
        {
            return Ok(false);
        }
        rows.insert(
            key.to_string(),
            LockRow {
                holder: holder.to_string(),
                expires: Instant::now() + ttl,
                statement_open: false,
            },
        );
        Ok(true)
    }

    async fn run_exclusive<'a, F>(
        &'a self,
        key: &'a str,
        holder: &'a str,
        ttl: Duration,
        action: F,
    ) -> Result<u64, MemoriaError>
    where
        F: Future<Output = Result<u64, MemoriaError>> + Send + 'a,
    {
        {
            let mut rows = self.rows.lock().expect("lock");
            match rows.get_mut(key) {
                Some(row) if row.holder == holder && !row.statement_open => {
                    row.statement_open = true;
                }
                _ => return Err(lock_lost_error()),
            }
        }
        let result = action.await;
        let mut rows = self.rows.lock().expect("lock");
        if let Some(row) = rows.get_mut(key) {
            if row.holder == holder {
                row.statement_open = false;
                row.expires = Instant::now() + ttl;
            }
        }
        result
    }

    async fn renew(&self, key: &str, holder: &str, ttl: Duration) -> Result<bool, MemoriaError> {
        self.renew_calls.fetch_add(1, Ordering::SeqCst);
        let behavior = *self.renew.lock().expect("lock");
        match behavior {
            RenewBehavior::Fail => {
                Err(MemoriaError::Database("memory erase lock renewal failed".into()))
            }
            RenewBehavior::Reject => {
                let mut rows = self.rows.lock().expect("lock");
                match rows.get(key) {
                    Some(row) if row.holder == holder && row.statement_open => Ok(false),
                    Some(row) if row.holder == holder => {
                        rows.remove(key);
                        Ok(false)
                    }
                    _ => Ok(false),
                }
            }
            RenewBehavior::Extend => {
                let mut rows = self.rows.lock().expect("lock");
                match rows.get_mut(key) {
                    Some(row) if row.holder == holder && row.expires > Instant::now() => {
                        row.expires = Instant::now() + ttl;
                        Ok(true)
                    }
                    _ => Ok(false),
                }
            }
        }
    }

    async fn release(&self, key: &str, holder: &str) -> Result<bool, MemoriaError> {
        let mut rows = self.rows.lock().expect("lock");
        match rows.get(key) {
            Some(row) if row.holder == holder && row.statement_open => Ok(false),
            Some(row) if row.holder == holder => {
                rows.remove(key);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn held_by(&self, key: &str, holder: &str) -> Result<bool, MemoriaError> {
        let rows = self.rows.lock().expect("lock");
        Ok(rows.get(key).is_some_and(|row| {
            row.holder == holder && (row.statement_open || row.expires > Instant::now())
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn short_session(
        lock: Arc<MemoryLock>,
    ) -> impl Future<Output = Result<EraseSession<MemoryLock>, MemoriaError>> {
        EraseSession::start_with(
            lock,
            "memory-erase:scope".into(),
            Duration::from_millis(80),
            Duration::from_millis(20),
        )
    }

    async fn wait_until_lost(gate: &GateState<MemoryLock>) {
        for _ in 0..50 {
            if gate.lost() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("lock loss was not recorded");
    }

    #[tokio::test]
    async fn renew_false_does_not_start_the_next_step() {
        let lock = Arc::new(MemoryLock::new(RenewBehavior::Reject));
        let session = short_session(Arc::clone(&lock)).await.expect("acquired");
        let gate = session.gate();
        let mut started = Vec::new();
        gate.checkpoint().await.expect("first step still holds the lock");
        started.push(1);
        wait_until_lost(&gate).await;
        let next = gate.checkpoint().await;
        assert!(next.is_err());
        assert!(next.unwrap_err().to_string().contains("lock"));
        assert_eq!(started, vec![1]);
        drop(session);
    }

    #[tokio::test]
    async fn renew_error_does_not_start_the_next_step() {
        let lock = Arc::new(MemoryLock::new(RenewBehavior::Fail));
        let session = short_session(Arc::clone(&lock)).await.expect("acquired");
        let gate = session.gate();
        let mut started = Vec::new();
        gate.checkpoint().await.expect("first step");
        started.push(1);
        wait_until_lost(&gate).await;
        assert!(gate.checkpoint().await.is_err());
        assert_eq!(started, vec![1]);
        drop(session);
    }

    #[tokio::test]
    async fn active_renewal_keeps_a_second_request_out() {
        let lock = Arc::new(MemoryLock::new(RenewBehavior::Extend));
        let session = EraseSession::start_with(
            Arc::clone(&lock),
            "memory-erase:scope".into(),
            Duration::from_millis(200),
            Duration::from_millis(20),
        )
        .await
        .expect("acquired");
        tokio::time::sleep(Duration::from_millis(350)).await;
        let second = EraseSession::start_with(
            Arc::clone(&lock),
            "memory-erase:scope".into(),
            Duration::from_millis(200),
            Duration::from_millis(20),
        )
        .await;
        assert!(matches!(second, Err(MemoriaError::Blocked(_))));
        session.gate().checkpoint().await.expect("holder still owns the lock");
        session.finish().await.expect("release");
        drop(session);
    }

    #[tokio::test]
    async fn old_holder_cannot_change_the_new_lock() {
        let lock = Arc::new(MemoryLock::new(RenewBehavior::Extend));
        let first = short_session(Arc::clone(&lock)).await.expect("first");
        let old_holder = first.gate().holder().to_string();
        first.finish().await.expect("release old");
        drop(first);
        let second = short_session(Arc::clone(&lock)).await.expect("second");
        let new_holder = second.gate().holder().to_string();
        assert_ne!(old_holder, new_holder);
        assert!(!lock
            .renew("memory-erase:scope", &old_holder, Duration::from_secs(30))
            .await
            .expect("old renew"));
        assert!(!lock
            .release("memory-erase:scope", &old_holder)
            .await
            .expect("old release"));
        assert!(lock
            .held_by("memory-erase:scope", &new_holder)
            .await
            .expect("new holder"));
        second.finish().await.expect("release new");
        drop(second);
    }

    #[tokio::test]
    async fn dropping_the_session_stops_renewal() {
        let lock = Arc::new(MemoryLock::new(RenewBehavior::Extend));
        let session = short_session(Arc::clone(&lock)).await.expect("acquired");
        for _ in 0..50 {
            if lock.renew_calls() > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(lock.renew_calls() > 0);
        drop(session);
        tokio::time::sleep(Duration::from_millis(30)).await;
        let calls = lock.renew_calls();
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(lock.renew_calls(), calls);
        let mut released = false;
        for _ in 0..20 {
            if lock
                .try_acquire("memory-erase:scope", "later", Duration::from_millis(80))
                .await
                .expect("acquire after drop")
            {
                released = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(released);
    }

    #[tokio::test]
    async fn cancelled_parent_does_not_leave_a_renew_task() {
        let lock = Arc::new(MemoryLock::new(RenewBehavior::Extend));
        let session = short_session(Arc::clone(&lock)).await.expect("acquired");
        let (cancel, lease, gate) = session.handoff();
        gate.checkpoint().await.expect("statement already admitted");
        drop(cancel);
        let next = gate.checkpoint().await;
        assert!(next.unwrap_err().to_string().contains("lock"));
        let calls = lock.renew_calls();
        lease.shutdown_renew();
        drop(lease);
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(lock.renew_calls(), calls);
        let _ = gate.finish().await;
    }

    #[tokio::test]
    async fn in_flight_statement_keeps_the_lock_after_renewal_fails() {
        let lock = Arc::new(MemoryLock::new(RenewBehavior::Extend));
        let session = EraseSession::start_with(
            Arc::clone(&lock),
            "memory-erase:scope".into(),
            Duration::from_secs(30),
            Duration::from_millis(20),
        )
        .await
        .expect("acquired");
        let (cancel, lease, gate) = session.handoff();
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release_statement = Arc::new(tokio::sync::Notify::new());
        let started_flag = Arc::clone(&started);
        let release = Arc::clone(&release_statement);
        let steps = EraseStepGate::new(gate.clone());
        let statement = memoria_storage::run_gated_erase_step(&steps, async move {
            started_flag.store(true, Ordering::SeqCst);
            release.notified().await;
            Ok(1)
        });
        tokio::pin!(statement);
        for _ in 0..50 {
            tokio::select! {
                _ = &mut statement => panic!("statement finished before the lock check"),
                _ = tokio::time::sleep(Duration::from_millis(5)) => {
                    if started.load(Ordering::SeqCst) {
                        break;
                    }
                }
            }
        }
        assert!(started.load(Ordering::SeqCst));
        lock.expire("memory-erase:scope");
        lock.set_renew(RenewBehavior::Fail);
        for _ in 0..50 {
            if gate.lost() {
                break;
            }
            tokio::select! {
                _ = &mut statement => panic!("statement finished before renewal failed"),
                _ = tokio::time::sleep(Duration::from_millis(5)) => {}
            }
        }
        assert!(gate.lost());
        assert!(!lock
            .try_acquire(
                "memory-erase:scope",
                "second",
                Duration::from_secs(30),
            )
            .await
            .expect("acquire while statement is running"));
        release_statement.notify_one();
        let deleted = statement.await.expect("statement");
        assert_eq!(deleted, 1);
        let polled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&polled);
        let next = memoria_storage::run_gated_erase_step(&steps, async move {
            flag.store(true, Ordering::SeqCst);
            Ok(2)
        })
        .await;
        assert!(next.expect_err("next step").to_string().contains("lock"));
        assert!(!polled.load(Ordering::SeqCst));
        gate.finish().await.expect("release");
        assert!(lock
            .try_acquire("memory-erase:scope", "second", Duration::from_secs(30))
            .await
            .expect("acquire after the statement"));
        drop(cancel);
        drop(lease);
    }

    #[tokio::test]
    async fn partial_delete_finishes_as_incomplete() {
        let lock = Arc::new(MemoryLock::new(RenewBehavior::Extend));
        let session = short_session(Arc::clone(&lock)).await.expect("acquired");
        let (cancel, lease, gate) = session.handoff();
        lease.shutdown_renew();
        let mut deleted = 0u64;
        let result = async {
            gate.checkpoint().await?;
            deleted = 1;
            lock.expire("memory-erase:scope");
            gate.checkpoint().await?;
            deleted = 2;
            Ok(MemoryEraseReport {
                memories: deleted,
                ..MemoryEraseReport::default()
            })
        }
        .await;
        let result = conclude_erase(result, &gate).await;
        let message = result.expect_err("partial erase").to_string();
        assert!(message.contains("lock"));
        assert!(!message.contains("rolled back"));
        assert!(!message.contains("nothing"));
        assert_eq!(deleted, 1);
        drop(cancel);
        drop(lease);
    }

    #[tokio::test]
    async fn contention_before_acquire_is_busy_and_starts_no_renewal() {
        let lock = Arc::new(MemoryLock::new(RenewBehavior::Extend));
        assert!(lock
            .try_acquire("memory-erase:scope", "existing", Duration::from_secs(5))
            .await
            .expect("seed"));
        let started = short_session(Arc::clone(&lock)).await;
        assert!(matches!(started, Err(MemoriaError::Blocked(_))));
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(lock.renew_calls(), 0);
    }
}
