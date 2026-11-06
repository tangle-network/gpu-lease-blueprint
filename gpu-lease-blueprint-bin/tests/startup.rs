use std::process::Command;

#[test]
fn policy_extrema_are_validated_before_startup() {
    let policy = r#"{"base_price_per_second":{"nvidia-h100":BASE},"class_bps":{"nvidia-h100":FACTOR},"tee_premium_bps":2500,"utilization_curve":[[0,8000],[5000,10000],[10000,12000]],"min_duration_seconds":60,"max_duration_seconds":3600}"#;
    for (base, factor, expected) in [
        (
            "1",
            "10000",
            "GPU quote policy produces zero price or overflows escrow",
        ),
        (
            "2",
            "1",
            "GPU quote policy produces zero price or overflows escrow",
        ),
        (
            "340282366920938463463374607431768211455",
            "10000",
            "GPU quote policy produces zero price or overflows escrow",
        ),
        ("2", "10000", "GPU_INVENTORY_JSON is required"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_gpu-lease-blueprint"))
            .env_clear()
            .env(
                "GPU_QUOTE_POLICY_JSON",
                policy.replace("BASE", base).replace("FACTOR", factor),
            )
            .output()
            .expect("operator CLI must start");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "operator accepted incomplete config"
        );
        assert!(
            stderr.contains(expected),
            "base={base}, factor={factor}: {stderr}"
        );
    }
}
