//! Memory governance scheduler — periodic orchestration over pluggable governance strategies.
//!
//! Tasks:
//!   hourly  (3600s)  — cleanup tool_results, archive stale working memories
//!   daily   (86400s) — quarantine low-confidence, cleanup stale memories
//!   weekly  (604800s)— cleanup old branches and snapshots
//!
//! Uses existing cooldown mechanism to avoid duplicate runs across restarts.
//! Enable via MEMORIA_GOVERNANCE_ENABLED=true (default: false in scheduler, opt-in).

use std::sync::Arc;

use memoria_storage::{configured_multi_db_pool_size, MultiDbPoolKind, SqlMemoryStore};
use tokio::time::{interval, Duration};
use tracing::{error, info, warn};

use crate::{
    config::Config,
    distributed::DistributedLock,
    governance::{
        DefaultGovernanceStrategy, GovernanceExecution, GovernanceStore, GovernanceStrategy,
        GovernanceTask,
    },
    plugin::{
        build_local_governance_strategy, load_active_governance_plugin, load_plugin_package,
        record_runtime_plugin_event, HostPluginPolicy,
    },
    plugin_registry::PluginRegistry,
    strategy_domain::StrategyStatus,
    MemoriaError, MemoryService,
};

pub struct GovernanceScheduler {
    store: Option<Arc<dyn GovernanceStore>>,
    sql_store: Option<Arc<SqlMemoryStore>>,
    strategy: Arc<dyn GovernanceStrategy>,
    fallback_strategy: Arc<dyn GovernanceStrategy>,
    enabled: bool,
    breaker_threshold: usize,
    breaker_cooldown: Duration,
    observed_plugin: Option<ObservedPlugin>,
    lock: Arc<dyn DistributedLock>,
    instance_id: String,
    lock_ttl: Duration,
    isolated_pool_max_connections: Option<u32>,
}

#[derive(Clone)]
struct ObservedPlugin {
    binding_key: String,
    subject_key: String,
    plugin_key: String,
    version: String,
}

const DEFAULT_BREAKER_THRESHOLD: usize = 3;
const DEFAULT_BREAKER_COOLDOWN_SECS: u64 = 300;

impl GovernanceScheduler {
    pub fn new(service: Arc<MemoryService>) -> Self {
        let default_strategy: Arc<dyn GovernanceStrategy> = Arc::new(DefaultGovernanceStrategy);
        Self::from_parts(
            service,
            default_strategy.clone(),
            default_strategy,
            None,
            Arc::new(crate::distributed::NoopDistributedLock),
            "single".into(),
            Duration::from_secs(120),
        )
    }

    pub async fn from_config(
        service: Arc<MemoryService>,
        config: &Config,
    ) -> Result<Self, MemoriaError> {
        let delegate: Arc<dyn GovernanceStrategy> = Arc::new(DefaultGovernanceStrategy);
        let lock_ttl = Duration::from_secs(config.lock_ttl_secs);
        let instance_id = config.instance_id.clone();

        // Create an isolated pool for governance so long-running operations
        // (consolidation, cleanup, DDL rebuilds) do not starve request connections.
        let governance_pool_size = match std::env::var("GOVERNANCE_POOL_SIZE")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
        {
            Some(0) => 0,
            Some(raw) => raw.clamp(
                1,
                memoria_storage::multi_db_pool_max_size(MultiDbPoolKind::Governance),
            ),
            None if service.db_router.is_some() => {
                configured_multi_db_pool_size("GOVERNANCE_POOL_SIZE", MultiDbPoolKind::Governance)
            }
            None => 4,
        };
        #[allow(clippy::type_complexity)]
        let (gov_store, gov_sql_store, lock, isolated_pool_max_connections): (
            Option<Arc<dyn GovernanceStore>>,
            Option<Arc<SqlMemoryStore>>,
            Arc<dyn DistributedLock>,
            Option<u32>,
        ) = match &service.sql_store {
            Some(store) => {
                if governance_pool_size == 0 {
                    info!("Governance isolated pool disabled; falling back to main pool");
                    (
                        Some(store.clone() as Arc<dyn GovernanceStore>),
                        Some(store.clone()),
                        store.clone() as Arc<dyn DistributedLock>,
                        None,
                    )
                } else {
                    match store.spawn_background_store(governance_pool_size).await {
                        Ok(bg) => {
                            info!(
                                governance_pool_size,
                                "Governance scheduler using isolated pool"
                            );
                            (
                                Some(bg.clone() as Arc<dyn GovernanceStore>),
                                Some(bg.clone()),
                                bg as Arc<dyn DistributedLock>,
                                Some(governance_pool_size),
                            )
                        }
                        Err(e) => {
                            warn!(
                                error = %e,
                                governance_pool_size,
                                "Governance isolated pool failed, falling back to main pool"
                            );
                            (
                                Some(store.clone() as Arc<dyn GovernanceStore>),
                                Some(store.clone()),
                                store.clone() as Arc<dyn DistributedLock>,
                                None,
                            )
                        }
                    }
                }
            }
            None => (
                None,
                None,
                Arc::new(crate::distributed::NoopDistributedLock) as Arc<dyn DistributedLock>,
                None,
            ),
        };

        // Helper to build the scheduler with governance-isolated stores
        let build = |strategy: Arc<dyn GovernanceStrategy>,
                     fallback: Arc<dyn GovernanceStrategy>,
                     plugin: Option<ObservedPlugin>| {
            let mut scheduler = Self::new_with_components(
                gov_store.clone(),
                gov_sql_store.clone(),
                strategy,
                fallback,
                read_enabled_flag(),
                plugin,
                lock.clone(),
                instance_id.clone(),
                lock_ttl,
            );
            scheduler.isolated_pool_max_connections = isolated_pool_max_connections;
            scheduler
        };

        // Dev mode: load plugin directly from local filesystem (hot-reload friendly)
        if let Some(ref dir) = config.governance_plugin_dir {
            let path = std::path::PathBuf::from(dir);
            if path.join("manifest.json").exists() {
                let policy = HostPluginPolicy::development();
                let package = load_plugin_package(path.clone(), &policy)?;
                let plugin_key = package.plugin_key.clone();
                let version = package.manifest.version.clone();
                let strategy = build_local_governance_strategy(&package, delegate.clone())?;
                info!(
                    plugin_dir = %dir,
                    plugin_key = %plugin_key,
                    plugin_version = %version,
                    "Loaded governance plugin from local directory (dev mode)"
                );
                return Ok(build(
                    strategy,
                    delegate,
                    Some(ObservedPlugin {
                        binding_key: "local".into(),
                        subject_key: "dev".into(),
                        plugin_key,
                        version,
                    }),
                ));
            }
            warn!(plugin_dir = %dir, "MEMORIA_GOVERNANCE_PLUGIN_DIR set but manifest.json not found, falling back");
        }

        // Use the main store for plugin lookup (one-time startup read)
        if let Some(store) = &service.sql_store {
            if let Some(active_plugin) = load_active_governance_plugin(
                store.as_ref(),
                &config.governance_plugin_binding,
                &config.governance_plugin_subject,
                delegate.clone(),
            )
            .await?
            {
                info!(
                    plugin_key = %active_plugin.plugin_key,
                    plugin_version = %active_plugin.version,
                    binding_key = %active_plugin.binding_key,
                    "Loaded governance plugin from shared repository binding"
                );
                return Ok(build(
                    active_plugin.strategy.clone(),
                    delegate,
                    Some(ObservedPlugin {
                        binding_key: active_plugin.binding_key,
                        subject_key: active_plugin.subject_key,
                        plugin_key: active_plugin.plugin_key,
                        version: active_plugin.version,
                    }),
                ));
            }
        }

        Ok(build(delegate.clone(), delegate, None))
    }

