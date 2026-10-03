//! Quote policy — SPEC §2 Operator policy: "GPU classes, per-class PRICE_BPS
//! multipliers, idle/full utilization curve, TEE premium — same composable
//! policy as the sandbox operator's `/api/quote` (ai-agent-sandbox-blueprint#177)".
//!
//! Hot-reloadable from env/config. NEVER a contract change. The signed RFQ
//! quote binds `requester + inputsHash + confidentiality + price` (#1568);
//! this module decides `price` and validates that an on-chain intent's price
//! is still within operator bounds (fail-closed, like the vault's create).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum QuoteValidationError {
    #[error("unknown gpu class {0}")]
    UnknownClass(String),
    #[error("price {actual} wei/s exceeds policy ceiling {ceiling} wei/s for class {class}")]
    PriceAboveCeiling {
        class: String,
        actual: u128,
        ceiling: u128,
    },
    #[error("duration {requested}s out of policy bounds [{min},{max}]")]
    DurationOutOfBounds { requested: u64, min: u64, max: u64 },
}

/// The priced intent — the off-chain half of the RFQ quote.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuoteInputs {
    pub gpu_class: String,
    pub duration_seconds: u64,
    /// 0 = no TEE binding; 1 = TEE required (quote is structurally
    /// unservable by non-TEE operators — #1568).
    pub confidentiality: u8,
    /// Current utilization of the class, in basis points [0, 10000].
    pub utilization_bps: u32,
}

/// The computed quote — the fields the operator signs in the RFQ flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuLeaseQuote {
    pub gpu_class: String,
    pub price_per_second: u128,
    pub duration_seconds: u64,
    pub confidentiality: u8,
    pub escrow_total: u128,
    pub schema_version: u16,
}

/// Composable pricing policy. All knobs are bps (1/10_000) so operators tune
/// behavior without touching code paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotePolicy {
    /// Base price per class, wei/second.
    pub base_price_per_second: BTreeMap<String, u128>,
    /// Per-class multiplier, bps (10_000 = neutral).
    pub class_bps: BTreeMap<String, u32>,
    /// TEE premium, bps.
    pub tee_premium_bps: u32,
    /// Utilization curve: (utilization_bps, price_factor_bps), piecewise-
    /// linear. Idle capacity discounts, saturated capacity premiums.
    pub utilization_curve: Vec<(u32, u32)>,
    /// Duration bounds the operator will quote for.
    pub min_duration_seconds: u64,
    pub max_duration_seconds: u64,
}

impl Default for QuotePolicy {
    fn default() -> Self {
        Self {
            base_price_per_second: BTreeMap::from([
                ("a100-80gb".to_string(), 120_000_000_000_000u128), // gwei-scale/s
                ("h100".to_string(), 300_000_000_000_000u128),
                ("h100-tee".to_string(), 300_000_000_000_000u128),
                ("b200".to_string(), 900_000_000_000_000u128),
            ]),
            class_bps: BTreeMap::new(),
            tee_premium_bps: 2_500, // +25% for TEE-bound confidentiality
            utilization_curve: vec![
                (0, 8_000),       // idle: -20%
                (5_000, 10_000),  // half: neutral
                (10_000, 12_000), // saturated: +20%
            ],
            min_duration_seconds: 60,
            max_duration_seconds: 30 * 24 * 3600,
        }
    }
}

impl QuotePolicy {
    /// Policy from `GPU_QUOTE_POLICY_JSON`, falling back to defaults —
    /// hot-reloadable operator config (SPEC §2).
    pub fn from_env() -> Self {
        std::env::var("GPU_QUOTE_POLICY_JSON")
            .ok()
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default()
    }

    fn base_price(&self, gpu_class: &str) -> Result<u128, QuoteValidationError> {
        self.base_price_per_second
            .get(gpu_class)
            .copied()
            .ok_or_else(|| QuoteValidationError::UnknownClass(gpu_class.to_string()))
    }

    fn utilization_factor_bps(&self, utilization_bps: u32) -> u32 {
        let u = utilization_bps.min(10_000);
        let curve = &self.utilization_curve;
        if curve.is_empty() {
            return 10_000;
        }
        // Piecewise-linear interpolation over sorted breakpoints.
        let mut points = curve.clone();
        points.sort();
        for window in points.windows(2) {
            let (u0, f0) = (window[0].0, window[0].1);
            let (u1, f1) = (window[1].0, window[1].1);
            if u0 <= u && u <= u1 {
                if u1 == u0 {
                    return f0;
                }
                return f0 + (f1 - f0) * (u - u0) / (u1 - u0);
            }
        }
        if u < points[0].0 {
            points[0].1
        } else {
            points[points.len() - 1].1
        }
    }

