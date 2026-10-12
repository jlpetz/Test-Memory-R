//! The previous halt reproduction must now finalize, serialize and report.
use super::*;

fn summary(step: usize, ran: bool) -> TestSummary {
    TestSummary {
        step, test_index: step - 1, id: None,
        name: "Mem-SimpleV2".into(), label: format!("review {step}"),
        duration_ms: if ran { 1000 } else { 0 },
        bytes_processed: if ran { 4096 } else { 0 },
        throughput_mib_s: if ran { 4096.0 / (1024.0 * 1024.0) } else { 0.0 },
        errors: 0, alarms: 0, not_reproduced: 0,
        seal_errors: if ran { 0 } else { 1 },
        whea_total: 0, whea_corrected: 0,
        latency_samples: None, latency_p5_ns: None, latency_p10_ns: None,
        latency_p25_ns: None, latency_p50_ns: None, latency_p75_ns: None,
        latency_p90_ns: None, latency_p95_ns: None, latency_p99_ns: None,
        latency_p99_9_ns: None, latency_spread: None,
    }
}

#[test]
fn review_halt_before_any_run_saves_and_has_finite_summary() {
    // Include two same-name skipped steps: the console merges their zero-run rows.
    let mut result = TestRunResult::new(Default::default());
    result.add_cycle(1, 1, vec![summary(1, false), summary(2, false)], None);
    result.finalize(std::time::Duration::from_secs(1));
    assert_eq!((result.overall_stats.total_errors, result.overall_stats.seal_errors), (2, 2));
    assert!(result.overall_stats.per_step_averages.iter().all(|a| a.runs == 0));
    let json = serde_json::to_string(&result).unwrap();
    let restored: TestRunResult = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.overall_stats.seal_errors, 2);
    let report = crate::reporting::converters::create_overall_stats_summary_report(&result);
    assert!(report.per_test_summaries.iter().all(|r|
        r.average_duration_secs.is_finite() && r.average_data_gib.is_finite()
        && r.average_throughput_mib_s.is_finite()
    ));
    println!("PASS: pre-execution seal halt finalizes, JSON round-trips and produces finite summary rows.");
}

#[test]
fn review_completed_run_followed_by_skipped_run_keeps_correct_denominator() {
    let mut result = TestRunResult::new(Default::default());
    result.add_cycle(1, 1, vec![summary(1, true)], None);
    result.add_cycle(2, 1, vec![summary(1, false)], None);
    result.finalize(std::time::Duration::from_secs(2));
    let row = &result.overall_stats.per_step_averages[0];
    assert_eq!((row.runs, row.avg_bytes_processed, row.avg_duration_ms, row.total_seal_errors),
               (1, 4096, 1000, 1));
}
