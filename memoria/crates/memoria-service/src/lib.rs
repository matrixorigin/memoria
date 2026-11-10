pub mod config;
pub mod content_filter;
pub mod distributed;
pub mod governance;
pub mod graph_domains;
pub mod pipeline;
pub mod plugin;
pub mod plugin_registry;
pub mod rebuild_worker;
pub mod scheduler;
pub mod scoring;
pub mod service;
pub mod stats_reporter;
pub mod strategy;
pub mod strategy_domain;
pub mod vector_index_monitor;
pub use config::Config;
pub use distributed::{AsyncTask, AsyncTaskStore, DistributedLock, NoopDistributedLock};
pub use governance::{
    DefaultGovernanceStrategy, GovernanceExecution, GovernancePlan, GovernanceRunSummary,
    GovernanceStore, GovernanceStrategy, GovernanceTask,
};
pub use graph_domains::{
    Conflict, ConflictDetector, ConflictInput, ConsolidationInput, ConsolidationStrategy,
    DefaultConflictDetector, DefaultConsolidationStrategy, DefaultTrustLifecycleStrategy,
    GraphDomainStore, TrustEvaluationInput, TrustLifecycleStrategy,
};
pub use memoria_core::MemoriaError;
pub use pipeline::{MemoryPipeline, PipelineResult};
pub use plugin::{
    activate_plugin_binding, build_local_governance_strategy, compute_package_sha256,
    get_plugin_audit_events, grpc_proto, list_binding_rules, list_plugin_compatibility_matrix,
    list_plugin_repository_entries, list_trusted_plugin_signers, load_active_governance_plugin,
    publish_plugin_package, publish_plugin_package_dev, record_runtime_plugin_event,
    review_plugin_package, score_plugin_package, upsert_plugin_binding_rule,
    upsert_trusted_plugin_signer, ActiveGovernancePlugin, BindingRuleInput,
    GovernancePluginContractHarness, GovernancePluginContractResult, GrpcGovernanceStrategy,
    HostPluginPolicy, PluginAuditEvent, PluginBindingRule, PluginCompatibility,
    PluginCompatibilityEntry, PluginManifest, PluginPackage, PluginRepositoryEntry, PluginRuntime,
    RhaiGovernanceStrategy, TrustedPluginSignerEntry, GOVERNANCE_RHAI_TEMPLATE,
    GOVERNANCE_RHAI_TEMPLATE_ENTRYPOINT,
};
pub use plugin_registry::{GovernancePluginMetadata, PluginRegistry};
pub use scheduler::GovernanceScheduler;
pub use scoring::{
    DefaultScoringPlugin, FeedbackTotals, ScoringPlugin, ScoringStore, TuningResult,
};
pub use service::{
    CandidateScore, ExplainLevel, FulltextSearchOptions, InMemoryFlusher, ListActiveOptions,
    MemoryService, PurgeResult, RetrievalExplain, RetrieveOptions, SessionScope,
    StructuredQueryOptions,
    ENTITY_EXTRACTION_DROPS,
};
pub use stats_reporter::StatsReporter;

/// Wait for SIGTERM or Ctrl-C. Shared across CLI, MCP, and API servers.
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
pub use strategy::{RetrievalStrategy, StrategyRegistry};
pub use strategy_domain::{StrategyDecision, StrategyEvidence, StrategyReport, StrategyStatus};
