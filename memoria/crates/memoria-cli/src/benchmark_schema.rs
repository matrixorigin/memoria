use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Deserialize)]
pub struct ScenarioDataset {
    pub dataset_id: String,
    pub version: String,
    pub scenarios: Vec<Scenario>,
}

#[derive(Deserialize)]
pub struct Scenario {
    pub scenario_id: String,
    pub title: String,
    #[allow(dead_code)]
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub domain: String,
    pub difficulty: String,
    pub horizon: String,
    pub tags: Vec<String>,
    #[serde(default)]
    pub source_family: Option<String>,
    #[serde(default)]
    pub question_type: Option<String>,
    #[serde(default)]
    pub metadata: HashMap<String, Value>,
    pub seed_memories: Vec<SeedMemory>,
    #[serde(default)]
    pub maturation: Vec<String>,
    #[serde(default)]
    pub steps: Vec<ScenarioStep>,
    pub assertions: Vec<MemoryAssertion>,
}

#[derive(Deserialize)]
pub struct SeedMemory {
    pub content: String,
    #[serde(default = "default_semantic")]
    pub memory_type: String,
    #[serde(default)]
    pub is_outdated: bool,
    pub age_days: Option<f64>,
    pub initial_confidence: Option<f64>,
    pub trust_tier: Option<String>,
}

fn default_semantic() -> String {
    "semantic".into()
}

#[derive(Deserialize)]
pub struct ScenarioStep {
    pub action: String,
    pub content: Option<String>,
    pub memory_type: Option<String>,
    pub query: Option<String>,
    pub top_k: Option<i64>,
    pub reason: Option<String>,
    pub topic: Option<String>,
    pub age_days: Option<f64>,
    pub initial_confidence: Option<f64>,
    pub trust_tier: Option<String>,
}

#[derive(Deserialize)]
pub struct MemoryAssertion {
    pub query: String,
    #[serde(default = "default_top_k")]
    pub top_k: i64,
    pub expected_contents: Vec<String>,
    #[serde(default)]
    pub excluded_contents: Vec<String>,
}

fn default_top_k() -> i64 {
    5
}

pub struct StepResult {
    pub action: String,
    pub success: bool,
    pub error: Option<String>,
}

pub struct AssertionResult {
    pub query: String,
    pub returned_contents: Vec<String>,
    pub error: Option<String>,
}

pub struct ScenarioExecution {
    pub _scenario_id: String,
    pub step_results: Vec<StepResult>,
    pub assertion_results: Vec<AssertionResult>,
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct ScenarioResult {
    pub scenario_id: String,
    pub title: String,
    pub domain: String,
    pub difficulty: String,
    pub horizon: String,
    pub tags: Vec<String>,
    pub source_family: Option<String>,
    pub question_type: Option<String>,
    pub official_category: Option<String>,
    pub official_category_label: Option<String>,
    pub total_score: f64,
    pub grade: String,
    pub mqs_precision: f64,
    pub mqs_recall: f64,
    pub mqs_noise_rejection: f64,
    pub aus_step_success: f64,
    pub aus_assertion_pass: f64,
}

#[derive(Serialize)]
pub struct CategoryBreakdown {
    pub label: String,
    pub scenario_count: usize,
    pub score: f64,
    pub grade: String,
}

#[derive(Serialize)]
pub struct BenchmarkReport {
    pub dataset_id: String,
    pub version: String,
    pub scenario_count: usize,
    pub overall_score: f64,
    pub overall_grade: String,
    pub by_difficulty: HashMap<String, f64>,
    pub by_tag: HashMap<String, f64>,
    pub by_domain: HashMap<String, f64>,
    pub by_source_family: HashMap<String, CategoryBreakdown>,
    pub by_longmemeval_category: HashMap<String, CategoryBreakdown>,
    pub by_beam_ability: HashMap<String, CategoryBreakdown>,
    pub results: Vec<ScenarioResult>,
}
