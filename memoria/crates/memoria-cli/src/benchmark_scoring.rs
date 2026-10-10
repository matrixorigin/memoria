use super::benchmark_schema::{
    BenchmarkReport, MemoryAssertion, Scenario, ScenarioDataset, ScenarioExecution, ScenarioResult,
};
use super::benchmark_taxonomy::{
    category_breakdown, grade, official_category, scenario_question_type, scenario_source_family,
};
use std::collections::HashMap;

fn score_contents(assertion: &MemoryAssertion, returned: &[String]) -> (f64, f64, f64, bool) {
    let hits = assertion
        .expected_contents
        .iter()
        .filter(|exp| {
            returned
                .iter()
                .any(|c| c.to_lowercase().contains(&exp.to_lowercase()))
        })
        .count();
    let recall = if assertion.expected_contents.is_empty() {
        100.0
    } else {
        100.0 * hits as f64 / assertion.expected_contents.len() as f64
    };

    let precision = if returned.is_empty() {
        0.0
    } else {
        let relevant = returned
            .iter()
            .filter(|c| {
                assertion
                    .expected_contents
                    .iter()
                    .any(|exp| c.to_lowercase().contains(&exp.to_lowercase()))
            })
            .count();
        100.0 * relevant as f64 / returned.len() as f64
    };

    let noise_rejection = if assertion.excluded_contents.is_empty() {
        100.0
    } else {
        let noise_hits = assertion
            .excluded_contents
            .iter()
            .filter(|exc| {
                returned
                    .iter()
                    .any(|c| c.to_lowercase().contains(&exc.to_lowercase()))
            })
            .count();
        100.0 * (assertion.excluded_contents.len() - noise_hits) as f64
            / assertion.excluded_contents.len() as f64
    };

    let passed = recall >= 80.0 && noise_rejection >= 80.0;
    (precision, recall, noise_rejection, passed)
}

pub fn score_scenario(scenario: &Scenario, exec: &ScenarioExecution) -> ScenarioResult {
    let source_family = scenario_source_family("", scenario);
    let question_type = scenario_question_type(scenario);
    let official = official_category(source_family.as_deref(), question_type.as_deref());
    if exec.error.is_some() {
        return ScenarioResult {
            scenario_id: scenario.scenario_id.clone(),
            title: scenario.title.clone(),
            domain: scenario.domain.clone(),
            difficulty: scenario.difficulty.clone(),
            horizon: scenario.horizon.clone(),
            tags: scenario.tags.clone(),
            source_family,
            question_type,
            official_category: official.as_ref().map(|(key, _)| key.clone()),
            official_category_label: official.as_ref().map(|(_, label)| label.clone()),
            total_score: 0.0,
            grade: "D".into(),
            mqs_precision: 0.0,
            mqs_recall: 0.0,
            mqs_noise_rejection: 100.0,
            aus_step_success: 0.0,
            aus_assertion_pass: 0.0,
        };
    }

    let mut precisions = vec![];
    let mut recalls = vec![];
    let mut noises = vec![];
    let mut passed_count = 0usize;
    for (i, assertion) in scenario.assertions.iter().enumerate() {
        let result = exec.assertion_results.get(i);
        // A retrieval that failed tells us nothing about recall or noise. Scoring
        // it like an empty-but-successful answer would let an assertion with no
        // expected_contents pass on a service failure.
        let (p, r, n, ok) = match result {
            Some(result) if result.error.is_some() => (0.0, 0.0, 0.0, false),
            _ => score_contents(
                assertion,
                result.map(|r| r.returned_contents.as_slice()).unwrap_or(&[]),
            ),
        };
        precisions.push(p);
        recalls.push(r);
        noises.push(n);
        if ok {
            passed_count += 1;
        }
    }

    let avg = |v: &[f64]| {
        if v.is_empty() {
            0.0
        } else {
            v.iter().sum::<f64>() / v.len() as f64
        }
    };
    let mqs_p = avg(&precisions);
    let mqs_r = avg(&recalls);
    let mqs_n = avg(&noises);
    let assertion_pass = if scenario.assertions.is_empty() {
        0.0
    } else {
        100.0 * passed_count as f64 / scenario.assertions.len() as f64
    };
    let step_success = if exec.step_results.is_empty() {
        100.0
    } else {
        100.0 * exec.step_results.iter().filter(|s| s.success).count() as f64
            / exec.step_results.len() as f64
    };

    let mqs = (mqs_p + mqs_r + mqs_n) / 3.0;
    let aus = (step_success + assertion_pass) / 2.0;
    let total = 0.65 * mqs + 0.35 * aus;

    ScenarioResult {
        scenario_id: scenario.scenario_id.clone(),
        title: scenario.title.clone(),
        domain: scenario.domain.clone(),
        difficulty: scenario.difficulty.clone(),
        horizon: scenario.horizon.clone(),
        tags: scenario.tags.clone(),
        source_family,
        question_type,
        official_category: official.as_ref().map(|(key, _)| key.clone()),
        official_category_label: official.as_ref().map(|(_, label)| label.clone()),
        total_score: (total * 100.0).round() / 100.0,
        grade: grade(total).into(),
        mqs_precision: (mqs_p * 100.0).round() / 100.0,
        mqs_recall: (mqs_r * 100.0).round() / 100.0,
        mqs_noise_rejection: (mqs_n * 100.0).round() / 100.0,
        aus_step_success: (step_success * 100.0).round() / 100.0,
        aus_assertion_pass: (assertion_pass * 100.0).round() / 100.0,
    }
}