    pub fn new_with_registry(
        service: Arc<MemoryService>,
        registry: &PluginRegistry,
        governance_key: &str,
    ) -> Result<Self, MemoriaError> {
        let store = service
            .sql_store
            .clone()
            .map(|store| -> Arc<dyn GovernanceStore> { store });
        Self::from_store_with_registry(store, registry, governance_key, read_enabled_flag())
    }

    pub fn from_store_with_registry(
        store: Option<Arc<dyn GovernanceStore>>,
        registry: &PluginRegistry,
        governance_key: &str,
        enabled: bool,
    ) -> Result<Self, MemoriaError> {
        let strategy = registry.create_governance(governance_key).ok_or_else(|| {
            MemoriaError::Blocked(format!(
                "Governance plugin `{governance_key}` is not registered"
            ))
        })?;
        let fallback_strategy: Arc<dyn GovernanceStrategy> = Arc::new(DefaultGovernanceStrategy);
        Ok(Self::new_with_components(
            store,
            None,
            strategy,
            fallback_strategy,
            enabled,
            None,
            Arc::new(crate::distributed::NoopDistributedLock),
            "single".into(),
            Duration::from_secs(120),
        ))
    }

    fn from_parts(
        service: Arc<MemoryService>,
        strategy: Arc<dyn GovernanceStrategy>,
        fallback_strategy: Arc<dyn GovernanceStrategy>,
        observed_plugin: Option<ObservedPlugin>,
        lock: Arc<dyn DistributedLock>,
        instance_id: String,
        lock_ttl: Duration,
    ) -> Self {
        let store = service
            .sql_store
            .clone()
            .map(|store| -> Arc<dyn GovernanceStore> { store });
        Self::new_with_components(
            store,
            service.sql_store.clone(),
            strategy,
            fallback_strategy,
            read_enabled_flag(),
            observed_plugin,
            lock,
            instance_id,
            lock_ttl,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_components(
        store: Option<Arc<dyn GovernanceStore>>,
        sql_store: Option<Arc<SqlMemoryStore>>,
        strategy: Arc<dyn GovernanceStrategy>,
        fallback_strategy: Arc<dyn GovernanceStrategy>,
        enabled: bool,
        observed_plugin: Option<ObservedPlugin>,
        lock: Arc<dyn DistributedLock>,
        instance_id: String,
        lock_ttl: Duration,
    ) -> Self {
        Self::new_with_components_and_breaker(
            store,
            sql_store,
            strategy,
            fallback_strategy,
            enabled,
            DEFAULT_BREAKER_THRESHOLD,
            Duration::from_secs(DEFAULT_BREAKER_COOLDOWN_SECS),
            observed_plugin,
            lock,
            instance_id,
            lock_ttl,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_components_and_breaker(
        store: Option<Arc<dyn GovernanceStore>>,
        sql_store: Option<Arc<SqlMemoryStore>>,
        strategy: Arc<dyn GovernanceStrategy>,
        fallback_strategy: Arc<dyn GovernanceStrategy>,
        enabled: bool,
        breaker_threshold: usize,
        breaker_cooldown: Duration,
        observed_plugin: Option<ObservedPlugin>,
        lock: Arc<dyn DistributedLock>,
        instance_id: String,
        lock_ttl: Duration,
    ) -> Self {
        Self {
            store,
            sql_store,
            strategy,
            fallback_strategy,
            enabled,
            breaker_threshold,
            breaker_cooldown,
            observed_plugin,
            lock,
            instance_id,
            lock_ttl,
            isolated_pool_max_connections: None,
        }
    }

    pub fn strategy_key(&self) -> &str {
        self.strategy.strategy_key()
    }

    pub fn fallback_strategy_key(&self) -> &str {
        self.fallback_strategy.strategy_key()
    }

    pub fn isolated_pool_max_connections(&self) -> Option<u32> {
        self.isolated_pool_max_connections
    }

    pub fn sql_store_configured_max_connections(&self) -> Option<u32> {
        self.sql_store
            .as_ref()
            .and_then(|store| store.configured_max_connections())
    }

    /// Spawn background tasks. Returns immediately; tasks run in background.
    pub fn start(self: Arc<Self>) {
        if !self.enabled {
            info!("Governance scheduler disabled (set MEMORIA_GOVERNANCE_ENABLED=true to enable)");
            return;
        }
        info!(
            strategy = self.strategy.strategy_key(),
            fallback = self.fallback_strategy.strategy_key(),
            instance_id = %self.instance_id,
            "Governance scheduler starting"
        );

        let s = self.clone();
        tokio::spawn(async move { s.run_loop(GovernanceTask::Hourly, 3600).await });

        let s = self.clone();
        tokio::spawn(async move { s.run_loop(GovernanceTask::Daily, 86400).await });

        let s = self.clone();
        tokio::spawn(async move { s.run_loop(GovernanceTask::Weekly, 604800).await });
    }

    async fn run_loop(&self, task: GovernanceTask, interval_secs: u64) {
        let mut ticker = interval(Duration::from_secs(interval_secs));
        ticker.tick().await; // skip first immediate tick
        let lock_key = format!("governance:{}", task.as_str());
        loop {
            ticker.tick().await;
            // Distributed leader election: only one instance runs each task
            match self
                .lock
                .try_acquire(&lock_key, &self.instance_id, self.lock_ttl)
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    info!(
                        task = task.as_str(),
                        instance_id = %self.instance_id,
                        "Skipping governance task — another instance holds the lock"
                    );
                    continue;
                }
                Err(err) => {
                    error!(task = task.as_str(), %err, "Failed to acquire governance lock, skipping");
                    continue;
                }
            }
            // Heartbeat: renew lock periodically while task runs
            let hb_lock = Arc::clone(&self.lock);
            let hb_key = lock_key.clone();
            let hb_holder = self.instance_id.clone();
            let hb_ttl = self.lock_ttl;
            let (hb_stop_tx, mut hb_stop_rx) = tokio::sync::watch::channel(false);
            tokio::spawn(async move {
                let mut tick = interval(hb_ttl / 3);
                tick.tick().await; // skip immediate
                loop {
                    tokio::select! {
                        Ok(()) = hb_stop_rx.changed() => break,
                        _ = tick.tick() => {
                            if let Err(e) = hb_lock.renew(&hb_key, &hb_holder, hb_ttl).await {
                                warn!(key = %hb_key, %e, "Lock heartbeat renew failed");
                            }
                        }
                    }
                }
            });
            let result = self.run_task(task).await;
            let _ = hb_stop_tx.send(true);
            // Release lock after task completes (best-effort)
            let _ = self.lock.release(&lock_key, &self.instance_id).await;
            if let Err(err) = result {
                error!(task = task.as_str(), %err, "Governance task failed");
            }
        }
    }

    async fn run_task(
        &self,
        task: GovernanceTask,
    ) -> Result<GovernanceExecution, crate::MemoriaError> {
        let Some(store) = &self.store else {
            return Ok(GovernanceExecution::default());
        };

        let primary_key = self.strategy.strategy_key();
        let fallback_key = self.fallback_strategy.strategy_key();
        let breaker_remaining = store.check_shared_breaker(primary_key, task).await?;
        let mut execution = if let Some(remaining_secs) =
            breaker_remaining.filter(|_| primary_key != fallback_key)
        {
            warn!(
                task = task.as_str(),
                strategy = primary_key,
                fallback = fallback_key,
                remaining_secs,
                "Primary governance strategy circuit breaker is open; using fallback"
            );
            self.run_degraded_fallback(
                store.as_ref(),
                task,
                primary_key,
                fallback_key,
                format!(
                    "Primary strategy {primary_key} circuit breaker is open for another {remaining_secs}s. Fell back to {fallback_key}."
                ),
                true,
            )
            .await?
        } else {
            match self.strategy.run(store.as_ref(), task).await {
                Ok(execution) => {
                    store.clear_shared_breaker(primary_key, task).await?;
                    execution
                }
                Err(primary_err) => {
                    if primary_key == fallback_key {
                        return Err(primary_err);
                    }
                    let breaker_remaining = store
                        .record_shared_breaker_failure(
                            primary_key,
                            task,
                            self.breaker_threshold,
                            self.breaker_cooldown.as_secs() as i64,
                        )
                        .await?;
                    let breaker_opened = breaker_remaining.is_some();
                    warn!(
                        task = task.as_str(),
                        strategy = primary_key,
                        fallback = fallback_key,
                        %primary_err,
                        breaker_opened,
                        "Primary governance strategy failed; degrading to fallback"
                    );

                    let reason = if breaker_opened {
                        format!(
                            "Primary strategy {primary_key} failed: {primary_err}. Fell back to {fallback_key} and opened circuit breaker."
                        )
                    } else {
                        format!(
                            "Primary strategy {primary_key} failed: {primary_err}. Fell back to {fallback_key}."
                        )
                    };
                    self.run_degraded_fallback(
                        store.as_ref(),
                        task,
                        primary_key,
                        fallback_key,
                        reason,
                        breaker_opened,
                    )
                    .await?
                }
            }
        };

        if execution.report.status == StrategyStatus::Failed {
            execution
                .report
                .warnings
                .push("Governance execution finished in failed state".to_string());
        }

        info!(
            task = task.as_str(),
            strategy = primary_key,
            report_status = execution.report.status.as_str(),
            users = execution.summary.users_processed,
            quarantined = execution.summary.total_quarantined,
            cleaned = execution.summary.total_cleaned,
            tool_results_cleaned = execution.summary.tool_results_cleaned,
            async_tasks_cleaned = execution.summary.async_tasks_cleaned,
            archived_working = execution.summary.archived_working,
            stale_cleaned = execution.summary.stale_cleaned,
            redundant_compressed = execution.summary.redundant_compressed,
            orphaned_incrementals_cleaned = execution.summary.orphaned_incrementals_cleaned,
            vector_index_rows = execution.summary.vector_index_rows,
            snapshots_cleaned = execution.summary.snapshots_cleaned,
            orphan_branches_cleaned = execution.summary.orphan_branches_cleaned,
            warnings = execution.report.warnings.len(),
            decisions = execution.report.decisions.len(),
            "Governance task complete"
        );
        Ok(execution)
    }

    async fn run_degraded_fallback(
        &self,
        store: &dyn GovernanceStore,
        task: GovernanceTask,
        primary_key: &str,
        fallback_key: &str,
        warning: String,
        circuit_open: bool,
    ) -> Result<GovernanceExecution, crate::MemoriaError> {
        let mut fallback_execution = self.fallback_strategy.run(store, task).await?;
        fallback_execution.report.status = StrategyStatus::Degraded;
        fallback_execution.report.warnings.push(warning);
        fallback_execution
            .report
            .metrics
            .insert("governance.degraded".to_string(), 1.0);
        if circuit_open {
            fallback_execution
                .report
                .metrics
                .insert("governance.circuit_open".to_string(), 1.0);
            fallback_execution.report.warnings.push(format!(
                "Primary strategy {primary_key} remains fenced off until the scheduler breaker cools down; fallback {fallback_key} handled this run."
            ));
        }
        self.record_runtime_event(
            "governance.degraded",
            "degraded",
            &format!(
                "Task {} degraded from {} to {}",
                task.as_str(),
                primary_key,
                fallback_key
            ),
        )
        .await?;
        Ok(fallback_execution)
    }

    async fn record_runtime_event(
        &self,
        event_type: &str,
        status: &str,
        message: &str,
    ) -> Result<(), MemoriaError> {
        let (Some(sql_store), Some(plugin)) = (&self.sql_store, &self.observed_plugin) else {
            return Ok(());
        };
        record_runtime_plugin_event(
            sql_store.as_ref(),
            "governance",
            Some(&plugin.binding_key),
            Some(&plugin.subject_key),
            Some(&plugin.plugin_key),
            Some(&plugin.version),
            event_type,
            status,
            message,
        )
        .await
    }
}

fn read_enabled_flag() -> bool {
    std::env::var("MEMORIA_GOVERNANCE_ENABLED")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(false)
}

/// Convenience type alias for use in AppState
pub type SchedulerHandle = Arc<GovernanceScheduler>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Config, GovernancePlan, GovernanceRunSummary, HostPluginPolicy, PluginRegistry,
        StrategyReport,
    };
    use async_trait::async_trait;
    use memoria_core::{interfaces::MemoryStore, Memory};
    use std::fs;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[derive(Default)]
    struct NoopStore;

