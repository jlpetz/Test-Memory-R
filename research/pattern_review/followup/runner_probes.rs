//! Normalization and dependency checks through production functions.
use super::*;

#[test]
fn review_all_valid_round_counts_normalize_without_losing_pairs() {
    let all = create_test_definitions(crate::tests::get_cache_info());
    let base = all.iter().find(|t| t.actual_name == "Mem-SimpleV2").unwrap().clone();
    for rounds in 1..=10_000 {
        let mut defs = [base.clone()];
        defs[0].config.write_read_cycles = rounds;
        pair_native_rounds(&mut defs);
        assert_eq!(defs[0].config.write_read_cycles, rounds + rounds % 2);
        assert!(defs[0].config.write_read_cycles <= 10_000);
    }
    for special in ["legacy", "verify-only"] {
        let mut defs = [base.clone()];
        defs[0].config.write_read_cycles = 3;
        if special == "legacy" { defs[0].config.pattern_mode = Some(14); }
        else { defs[0].config.test_reps = 0; }
        pair_native_rounds(&mut defs);
        assert_eq!(defs[0].config.write_read_cycles, 3, "{special}");
    }
    println!("PASS: all 10,000 accepted config round counts pair correctly; legacy and verify-only are exempt.");
}

#[test]
fn review_every_native_bench_dependency_matches_only_its_family() {
    let all = create_test_definitions(crate::tests::get_cache_info());
    let get = |name: &str| all.iter().find(|t| t.actual_name == name).unwrap().clone();
    for &(init, verify, _, _) in crate::tests::native_bench_tests().iter() {
        let mut dependent = get(verify);
        dependent.config.skip_init = true;
        assert!(validate_test_plan(&[get(init), dependent.clone()]).is_empty(), "{init} -> {verify}");
        for &(other, _, _, _) in crate::tests::native_bench_tests().iter() {
            if other == init { continue; }
            assert_eq!(validate_test_plan(&[get(other), dependent.clone()]).len(), 1, "{other} -> {verify}");
        }
    }
    println!("PASS: all five matching native Bench pairs accepted; all twenty mismatched pairs rejected.");
}

#[test]
fn review_submillisecond_thread_throughput_uses_duration_precision() {
    let elapsed = std::time::Duration::from_micros(250);
    let report = crate::reporting::converters::create_thread_timing_report(
        &[(0, 0, 1 << 20, elapsed, 0, 0, 1)], Default::default()
    );
    // The per-thread field name is verified against the actual report model.
    let row = &report.thread_timings[0];
    assert_eq!(row.throughput_mib_s, 4000.0);
    println!("PASS: a 250 us interval retains its precision for throughput.");
}
