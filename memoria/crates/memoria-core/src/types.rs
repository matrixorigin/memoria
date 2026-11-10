use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Memory type — must have exactly 6 variants matching Python implementation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryType {
    Semantic,
    Working,
    Episodic,
    Profile,
    ToolResult,
    Procedural,
}

impl MemoryType {
    /// Canonical set of all memory-type names as lowercase strings.
    /// Used by metrics validation to prevent bucket drift.
    pub const ALL_NAMES: &[&str] = &[
        "semantic",
        "working",
        "episodic",
        "profile",
        "tool_result",
        "procedural",
    ];
}

/// Canonical feedback signal names accepted by the product.
/// Must stay in sync with `FeedbackStats` fields in `memoria-storage`
/// and the `record_feedback` API in `memoria-service`.
pub const FEEDBACK_SIGNALS: &[&str] = &["useful", "irrelevant", "outdated", "wrong"];

impl std::fmt::Display for MemoryType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            MemoryType::Semantic => "semantic",
            MemoryType::Working => "working",
            MemoryType::Episodic => "episodic",
            MemoryType::Profile => "profile",
            MemoryType::ToolResult => "tool_result",
            MemoryType::Procedural => "procedural",
        };
        write!(f, "{s}")
    }
}

impl std::str::FromStr for MemoryType {
    type Err = crate::MemoriaError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "semantic" => Ok(MemoryType::Semantic),
            "working" => Ok(MemoryType::Working),
            "episodic" => Ok(MemoryType::Episodic),
            "profile" => Ok(MemoryType::Profile),
            "tool_result" => Ok(MemoryType::ToolResult),
            "procedural" => Ok(MemoryType::Procedural),
            other => Err(crate::MemoriaError::InvalidMemoryType(other.to_string())),
        }
    }
}

/// Trust tier — T1 (verified) → T4 (unverified).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TrustTier {
    #[serde(rename = "T1")]
    T1Verified,
    #[serde(rename = "T2")]
    T2Curated,
    #[serde(rename = "T3")]
    #[default]
    T3Inferred,
    #[serde(rename = "T4")]
    T4Unverified,
}

impl TrustTier {
    pub fn default_half_life_days(&self) -> f64 {
        match self {
            TrustTier::T1Verified => 365.0,
            TrustTier::T2Curated => 180.0,
            TrustTier::T3Inferred => 60.0,
            TrustTier::T4Unverified => 30.0,
        }
    }

    pub fn initial_confidence(&self) -> f64 {
        match self {
            TrustTier::T1Verified => 0.95,
            TrustTier::T2Curated => 0.85,
            TrustTier::T3Inferred => 0.65,
            TrustTier::T4Unverified => 0.40,
        }
    }
}

impl std::fmt::Display for TrustTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            TrustTier::T1Verified => "T1",
            TrustTier::T2Curated => "T2",
            TrustTier::T3Inferred => "T3",
            TrustTier::T4Unverified => "T4",
        };
        write!(f, "{s}")
    }
}

impl std::str::FromStr for TrustTier {
    type Err = crate::MemoriaError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "T1" => Ok(TrustTier::T1Verified),
            "T2" => Ok(TrustTier::T2Curated),
            "T3" => Ok(TrustTier::T3Inferred),
            "T4" => Ok(TrustTier::T4Unverified),
            other => Err(crate::MemoriaError::InvalidTrustTier(other.to_string())),
        }
    }
}

/// Core memory record — mirrors Python `Memory` dataclass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub memory_id: String,
    pub user_id: String,
    /// Real author in group mode (the human who created the memory).
    /// `None` in personal mode (user_id is already the author).
    pub author_id: Option<String>,
    /// Stable business ID of the memory subject (e.g. end-user, patient).
    /// Set by the MOI extraction worker; `None` for memories without subject binding.
    pub subject_id: Option<String>,
    pub memory_type: MemoryType,
    pub content: String,
    pub initial_confidence: f64,
    pub embedding: Option<Vec<f32>>,
    pub source_event_ids: Vec<String>,
    pub superseded_by: Option<String>,
    pub is_active: bool,
    pub access_count: i32,
    pub session_id: Option<String>,
    pub observed_at: Option<DateTime<Utc>>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub extra_metadata: Option<HashMap<String, serde_json::Value>>,
    pub trust_tier: TrustTier,
    /// Set by retriever; None if not retrieved via scoring.
    pub retrieval_score: Option<f64>,
}