    #[async_trait]
    impl GovernanceStore for NoopStore {
        async fn list_active_users(&self) -> Result<Vec<String>, crate::MemoriaError> {
            Ok(vec![])
        }
        async fn cleanup_tool_results(&self, _: i64) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_async_tasks(&self, _: i64) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn archive_stale_working(
            &self,
            _: i64,
        ) -> Result<Vec<(String, i64)>, crate::MemoriaError> {
            Ok(vec![])
        }
        async fn cleanup_stale(&self, _: &str) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn quarantine_low_confidence(&self, _: &str) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn compress_redundant(
            &self,
            _: &str,
            _: f64,
            _: i64,
            _: usize,
        ) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_orphaned_incrementals(
            &self,
            _: &str,
            _: i64,
        ) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn rebuild_vector_index(&self, _: &str) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_snapshots(&self, _: usize) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_orphan_branches(&self) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_orphan_stats(&self) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_orphan_graph_data(&self) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_edit_log(&self, _: i64) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_feedback(&self, _: i64) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn create_safety_snapshot(&self, _: &str) -> (Option<String>, Option<String>) {
            (Some("mem_snap_pre_daily_test".into()), None)
        }
        async fn log_edit(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
            _: &str,
            _: Option<&str>,
        ) {
        }
    }

    struct FallbackStore;

