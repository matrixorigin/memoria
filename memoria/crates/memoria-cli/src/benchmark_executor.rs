use crate::benchmark::{
    AssertionResult, MemoryAssertion, Scenario, ScenarioExecution, ScenarioStep, StepResult,
};
use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct BenchmarkExecutor {
    base_url: String,
    token: String,
    run_id: String,
}

impl BenchmarkExecutor {
    pub fn new(api_url: &str, token: &str) -> Self {
        // A second-granularity timestamp alone collides: independent runs started
        // in the same second share a namespace and can purge/correct each other's
        // memories. Mix in random bits so each run is isolated.
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let run_id = format!("{secs}-{:08x}", fastrand::u32(..));
        Self {
            base_url: api_url.trim_end_matches('/').into(),
            token: token.into(),
            run_id,
        }
    }

    fn client(&self, scenario_suffix: &str) -> Client {
        let user_id = format!("bench-{}-{}", self.run_id, scenario_suffix);
        Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .no_proxy()
            .default_headers({
                let mut h = reqwest::header::HeaderMap::new();
                h.insert(
                    "Authorization",
                    format!("Bearer {}", self.token).parse().unwrap(),
                );
                h.insert("X-Impersonate-User", user_id.parse().unwrap());
                h
            })
            .build()
            .unwrap()
    }

    pub fn execute(&self, scenario: &Scenario) -> ScenarioExecution {
        let sid = scenario.scenario_id.to_lowercase();
        let client = self.client(&sid);
        let session_id = format!("bench-{}-{}", self.run_id, sid);
        let user_id = format!("bench-{}-{}", self.run_id, sid);
        let mut exec = ScenarioExecution {
            _scenario_id: scenario.scenario_id.clone(),
            step_results: vec![],
            assertion_results: vec![],
            error: None,
        };

        for seed in &scenario.seed_memories {
            match self.store(
                &client,
                &seed.content,
                &seed.memory_type,
                &session_id,
                seed.age_days,
                seed.initial_confidence,
                seed.trust_tier.as_deref(),
            ) {
                Ok(mid) => {
                    if seed.is_outdated && !mid.is_empty() {
                        let _ = self.purge_ids(&client, &[mid], "seed is_outdated");
                    }
                }
                Err(e) => {
                    exec.error = Some(format!("seed failed: {e}"));
                    return exec;
                }
            }
        }

        for op in &scenario.maturation {
            let _ = client
                .post(format!(
                    "{}/admin/governance/{}/trigger",
                    self.base_url, user_id
                ))
                .query(&[("op", op.as_str())])
                .send();
        }

        for step in &scenario.steps {
            exec.step_results
                .push(self.run_step(&client, step, &session_id));
        }

        for assertion in &scenario.assertions {
            exec.assertion_results
                .push(self.run_assertion(&client, assertion, &session_id));
        }
        exec
    }

    #[allow(clippy::too_many_arguments)]
    fn store(
        &self,
        client: &Client,
        content: &str,
        memory_type: &str,
        session_id: &str,
        age_days: Option<f64>,
        confidence: Option<f64>,
        trust_tier: Option<&str>,
    ) -> anyhow::Result<String> {
        let mut body = json!({
            "content": content, "memory_type": memory_type,
            "session_id": session_id, "source": "benchmark",
        });
        if let Some(days) = age_days {
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs_f64()
                - days * 86400.0;
            body["observed_at"] = json!(chrono_like_iso(secs));
        }
        if let Some(c) = confidence {
            body["initial_confidence"] = json!(c);
        }
        if let Some(t) = trust_tier {
            body["trust_tier"] = json!(t);
        }

        let resp = client
            .post(format!("{}/v1/memories", self.base_url))
            .json(&body)
            .send()?
            .error_for_status()?;
        let data: Value = resp.json()?;
        match data["memory_id"].as_str() {
            Some(id) if !id.is_empty() => Ok(id.to_string()),
            // A 2xx without a memory_id is a response-shape failure, not a write
            // that happened to return nothing.
            _ => anyhow::bail!("store response carried no memory_id"),
        }
    }