impl Memory {
    /// Confidence decay: C(t) = C0 * exp(-age_days / half_life).
    pub fn effective_confidence(&self, half_life_days: Option<f64>) -> f64 {
        let Some(observed_at) = self.observed_at else {
            return self.initial_confidence;
        };
        let half_life = half_life_days.unwrap_or_else(|| self.trust_tier.default_half_life_days());
        let age_days = (Utc::now() - observed_at).num_seconds() as f64 / 86400.0;
        self.initial_confidence * (-age_days / half_life).exp()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_type_has_six_variants() {
        let types = [
            MemoryType::Semantic,
            MemoryType::Working,
            MemoryType::Episodic,
            MemoryType::Profile,
            MemoryType::ToolResult,
            MemoryType::Procedural,
        ];
        assert_eq!(types.len(), 6);
    }

    #[test]
    fn all_names_matches_display() {
        let display_names: Vec<String> = [
            MemoryType::Semantic,
            MemoryType::Working,
            MemoryType::Episodic,
            MemoryType::Profile,
            MemoryType::ToolResult,
            MemoryType::Procedural,
        ]
        .iter()
        .map(|t| t.to_string())
        .collect();
        let mut sorted_display: Vec<&str> = display_names.iter().map(|s| s.as_str()).collect();
        sorted_display.sort();
        let mut sorted_all: Vec<&str> = MemoryType::ALL_NAMES.to_vec();
        sorted_all.sort();
        assert_eq!(
            sorted_all, sorted_display,
            "MemoryType::ALL_NAMES drifted from Display impl"
        );
    }

    #[test]
    fn feedback_signals_is_non_empty() {
        assert!(!super::FEEDBACK_SIGNALS.is_empty());
        for s in super::FEEDBACK_SIGNALS {
            assert!(!s.is_empty(), "empty string in FEEDBACK_SIGNALS");
        }
    }

    #[test]
    fn test_memory_type_roundtrip() {
        for (s, expected) in [
            ("semantic", MemoryType::Semantic),
            ("working", MemoryType::Working),
            ("episodic", MemoryType::Episodic),
            ("profile", MemoryType::Profile),
            ("tool_result", MemoryType::ToolResult),
            ("procedural", MemoryType::Procedural),
        ] {
            let parsed: MemoryType = s.parse().unwrap();
            assert_eq!(parsed, expected);
            assert_eq!(parsed.to_string(), s);
        }
    }

    #[test]
    fn test_trust_tier_roundtrip() {
        for (s, expected) in [
            ("T1", TrustTier::T1Verified),
            ("T2", TrustTier::T2Curated),
            ("T3", TrustTier::T3Inferred),
            ("T4", TrustTier::T4Unverified),
        ] {
            let parsed: TrustTier = s.parse().unwrap();
            assert_eq!(parsed, expected);
            assert_eq!(parsed.to_string(), s);
        }
    }

    #[test]
    fn memory_type_parse_is_exact_and_reports_input() {
        for bad in [
            "",
            "Semantic",
            "SEMANTIC",
            " semantic",
            "semantic ",
            "tool-result",
            "toolresult",
            "unknown",
        ] {
            match bad.parse::<MemoryType>() {
                Err(crate::MemoriaError::InvalidMemoryType(got)) => assert_eq!(got, bad),
                other => panic!("{bad:?} parsed as {other:?}"),
            }
        }
    }

    #[test]
    fn trust_tier_parse_is_exact_and_reports_input() {
        for bad in ["", "t1", "T0", "T5", "T1 ", "verified", "T1Verified"] {
            match bad.parse::<TrustTier>() {
                Err(crate::MemoriaError::InvalidTrustTier(got)) => assert_eq!(got, bad),
                other => panic!("{bad:?} parsed as {other:?}"),
            }
        }
    }

    #[test]
    fn serde_names_match_display_names() {
        for name in MemoryType::ALL_NAMES {
            let parsed: MemoryType = name.parse().unwrap();
            assert_eq!(
                serde_json::to_value(&parsed).unwrap(),
                serde_json::json!(name)
            );
            let back: MemoryType = serde_json::from_value(serde_json::json!(name)).unwrap();
            assert_eq!(back, parsed);
        }
        for tier in ["T1", "T2", "T3", "T4"] {
            let parsed: TrustTier = tier.parse().unwrap();
            assert_eq!(
                serde_json::to_value(&parsed).unwrap(),
                serde_json::json!(tier)
            );
            let back: TrustTier = serde_json::from_value(serde_json::json!(tier)).unwrap();
            assert_eq!(back, parsed);
        }
        assert!(serde_json::from_value::<MemoryType>(serde_json::json!("ToolResult")).is_err());
        assert!(serde_json::from_value::<TrustTier>(serde_json::json!("T1Verified")).is_err());
    }

    #[test]
    fn trust_tier_defaults_and_ordering_of_parameters() {
        assert_eq!(TrustTier::default(), TrustTier::T3Inferred);
        let tiers = [
            TrustTier::T1Verified,
            TrustTier::T2Curated,
            TrustTier::T3Inferred,
            TrustTier::T4Unverified,
        ];
        // Higher trust must never decay faster or start less confident.
        for pair in tiers.windows(2) {
            assert!(pair[0].default_half_life_days() > pair[1].default_half_life_days());
            assert!(pair[0].initial_confidence() > pair[1].initial_confidence());
        }
        for tier in tiers {
            let c = tier.initial_confidence();
            assert!((0.0..=1.0).contains(&c), "{tier}: {c}");
            assert!(tier.default_half_life_days() > 0.0);
        }
    }

    fn memory_observed(observed_at: Option<DateTime<Utc>>, tier: TrustTier) -> Memory {
        Memory {
            memory_id: "m1".into(),
            user_id: "u1".into(),
            author_id: None,
            subject_id: None,
            memory_type: MemoryType::Semantic,
            content: "c".into(),
            initial_confidence: 0.8,
            embedding: None,
            source_event_ids: vec![],
            superseded_by: None,
            is_active: true,
            access_count: 0,
            session_id: None,
            observed_at,
            created_at: None,
            updated_at: None,
            extra_metadata: None,
            trust_tier: tier,
            retrieval_score: None,
        }
    }

    #[test]
    fn effective_confidence_without_observation_time_is_initial() {
        let m = memory_observed(None, TrustTier::T4Unverified);
        assert_eq!(m.effective_confidence(None), 0.8);
        assert_eq!(m.effective_confidence(Some(1.0)), 0.8);
    }

    #[test]
    fn effective_confidence_decays_with_tier_or_explicit_half_life() {
        let close = |a: f64, b: f64| (a - b).abs() < 1e-3;
        // Just observed: no measurable decay.
        let fresh = memory_observed(Some(Utc::now()), TrustTier::T1Verified);
        assert!(close(fresh.effective_confidence(None), 0.8));

        // 30 days old with the T4 default (30 days): C0 * e^-1.
        let t4 = memory_observed(
            Some(Utc::now() - chrono::Duration::days(30)),
            TrustTier::T4Unverified,
        );
        assert!(close(t4.effective_confidence(None), 0.8 * (-1.0f64).exp()));
        // An explicit half-life overrides the tier default.
        assert!(close(
            t4.effective_confidence(Some(60.0)),
            0.8 * (-0.5f64).exp()
        ));

        // Same age decays less for a more trusted tier.
        let t1 = memory_observed(
            Some(Utc::now() - chrono::Duration::days(30)),
            TrustTier::T1Verified,
        );
        assert!(t1.effective_confidence(None) > t4.effective_confidence(None));
        assert!(t1.effective_confidence(None) < 0.8);
    }
}
