//! Goldens of the run's own code through the CLI's frontend (TODO 29): a cycle of
//! `execute_test_cycle`, the plan `--startup-debug` prints, and the stops before testing an
//! override can reach, each driven in the engine (`tmr::test_support`); and the order of a run's
//! events against its log records. The engine's tests read the same cycle's results.

use std::sync::Arc;

use tmr::frontend::{ErrorMode, Event, Frontend, RestartChoice, Run, RunStatus};
use tmr::test_support::{default_plan, FakeCycle, Flip, GoldenRun, TestRunOverrides};

use super::CliFrontend;
use crate::golden::{capture, capture_with_debug, check, mask_plan, mask_run, mask_stamps};

/// A cycle as the console shows it (TODO 29): the fake cycle (`FakeCycle`), logging its errors.
/// Times, rates and the scheduler's order are masked (`golden::mask_run`). Nothing printed depends
/// on the run's seed either, which can't be fixed here (a process takes it once, and other tests
/// take it from the clock): mode 10 keys its words by its own base, and the seal, which the seed
/// keys, finds no error to print. A step whose output does depend on it needs a seed of its own
/// first. What the cycle recorded is the engine's test's (`a_cycle_records_each_step_and_its_threads`).
#[test]
fn a_cycle_prints_as_its_golden() {
    let cycle = FakeCycle::new(ErrorMode::Log, Flip::Verify);
    let (ran, text) = capture(|| cycle.run(&CliFrontend::new()));
    assert_eq!(ran.ended(), (None, false), "the cycle ran to its end");
    assert_eq!(ran.errors(), [0, 1, 0, 0], "the flipped bit is the verify's one error");
    check("cycle", &mask_run(&text));
}

/// The console's frontend, as the CLI hands it to the run
fn cli() -> Arc<dyn Frontend> {
    Arc::new(CliFrontend::new())
}

/// A frontend that shows each event as one line, `<Event ...>`, above the ticker as the log
/// records are, so a capture has the events in their order against the records.
struct Recording;

impl Frontend for Recording {
    fn event(&self, event: Event) {
        let Event::Run(run) = event else {
            unreachable!("a run sends only its own events");
        };
        let line = match run {
            Run::MemoryPlan(_) => "MemoryPlan".to_string(),
            Run::NotRun(_) => "NotRun".to_string(),
            Run::Matchable { .. } => "Matchable".to_string(),
            Run::LeftOut { .. } => "LeftOut".to_string(),
            Run::Channels { .. } => "Channels".to_string(),
            Run::TestPlan(_) => "TestPlan".to_string(),
            Run::WheaMonitoring(_) => "WheaMonitoring".to_string(),
            Run::AllocationStarting { .. } => "AllocationStarting".to_string(),
            Run::Allocated(_) => "Allocated".to_string(),
            Run::SuiteStarted { .. } => "SuiteStarted".to_string(),
            Run::CycleStarted { cycle, .. } => format!("CycleStarted {cycle}"),
            Run::CycleFinished { cycle, .. } => format!("CycleFinished {cycle}"),
            Run::SealStage { op, .. } => format!("SealStage {op:?}"),
            Run::StepFinished(report) => format!("StepFinished {}, {} errors", report.summary.step, report.summary.errors),
            Run::LateWhea(_) => "LateWhea".to_string(),
            Run::CpuPerformance(_) => "CpuPerformance".to_string(),
            Run::SuiteEnded { .. } => "SuiteEnded".to_string(),
            Run::FinalSummary(_) => "FinalSummary".to_string(),
            Run::ResultsSaved { .. } => "ResultsSaved".to_string(),
        };
        crate::console::print_above(&format!("<{line}>"));
    }

    fn large_page_restart(&self) -> RestartChoice {
        unreachable!("a run asks nothing: the restart is setup's question")
    }
}