    /// `Ok(vec![])` means the service answered with no matches; an `Err` means the
    /// query never produced an answer. Collapsing both into an empty vector hid
    /// service failures behind valid-looking empty retrievals.
    fn retrieve(
        &self,
        client: &Client,
        query: &str,
        session_id: &str,
        top_k: i64,
    ) -> anyhow::Result<Vec<String>> {
        let resp = client
            .post(format!("{}/v1/memories/retrieve", self.base_url))
            .json(&json!({"query": query, "top_k": top_k, "session_id": session_id}))
            .send()?
            .error_for_status()?;
        let data: Value = resp.json()?;
        let items = if data.is_array() {
            data.as_array()
        } else {
            data["results"].as_array()
        };
        let items = items.ok_or_else(|| {
            anyhow::anyhow!("retrieve response was neither an array nor {{\"results\": [...]}}")
        })?;
        // filter_map would silently drop an entry whose content is missing or not
        // a string, so a malformed backend response could still score full marks.
        items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                item["content"].as_str().map(String::from).ok_or_else(|| {
                    anyhow::anyhow!("retrieve result [{i}] has no string \"content\" field")
                })
            })
            .collect()
    }

    fn run_step(&self, client: &Client, step: &ScenarioStep, session_id: &str) -> StepResult {
        let action = step.action.clone();
        let result = (|| -> anyhow::Result<()> {
            match action.as_str() {
                "store" => {
                    self.store(
                        client,
                        step.content.as_deref().unwrap_or(""),
                        step.memory_type.as_deref().unwrap_or("semantic"),
                        session_id,
                        step.age_days,
                        step.initial_confidence,
                        step.trust_tier.as_deref(),
                    )?;
                }
                "retrieve" => {
                    self.retrieve(
                        client,
                        step.query.as_deref().unwrap_or(""),
                        session_id,
                        step.top_k.unwrap_or(5),
                    )?;
                }
                "search" => {
                    client
                        .post(format!("{}/v1/memories/search", self.base_url))
                        .json(&json!({"query": step.query, "top_k": step.top_k.unwrap_or(10)}))
                        .send()?
                        .error_for_status()?;
                }
                "correct" => {
                    client
                        .post(format!("{}/v1/memories/correct", self.base_url))
                        .json(&json!({"query": step.query, "new_content": step.content,
                            "reason": step.reason.as_deref().unwrap_or("benchmark")}))
                        .send()?
                        .error_for_status()?;
                }
                "purge" => {
                    let mut body = json!({"reason": step.reason.as_deref().unwrap_or("benchmark")});
                    if let Some(t) = &step.topic {
                        body["topic"] = json!(t);
                    }
                    client
                        .post(format!("{}/v1/memories/purge", self.base_url))
                        .json(&body)
                        .send()?
                        .error_for_status()?;
                }
                _ => {}
            }
            Ok(())
        })();
        match result {
            Ok(()) => StepResult {
                action,
                success: true,
                error: None,
            },
            Err(e) => StepResult {
                action,
                success: false,
                error: Some(format!("{e:#}")),
            },
        }
    }

    fn run_assertion(
        &self,
        client: &Client,
        assertion: &MemoryAssertion,
        session_id: &str,
    ) -> AssertionResult {
        match self.retrieve(client, &assertion.query, session_id, assertion.top_k) {
            Ok(contents) => AssertionResult {
                query: assertion.query.clone(),
                returned_contents: contents,
                error: None,
            },
            Err(e) => AssertionResult {
                query: assertion.query.clone(),
                returned_contents: vec![],
                error: Some(format!("{e:#}")),
            },
        }
    }

    fn purge_ids(&self, client: &Client, ids: &[String], reason: &str) -> anyhow::Result<()> {
        client
            .post(format!("{}/v1/memories/purge", self.base_url))
            .json(&json!({"memory_ids": ids, "reason": reason}))
            .send()?
            .error_for_status()?;
        Ok(())
    }
}

fn chrono_like_iso(epoch_secs: f64) -> String {
    let secs = epoch_secs as i64;
    let d = secs / 86400 + 719468;
    let era = if d >= 0 { d } else { d - 146096 } / 146097;
    let doe = (d - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    let rem = secs.rem_euclid(86400);
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}