    #[async_trait]
    impl GovernanceStore for FallbackStore {
        async fn list_active_users(&self) -> Result<Vec<String>, crate::MemoriaError> {
            Ok(vec!["u1".into()])
        }
        async fn cleanup_tool_results(&self, _: i64) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_async_tasks(&self, _: i64) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn archive_stale_working(
            &self,
            _: i64,
        ) -> Result<Vec<(String, i64)>, crate::MemoriaError> {
            Ok(vec![])
        }
        async fn cleanup_stale(&self, _: &str) -> Result<i64, crate::MemoriaError> {
            Ok(1)
        }
        async fn quarantine_low_confidence(&self, _: &str) -> Result<i64, crate::MemoriaError> {
            Ok(2)
        }
        async fn compress_redundant(
            &self,
            _: &str,
            _: f64,
            _: i64,
            _: usize,
        ) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_orphaned_incrementals(
            &self,
            _: &str,
            _: i64,
        ) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn rebuild_vector_index(&self, _: &str) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_snapshots(&self, _: usize) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_orphan_branches(&self) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_orphan_stats(&self) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_orphan_graph_data(&self) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_edit_log(&self, _: i64) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_feedback(&self, _: i64) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn create_safety_snapshot(&self, _: &str) -> (Option<String>, Option<String>) {
            (Some("mem_snap_pre_daily_fallback".into()), None)
        }
        async fn log_edit(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
            _: &str,
            _: Option<&str>,
        ) {
        }
    }

    #[derive(Default)]
    struct SharedBreakerStore {
        state: Mutex<TestBreakerState>,
    }

    #[derive(Default)]
    struct TestBreakerState {
        consecutive_failures: usize,
        remaining_secs: Option<i64>,
    }

    #[async_trait]
    impl GovernanceStore for SharedBreakerStore {
        async fn list_active_users(&self) -> Result<Vec<String>, crate::MemoriaError> {
            Ok(vec![])
        }
        async fn cleanup_tool_results(&self, _: i64) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_async_tasks(&self, _: i64) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn archive_stale_working(
            &self,
            _: i64,
        ) -> Result<Vec<(String, i64)>, crate::MemoriaError> {
            Ok(vec![])
        }
        async fn cleanup_stale(&self, _: &str) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn quarantine_low_confidence(&self, _: &str) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn compress_redundant(
            &self,
            _: &str,
            _: f64,
            _: i64,
            _: usize,
        ) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_orphaned_incrementals(
            &self,
            _: &str,
            _: i64,
        ) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn rebuild_vector_index(&self, _: &str) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_snapshots(&self, _: usize) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_orphan_branches(&self) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_orphan_stats(&self) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_orphan_graph_data(&self) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_edit_log(&self, _: i64) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_feedback(&self, _: i64) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn create_safety_snapshot(&self, _: &str) -> (Option<String>, Option<String>) {
            (None, None)
        }
        async fn log_edit(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
            _: &str,
            _: Option<&str>,
        ) {
        }

        async fn check_shared_breaker(
            &self,
            _: &str,
            _: GovernanceTask,
        ) -> Result<Option<i64>, crate::MemoriaError> {
            Ok(self.state.lock().unwrap().remaining_secs)
        }

