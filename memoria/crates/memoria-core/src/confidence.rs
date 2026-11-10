use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::MemoriaError;

/// Opt-in confidence lifecycle, independent of retrieval ranking and destructive governance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfidenceDecayPolicy {
    pub enabled: bool,
    pub half_life_days: f64,
    pub inactive_threshold: f64,
}

impl Default for ConfidenceDecayPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            half_life_days: 30.0,
            inactive_threshold: 0.3,
        }
    }
}

impl ConfidenceDecayPolicy {
    pub fn validate(&self) -> Result<(), MemoriaError> {
        if !self.half_life_days.is_finite()
            || self.half_life_days <= 0.0
            || !(self.half_life_days * 86400.0).is_finite()
            || !self.inactive_threshold.is_finite()
            || !(0.0..=1.0).contains(&self.inactive_threshold)
        {
            return Err(MemoriaError::Validation(
                "invalid confidence decay policy".into(),
            ));
        }
        Ok(())
    }

    pub fn effective_confidence(
        &self,
        initial: f64,
        observed_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> f64 {
        let days = (now - observed_at).num_seconds().max(0) as f64 / 86400.0;
        initial * (-days / self.half_life_days).exp2()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn half_life_and_strict_threshold() {
        let p = ConfidenceDecayPolicy {
            enabled: true,
            ..Default::default()
        };
        let now = Utc::now();
        for (days, expected) in [(0, 0.8), (30, 0.4), (60, 0.2)] {
            assert!(
                (p.effective_confidence(0.8, now - chrono::Duration::days(days), now) - expected)
                    .abs()
                    < 1e-12
            );
        }
        assert_eq!(p.effective_confidence(0.3, now, now), p.inactive_threshold);
        assert_eq!(
            p.effective_confidence(0.8, now + chrono::Duration::days(1), now),
            0.8
        );
    }
    #[test]
    fn rejects_invalid_parameters_and_defaults_off() {
        assert!(!ConfidenceDecayPolicy::default().enabled);
        for days in [0.0, -1.0, f64::INFINITY, f64::NAN] {
            assert!(ConfidenceDecayPolicy {
                half_life_days: days,
                ..Default::default()
            }
            .validate()
            .is_err());
        }
        for threshold in [-0.1, 1.1, f64::NAN] {
            assert!(ConfidenceDecayPolicy {
                inactive_threshold: threshold,
                ..Default::default()
            }
            .validate()
            .is_err());
        }
    }
}
