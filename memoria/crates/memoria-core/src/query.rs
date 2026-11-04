//! Typed, bounded selectors shared by all memory retrieval paths.

use crate::MemoriaError;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MetadataOperator {
    Equal,
    Contains,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MetadataPredicate {
    /// Dot-separated object keys, not an arbitrary JSONPath expression.
    pub path: String,
    pub operator: MetadataOperator,
    pub value: Value,
}

/// Bounded object-key selector shared by retrieval filters and memory SQL.
/// This intentionally does not accept recursive or wildcard JSONPath.
pub fn valid_metadata_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 128
        && path.split('.').all(|key| {
            let mut chars = key.chars();
            chars
                .next()
                .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
                && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        })
        && !path.split('.').next().is_some_and(crate::is_reserved_metadata_key)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MemoryFilter {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metadata: Vec<MetadataPredicate>,
    /// Inclusive source observation time, rather than the database write time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at_gte: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at_lte: Option<DateTime<Utc>>,
    /// Explicitly the initial confidence, not a decayed or reranking score.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_initial_confidence: Option<f64>,
}

impl MemoryFilter {
    pub fn is_empty(&self) -> bool {
        self.metadata.is_empty()
            && self.observed_at_gte.is_none()
            && self.observed_at_lte.is_none()
            && self.min_initial_confidence.is_none()
    }

    pub fn validate(&self) -> Result<(), MemoriaError> {
        if self.metadata.len() > 16 {
            return Err(MemoriaError::Validation(
                "filter.metadata supports at most 16 predicates".into(),
            ));
        }
        for predicate in &self.metadata {
            if !valid_metadata_path(&predicate.path) {
                return Err(MemoriaError::Validation(
                    "filter metadata path must contain bounded object keys, not JSONPath".into(),
                ));
            }
            if !matches!(
                predicate.value,
                Value::String(_) | Value::Number(_) | Value::Bool(_)
            ) || serde_json::to_vec(&predicate.value)?.len() > 1024
            {
                return Err(MemoriaError::Validation(
                    "filter metadata value must be a scalar of at most 1024 JSON bytes".into(),
                ));
            }
        }
        if matches!((self.observed_at_gte, self.observed_at_lte), (Some(from), Some(to)) if from > to)
        {
            return Err(MemoriaError::Validation(
                "filter observation time range is reversed".into(),
            ));
        }
        if self
            .min_initial_confidence
            .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
        {
            return Err(MemoriaError::Validation(
                "filter.min_initial_confidence must be finite and between 0 and 1".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accepts_nested_equal_array_membership_and_explicit_time_confidence() {
        let filter: MemoryFilter = serde_json::from_value(json!({
            "metadata": [
                {"path": "labels.scene", "operator": "equal", "value": "support"},
                {"path": "labels.scene_tags", "operator": "contains", "value": "follow_up"}
            ],
            "observed_at_gte": "2026-11-03T12:00:00+08:00",
            "observed_at_lte": "2026-11-03T04:00:00Z",
            "min_initial_confidence": 0.8
        }))
        .unwrap();
        filter.validate().unwrap();
        assert!(!filter.is_empty());
        assert_eq!(filter.observed_at_gte, filter.observed_at_lte);
        assert_eq!(filter.min_initial_confidence, Some(0.8));
        assert_eq!(filter.metadata[1].operator, MetadataOperator::Contains);
        assert_eq!(filter.metadata[1].value, "follow_up");
    }

    #[test]
    fn rejects_unsafe_paths_and_non_scalar_operands() {
        for path in [
            "",
            "labels..scene",
            "$.labels.scene",
            "labels[*]",
            "x' OR 1=1",
            "labels.scene;drop",
            crate::STORE_REQUEST_HASH_FIELD,
        ] {
            let filter = MemoryFilter {
                metadata: vec![MetadataPredicate {
                    path: path.into(),
                    operator: MetadataOperator::Equal,
                    value: json!("value"),
                }],
                ..Default::default()
            };
            assert!(filter.validate().is_err(), "accepted path {path:?}");
        }
        for value in [Value::Null, json!({"x": 1}), json!(["x"])] {
            let filter = MemoryFilter {
                metadata: vec![MetadataPredicate {
                    path: "labels.scene".into(),
                    operator: MetadataOperator::Contains,
                    value,
                }],
                ..Default::default()
            };
            assert!(filter.validate().is_err());
        }
    }

    #[test]
    fn rejects_reversed_time_range_and_invalid_confidence() {
        let reversed: MemoryFilter = serde_json::from_value(json!({
            "observed_at_gte": "2026-11-04T00:00:00Z", "observed_at_lte": "2026-11-03T00:00:00Z"
        }))
        .unwrap();
        assert!(reversed.validate().is_err());
        for value in [f64::NAN, f64::INFINITY, -0.01, 1.01] {
            let filter = MemoryFilter {
                min_initial_confidence: Some(value),
                ..Default::default()
            };
            assert!(filter.validate().is_err());
        }
        assert!(serde_json::from_value::<MemoryFilter>(
            json!({"observed_at_gte": "2026-11-03T00:00:00"})
        )
        .is_err());
        assert!(serde_json::from_value::<MemoryFilter>(json!({"unknown": true})).is_err());
        assert!(serde_json::from_value::<MetadataPredicate>(
            json!({"path": "x", "operator": "sql", "value": "x"})
        )
        .is_err());
    }
}