        async fn record_shared_breaker_failure(
            &self,
            _: &str,
            _: GovernanceTask,
            threshold: usize,
            cooldown_secs: i64,
        ) -> Result<Option<i64>, crate::MemoriaError> {
            let mut state = self.state.lock().unwrap();
            if state.remaining_secs.is_some() {
                return Ok(state.remaining_secs);
            }
            state.consecutive_failures += 1;
            if state.consecutive_failures >= threshold {
                state.consecutive_failures = 0;
                state.remaining_secs = Some(cooldown_secs);
            }
            Ok(state.remaining_secs)
        }

        async fn clear_shared_breaker(
            &self,
            _: &str,
            _: GovernanceTask,
        ) -> Result<(), crate::MemoriaError> {
            let mut state = self.state.lock().unwrap();
            state.consecutive_failures = 0;
            state.remaining_secs = None;
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingStrategy {
        tasks: Mutex<Vec<GovernanceTask>>,
    }

    #[async_trait]
    impl GovernanceStrategy for RecordingStrategy {
        fn strategy_key(&self) -> &str {
            "governance:test:v1"
        }

        async fn plan(
            &self,
            _: &dyn GovernanceStore,
            task: GovernanceTask,
        ) -> Result<GovernancePlan, crate::MemoriaError> {
            self.tasks.lock().unwrap().push(task);
            Ok(GovernancePlan::default())
        }

        async fn execute(
            &self,
            _: &dyn GovernanceStore,
            _: GovernanceTask,
            _: &GovernancePlan,
        ) -> Result<GovernanceExecution, crate::MemoriaError> {
            Ok(GovernanceExecution {
                summary: GovernanceRunSummary::default(),
                report: StrategyReport::default(),
            })
        }
    }

    struct FailingStrategy;

    #[async_trait]
    impl GovernanceStrategy for FailingStrategy {
        fn strategy_key(&self) -> &str {
            "governance:failing:v1"
        }

        async fn plan(
            &self,
            _: &dyn GovernanceStore,
            _: GovernanceTask,
        ) -> Result<GovernancePlan, crate::MemoriaError> {
            Err(crate::MemoriaError::Internal(
                "primary strategy failed".into(),
            ))
        }

        async fn execute(
            &self,
            _: &dyn GovernanceStore,
            _: GovernanceTask,
            _: &GovernancePlan,
        ) -> Result<GovernanceExecution, crate::MemoriaError> {
            unreachable!("failing strategy never reaches execute")
        }
    }

    #[derive(Default)]
    struct CountingFailingStrategy {
        calls: Mutex<usize>,
    }

    #[async_trait]
    impl GovernanceStrategy for CountingFailingStrategy {
        fn strategy_key(&self) -> &str {
            "governance:counting-failure:v1"
        }

        async fn plan(
            &self,
            _: &dyn GovernanceStore,
            _: GovernanceTask,
        ) -> Result<GovernancePlan, crate::MemoriaError> {
            *self.calls.lock().unwrap() += 1;
            Err(crate::MemoriaError::Internal(
                "counting primary strategy failed".into(),
            ))
        }

        async fn execute(
            &self,
            _: &dyn GovernanceStore,
            _: GovernanceTask,
            _: &GovernancePlan,
        ) -> Result<GovernanceExecution, crate::MemoriaError> {
            unreachable!("counting failure strategy never reaches execute")
        }
    }

    #[derive(Default)]
    struct FlakyStrategy {
        calls: Mutex<usize>,
    }

    #[async_trait]
    impl GovernanceStrategy for FlakyStrategy {
        fn strategy_key(&self) -> &str {
            "governance:flaky:v1"
        }

        async fn plan(
            &self,
            _: &dyn GovernanceStore,
            _: GovernanceTask,
        ) -> Result<GovernancePlan, crate::MemoriaError> {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            if *calls == 1 {
                Err(crate::MemoriaError::Internal("flaky primary failed".into()))
            } else {
                Ok(GovernancePlan::default())
            }
        }

        async fn execute(
            &self,
            _: &dyn GovernanceStore,
            _: GovernanceTask,
            _: &GovernancePlan,
        ) -> Result<GovernanceExecution, crate::MemoriaError> {
            Ok(GovernanceExecution {
                summary: GovernanceRunSummary::default(),
                report: StrategyReport::default(),
            })
        }
    }

    #[derive(Default)]
    struct NoopMemoryStore;

    #[async_trait]
    impl MemoryStore for NoopMemoryStore {
        async fn insert(&self, _: &Memory) -> Result<(), crate::MemoriaError> {
            Ok(())
        }

        async fn get(&self, _: &str) -> Result<Option<Memory>, crate::MemoriaError> {
            Ok(None)
        }

        async fn get_including_inactive(&self, _: &str) -> Result<Option<Memory>, crate::MemoriaError> {
            Ok(None)
        }

        async fn update(&self, _: &Memory) -> Result<(), crate::MemoriaError> {
            Ok(())
        }

        async fn soft_delete(&self, _: &str) -> Result<(), crate::MemoriaError> {
            Ok(())
        }

        async fn list_active(&self, _: &str, _: i64) -> Result<Vec<Memory>, crate::MemoriaError> {
            Ok(vec![])
        }

        async fn search_fulltext(
            &self,
            _: &str,
            _: &str,
            _: i64,
        ) -> Result<Vec<Memory>, crate::MemoriaError> {
            Ok(vec![])
        }

        async fn search_vector(
            &self,
            _: &str,
            _: &[f32],
            _: i64,
        ) -> Result<Vec<Memory>, crate::MemoriaError> {
            Ok(vec![])
        }
    }

    fn temp_plugin_dir(name: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("memoria-scheduler-plugin-{name}-{nonce}"))
    }

