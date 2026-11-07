//! Quote policy — SPEC §2 Operator policy: "GPU classes, per-class PRICE_BPS
//! multipliers, idle/full utilization curve, TEE premium — same composable
//! policy as the sandbox operator's `/api/quote` (ai-agent-sandbox-blueprint#177)".
//!
//! Hot-reloadable from env/config. NEVER a contract change. The signed RFQ
//! quote binds `requester + inputsHash + confidentiality + price` (#1568);
//! this module decides `price`, and LEASE refuses a vault price below the
//! operator's idle quote or outside its duration and device bounds.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum QuoteValidationError {
    #[error("GPU quote price/escrow overflow or zero price")]
    Overflow,
    #[error("unknown gpu class {0}")]
    UnknownClass(String),
    #[error("price {0} wei/s is below the policy floor {1} wei/s")]
    PriceBelowFloor(u128, u128),
    #[error("duration {requested}s out of policy bounds [{min},{max}]")]
    DurationOutOfBounds { requested: u64, min: u64, max: u64 },
    #[error("device count {requested} out of bounds [1,{max}]")]
    DeviceCountOutOfBounds { requested: u16, max: u16 },
}

/// The priced intent — the off-chain half of the RFQ quote.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuoteInputs {
    pub gpu_class: String,
    pub duration_seconds: u64,
    /// 0 = no TEE binding; 1 = TEE required (quote is structurally
    /// unservable by non-TEE operators — #1568).
    pub confidentiality: u8,
    /// Devices rented under the lease — escrow scales per device.
    #[serde(default = "one_device")]
    pub device_count: u16,
    /// Current utilization of the class, in basis points [0, 10000].
    pub utilization_bps: u32,
}

fn one_device() -> u16 {
    1
}

/// The computed quote — the fields the operator signs in the RFQ flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuLeaseQuote {
    pub gpu_class: String,
    pub price_per_second: u128,
    pub duration_seconds: u64,
    pub confidentiality: u8,
    pub device_count: u16,
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
            base_price_per_second: BTreeMap::new(),
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
    /// Load required policy and validate the lowest price and highest escrow.
    pub fn from_env() -> Result<Self, String> {
        let policy: Self = serde_json::from_str(
            &std::env::var("GPU_QUOTE_POLICY_JSON")
                .map_err(|_| "GPU_QUOTE_POLICY_JSON is required")?,
        )
        .map_err(|_| "invalid GPU_QUOTE_POLICY_JSON")?;
        if policy.base_price_per_second.is_empty()
            || policy.base_price_per_second.values().any(|p| *p == 0)
            || policy.min_duration_seconds == 0
            || policy.min_duration_seconds > policy.max_duration_seconds
            || policy.tee_premium_bps > 100_000
            || policy.class_bps.values().any(|v| *v == 0 || *v > 100_000)
            || policy
                .utilization_curve
                .iter()
                .any(|(u, f)| *u > 10_000 || *f == 0 || *f > 100_000)
            || policy
                .utilization_curve
                .windows(2)
                .any(|w| w[0].0 >= w[1].0 || w[0].1 > w[1].1)
        {
            return Err("invalid GPU quote policy bounds".into());
        }
        for class in policy.base_price_per_second.keys() {
            for (utilization_bps, confidentiality) in [(0, 0), (10_000, 1)] {
                policy
                    .quote(&QuoteInputs {
                        gpu_class: class.clone(),
                        duration_seconds: policy.max_duration_seconds,
                        confidentiality,
                        device_count: crate::MAX_DEVICE_COUNT_PER_LEASE,
                        utilization_bps,
                    })
                    .map_err(|_| "GPU quote policy produces zero price or overflows escrow")?;
            }
        }
        Ok(policy)
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

    /// LEASE admission: the lessee-chosen vault price must reach this operator's
    /// idle quote, which also enforces the duration and device-count bounds.
    pub fn require_lease_price(
        &self,
        r: &crate::GpuLeaseRequest,
    ) -> Result<(), QuoteValidationError> {
        let floor = self.quote(&QuoteInputs {
            gpu_class: r.gpuClass.clone(),
            duration_seconds: r.durationSeconds,
            confidentiality: r.confidentiality,
            device_count: r.deviceCount,
            utilization_bps: 0,
        })?;
        if r.pricePerSecond < floor.price_per_second {
            return Err(QuoteValidationError::PriceBelowFloor(
                r.pricePerSecond,
                floor.price_per_second,
            ));
        }
        Ok(())
    }

    /// Price a quote request: base × class_bps × utilization × tee_premium.
    /// Pure function — deterministic, unit-pinned. Escrow is per-device price
    /// × duration × deviceCount (the vault re-verifies the same product).
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
        if inputs.device_count == 0 || inputs.device_count > crate::MAX_DEVICE_COUNT_PER_LEASE {
            return Err(QuoteValidationError::DeviceCountOutOfBounds {
                requested: inputs.device_count,
                max: crate::MAX_DEVICE_COUNT_PER_LEASE,
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
            .checked_mul(u128::from(class_bps))
            .and_then(|v| v.checked_mul(u128::from(util_bps)))
            .and_then(|v| v.checked_mul(u128::from(tee_bps)))
            .ok_or(QuoteValidationError::Overflow)?
            / u128::from(10_000u32)
            / u128::from(10_000u32)
            / u128::from(10_000u32);
        if price == 0 {
            return Err(QuoteValidationError::Overflow);
        }
        Ok(GpuLeaseQuote {
            gpu_class: inputs.gpu_class.clone(),
            price_per_second: price,
            duration_seconds: inputs.duration_seconds,
            confidentiality: inputs.confidentiality,
            device_count: inputs.device_count,
            escrow_total: price
                .checked_mul(u128::from(inputs.duration_seconds))
                .and_then(|v| v.checked_mul(u128::from(inputs.device_count)))
                .ok_or(QuoteValidationError::Overflow)?,
            schema_version: crate::SCHEMA_VERSION,
        })
    }
}