/// TODO 29: each event comes where its prints were, against the log records around it, so a
/// frontend that renders as it is called keeps the console's order. A cycle's: the records of a
/// step's results come before its report, the halt's after it, and the final seal check's
/// errors after its line; and a run's start: the memory plan before the layout's records, the
/// config error that stops it after the plan's. Where a list's build logs (at debug level),
/// its records come between the line before the list and the list: the channels and the test
/// plan, a filter that matched nothing and what it could have matched.
#[test]
fn events_come_in_order_with_the_log_records() {
    // The events and the run's own records: the workers' order among themselves is the
    // scheduler's
    let order = |text: &str| -> Vec<String> {
        mask_stamps(text).lines()
            .filter(|line| line.starts_with('<') || line.contains(" tmr::runner]") || line.contains(" tmr::layout]"))
            .map(|line| line.split_once("\x1b[0m ").map_or(line, |(_, message)| message).to_string())
            .collect()
    };
    let fake_cycle = |error_mode, flip| {
        let cycle = FakeCycle::new(error_mode, flip);
        capture(|| cycle.run(&Recording)).1
    };
    let text = fake_cycle(ErrorMode::Log, Flip::Verify);
    let verify_errors = "Thread 0 reported 1 memory errors in test 'Bench-Verify-TMR-10' (1 failed checks, 0 not reproduced on rescan)";
    assert_eq!(order(&text), ["<SealStage SealAll>", "<StepFinished 1, 0 errors>", verify_errors, "<StepFinished 2, 1 errors>",
                              "<StepFinished 3, 0 errors>", "<StepFinished 4, 0 errors>", "<SealStage FinalCheck>"]);
    let text = fake_cycle(ErrorMode::Halt, Flip::Verify);
    assert_eq!(order(&text), ["<SealStage SealAll>", "<StepFinished 1, 0 errors>", verify_errors, "<StepFinished 2, 1 errors>",
                              "Halting test suite due to memory errors"]);
    let text = fake_cycle(ErrorMode::Log, Flip::VerifyAndSealed);
    assert_eq!(order(&text), ["<SealStage SealAll>", "<StepFinished 1, 0 errors>", verify_errors, "<StepFinished 2, 1 errors>",
                              "<StepFinished 3, 0 errors>", "<StepFinished 4, 0 errors>", "<SealStage FinalCheck>",
                              "The final seal check of cycle 1 found 1 errors"]);

    let overrides = TestRunOverrides { plan: Some(default_plan()), sequence_override: Some("W 2Q"), ..Default::default() };
    let (run, fe): (_, Arc<dyn Frontend>) = (GoldenRun::new(overrides), Arc::new(Recording));
    let (status, text) = capture(|| run.run(&fe));
    assert_eq!(status, RunStatus::NotRun);
    assert_eq!(order(&text), ["<MemoryPlan>", "🧵 Thread Memory Layout:", "  Total threads: 4", "  Allocation per thread: 0.00 GiB",
                              "  Total planned allocation: 0.02 GiB", "Using the built-in suite, plan stability", "<NotRun>"]);

    // How many Debug records came between two events
    let debug_between = |text: &str, first: &str, second: &str| -> usize {
        let lines: Vec<&str> = text.lines().collect();
        let at = |event: &str| lines.iter().position(|line| *line == event).unwrap_or_else(|| panic!("{event}:\n{text}"));
        lines[at(first)..at(second)].iter().filter(|line| line.contains(" DEBUG ")).count()
    };
    let stability = Some(default_plan());
    let plan_only = TestRunOverrides { plan: stability, plan_only: true, ..Default::default() };
    let (run, fe): (_, Arc<dyn Frontend>) = (GoldenRun::new(plan_only), Arc::new(Recording));
    let (status, text) = capture_with_debug(|| run.run(&fe));
    assert_eq!(status, RunStatus::Passed);
    assert!(debug_between(&text, "<Channels>", "<TestPlan>") > 0, "the plan's build logs after the channels:\n{text}");
    for plan in [stability, None] {
        let no_match = TestRunOverrides { plan, single_test_filter: Some("Nope-*"), ..Default::default() };
        let (run, fe): (_, Arc<dyn Frontend>) = (GoldenRun::new(no_match), Arc::new(Recording));
        let (status, text) = capture_with_debug(|| run.run(&fe));
        assert_eq!(status, RunStatus::NotRun);
        assert!(debug_between(&text, "<NotRun>", "<Matchable>") > 0, "the list's build logs after the stop:\n{text}");
    }
}

/// The plan as `--startup-debug` shows it (TODO 29): the run's entry point up to its return
/// before WHEA and allocation, on the goldens' layout and the default plan, at 256 bits
/// (`GoldenRun`). A thread's 4 MiB is less than what a cache asks of a chunk or an extent on any
/// CPU with more than 2 MiB of L3 (DRAM*2 is twice the L3, CacheTotal 2.00x twice the caches), so
/// on every such machine those chunks and extents are the full 4 MiB and their notes print; masked
/// are the log stamps and what this machine's L3 asked of a chunk (`golden::mask_plan`). A CPU
/// with 2 MiB of L3 or less prints another plan.
#[test]
fn the_plan_prints_as_its_golden() {
    let run = GoldenRun::new(TestRunOverrides { plan: Some(default_plan()), plan_only: true, ..Default::default() });
    let (status, text) = capture(|| run.run(&cli()));
    assert_eq!(status, RunStatus::Passed);
    check("plan_only", &mask_plan(&text));
}

/// What a run that stops before testing prints (TODO 29), on the goldens' layout at 256 bits
/// (`GoldenRun`): a filter no test matches, in the default plan and in the whole suite, and a
/// config error from the checks before WHEA and allocation that an override can reach (a
/// sequence's, a parameter's, the seal's; the plan's own check needs a config's dependent step).
/// Each case's output follows a `## case` line.
#[test]
fn what_stops_a_run_prints_as_its_golden() {
    let stability = Some(default_plan());
    let cases = [
        ("a filter no test of the plan matches", TestRunOverrides { plan: stability, single_test_filter: Some("Nope-*"), ..Default::default() }),
        ("a filter no test matches", TestRunOverrides { single_test_filter: Some("Nope-*"), ..Default::default() }),
        ("a sequence that doesn't parse", TestRunOverrides { plan: stability, sequence_override: Some("W 2Q"), ..Default::default() }),
        ("a verify of the fill alone in a mode the cycle keys",
         TestRunOverrides { plan: stability, pattern_mode_override: Some(0), test_reps_override: Some(0), ..Default::default() }),
        ("a seal that doesn't parse", TestRunOverrides { plan: stability, seal_override: Some("bogus"), ..Default::default() }),
    ];
    let mut text = String::new();
    for (case, overrides) in cases {
        let run = GoldenRun::new(overrides);
        let (status, printed) = capture(|| run.run(&cli()));
        assert_eq!(status, RunStatus::NotRun, "{case}");
        text += &format!("## {case}\n{printed}");
    }
    check("not_run", &mask_stamps(&text));
}