    fn write_manifest(dir: &std::path::Path, script: &str, timeout_ms: u64) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("plugin.rhai"), script).unwrap();
        let mut manifest = serde_json::json!({
            "name": "memoria-governance-scheduler-test",
            "version": "1.0.0",
            "api_version": "v1",
            "runtime": "rhai",
            "entry": { "rhai": { "script": "plugin.rhai", "entrypoint": "memoria_plugin" } },
            "capabilities": ["governance.plan", "governance.execute"],
            "compatibility": { "memoria": ">=0.1.0-rc1 <0.2.0" },
            "permissions": { "network": false, "filesystem": false, "env": [] },
            "limits": { "timeout_ms": timeout_ms, "max_memory_mb": 64, "max_output_bytes": 16384 },
            "integrity": { "sha256": "", "signature": "dev-signature", "signer": "dev-signer" },
            "metadata": { "display_name": "Scheduler Test Plugin" }
        });
        fs::write(
            dir.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let sha = crate::plugin::compute_package_sha256(dir).unwrap();
        manifest["integrity"]["sha256"] = serde_json::Value::String(sha);
        fs::write(
            dir.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn scheduler_delegates_task_execution_to_strategy() {
        let strategy = Arc::new(RecordingStrategy::default());
        let fallback = Arc::new(DefaultGovernanceStrategy);
        let scheduler = GovernanceScheduler::new_with_components(
            Some(Arc::new(NoopStore)),
            None,
            strategy.clone(),
            fallback,
            true,
            None,
            Arc::new(crate::distributed::NoopDistributedLock),
            "test".into(),
            Duration::from_secs(120),
        );

        let execution = scheduler
            .run_task(GovernanceTask::Daily)
            .await
            .expect("scheduler should delegate without error");

        assert_eq!(*strategy.tasks.lock().unwrap(), vec![GovernanceTask::Daily]);
        assert_eq!(execution.report.status, StrategyStatus::Success);
    }

    #[tokio::test]
    async fn scheduler_skips_task_when_store_is_missing() {
        let strategy = Arc::new(RecordingStrategy::default());
        let fallback = Arc::new(DefaultGovernanceStrategy);
        let scheduler = GovernanceScheduler::new_with_components(
            None,
            None,
            strategy.clone(),
            fallback,
            true,
            None,
            Arc::new(crate::distributed::NoopDistributedLock),
            "test".into(),
            Duration::from_secs(120),
        );

        let execution = scheduler
            .run_task(GovernanceTask::Weekly)
            .await
            .expect("missing store should be treated as a no-op");

        assert!(strategy.tasks.lock().unwrap().is_empty());
        assert_eq!(execution.summary, GovernanceRunSummary::default());
    }

    #[tokio::test]
    async fn scheduler_falls_back_when_primary_strategy_fails() {
        let scheduler = GovernanceScheduler::new_with_components(
            Some(Arc::new(NoopStore)),
            None,
            Arc::new(FailingStrategy),
            Arc::new(DefaultGovernanceStrategy),
            true,
            None,
            Arc::new(crate::distributed::NoopDistributedLock),
            "test".into(),
            Duration::from_secs(120),
        );

        let execution = scheduler
            .run_task(GovernanceTask::Daily)
            .await
            .expect("scheduler should fall back to default strategy");

        assert_eq!(execution.report.status, StrategyStatus::Degraded);
        assert!(execution
            .report
            .warnings
            .iter()
            .any(|warning| warning.contains("Fell back")));
        assert_eq!(execution.report.metrics["governance.degraded"], 1.0);
    }

    #[tokio::test]
    async fn scheduler_fallback_uses_default_strategy_results() {
        let scheduler = GovernanceScheduler::new_with_components(
            Some(Arc::new(FallbackStore)),
            None,
            Arc::new(FailingStrategy),
            Arc::new(DefaultGovernanceStrategy),
            true,
            None,
            Arc::new(crate::distributed::NoopDistributedLock),
            "test".into(),
            Duration::from_secs(120),
        );

        let execution = scheduler
            .run_task(GovernanceTask::Daily)
            .await
            .expect("scheduler should return fallback default strategy results");

        assert_eq!(execution.report.status, StrategyStatus::Degraded);
        assert_eq!(execution.summary.users_processed, 1);
        assert_eq!(execution.summary.total_quarantined, 2);
        assert_eq!(execution.summary.total_cleaned, 1);
        assert!(!execution.report.decisions.is_empty());
        assert_eq!(execution.report.metrics["governance.snapshot_created"], 1.0);
    }

    #[tokio::test]
    async fn scheduler_opens_circuit_after_repeated_failures() {
        let primary = Arc::new(CountingFailingStrategy::default());
        let scheduler = GovernanceScheduler::new_with_components_and_breaker(
            Some(Arc::new(SharedBreakerStore::default())),
            None,
            primary.clone(),
            Arc::new(DefaultGovernanceStrategy),
            true,
            2,
            Duration::from_secs(60),
            None,
            Arc::new(crate::distributed::NoopDistributedLock),
            "test".into(),
            Duration::from_secs(120),
        );

        let first = scheduler.run_task(GovernanceTask::Daily).await.unwrap();
        let second = scheduler.run_task(GovernanceTask::Daily).await.unwrap();
        let third = scheduler.run_task(GovernanceTask::Daily).await.unwrap();

        assert_eq!(*primary.calls.lock().unwrap(), 2);
        assert_eq!(first.report.status, StrategyStatus::Degraded);
        assert_eq!(second.report.status, StrategyStatus::Degraded);
        assert_eq!(third.report.status, StrategyStatus::Degraded);
        assert_eq!(
            third.report.metrics.get("governance.circuit_open"),
            Some(&1.0)
        );
        assert!(third
            .report
            .warnings
            .iter()
            .any(|warning| warning.contains("circuit breaker is open")));
    }

    #[tokio::test]
    async fn scheduler_resets_circuit_after_primary_success() {
        let primary = Arc::new(FlakyStrategy::default());
        let scheduler = GovernanceScheduler::new_with_components_and_breaker(
            Some(Arc::new(SharedBreakerStore::default())),
            None,
            primary.clone(),
            Arc::new(DefaultGovernanceStrategy),
            true,
            2,
            Duration::from_secs(60),
            None,
            Arc::new(crate::distributed::NoopDistributedLock),
            "test".into(),
            Duration::from_secs(120),
        );

        let first = scheduler.run_task(GovernanceTask::Daily).await.unwrap();
        let second = scheduler.run_task(GovernanceTask::Daily).await.unwrap();

        assert_eq!(*primary.calls.lock().unwrap(), 2);
        assert_eq!(first.report.status, StrategyStatus::Degraded);
        assert_eq!(second.report.status, StrategyStatus::Success);
        assert!(second
            .report
            .metrics
            .get("governance.circuit_open")
            .is_none());
    }

    #[tokio::test]
    async fn scheduler_uses_registered_rhai_plugin() {
        let dir = temp_plugin_dir("registry");
        write_manifest(
            &dir,
            r#"
                fn memoria_plugin(ctx) {
                    if ctx["phase"] == "plan" {
                        return #{
                            requires_approval: true,
                            actions: [ decision("plugin:scheduler", "scheduler loaded plugin", 0.8) ]
                        };
                    }
                    return #{ "warnings": ["scheduler plugin executed"], "metrics": #{ "plugin.scheduler.executed": 1.0 } };
                }
            "#,
            200,
        );

        let mut registry = PluginRegistry::new();
        let key = registry
            .register_rhai_governance_plugin(
                &dir,
                HostPluginPolicy::development(),
                Arc::new(DefaultGovernanceStrategy),
            )
            .unwrap();

        let scheduler = GovernanceScheduler::from_store_with_registry(
            Some(Arc::new(NoopStore)),
            &registry,
            &key,
            true,
        )
        .unwrap();
        let execution = scheduler.run_task(GovernanceTask::Weekly).await.unwrap();

        assert_eq!(execution.report.status, StrategyStatus::Success);
        assert!(execution
            .report
            .warnings
            .iter()
            .any(|warning| warning.contains("scheduler plugin executed")));
        assert_eq!(
            execution.report.metrics.get("plugin.scheduler.executed"),
            Some(&1.0)
        );
        assert!(registry.governance_metadata(&key).is_some());

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn scheduler_falls_back_when_registered_plugin_exceeds_limits() {
        let dir = temp_plugin_dir("limit");
        write_manifest(
            &dir,
            r#"
                fn memoria_plugin(ctx) {
                    if ctx["phase"] == "plan" {
                        while true {}
                    }
                    return #{};
                }
            "#,
            5,
        );

        let mut registry = PluginRegistry::new();
        let key = registry
            .register_rhai_governance_plugin(
                &dir,
                HostPluginPolicy::development(),
                Arc::new(DefaultGovernanceStrategy),
            )
            .unwrap();

        let scheduler = GovernanceScheduler::from_store_with_registry(
            Some(Arc::new(FallbackStore)),
            &registry,
            &key,
            true,
        )
        .unwrap();
        let execution = scheduler.run_task(GovernanceTask::Daily).await.unwrap();

        assert_eq!(execution.report.status, StrategyStatus::Degraded);
        assert!(execution
            .report
            .warnings
            .iter()
            .any(|warning| warning.contains("Fell back")));
        assert_eq!(
            execution.report.metrics.get("governance.degraded"),
            Some(&1.0)
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn scheduler_from_config_without_shared_store_uses_default_strategy() {
        let config = Config {
            db_url: "mysql://root:111@localhost:6001/memoria".into(),
            db_name: "memoria".into(),
            shared_db_url: "mysql://root:111@localhost:6001/memoria_shared".into(),
            multi_db: false,
            embedding_provider: "mock".into(),
            embedding_model: "BAAI/bge-m3".into(),
            embedding_dim: 1024,
            embedding_api_key: String::new(),
            embedding_base_url: String::new(),
            embedding_endpoints: vec![],
            llm_api_key: None,
            llm_base_url: "https://api.openai.com/v1".into(),
            llm_model: "gpt-4o-mini".into(),
            user: "default".into(),
            governance_plugin_binding: "default".into(),
            governance_plugin_subject: "system".into(),
            governance_plugin_dir: None,
            instance_id: "test-instance".into(),
            lock_ttl_secs: 120,
            ops_metrics_enabled: false,
        };
        let scheduler = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async {
                let service = Arc::new(MemoryService::new(Arc::new(NoopMemoryStore), None, None));
                GovernanceScheduler::from_config(service, &config).await
            })
            .unwrap();

        assert_eq!(scheduler.strategy.strategy_key(), "governance:default:v1");
        assert_eq!(
            scheduler.fallback_strategy.strategy_key(),
            "governance:default:v1"
        );
    }

    /// In-memory distributed lock for testing leader election.
    struct InMemoryLock {
        locks: Mutex<std::collections::HashMap<String, String>>,
    }

    impl InMemoryLock {
        fn new() -> Self {
            Self {
                locks: Mutex::new(std::collections::HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl crate::distributed::DistributedLock for InMemoryLock {
        async fn try_acquire(
            &self,
            key: &str,
            holder: &str,
            _ttl: Duration,
        ) -> Result<bool, crate::MemoriaError> {
            let mut locks = self.locks.lock().unwrap();
            if let Some(existing) = locks.get(key) {
                return Ok(existing == holder);
            }
            locks.insert(key.to_string(), holder.to_string());
            Ok(true)
        }
        async fn renew(
            &self,
            key: &str,
            holder: &str,
            _ttl: Duration,
        ) -> Result<bool, crate::MemoriaError> {
            let locks = self.locks.lock().unwrap();
            Ok(locks.get(key).map_or(false, |h| h == holder))
        }
        async fn release(&self, key: &str, holder: &str) -> Result<(), crate::MemoriaError> {
            let mut locks = self.locks.lock().unwrap();
            if locks.get(key).map_or(false, |h| h == holder) {
                locks.remove(key);
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn scheduler_leader_election_only_one_instance_runs_task() {
        let strategy_a = Arc::new(RecordingStrategy::default());
        let strategy_b = Arc::new(RecordingStrategy::default());
        let shared_lock = Arc::new(InMemoryLock::new());

        let scheduler_a = Arc::new(GovernanceScheduler::new_with_components(
            Some(Arc::new(NoopStore)),
            None,
            strategy_a.clone(),
            Arc::new(DefaultGovernanceStrategy),
            true,
            None,
            shared_lock.clone(),
            "instance_a".into(),
            Duration::from_secs(120),
        ));

        let scheduler_b = Arc::new(GovernanceScheduler::new_with_components(
            Some(Arc::new(NoopStore)),
            None,
            strategy_b.clone(),
            Arc::new(DefaultGovernanceStrategy),
            true,
            None,
            shared_lock.clone(),
            "instance_b".into(),
            Duration::from_secs(120),
        ));

        // Simulate run_loop lock acquisition: A acquires first
        let lock_key = "governance:daily";
        let acquired_a = shared_lock
            .try_acquire(lock_key, "instance_a", Duration::from_secs(120))
            .await
            .unwrap();
        assert!(acquired_a, "instance_a should acquire the lock");

        // B tries to acquire — should fail
        let acquired_b = shared_lock
            .try_acquire(lock_key, "instance_b", Duration::from_secs(120))
            .await
            .unwrap();
        assert!(
            !acquired_b,
            "instance_b should NOT acquire while instance_a holds it"
        );

        // Only A runs the task
        let exec_a = scheduler_a.run_task(GovernanceTask::Daily).await.unwrap();
        assert_eq!(
            *strategy_a.tasks.lock().unwrap(),
            vec![GovernanceTask::Daily]
        );
        assert_eq!(exec_a.report.status, StrategyStatus::Success);

        // B's strategy was never called
        assert!(strategy_b.tasks.lock().unwrap().is_empty());

        // A releases, now B can acquire and run
        shared_lock.release(lock_key, "instance_a").await.unwrap();
        let acquired_b = shared_lock
            .try_acquire(lock_key, "instance_b", Duration::from_secs(120))
            .await
            .unwrap();
        assert!(acquired_b);

        let exec_b = scheduler_b.run_task(GovernanceTask::Daily).await.unwrap();
        assert_eq!(
            *strategy_b.tasks.lock().unwrap(),
            vec![GovernanceTask::Daily]
        );
        assert_eq!(exec_b.report.status, StrategyStatus::Success);

        shared_lock.release(lock_key, "instance_b").await.unwrap();
    }

    /// Gap: breaker state persists across scheduler instances sharing the same store.
    /// Simulates process restart: scheduler_a opens the breaker, scheduler_b (new process)
    /// sees the open breaker and uses fallback without calling primary.
    #[tokio::test]
    async fn scheduler_breaker_state_survives_across_instances() {
        let shared_breaker = Arc::new(SharedBreakerStore::default());

        // Instance A: fail twice to open the breaker (threshold=2)
        let primary_a = Arc::new(CountingFailingStrategy::default());
        let scheduler_a = GovernanceScheduler::new_with_components_and_breaker(
            Some(shared_breaker.clone()),
            None,
            primary_a.clone(),
            Arc::new(DefaultGovernanceStrategy),
            true,
            2,
            Duration::from_secs(300),
            None,
            Arc::new(crate::distributed::NoopDistributedLock),
            "instance_a".into(),
            Duration::from_secs(120),
        );

        // Two failures → breaker opens
        let _ = scheduler_a.run_task(GovernanceTask::Daily).await.unwrap();
        let _ = scheduler_a.run_task(GovernanceTask::Daily).await.unwrap();
        assert_eq!(*primary_a.calls.lock().unwrap(), 2);

        // Drop scheduler_a, simulating process restart.
        drop(scheduler_a);

        // Instance B: new scheduler, same shared breaker store, fresh primary strategy
        let primary_b = Arc::new(RecordingStrategy::default());
        let scheduler_b = GovernanceScheduler::new_with_components_and_breaker(
            Some(shared_breaker.clone()),
            None,
            primary_b.clone(),
            Arc::new(DefaultGovernanceStrategy),
            true,
            2,
            Duration::from_secs(300),
            None,
            Arc::new(crate::distributed::NoopDistributedLock),
            "instance_b".into(),
            Duration::from_secs(120),
        );

        let exec = scheduler_b.run_task(GovernanceTask::Daily).await.unwrap();

        // Primary should NOT have been called — breaker is still open
        assert!(
            primary_b.tasks.lock().unwrap().is_empty(),
            "primary_b should not be called while breaker is open"
        );
        assert_eq!(exec.report.status, StrategyStatus::Degraded);
        assert!(exec
            .report
            .warnings
            .iter()
            .any(|w| w.contains("circuit breaker is open")));
    }

    /// Gap: two scheduler instances competing for the same task — only one executes,
    /// and both strategies produce correct results.
    #[tokio::test]
    async fn scheduler_two_instances_compete_only_winner_executes() {
        let strategy_a = Arc::new(RecordingStrategy::default());
        let strategy_b = Arc::new(RecordingStrategy::default());
        let shared_lock = Arc::new(InMemoryLock::new());

        let scheduler_a = Arc::new(GovernanceScheduler::new_with_components(
            Some(Arc::new(FallbackStore)),
            None,
            strategy_a.clone(),
            Arc::new(DefaultGovernanceStrategy),
            true,
            None,
            shared_lock.clone(),
            "instance_a".into(),
            Duration::from_secs(120),
        ));

        let scheduler_b = Arc::new(GovernanceScheduler::new_with_components(
            Some(Arc::new(FallbackStore)),
            None,
            strategy_b.clone(),
            Arc::new(DefaultGovernanceStrategy),
            true,
            None,
            shared_lock.clone(),
            "instance_b".into(),
            Duration::from_secs(120),
        ));

        // Both try to run hourly concurrently
        let lock_key = "governance:hourly";
        let a_got = shared_lock
            .try_acquire(lock_key, "instance_a", Duration::from_secs(120))
            .await
            .unwrap();
        let b_got = shared_lock
            .try_acquire(lock_key, "instance_b", Duration::from_secs(120))
            .await
            .unwrap();

        assert!(a_got ^ b_got, "exactly one should acquire the lock");

        let (winner, loser_strategy) = if a_got {
            (scheduler_a.clone(), strategy_b.clone())
        } else {
            (scheduler_b.clone(), strategy_a.clone())
        };

        let exec = winner.run_task(GovernanceTask::Hourly).await.unwrap();
        assert_eq!(exec.report.status, StrategyStatus::Success);
        assert!(
            loser_strategy.tasks.lock().unwrap().is_empty(),
            "loser should not have run"
        );

        // Release and let loser run
        let holder = if a_got { "instance_a" } else { "instance_b" };
        shared_lock.release(lock_key, holder).await.unwrap();

        let loser = if a_got {
            scheduler_b.clone()
        } else {
            scheduler_a.clone()
        };
        let loser_holder = if a_got { "instance_b" } else { "instance_a" };
        let got = shared_lock
            .try_acquire(lock_key, loser_holder, Duration::from_secs(120))
            .await
            .unwrap();
        assert!(got);

        let exec2 = loser.run_task(GovernanceTask::Hourly).await.unwrap();
        assert_eq!(exec2.report.status, StrategyStatus::Success);
        shared_lock.release(lock_key, loser_holder).await.unwrap();
    }
}