pub fn score_dataset(
    dataset: &ScenarioDataset,
    executions: &HashMap<String, ScenarioExecution>,
) -> BenchmarkReport {
    let mut results = vec![];
    let mut by_diff: HashMap<String, Vec<f64>> = HashMap::new();
    let mut by_tag: HashMap<String, Vec<f64>> = HashMap::new();
    let mut by_domain: HashMap<String, Vec<f64>> = HashMap::new();
    let mut by_family: HashMap<String, Vec<f64>> = HashMap::new();
    let mut by_lme_category: HashMap<String, Vec<f64>> = HashMap::new();
    let mut lme_labels: HashMap<String, String> = HashMap::new();
    let mut by_beam_ability: HashMap<String, Vec<f64>> = HashMap::new();
    let mut beam_labels: HashMap<String, String> = HashMap::new();
    let mut family_labels: HashMap<String, String> = HashMap::new();

    for scenario in &dataset.scenarios {
        let empty = ScenarioExecution {
            _scenario_id: scenario.scenario_id.clone(),
            step_results: vec![],
            assertion_results: vec![],
            error: Some("no execution".into()),
        };
        let exec = executions.get(&scenario.scenario_id).unwrap_or(&empty);
        let result = score_scenario(scenario, exec);
        by_diff
            .entry(scenario.difficulty.clone())
            .or_default()
            .push(result.total_score);
        if !scenario.domain.is_empty() {
            by_domain
                .entry(scenario.domain.clone())
                .or_default()
                .push(result.total_score);
        }
        for tag in &scenario.tags {
            by_tag
                .entry(tag.clone())
                .or_default()
                .push(result.total_score);
        }
        if let Some(family) = result.source_family.as_ref() {
            by_family
                .entry(family.clone())
                .or_default()
                .push(result.total_score);
            family_labels
                .entry(family.clone())
                .or_insert_with(|| match family.as_str() {
                    "longmemeval" => "LongMemEval".into(),
                    "beam" => "BEAM".into(),
                    _ => family.clone(),
                });
        }
        if let (Some(family), Some(category), Some(label)) = (
            result.source_family.as_ref(),
            result.official_category.as_ref(),
            result.official_category_label.as_ref(),
        ) {
            if family == "longmemeval" {
                by_lme_category
                    .entry(category.clone())
                    .or_default()
                    .push(result.total_score);
                lme_labels
                    .entry(category.clone())
                    .or_insert_with(|| label.clone());
            } else if family == "beam" {
                by_beam_ability
                    .entry(category.clone())
                    .or_default()
                    .push(result.total_score);
                beam_labels
                    .entry(category.clone())
                    .or_insert_with(|| label.clone());
            }
        }
        results.push(result);
    }

    let avg = |v: &[f64]| {
        if v.is_empty() {
            0.0
        } else {
            v.iter().sum::<f64>() / v.len() as f64
        }
    };
    let all: Vec<f64> = results.iter().map(|r| r.total_score).collect();
    let overall = avg(&all);

    BenchmarkReport {
        dataset_id: dataset.dataset_id.clone(),
        version: dataset.version.clone(),
        scenario_count: results.len(),
        overall_score: (overall * 100.0).round() / 100.0,
        overall_grade: grade(overall).into(),
        by_difficulty: by_diff
            .iter()
            .map(|(k, v)| (k.clone(), (avg(v) * 100.0).round() / 100.0))
            .collect(),
        by_tag: by_tag
            .iter()
            .map(|(k, v)| (k.clone(), (avg(v) * 100.0).round() / 100.0))
            .collect(),
        by_domain: by_domain
            .iter()
            .map(|(k, v)| (k.clone(), (avg(v) * 100.0).round() / 100.0))
            .collect(),
        by_source_family: category_breakdown(&by_family, &family_labels),
        by_longmemeval_category: category_breakdown(&by_lme_category, &lme_labels),
        by_beam_ability: category_breakdown(&by_beam_ability, &beam_labels),
        results,
    }
}