    /// Price a quote request: base × class_bps × utilization × tee_premium.
    /// Pure function — deterministic, unit-pinned.
    pub fn quote(&self, inputs: &QuoteInputs) -> Result<GpuLeaseQuote, QuoteValidationError> {
        if inputs.duration_seconds < self.min_duration_seconds
            || inputs.duration_seconds > self.max_duration_seconds
        {
            return Err(QuoteValidationError::DurationOutOfBounds {
                requested: inputs.duration_seconds,
                min: self.min_duration_seconds,
                max: self.max_duration_seconds,
            });
        }
        let base = self.base_price(&inputs.gpu_class)?;
        let class_bps = self
            .class_bps
            .get(&inputs.gpu_class)
            .copied()
            .unwrap_or(10_000);
        let util_bps = self.utilization_factor_bps(inputs.utilization_bps);
        let tee_bps = if inputs.confidentiality > 0 {
            10_000 + self.tee_premium_bps
        } else {
            10_000
        };
        // bps composition: three factors of 10_000, applied in u128.
        let price = base
            .saturating_mul(u128::from(class_bps))
            .saturating_mul(u128::from(util_bps))
            .saturating_mul(u128::from(tee_bps))
            / u128::from(10_000u32)
            / u128::from(10_000u32)
            / u128::from(10_000u32);
        Ok(GpuLeaseQuote {
            gpu_class: inputs.gpu_class.clone(),
            price_per_second: price,
            duration_seconds: inputs.duration_seconds,
            confidentiality: inputs.confidentiality,
            escrow_total: price.saturating_mul(u128::from(inputs.duration_seconds)),
            schema_version: crate::SCHEMA_VERSION,
        })
    }

    /// Fail-closed check that an on-chain intent's redeemed price is still
    /// within operator bounds (mirrors the vault's own re-verification).
    pub fn validate_redeemed_price(
        &self,
        gpu_class: &str,
        price_per_second: u128,
    ) -> Result<(), QuoteValidationError> {
        // Ceiling: base × class multiplier × saturated utilization × TEE premium.
        let ceiling_inputs = QuoteInputs {
            gpu_class: gpu_class.to_string(),
            duration_seconds: self.min_duration_seconds,
            confidentiality: 1,
            utilization_bps: 10_000,
        };
        let ceiling = self.quote(&ceiling_inputs)?.price_per_second;
        if price_per_second > ceiling {
            return Err(QuoteValidationError::PriceAboveCeiling {
                class: gpu_class.to_string(),
                actual: price_per_second,
                ceiling,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle(class: &str, dur: u64) -> QuoteInputs {
        QuoteInputs {
            gpu_class: class.into(),
            duration_seconds: dur,
            confidentiality: 0,
            utilization_bps: 0,
        }
    }

    #[test]
    fn unknown_class_fails_closed() {
        let p = QuotePolicy::default();
        assert!(matches!(
            p.quote(&idle("v100", 3600)),
            Err(QuoteValidationError::UnknownClass(_))
        ));
    }

    #[test]
    fn duration_bounds_enforced() {
        let p = QuotePolicy::default();
        assert!(matches!(
            p.quote(&idle("h100", 1)),
            Err(QuoteValidationError::DurationOutOfBounds { .. })
        ));
    }

    #[test]
    fn idle_discount_and_saturation_premium() {
        let p = QuotePolicy::default();
        let q_idle = p.quote(&idle("h100", 3600)).unwrap();
        let mut sat = idle("h100", 3600);
        sat.utilization_bps = 10_000;
        let q_sat = p.quote(&sat).unwrap();
        assert_eq!(q_idle.price_per_second * 3 / 2, q_sat.price_per_second);
        // Idle = base × 8000/10000 exactly.
        let base = p.base_price("h100").unwrap();
        assert_eq!(q_idle.price_per_second, base * 8_000 / 10_000);
    }

    #[test]
    fn utilization_interpolates_midpoint() {
        let p = QuotePolicy::default();
        let mut half = idle("h100", 3600);
        half.utilization_bps = 5_000;
        let q = p.quote(&half).unwrap();
        let base = p.base_price("h100").unwrap();
        assert_eq!(q.price_per_second, base, "half utilization is neutral");
    }

    #[test]
    fn tee_premium_applies_only_when_bound() {
        let p = QuotePolicy::default();
        let mut tee = idle("h100-tee", 3600);
        tee.confidentiality = 1;
        let plain = p.quote(&idle("h100-tee", 3600)).unwrap();
        let teed = p.quote(&tee).unwrap();
        assert_eq!(
            teed.price_per_second,
            plain.price_per_second * 12_500 / 10_000
        );
    }

    #[test]
    fn escrow_is_price_times_duration() {
        let p = QuotePolicy::default();
        let q = p.quote(&idle("h100", 7200)).unwrap();
        assert_eq!(q.escrow_total, q.price_per_second * 7200);
    }

    #[test]
    fn redeemed_price_ceiling_fails_closed() {
        let p = QuotePolicy::default();
        let ceiling = p
            .quote(&QuoteInputs {
                gpu_class: "h100".into(),
                duration_seconds: 60,
                confidentiality: 1,
                utilization_bps: 10_000,
            })
            .unwrap()
            .price_per_second;
        assert!(p.validate_redeemed_price("h100", ceiling).is_ok());
        assert!(matches!(
            p.validate_redeemed_price("h100", ceiling + 1),
            Err(QuoteValidationError::PriceAboveCeiling { .. })
        ));
    }
}
