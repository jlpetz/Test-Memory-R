//! Bounded reproductions, appended only to the copied results.rs.
use super::*;

fn empty_result() -> TestRunResult {
    TestRunResult {
        metadata: TestRunMetadata {
            tmr_version: "review".into(),
            start_time: 0,
            start_time_utc: String::new(),
            start_time_local: String::new(),
            filename: String::new(),
            config_name: None,
        },
        identity: Default::default(),
        run_config: Default::default(),
        cycles: Vec::new(),
        overall_stats: OverallStats {
            total_runtime_secs: 0,
            cycles_completed: 0,
            total_data_processed_gib: 0.0,
            overall_throughput_mib_s: 0.0,
            total_errors: 0,
            whea_total: 0,
            whea_corrected: 0,
            whea_monitored: false,
            seal_errors: 0,
            per_step_averages: Vec::new(),
        },
    }
}

fn skipped_step() -> TestSummary {
    TestSummary {
        step: 1, test_index: 0, id: None,
        name: "Mem-SimpleV2".into(), label: "review".into(),
        duration_ms: 0, bytes_processed: 0, throughput_mib_s: 0.0,
        errors: 0, alarms: 0, not_reproduced: 0, seal_errors: 1,
        whea_total: 0, whea_corrected: 0,
        latency_samples: None, latency_p5_ns: None, latency_p10_ns: None,
        latency_p25_ns: None, latency_p50_ns: None, latency_p75_ns: None,
        latency_p90_ns: None, latency_p95_ns: None, latency_p99_ns: None,
        latency_p99_9_ns: None, latency_spread: None,
    }
}

#[test]
fn review_repro_halt_before_step_panics_while_finalizing() {
    let mut result = empty_result();
    // Exactly the new runner's entry when a between-step seal check halts
    // the run before this step's first execution.
    result.add_cycle(1, 1, vec![skipped_step()], Some(CycleSeal::default()));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        result.finalize(std::time::Duration::from_secs(1));
    }));
    assert!(outcome.is_err(), "the reproduction should be retired after fixing zero-run aggregation");
    println!("REPRO: first-cycle seal halt -> finalize panics (zero-run division), before result save.");
}

#[test]
fn review_halt_after_a_previously_completed_step_does_not_hit_zero_runs() {
    let mut result = empty_result();
    let mut complete = skipped_step();
    complete.duration_ms = 1000;
    complete.bytes_processed = 4096;
    complete.throughput_mib_s = 4096.0 / (1024.0 * 1024.0);
    complete.seal_errors = 0;
    result.add_cycle(1, 1, vec![complete], None);
    result.add_cycle(2, 1, vec![skipped_step()], None);
    result.finalize(std::time::Duration::from_secs(2));
    assert_eq!(result.overall_stats.per_step_averages[0].runs, 1);
    assert_eq!(result.overall_stats.total_errors, 1);
    println!("PASS: same path after an earlier completed execution retains one run and one seal error.");
}
