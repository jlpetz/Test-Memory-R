//! Plan-validation tests, appended only to the copied runner.rs.
use super::*;

#[test]
fn review_repro_matching_native_bench_dependency_rejected() {
    // Dependent mode is an internal API today, not a JSON/CLI control.
    let cache = crate::tests::get_cache_info();
    let all = create_test_definitions(cache);
    let mut pair: Vec<_> = ["Bench-Init-Ident", "Bench-Verify-Ident"].iter()
        .map(|name| all.iter().find(|t| t.actual_name == *name).unwrap().clone())
        .collect();
    pair[1].config.skip_init = true;
    let errors = validate_test_plan(&pair);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].1.contains("mode=10") && errors[0].1.contains("mode=300"));
    println!("REPRO (internal API): matching native Bench pair rejected: {}", errors[0].1);
}
