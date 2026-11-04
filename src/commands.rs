//! The one-shot commands (TODO 29): `--help`, `--version`, `--create-demo-configs`, `--compare-results`, `--show-results`,
//! `--debug-topology`, `--show-topology`, `--setup-large-pages`, `--remove-large-pages` and the calibrations. A session that is handed one
//! returns it (`session::SessionEnd::Command`), past the parameters every session reads first (`core-priority=` types the topology the
//! topology commands show); its caller runs it here, writing to a writer of its own. The CLI does, on the console; the window doesn't offer
//! them.
//!
//! A calibration asks whether to save what it measured over what tmr-cfg.json holds. That question is the caller's to put: it passes
//! `confirm`, which gets the question and says yes or no, unless the command line has answered it already (`--yes`, `--no-save`:
//! `SaveChoice`). `--show-results` ends with a run's final summary, a report whose tables the caller renders: it passes `text`, which gives
//! the text it shows for an event, as for a run's (TODO 72).

use std::io::{self, Write};

use crate::frontend::{Event, Run};
use crate::progress::{ErrorCounts, RunOutcome};
use crate::reporting::models::FinalTestSummaryReport;
use crate::results::{TestRunMetadata, TestRunResult};
use crate::runner::RunStatus;

/// A one-shot command, as the command line names it after the session's own parameters.
pub enum Command {
    /// `--create-demo-configs`: two demo configs in the configs folder in TMR's home
    DemoConfigs,
    /// `--compare-results <baseline.json> <current.json> [output.json]`: the command line, the exe's name first
    Compare(Vec<String>),
    /// `--show-results <results.json>`: the command line, the exe's name first
    ShowResults(Vec<String>),
    /// `--debug-topology`: every topology detector, alone and as Auto's cascade
    DebugTopology,
    /// `--show-topology`: each core's type, scheduling class, NUMA node and CPUs
    ShowTopology,
    /// `--setup-large-pages [User:<name> | Group:<name>]`: grant the right large pages need, to the account TMR runs as or the user or
    /// group named, and check it (TODO 113): the command line, the exe's name first
    SetupLargePages(Vec<String>),
    /// `--remove-large-pages [User:<name> | Group:<name>]`: remove that right, from the account TMR runs as or the user or group named:
    /// the command line, the exe's name first
    RemoveLargePages(Vec<String>),
    /// `--calibrate-cache`, `--calibrate-cache-ext`, `--calibrate-seal`, `--calibrate-all`: the command line, the exe's name first, then
    /// the command
    Calibrate(Vec<String>),
    /// `--help`: the exe's name, as the usage lines give it
    Help(String),
    /// `--version`
    Version,
}

impl Command {
    /// The command `args` (the exe's name first) names with its first argument, if it names one.
    pub(crate) fn parse(args: &[String]) -> Option<Command> {
        if args.len() > 1 {
            match args[1].as_str() {
                "--create-demo-configs" => return Some(Command::DemoConfigs),
                "--compare-results" => return Some(Command::Compare(args.to_vec())),
                "--show-results" => return Some(Command::ShowResults(args.to_vec())),
                "--debug-topology" => return Some(Command::DebugTopology),
                "--show-topology" => return Some(Command::ShowTopology),
                // --cache-latency and --ram-latency are no commands: they pick tests for the run
                "--setup-large-pages" => return Some(Command::SetupLargePages(args.to_vec())),
                "--remove-large-pages" => return Some(Command::RemoveLargePages(args.to_vec())),
                // The calibrations (TODO 99 adds the seal's width): each measures, shows what it found and saves it to tmr-cfg.json after a
                // prompt
                "--calibrate-cache" | "--calibrate-cache-ext" | "--calibrate-seal" | "--calibrate-all" => {
                    return Some(Command::Calibrate(args.to_vec()));
                }
                "--help" | "-h" | "/?" | "-?" => return Some(Command::Help(args[0].clone())),
                "--version" | "-v" => return Some(Command::Version),
                _ => {}
            }
        }
        None
    }
}

/// Runs `command`, writing what it shows to `out`; a calibration asks `confirm` whether to save what it measured, and `--show-results` has
/// `text` give the final summary's text, the text the caller shows for that event at a run's end (the CLI's `transcript`). Returns
/// `Passed` when it did what it was asked, `NotRun` when it failed: exit statuses 0 and 2 (`RunStatus::exit_code`), as `--help` gives them.
pub fn run(
    command: &Command, out: &mut dyn Write, confirm: &mut dyn FnMut(&str) -> bool, text: &dyn Fn(&Event) -> String,
) -> io::Result<RunStatus> {
    match command {
        Command::DemoConfigs => return demo_configs(out),
        Command::Compare(args) => return compare(args, out),
        Command::ShowResults(args) => return show_results(args, out, text),
        Command::DebugTopology => {
            writeln!(out, "🔍 Debugging CPU Topology Detection")?;
            crate::cpu_topology::debug_topology_detection(out)?;
        }
        Command::ShowTopology => {
            writeln!(out, "🔍 Complete CPU Topology Mapping")?;
            crate::cpu_topology::show_complete_topology_mapping(out)?;
        }
        // The command is the request: it grants (or removes) without asking, as a session doesn't (`session::run`)
        Command::SetupLargePages(args) => {
            use crate::memory::large_page_setup::{Change, ThisMachine, run};
            return run(Change::Grant, args, &mut ThisMachine::new(), out);
        }
        Command::RemoveLargePages(args) => {
            use crate::memory::large_page_setup::{Change, ThisMachine, run};
            return run(Change::Remove, args, &mut ThisMachine::new(), out);
        }
        Command::Calibrate(args) => return calibrate(args, out, confirm),
        Command::Help(program_name) => help(out, program_name)?,
        Command::Version => {
            writeln!(out, "Test Memory R (TMR) version {}", crate::BUILD.version)?;
            writeln!(out, "High-Performance Memory Testing Tool with TM5 Compatibility")?;
            writeln!(out, "Three-Stage Memory Architecture with Comprehensive Testing")?;
        }
    }
    Ok(RunStatus::Passed)
}

/// The complete help text, `program_name` the exe's name in its usage lines: `--help`'s, and what follows an unknown flag's error.
pub fn help(out: &mut dyn Write, program_name: &str) -> io::Result<()> {
    writeln!(out, "🚀 Test Memory R (TMR) v{} - High-Performance Memory Testing Tool", crate::BUILD.version)?;
    writeln!(out, "===================================================================================")?;
    writeln!(out)?;
    writeln!(out, "USAGE:")?;
    writeln!(out, "  {}                                    # Run with defaults", program_name)?;
    writeln!(out, "  {} config=test.json                  # Load modern JSON config (v2.0)", program_name)?;
    writeln!(out, "  {} config=legacy.cfg                 # Load legacy TestMem5 config (v1.0)", program_name)?;
    writeln!(out, "  {} --create-demo-configs              # Create demo configuration files", program_name)?;
    writeln!(out, "  {} --compare-results baseline.json current.json [output.json]", program_name)?;
    writeln!(out, "                                          # Compare two test results (here, or in results\\)")?;
    writeln!(out, "  {} --show-results results.json      # How a run ended, then its final summary (here, or in results\\)", program_name)?;
    writeln!(
        out,
        "  {} --setup-large-pages              # Grant 'Lock pages in memory' to you (as administrator), then sign out and in",
        program_name
    )?;
    writeln!(out, "  {} --setup-large-pages Group:Administrators", program_name)?;
    writeln!(out, "                                          #   ...or to a group or another user: Group:<name>, User:<name>")?;
    writeln!(out, "                                          #   (DOMAIN\\name, or .\\name for this PC's; quote a name with spaces)")?;
    writeln!(
        out,
        "  {} --remove-large-pages             # Remove it from you, or from the Group:<name> or User:<name> given",
        program_name
    )?;
    writeln!(
        out,
        "  {} --calibrate-cache                # Measure the cache tiers' sizes; saved to tmr-cfg.json after a prompt",
        program_name
    )?;
    writeln!(
        out,
        "  {} --calibrate-cache-ext            #   ...finer, around each tier's edge (preferred when both are saved)",
        program_name
    )?;
    writeln!(
        out,
        "  {} --calibrate-seal                 # Time the seal's fill and check at each width, one thread; seal-width=auto",
        program_name
    )?;
    writeln!(out, "                                          #   then runs the fastest (a narrower one if it is as fast)")?;
    writeln!(
        out,
        "  {} --calibrate-all                  # --calibrate-cache, then --calibrate-seal (maxpage= sets both's page size)",
        program_name
    )?;
    writeln!(
        out,
        "  {} --calibrate-cache --yes          #   any calibration: --yes saves without asking, --no-save keeps what is",
        program_name
    )?;
    writeln!(out, "                                          #   stored; with neither it asks, and saves if no answer can come")?;
    writeln!(
        out,
        "  {} plan=stability                   # The default: the stability tests at the widest width (or width=)",
        program_name
    )?;
    writeln!(out, "  {} plan=quick                       # The stability tests once", program_name)?;
    writeln!(out, "  {} plan=widths                      # Every width of the tests that have them, side by side", program_name)?;
    writeln!(out, "  {} plan=perf                        # Latency and bandwidth", program_name)?;
    writeln!(out, "  {} plan=all                         # Every test in the suite; test= alone picks from it", program_name)?;
    writeln!(out, "  {} --quick-test                     # plan=quick, reserving 60% of available memory", program_name)?;
    writeln!(out, "  {} --ram-latency                    # Run DRAM latency tests (6 tests)", program_name)?;
    writeln!(out, "  {} --cache-latency                  # Run full cache hierarchy latency tests (12 tests)", program_name)?;
    writeln!(out, "  {} test=Spd-*                       # Run tests matching pattern (glob wildcards)", program_name)?;
    writeln!(out, "  {} test=Mem-Mirror*,Lat-L3-*        # Run multiple patterns (comma-separated)", program_name)?;
    writeln!(
        out,
        "  {} config=x.cfg --startup-debug     # Print the memory and test plan; allocate no test memory, run nothing",
        program_name
    )?;
    writeln!(out, "  {} --version                         # Show version information", program_name)?;
    writeln!(out, "  {} --show-topology                   # Show CPU Topology Mapping for debugging", program_name)?;
    writeln!(out, "  {} --debug-topology                  # Runs multiple CPU Topology checks to debug if one works better", program_name)?;
    writeln!(out)?;
    writeln!(out, "EXIT STATUS: 0 passed, 1 tests failed or found errors, 2 stopped before or instead of testing")?;
    writeln!(out, "             (bad config, failed command), 3 interrupted (Ctrl+C); other codes: crashed or killed")?;
    writeln!(out, "             (errors=panic aborts on the first error)")?;
    writeln!(out)?;
    writeln!(out, "COMMAND LINE PARAMETERS:")?;
    writeln!(out, "  memory=20%                           # Reserve 20% of system memory")?;
    writeln!(out, "  memory=2GiB                         # Reserve 2 GiB")?;
    writeln!(out, "  memory=2048MB                       # TM5-compatible allocation (2048MB from available)")?;
    writeln!(out, "  memory=10%-from-available           # Reserve 10% from currently available memory")?;
    writeln!(out, "  memory=8GiB-from-total              # Reserve 8 GiB from total system memory")?;
    writeln!(out, "  memory=64GiB-target                 # Target 64 GiB allocation (may fail if unavailable)")?;
    writeln!(out, "  cycles=5                            # Run 5 complete test cycles")?;
    writeln!(out, "  duration=600                        # Maximum 10 minutes runtime")?;
    writeln!(out, "  cpus=50%                            # Use 50% of available CPUs, from the front of the core order:")?;
    writeln!(out, "                                      #   each L3's cores by Windows' core priority, the L3s in turn")?;
    writeln!(out, "  cpus=4                              #   ...or a count of them")?;
    writeln!(out, "  cputype=cores                       # Use physical cores (vs threads/SMT)")?;
    writeln!(out, "  skip-cores=1                        # Leave out the last N cores in the core order, the weakest")?;
    writeln!(out, "                                      #   (default: 1, for the system's responsiveness)")?;
    writeln!(out, "  skip-cores=25%                      #   ...or a share of the cores, from the end too (never every core)")?;
    writeln!(out, "  skip-cores=0-7                      #   ...or the cores numbered A-B (isolate a memory domain/CCD)")?;
    writeln!(out, "  cpu-stride=2                        # Take the cores by number instead of the core order, every Nth")?;
    writeln!(out, "  cpu-stride=even                     #   ...or the count spread evenly over them (1: packed); core types too")?;
    writeln!(out, "  --disable-pinning                   # Disable CPU thread pinning")?;
    writeln!(out, "  priority=high                       # Worker priority: normal, high (default), realtime")?;
    writeln!(out, "                                      #   realtime is THREAD_PRIORITY_TIME_CRITICAL, the top of the")?;
    writeln!(out, "                                      #   normal range (not the realtime class); leave a core free with it")?;
    writeln!(out, "  window-priority=high                # The window's thread during a run: normal, high, realtime (default:")?;
    writeln!(out, "                                      #   the workers', priority='s); the console ignores it")?;
    writeln!(out, "  hugechunk=1GiB                      # First 1GB-page request size, halved on refusal (power of two, >= 1GiB)")?;
    writeln!(out, "  largechunk=128MiB                   # First 2MB-page request size, halved on refusal (default 128MiB)")?;
    writeln!(out, "  largefloor=16MiB                    # Smallest 2MB-page request (16MiB-1GiB)")?;
    writeln!(out, "  blkroundtarget=1GiB                 # Round each thread's share to a multiple of this (of largefloor)")?;
    writeln!(out, "  blkround=up                         # ...up (default; a smaller step rather than pass available), down, or nearest")?;
    writeln!(out, "  errors=halt                         # Error handling (log/halt/panic)")?;
    writeln!(out, "  seal=tmr                            # Seal: check and reseal around every chunk (tmr, the default; tm5; off)")?;
    writeln!(out, "  seal-width=auto                     # Seal kernels' SIMD width (auto, 128, 256, 512); unset, width='s;")?;
    writeln!(out, "                                      #   auto is --calibrate-seal's width on this machine, else 256")?;
    writeln!(out, "  width=auto                          # SIMD width of the tests that pick theirs at run time (the -Auto")?;
    writeln!(out, "                                      #   tests, Mem-Sequence, the Bench pairs) and of the seal: auto (the")?;
    writeln!(out, "                                      #   widest the CPU has; the seal its own auto), 128, 256 or 512")?;
    writeln!(out, "                                      #   (refused if the CPU lacks it). Mem-Sequence has no 128 and runs")?;
    writeln!(out, "                                      #   at 256; -128/-256/-512 tests keep theirs")?;
    writeln!(out, "  mirror=whole                        # Mirror tests: one mirror over each chunk (the default)")?;
    writeln!(out, "  mirror=subblocks:4                  #   ...each chunk in 2 or 4 parts, mirrored in lockstep")?;
    writeln!(out, "  mirror=jump:510                     #   ...128 B swaps every (N+1) x 128 B (TM5 MirrorMove128)")?;
    writeln!(out, "  pattern-mode=0                      # Pattern mode: 0-2 (TM5's), 10, 11, 13, 20, 30 (TMR's), or a native")?;
    writeln!(out, "                                      #   family alone on SimpleTest: 15 ident, 16 random, 17 linepair,")?;
    writeln!(out, "                                      #   21 lines, 31 switch, 32 quiet, 33 header; unset, SimpleTest runs")?;
    writeln!(out, "                                      #   native programs (doc/pattern_spec.md)")?;
    writeln!(out, "  sequence=W,2R,RW,2R@random          # Mem-Sequence's passes: W writes, R reads, RW reads and writes each line;")?;
    writeln!(out, "                                      #   a count before, an @order after (spaces or commas between)")?;
    writeln!(out, "  order=random                        # Mem-Sequence's order where a step names none: forward, reverse, random,")?;
    writeln!(out, "                                      #   stride (TM5's, from a config entry's parameter)")?;
    writeln!(out, "  patterns=legacy                     # A TM5 .cfg's SimpleTest modes as TMR's numbered modes, not native")?;
    writeln!(out, "                                      #   programs (default native), to compare the two")?;
    writeln!(out, "  seed=0x1234                         # The native patterns' run seed (default: from the clock, logged and")?;
    writeln!(out, "                                      #   saved in the results), to replay a run's bytes")?;
    writeln!(out, "  beat=32                             # Transfer width the switch pattern flips at: auto (SMBIOS, the")?;
    writeln!(out, "                                      #   default), 16 (LPDDR4/5), 32 (DDR5), 64 (DDR4), or a memory")?;
    writeln!(out, "                                      #   type (ddr4, ddr5, lpddr5, ...); 12 (lpddr6) and 24 (ddr6) are")?;
    writeln!(out, "                                      #   models: TMR hasn't verified their packet maps")?;
    writeln!(out, "  rescan=off                          # A failed check isn't rescanned to name each bad word (default on;")?;
    writeln!(out, "                                      #   off counts one error a failed check, for runs with many errors)")?;
    writeln!(out, "  verify-reps=5                       # Verify passes per write (TM5 retention stress: 5)")?;
    writeln!(out, "  test-reps=3                         # Test op repetitions per write (MirrorMove round-trips)")?;
    writeln!(out, "  write-read-cycles=4                 # Write+verify cycles per chunk (TM5 SimpleTest: 4)")?;
    writeln!(out, "  topology=windowsv2                  # Core types from one detector alone (default auto, the cascade):")?;
    writeln!(out, "                                      #   windows efficiency class, windowsv2 Windows' core priority,")?;
    writeln!(out, "                                      #   cpuid L3 size per core, amd AMD's CPUID core type")?;
    writeln!(out, "  core-priority=hybrid                # Hybrid CPUs that report no core types (e.g. Ryzen 8500G):")?;
    writeln!(out, "                                      #   Windows' higher-priority cores are P-cores (default: equal);")?;
    writeln!(out, "                                      #   labels only: the core order is the priority's either way")?;
    writeln!(out, "  --no-calibration                    # Skip loading calibration data (use CPUID heuristics; the seal's rule)")?;
    writeln!(
        out,
        "  --no-serials                        # Leave the DIMMs' serials and the system's UUID out of the output, log and results"
    )?;
    writeln!(out)?;
    writeln!(out, "CONFIG FILE OVERRIDES:")?;
    writeln!(out, "  All parameters can override config file values:")?;
    writeln!(out, "    {} config=test.json cycles=1 skip-cores=0 memory=4GiB", program_name)?;
    writeln!(out)?;
    writeln!(out, "LOGGING:")?;
    writeln!(out, "  RUST_LOG=info                       # What the console shows and the log file takes (error/warn/info/debug/trace)")?;
    writeln!(out, "                                        # Without it: the console warnings (a debug build: info), the log file info")?;
    writeln!(out, "                                        # The run's own lines (allocation, each step's start) show either way")?;
    writeln!(out, "                                        # Use debug for detailed per-thread logs")?;
    writeln!(out, "  Logs saved to: logs\\TMR_YYYY-MM-DD_HH-MM-SS.log      (local time)")?;
    writeln!(out, "  Results saved to: results\\TMR_YYYY-MM-DD_HH-MM-SS.json (same name as the log)")?;
    writeln!(out, "  Both beside tmr.exe, as are configs\\ and tmr-cfg.json; TMR_HOME=<folder> puts them there instead")?;
    writeln!(out)?;
    writeln!(out, "RESULT COMPARISON:")?;
    writeln!(out, "  Test results are automatically saved as JSON files to results\\")?;
    writeln!(out, "  Use --compare-results to analyze performance differences")?;
    writeln!(out, "  Use --show-results to see a run's final summary again, and how the run ended")?;
    writeln!(out, "  Useful for memory overclocking and timing optimization")?;
    writeln!(out)?;
    usage(out, program_name)
}

/// Quick usage examples (the end of `--help`)
fn usage(out: &mut dyn Write, program_name: &str) -> io::Result<()> {
    writeln!(out, "Quick Usage Examples:")?;
    writeln!(out, "  {} memory=10%-from-available     # Standard: Reserve 10% from currently available memory", program_name)?;
    writeln!(out, "  {} memory=2048MB                 # TM5 compatible: Reserve 2048MB from available memory", program_name)?;
    writeln!(out, "  {} memory=8GiB-from-total        # Failure test: Reserve 8 GiB from total (unrealistic)", program_name)?;
    writeln!(out, "  {} memory=64GiB-target           # Failure test: Target 64 GiB allocation (may fail)", program_name)?;
    writeln!(out, "  {} cycles=5 duration=600        # 5 cycles OR 10 minutes max", program_name)?;
    writeln!(out, "  {} cpus=50% cputype=cores       # Use 50% of CPU cores (cores=avoid SMT)", program_name)?;
    writeln!(out, "  {} skip-cores=0                 # Don't skip any core (default: leave out the weakest)", program_name)?;
    writeln!(out, "  {} skip-cores=0-7 cpus=100%     # Test ONLY cores 8+ (isolate a memory domain on multi-CCD parts)", program_name)?;
    writeln!(out, "  {} cpus=50%                     # Half the cores, spread over the L3s (the memory domains) evenly", program_name)?;
    writeln!(out, "  {} cpus=50% cpu-stride=even     # Half the cores by core number, spaced (cores / threads) apart", program_name)?;
    writeln!(out, "  {} --disable-pinning            # Disable CPU pinning (default: enabled)", program_name)?;
    writeln!(out, "  {} errors=halt                  # Stop on first error", program_name)?;
    writeln!(out, "  {} config=test.json             # Load comprehensive JSON config", program_name)?;
    writeln!(out, "  {} config=legacy.cfg            # Auto-convert TM5 config + add stuck bit test", program_name)?;
    writeln!(out, "  {} --create-demo-configs        # Create demo configurations", program_name)?;
    writeln!(out, "  {} --compare-results old.json new.json # Compare results from results\\", program_name)?;
    Ok(())
}

/// `--create-demo-configs`: writes the two demo configs to the configs folder in TMR's home, then says what each is.
fn demo_configs(out: &mut dyn Write) -> io::Result<RunStatus> {
    if let Err(e) = save_demo_configs() {
        writeln!(out, "❌ Failed to create demo configs: {}", e)?;
        return Ok(RunStatus::NotRun);
    }
    writeln!(out, "In {}:", crate::home::configs().display())?;
    writeln!(out, "✅ Created demo_comprehensive_test.json - Modern comprehensive memory testing")?;
    writeln!(out, "   Features: Full memory stuck bit test + timed stress tests")?;
    writeln!(out, "   Timing: 3 cycles, ~2-3 minutes per cycle with comprehensive coverage")?;
    writeln!(out, "   Memory: Uses full allocation for critical tests, optimized windows for others")?;
    writeln!(out)?;
    writeln!(out, "✅ Created demo_tm5_compatible.json - TM5-compatible configuration")?;
    writeln!(out, "   Features: TM5-style allocation with modern stuck bit test added")?;
    writeln!(out, "   Timing: 3 cycles, faster execution for compatibility")?;
    writeln!(out, "   Memory: Maximum allocation minus 128MB reserve, every test over all of it")?;
    writeln!(out)?;
    writeln!(out, "Configuration Architecture Summary:")?;
    writeln!(out, "  Stage 1: Memory Allocation - Maximum available memory per thread")?;
    writeln!(out, "  Stage 2: Extent - how much of the allocation each test covers")?;
    writeln!(out, "  Stage 3: Block/Chunk Size - Auto-optimized per test with alignment")?;
    writeln!(out, "  Timing: Per-test cycles/duration limits + global suite limits")?;
    writeln!(out, "  Critical: Mem-StuckBitAuto ensures full memory coverage for bit errors")?;
    Ok(RunStatus::Passed)
}

/// The two demo configs, saved where a relative `config=` finds them.
fn save_demo_configs() -> Result<(), String> {
    use crate::config::ModernConfig;
    let configs = crate::home::configs();
    std::fs::create_dir_all(&configs).map_err(|e| format!("{}: {e}", configs.display()))?;
    let in_configs = |name: &str| configs.join(name).display().to_string();
    // Create modern comprehensive config
    let modern_config = ModernConfig::create_demo_config();
    modern_config.save_to_file(&in_configs("demo_comprehensive_test.json"))?;

    // Create TM5-compatible config
    let tm5_config = ModernConfig::create_tm5_compatible_config();
    tm5_config.save_to_file(&in_configs("demo_tm5_compatible.json"))
}

/// Two results files compared, for the window's Compare tab (TODO 109): `--compare-results`' text but for its steps' lines, which come as a
/// table, each step both plans have with its changes in its time, speed and errors. The text has what differs between the runs, the
/// plans' changes (the steps only one has, the settings that differ of those both have) and the overall changes (TODO 72).
#[derive(Clone, Debug)]
pub struct ResultsComparison {
    pub text: String,
    pub per_test: crate::reporting::models::TableData,
}

/// Compares the results files `baseline` and `current`, each found as `--compare-results` finds it: as given, or by its name in TMR's
/// home's `results\` (`home::find`). Why it couldn't, otherwise.
pub fn compare_results(baseline: &str, current: &str) -> Result<ResultsComparison, String> {
    let find = |path: &str| {
        crate::home::find(path, &crate::home::results())
            .map(|found| found.display().to_string())
            .map_err(|looked| format!("Test result file does not exist: {path} ({looked})"))
    };
    let comparison = crate::results::compare_test_results(&find(baseline)?, &find(current)?)?;
    Ok(ResultsComparison { text: comparison.head_text(), per_test: comparison.per_test_table() })
}

/// `--compare-results <baseline.json> <current.json> [output.json]`, `args` the command line: the comparison of two results files, saved to
/// the third file when one is given.
fn compare(args: &[String], out: &mut dyn Write) -> io::Result<RunStatus> {
    if args.len() < 4 {
        writeln!(out, "❌ Usage: {} --compare-results <baseline.json> <current.json> [output.json]", args[0])?;
        return Ok(RunStatus::NotRun);
    }
    // Each as typed, from the current folder, or a results file in TMR's home by its name (`home::find`, TODO 104). The output is as
    // typed: from the current folder.
    let mut files = Vec::new();
    for path in &args[2..4] {
        match crate::home::find(path, &crate::home::results()) {
            Ok(found) => files.push(found.display().to_string()),
            Err(looked) => {
                writeln!(out, "❌ Comparison failed: Test result file does not exist: {path} ({looked})")?;
                return Ok(RunStatus::NotRun);
            }
        }
    }
    let (baseline, current) = (&files[0], &files[1]);
    let output = args.get(4).map(|s| s.as_str());

    let comparison = match crate::results::compare_test_results(baseline, current) {
        Ok(comparison) => comparison,
        Err(e) => {
            writeln!(out, "❌ Comparison failed: {}", e)?;
            return Ok(RunStatus::NotRun);
        }
    };

    writeln!(out, "{}", comparison.to_text_report())?;

    if let Some(output) = output {
        if let Err(e) = comparison.save_to_file(output) {
            writeln!(out, "❌ Comparison failed: {}", e)?;
            return Ok(RunStatus::NotRun);
        }
        writeln!(out, "\nComparison saved to: {}", output)?;
    }

    writeln!(out, "✅ Comparison completed successfully")?;
    Ok(RunStatus::Passed)
}

/// A results file as `--show-results` shows it (TODO 72): how its run ended and which run it was, then the final summary the console
/// showed at the run's end, from the file's figures, for a frontend to render as it renders a run's (`Run::FinalSummary`).
#[derive(Clone, Debug)]
pub struct SavedRun {
    /// The lines before the summary: how the run ended (`ending`), then which run it was (`which_run`)
    pub head: String,
    pub summary: FinalTestSummaryReport,
    /// The run's work by thread, CPU and core, summed from its steps' thread records as the run summed them (TODO 7); `None` for a file
    /// with no thread records (before TODO 72)
    pub work: Option<crate::frontend::CpuPerformanceSummary>,
}

/// The results file `file`, found as `--compare-results` finds one (as given, or by its name in TMR's home's `results\`, `home::find`), as
/// `--show-results` shows it. Why it couldn't be read, otherwise.
pub fn saved_run(file: &str) -> Result<SavedRun, String> {
    let found = crate::home::find(file, &crate::home::results())
        .map_err(|looked| format!("Test result file does not exist: {file} ({looked})"))?
        .display()
        .to_string();
    let result = TestRunResult::load_from_file(&found).map_err(|e| format!("{found}: {e}"))?;
    let summary = crate::reporting::converters::create_overall_stats_summary_report(&result);
    Ok(SavedRun { head: head(&result, &summary), summary, work: saved_work(&result) })
}

/// The run's work by thread, CPU and core from `result`'s thread records, summed over every step as the live run sums them (its
/// timed milliseconds, bytes and errors), a thread with no CPU left out as there; the run's WHEA and pinning from the file
fn saved_work(result: &TestRunResult) -> Option<crate::frontend::CpuPerformanceSummary> {
    use crate::reporting::models::ThreadPerformance;
    /// A thread's sums over the run's steps, and where it ran
    #[derive(Default)]
    struct Sums {
        cpu: Option<usize>,
        core: Option<usize>,
        node: Option<u32>,
        bytes: u64,
        ms: f64,
        errors: u64,
    }
    let mut by_thread: std::collections::BTreeMap<usize, Sums> = std::collections::BTreeMap::new();
    for record in result.cycles.iter().flat_map(|cycle| &cycle.tests).flat_map(|test| &test.threads) {
        let sums = by_thread.entry(record.thread).or_default();
        (sums.cpu, sums.core, sums.node) = (sums.cpu.or(record.cpu), sums.core.or(record.core), sums.node.or(record.node));
        (sums.bytes, sums.ms, sums.errors) = (sums.bytes + record.bytes, sums.ms + record.ms, sums.errors + record.errors);
    }
    let threads: Vec<ThreadPerformance> = by_thread
        .into_iter()
        .filter_map(|(thread_id, sums)| {
            let cpu_id = sums.cpu?;
            let throughput_mib_s = if sums.ms > 0.0 { (sums.bytes as f64 / (1024.0 * 1024.0)) / (sums.ms / 1000.0) } else { 0.0 };
            Some(ThreadPerformance {
                thread_id,
                cpu_id,
                physical_core_id: sums.core.unwrap_or(cpu_id),
                numa_node: sums.node.unwrap_or(0),
                total_time_ms: sums.ms as u128,
                total_bytes: sums.bytes,
                throughput_mib_s,
                total_errors: sums.errors,
            })
        })
        .collect();
    if threads.is_empty() {
        return None;
    }
    let whea = crate::whea::WheaCounts { total: result.overall_stats.whea_total, corrected: result.overall_stats.whea_corrected };
    Some(crate::runner::cpu_performance_of(threads, whea, result.run_config.threads.pinned))
}

/// `--show-results`' lines before the summary, `summary` the run's
fn head(result: &TestRunResult, summary: &FinalTestSummaryReport) -> String {
    format!("{}\n{}", ending(result, summary.errors()), which_run(&result.metadata))
}

/// `--show-results <results.json>`, `args` the command line: how the run ended and which run it was, then its work by thread, CPU and
/// core, and its final summary, `text`'s text of them, as a run's end shows them.
fn show_results(args: &[String], out: &mut dyn Write, text: &dyn Fn(&Event) -> String) -> io::Result<RunStatus> {
    let [_, _, file] = args else {
        writeln!(out, "❌ Usage: {} --show-results <results.json>", args[0])?;
        return Ok(RunStatus::NotRun);
    };
    match saved_run(file) {
        Ok(saved) => {
            out.write_all(saved.head.as_bytes())?;
            if let Some(work) = saved.work {
                out.write_all(text(&Run::CpuPerformance(Box::new(work)).into()).as_bytes())?;
            }
            out.write_all(text(&Run::FinalSummary(Box::new(saved.summary)).into()).as_bytes())?;
            Ok(RunStatus::Passed)
        }
        Err(e) => {
            writeln!(out, "❌ Couldn't show the results: {e}")?;
            Ok(RunStatus::NotRun)
        }
    }
}

/// How the run ended, `--show-results`' first line (TODO 72): from its final save, the outcome and the verdict, with `errors` when it
/// failed or was interrupted, and its cycles, the last one cut short when it stopped partway (`CycleResult::partial`); or, the file still
/// marked in progress, that it never finished, its figures those of its last saved cycle. A file from before the outcome was saved says
/// only that it ended, and one whose keys this build doesn't know quotes them.
fn ending(result: &TestRunResult, errors: ErrorCounts) -> String {
    let stats = &result.overall_stats;
    let cut_short = result.cycles.last().is_some_and(|cycle| cycle.partial);
    let n = stats.cycles_completed;
    if stats.in_progress {
        let saved = match n {
            0 => "no cycle saved".to_string(),
            n if cut_short => format!("its figures are up to its last saved cycle, {n}, which was cut short"),
            n => format!("its figures are up to its last saved cycle, {n}"),
        };
        return format!("⚠️  The run never finished: it was killed or crashed, unless it is still running; {saved}");
    }
    let cycles = match n {
        0 => "0 cycles".to_string(),
        1 if cut_short => "1 cycle, cut short".to_string(),
        n if cut_short => format!("{n} cycles, the last cut short"),
        1 => "1 cycle".to_string(),
        n => format!("{n} cycles"),
    };
    let (outcome, status) = (stats.outcome.as_deref(), stats.status.as_deref());
    let (how, verdict) = match (outcome.and_then(RunOutcome::from_key), status.and_then(RunStatus::from_key)) {
        (Some(how), Some(verdict)) => (how, verdict),
        _ if outcome.is_none() && status.is_none() => {
            return format!("ℹ  The run ended, {cycles}: a results file this old doesn't say how, or its verdict");
        }
        _ => {
            let given = |key: Option<&str>| key.map_or("none".to_string(), |key| format!("'{key}'"));
            return format!(
                "ℹ  The run ended, {cycles}, but this build can't say how: its file gives outcome {} and verdict {}",
                given(outcome),
                given(status)
            );
        }
    };
    let how = match how {
        RunOutcome::Completed => "finished",
        RunOutcome::TimeLimit => "ended at its time limit",
        RunOutcome::Interrupted => "was stopped (Ctrl+C, or the window's Stop)",
        RunOutcome::Halted => "halted on errors (errors=halt)",
    };
    let (icon, verdict) = match verdict {
        RunStatus::Passed => ("✅ ", "passed".to_string()),
        RunStatus::Failed => ("❌ ", format!("failed, {errors}")),
        RunStatus::NotRun => ("⚠️  ", "no test ran".to_string()),
        RunStatus::Interrupted => ("⚠️  ", format!("interrupted, {errors}")),
    };
    format!("{icon}The run {how}: {verdict}, {cycles}")
}

/// Which run the file is, `--show-results`' line after the first: its stem, which its log file's name shares, its start, its run ID and
/// that this build wrote it; or, when another build did, two lines more: that build, and that the summary is laid out as this one lays it
/// out.
fn which_run(metadata: &TestRunMetadata) -> String {
    let stem = std::path::Path::new(&metadata.filename).file_stem().and_then(|stem| stem.to_str()).unwrap_or(&metadata.filename);
    let id = metadata.run_id.as_deref().map_or("no run ID".to_string(), |id| format!("run ID {id}"));
    let this_build = crate::results::BuildRecord::from(crate::BUILD);
    let mut text = format!("📄 {stem}, started {}, {id}", metadata.start_time_local);
    if metadata.tmr_version == crate::BUILD.version && metadata.build.as_ref() == Some(&this_build) {
        text += ", by this build\n";
        return text;
    }
    let build = match &metadata.build {
        Some(build) => build.describe(),
        None => "from before results files recorded their build".to_string(),
    };
    text += &format!("\n⚠️  Not this build: TMR {}, {build}\n", metadata.tmr_version);
    text += "   The summary below is laid out as this build lays it out, which may not be how that run's console showed it\n";
    text
}

/// What a calibration does with what it measured, from its command line: asks (`confirm`), or, without asking, saves (`--yes`) or keeps
/// what tmr-cfg.json holds (`--no-save`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SaveChoice {
    Ask,
    Save,
    Keep,
}

impl SaveChoice {
    /// The command line's choice, or why there is none: `--yes` and `--no-save` together.
    fn of(args: &[String]) -> Result<Self, &'static str> {
        let given = |flag: &str| args.iter().any(|a| a == flag);
        match (given("--yes"), given("--no-save")) {
            (true, true) => Err("--yes and --no-save contradict each other: give one"),
            (true, false) => Ok(SaveChoice::Save),
            (false, true) => Ok(SaveChoice::Keep),
            (false, false) => Ok(SaveChoice::Ask),
        }
    }
}

/// A calibration's save question, answered by the command line (`SaveChoice`) or, when that leaves it open, by `confirm`.
struct Save<'a> {
    choice: SaveChoice,
    confirm: &'a mut dyn FnMut(&str) -> bool,
}

impl Save<'_> {
    /// Whether to save over what is stored: `confirm`'s answer to `question`, or the command line's, which `out` then shows after the
    /// question.
    fn answer(&mut self, question: &str, out: &mut dyn Write) -> io::Result<bool> {
        match self.choice {
            SaveChoice::Ask => Ok((self.confirm)(question)),
            SaveChoice::Save => {
                writeln!(out, "\n{question} yes (--yes)")?;
                Ok(true)
            }
            SaveChoice::Keep => {
                writeln!(out, "\n{question} no (--no-save)")?;
                Ok(false)
            }
        }
    }

    /// Whether to save when nothing is stored yet: without a question, unless `--no-save`.
    fn first(&self) -> bool {
        self.choice != SaveChoice::Keep
    }
}

/// The calibration commands: `--calibrate-cache` and `--calibrate-cache-ext` (the cache tiers), `--calibrate-seal` (the seal's width, TODO
/// 99) and `--calibrate-all` (the cache tiers as `--calibrate-cache` measures them, then the seal's width). Each part measures, shows what
/// it found and saves it to tmr-cfg.json if `confirm` (or `--yes` / `--no-save`) says to; a failure stops it before any more runs.
/// `maxpage=` sets the page size both ask for.
fn calibrate(args: &[String], out: &mut dyn Write, confirm: &mut dyn FnMut(&str) -> bool) -> io::Result<RunStatus> {
    let mut save = match SaveChoice::of(args) {
        Ok(choice) => Save { choice, confirm },
        Err(e) => {
            writeln!(out, "❌ {e}")?;
            return Ok(RunStatus::NotRun);
        }
    };
    // Where it saves (tmr-cfg.json, in TMR's home), checked before it measures, unless it won't save (TODO 104): not found out after
    if !matches!(save.choice, SaveChoice::Keep)
        && let Err(e) = crate::home::can_save(&crate::app_config::AppConfig::default_path())
    {
        writeln!(out, "❌ {e}")?;
        return Ok(RunStatus::NotRun);
    }
    let command = args[1].as_str();
    let (cache, extended, seal) = match command {
        "--calibrate-cache" => (true, false, false),
        "--calibrate-cache-ext" => (true, true, false),
        "--calibrate-seal" => (false, false, true),
        _ => (true, false, true),
    };
    match command {
        "--calibrate-cache" => {
            writeln!(out, "🔬 TMR Adaptive Cache Calibration")?;
            writeln!(out, "==================================\n")?;
        }
        "--calibrate-cache-ext" => {
            writeln!(out, "🔬 TMR Adaptive Cache Calibration (Extended - Phase 2)")?;
            writeln!(out, "======================================================\n")?;
        }
        "--calibrate-seal" => {
            writeln!(out, "🔬 TMR Seal Width Calibration")?;
            writeln!(out, "==============================\n")?;
        }
        _ => {
            writeln!(out, "🔬 TMR Calibration: Cache Tiers and Seal Width")?;
            writeln!(out, "===============================================\n")?;
        }
    }

    // Detect system info once (includes cache info, TSC, hypervisor)
    let system_info = crate::SystemInfo::detect();
    let cache_info = system_info.get_cache_info().clone();
    cache_info.print_info();
    let smbios = crate::smbios::get_smbios();
    writeln!(out)?;

    // Check for maxpage= override in remaining args
    let mut config = crate::calibration::CalibrationConfig::default();
    for arg in &args[2..] {
        if let Some(page_str) = arg.strip_prefix("maxpage=") {
            config.page_size = page_str.to_lowercase();
            writeln!(out, "📄 Page size override: {}\n", config.page_size)?;
        }
    }
    let page_size = config.page_size.clone();

    if cache && calibrate_cache(args, config, &system_info, smbios, extended, out, &mut save)? == RunStatus::NotRun {
        return Ok(RunStatus::NotRun);
    }
    if seal {
        return calibrate_seal(&page_size, &system_info, smbios, out, &mut save);
    }
    Ok(RunStatus::Passed)
}

/// `--calibrate-cache` (`extended`: `--calibrate-cache-ext`): the cache tiers, shown, saved if `save` says to and written to `--output`'s
/// file when given.
fn calibrate_cache(
    args: &[String], config: crate::calibration::CalibrationConfig, system_info: &crate::SystemInfo, smbios: &crate::smbios::SmbiosData,
    extended: bool, out: &mut dyn Write, save: &mut Save,
) -> io::Result<RunStatus> {
    let cache_info = system_info.get_cache_info();
    // Run calibration (extended: phase 1 sweep + phase 2 fine-grain)
    let calibration_test = crate::calibration::CalibrationTest::with_config(cache_info.clone(), config);
    let measured = if extended { calibration_test.run_extended() } else { calibration_test.run() };
    let what = if extended { "Extended calibration" } else { "Calibration" };
    match measured {
        Ok(results) => {
            crate::calibration::CalibrationTest::display_results(&results, cache_info, out)?;

            // Save to tmr-cfg.json — compare with existing if present
            let cfg_path = crate::app_config::AppConfig::default_path();
            let mut app_config = crate::app_config::AppConfig::load(&cfg_path);
            let (existing, cal_type) =
                if extended { (&app_config.calibration_extended, "extended") } else { (&app_config.calibration, "standard") };
            let stored = existing.is_some();
            let should_save = prompt_calibration_save(existing, &results, cal_type, out, save)?;
            if should_save {
                app_config.update_machine_id(system_info, smbios);
                if extended {
                    app_config.calibration_extended = Some(results.clone());
                } else {
                    app_config.calibration = Some(results.clone());
                }
                // A save that was asked for and failed is the command's failure (exit 2), as a script would want to know
                if let Err(e) = app_config.save(&cfg_path) {
                    writeln!(out, "❌ Failed to save calibration: {}", e)?;
                    return Ok(RunStatus::NotRun);
                }
                writeln!(out, "{} saved to {}", what, cfg_path.display())?;
            } else if stored {
                writeln!(out, "Calibration not saved (kept existing).")?;
            } else {
                writeln!(out, "Calibration not saved.")?;
            }

            // Check for --output flag to save results
            if let Some(output_idx) = args.iter().position(|a| a == "--output") {
                if let Some(output_path) = args.get(output_idx + 1) {
                    match serde_json::to_string_pretty(&results) {
                        Ok(json) => match std::fs::write(output_path, json) {
                            Ok(()) => writeln!(out, "Results saved to: {}", output_path)?,
                            Err(e) => writeln!(out, "Failed to write results: {}", e)?,
                        },
                        Err(e) => writeln!(out, "Failed to serialize results: {}", e)?,
                    }
                } else {
                    writeln!(out, "--output requires a file path")?;
                }
            }
            Ok(RunStatus::Passed)
        }
        Err(e) => {
            writeln!(out, "{} failed: {}", what, e)?;
            Ok(RunStatus::NotRun)
        }
    }
}

/// `--calibrate-seal`, and `--calibrate-all`'s second part (TODO 99): the seal's fill and check at every width the CPU has, on one thread
/// (`calibration::calibrate_seal`), shown, and saved if `save` says to as what `seal-width=auto` runs on this machine.
fn calibrate_seal(
    page_size: &str, system_info: &crate::SystemInfo, smbios: &crate::smbios::SmbiosData, out: &mut dyn Write, save: &mut Save,
) -> io::Result<RunStatus> {
    let cache_info = system_info.get_cache_info();
    match crate::calibration::calibrate_seal(cache_info, page_size) {
        Ok(seal) => {
            crate::calibration::display_seal_calibration(&seal, cache_info, out)?;
            let cfg_path = crate::app_config::AppConfig::default_path();
            let mut app_config = crate::app_config::AppConfig::load(&cfg_path);
            let stored = app_config.seal_calibration.is_some();
            if prompt_seal_calibration_save(&app_config.seal_calibration, &seal, out, save)? {
                app_config.set_seal_calibration(seal, system_info, smbios);
                if let Err(e) = app_config.save(&cfg_path) {
                    writeln!(out, "❌ Failed to save seal calibration: {}", e)?;
                    return Ok(RunStatus::NotRun);
                }
                writeln!(out, "Seal calibration saved to {}", cfg_path.display())?;
            } else if stored {
                writeln!(out, "Seal calibration not saved (kept existing).")?;
            } else {
                writeln!(out, "Seal calibration not saved.")?;
            }
            Ok(RunStatus::Passed)
        }
        Err(e) => {
            writeln!(out, "Seal calibration failed: {}", e)?;
            Ok(RunStatus::NotRun)
        }
    }
}

/// Compare new calibration results with existing and ask `save` whether to save them.
/// Returns true if the new results should be saved.
/// If no existing results, saves automatically (no question), unless `--no-save`.
fn prompt_calibration_save(
    existing: &Option<crate::calibration::CalibrationResults>, new_results: &crate::calibration::CalibrationResults, cal_type: &str,
    out: &mut dyn Write, save: &mut Save,
) -> io::Result<bool> {
    let existing = match existing {
        Some(e) => e,
        None if save.first() => {
            writeln!(out, "\nNo existing {} calibration — saving new results.", cal_type)?;
            return Ok(true);
        }
        None => {
            writeln!(out, "\nNo existing {} calibration; --no-save keeps it that way.", cal_type)?;
            return Ok(false);
        }
    };

    // Show comparison table
    writeln!(out, "\n📊 Calibration Comparison (existing vs new)")?;
    writeln!(out, "───────────────────────────────────────────────────────────────────────────")?;
    writeln!(
        out,
        "{:<10} {:>12} {:>12} {:>8}  {:>10} {:>10} {:>8}",
        "Tier", "Old Size", "New Size", "Delta", "Old Lat", "New Lat", "Delta"
    )?;
    writeln!(out, "───────────────────────────────────────────────────────────────────────────")?;

    for tier in crate::calibration::CacheTier::all_tiers() {
        let old_tier = existing.tiers.get(tier);
        let new_tier = new_results.tiers.get(tier);

        match (old_tier, new_tier) {
            (Some(old), Some(new)) => {
                let size_pct = if old.optimal_size > 0 { ((new.optimal_size as f64 / old.optimal_size as f64) - 1.0) * 100.0 } else { 0.0 };
                let lat_pct =
                    if old.median_latency_ns > 0.0 { ((new.median_latency_ns / old.median_latency_ns) - 1.0) * 100.0 } else { 0.0 };

                writeln!(
                    out,
                    "{:<10} {:>12} {:>12} {:>+7.1}%  {:>9.1}ns {:>9.1}ns {:>+7.1}%",
                    tier.name(),
                    format_calibration_size(old.optimal_size),
                    format_calibration_size(new.optimal_size),
                    size_pct,
                    old.median_latency_ns,
                    new.median_latency_ns,
                    lat_pct,
                )?;
            }
            (None, Some(new)) => {
                writeln!(
                    out,
                    "{:<10} {:>12} {:>12} {:>8}  {:>10} {:>9.1}ns {:>8}",
                    tier.name(),
                    "-",
                    format_calibration_size(new.optimal_size),
                    "new",
                    "-",
                    new.median_latency_ns,
                    "new"
                )?;
            }
            (Some(old), None) => {
                writeln!(
                    out,
                    "{:<10} {:>12} {:>12} {:>8}  {:>9.1}ns {:>10} {:>8}",
                    tier.name(),
                    format_calibration_size(old.optimal_size),
                    "-",
                    "gone",
                    old.median_latency_ns,
                    "-",
                    "gone"
                )?;
            }
            (None, None) => {}
        }
    }
    writeln!(out, "───────────────────────────────────────────────────────────────────────────")?;
    writeln!(out, "  Existing: {} ({})", existing.timestamp.format("%Y-%m-%d %H:%M"), existing.page_size)?;
    writeln!(out, "  New:      {} ({})", new_results.timestamp.format("%Y-%m-%d %H:%M"), new_results.page_size)?;

    save.answer(&format!("Save new {} calibration?", cal_type), out)
}

/// The seal calibration's save prompt (TODO 99), as `prompt_calibration_save`'s: with none stored it saves (unless `--no-save`); otherwise
/// it shows each width's fill + check, old and new, and the width each chose, and asks `save`.
fn prompt_seal_calibration_save(
    existing: &Option<crate::calibration::SealCalibration>, new: &crate::calibration::SealCalibration, out: &mut dyn Write, save: &mut Save,
) -> io::Result<bool> {
    let Some(existing) = existing else {
        if !save.first() {
            writeln!(out, "\nNo existing seal calibration; --no-save keeps it that way.")?;
            return Ok(false);
        }
        writeln!(out, "\nNo existing seal calibration — saving new results.")?;
        return Ok(true);
    };
    writeln!(out, "\n📊 Seal Calibration Comparison (existing vs new, fill + check)")?;
    writeln!(out, "───────────────────────────────────────────────────────────────────────────")?;
    writeln!(out, "{:<10} {:>12} {:>12} {:>8}", "Width", "Old", "New", "Delta")?;
    writeln!(out, "───────────────────────────────────────────────────────────────────────────")?;
    let ms = |seal: &crate::calibration::SealCalibration, bits: u32| seal.widths.iter().find(|w| w.bits == bits).map(|w| w.ms);
    for bits in [128, 256, 512] {
        match (ms(existing, bits), ms(new, bits)) {
            (Some(old), Some(new)) => {
                let pct = if old > 0.0 { (new / old - 1.0) * 100.0 } else { 0.0 };
                writeln!(out, "{:<10} {:>9.1} ms {:>9.1} ms {:>+7.1}%", bits, old, new, pct)?;
            }
            (None, Some(new)) => writeln!(out, "{:<10} {:>12} {:>9.1} ms {:>8}", bits, "-", new, "new")?,
            (Some(old), None) => writeln!(out, "{:<10} {:>9.1} ms {:>12} {:>8}", bits, old, "-", "gone")?,
            (None, None) => {}
        }
    }
    writeln!(out, "───────────────────────────────────────────────────────────────────────────")?;
    let line = |seal: &crate::calibration::SealCalibration| {
        format!("{} ({}, {} bits)", seal.timestamp.format("%Y-%m-%d %H:%M"), seal.page_size, seal.width_bits)
    };
    writeln!(out, "  Existing: {}", line(existing))?;
    writeln!(out, "  New:      {}", line(new))?;

    save.answer("Save new seal calibration?", out)
}

fn format_calibration_size(bytes: usize) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TODO 7: `--show-results` sums each thread's records over every step, as the live run sums its totals (bytes, timed ms, errors), its
    /// CPU, core and node from the records; a thread with no CPU isn't in the tables, as in the live run's; WHEA and pinning from the file;
    /// and a file with no records has no tables
    #[test]
    fn a_saved_run_has_its_work_by_thread() {
        use crate::results::ThreadStepRecord;
        let mut run = crate::test_support::run_with_errors();
        for test in run.cycles.iter_mut().flat_map(|cycle| &mut cycle.tests) {
            test.threads.clear();
        }
        assert!(saved_work(&run).is_none(), "no records, no tables");
        let record = |thread, cpu: Option<usize>, bytes, ms, errors| ThreadStepRecord {
            thread,
            cpu,
            core: cpu.map(|cpu| cpu / 2),
            node: cpu.map(|_| 0),
            bytes,
            ms,
            errors,
            ..Default::default()
        };
        for test in run.cycles.iter_mut().flat_map(|cycle| &mut cycle.tests) {
            test.threads =
                vec![record(0, Some(0), 1 << 30, 1000.0, 1), record(1, Some(2), 2 << 30, 500.0, 0), record(2, None, 1 << 30, 9.0, 0)];
        }
        let steps = run.cycles.iter().map(|cycle| cycle.tests.len()).sum::<usize>() as u64;
        (run.overall_stats.whea_total, run.overall_stats.whea_corrected, run.run_config.threads.pinned) = (3, 2, false);
        let work = saved_work(&run).expect("records");
        let threads = &work.by_thread.threads;
        let rows: Vec<_> =
            threads.iter().map(|t| (t.thread_id, t.cpu_id, t.physical_core_id, t.total_bytes, t.total_time_ms, t.total_errors)).collect();
        assert_eq!(rows, [(0, 0, 0, steps << 30, 1000 * steps as u128, steps), (1, 2, 1, (2 * steps) << 30, 500 * steps as u128, 0)]);
        assert!((threads[1].throughput_mib_s - 4096.0).abs() < 1e-6, "2 GiB a step in 0.5 s: {}", threads[1].throughput_mib_s);
        assert_eq!((work.by_cpu.cpus.len(), work.by_core.cores.len()), (2, 2));
        assert_eq!((work.by_thread.whea.total, work.by_thread.whea.corrected, work.by_thread.pinned), (3, 2, false));
    }

    fn args(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    /// TODO 109: the window's comparison is the command's text but for its steps' lines, which are a table, a row a step; the plans'
    /// changes are in the text (TODO 72); a file that isn't there says so
    #[test]
    fn a_comparison_for_the_window_has_its_steps_as_a_table() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join("result_files").join("before_todo_72.json");
        let fixture = fixture.to_str().unwrap();
        let comparison = compare_results(fixture, fixture).expect("a run with itself");
        assert!(comparison.text.starts_with("=== TMR Test Result Comparison ==="), "{}", comparison.text);
        assert!(comparison.text.contains("\nPlan Changes: none, the same 3 steps in both\n"), "{}", comparison.text);
        assert!(!comparison.text.contains("Per-Test Performance Changes"), "the steps are the table's: {}", comparison.text);
        assert_eq!(comparison.per_test.rows.len(), 3, "a row a step");
        let unchanged = |row: &Vec<String>| row[2] == "+0.0 s" && row[6] == "+0" && row[7].is_empty();
        assert!(comparison.per_test.rows.iter().all(unchanged), "{:?}", comparison.per_test.rows);
        let missing = compare_results("nowhere.json", fixture).expect_err("no such file");
        assert!(missing.starts_with("Test result file does not exist: nowhere.json"), "{missing}");
    }

    /// The commands that read and change nothing on this machine, as they write, each after a `## case` line with how it ended (TODO 29):
    /// `--version`, `--help`, `--compare-results` without its files, of one that isn't there, and of a results file with itself (its time
    /// masked, and TMR's home, which is the test binary's folder), `--show-results` without its file, with two, and of one that isn't
    /// there (TODO 72; the CLI's golden has one shown), and the large-page commands' refusals of their target (TODO 113), which come before
    /// anything is read. None asks a question, nor has an event rendered.
    #[test]
    fn commands_write_as_their_golden() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join("result_files").join("before_todo_72.json");
        let fixture = fixture.to_str().unwrap();
        let cases = [
            ("--version", Command::Version),
            ("--help", Command::Help("tmr.exe".to_string())),
            ("--compare-results without its files", Command::Compare(args(&["tmr.exe", "--compare-results", "a.json"]))),
            (
                "--compare-results of a file that isn't there",
                Command::Compare(args(&["tmr.exe", "--compare-results", "nowhere.json", fixture])),
            ),
            ("--compare-results of a run with itself", Command::Compare(args(&["tmr.exe", "--compare-results", fixture, fixture]))),
            ("--show-results without its file", Command::ShowResults(args(&["tmr.exe", "--show-results"]))),
            ("--show-results of two files", Command::ShowResults(args(&["tmr.exe", "--show-results", fixture, fixture]))),
            ("--show-results of a file that isn't there", Command::ShowResults(args(&["tmr.exe", "--show-results", "nowhere.json"]))),
            // TODO 113: each refusal before anything is looked up, the target's type missing, unknown or with no name, or two targets
            ("--setup-large-pages without a type", Command::SetupLargePages(args(&["tmr.exe", "--setup-large-pages", "witches"]))),
            ("--setup-large-pages with an unknown type", Command::SetupLargePages(args(&["tmr.exe", "--setup-large-pages", "Team:x"]))),
            ("--setup-large-pages with no name", Command::SetupLargePages(args(&["tmr.exe", "--setup-large-pages", "Group:"]))),
            (
                "--setup-large-pages with two targets",
                Command::SetupLargePages(args(&["tmr.exe", "--setup-large-pages", "Group:Remote", "Desktop Users"])),
            ),
            ("--remove-large-pages without a type", Command::RemoveLargePages(args(&["tmr.exe", "--remove-large-pages", "Everyone"]))),
        ];
        let mut no_question = |question: &str| -> bool { panic!("asked: {question}") };
        let no_text = |_: &Event| -> String { panic!("an event rendered") };
        let mut text = String::new();
        for (case, command) in cases {
            let mut out = Vec::new();
            let status = run(&command, &mut out, &mut no_question, &no_text).unwrap();
            let written = String::from_utf8(out).unwrap().replace(&crate::home::dir().display().to_string(), "<home>");
            let written: String = written
                .split_inclusive('\n')
                .map(|line| if line.starts_with("Comparison Time: ") { "Comparison Time: <time>\n" } else { line })
                .collect();
            text += &format!("## {case}: {status:?}\n{written}");
        }
        crate::test_support::check("commands", &text);
    }

    /// TODO 72: `--show-results`' lines before the summary, each case's after a `## case` line: how the run ended, each outcome with each
    /// verdict it has, a last cycle cut short, a file saved mid-run, a file older than the outcome's record (the pre-TODO 72 fixture, which
    /// has no build or run ID either), and keys this build doesn't know; then which run it was, by this build or not.
    #[test]
    fn the_first_lines_say_how_the_run_ended() {
        use RunOutcome::{Completed, Halted, Interrupted, TimeLimit};

        use crate::reporting::converters::create_overall_stats_summary_report;
        use crate::test_support::{another_build, run_with_errors, stamped, summary};
        // `cycles` sealed cycles of a clean step each
        let clean = |cycles: u32| {
            let mut result = TestRunResult::new(Default::default());
            for cycle in 1..=cycles {
                result.add_cycle(cycle, 30, vec![summary(1, 30_000, 30 << 30, 0)], Some(Default::default()), false);
            }
            result.finalize(std::time::Duration::from_secs(30 * u64::from(cycles)));
            result
        };
        // `result` by this build, its last cycle cut short or not, saved at its end with how it `ended`, or, with none, mid-run
        let saved = |mut result: TestRunResult, cut_short: bool, ended: Option<(RunOutcome, RunStatus)>| {
            stamped(&mut result);
            if let Some(last) = result.cycles.last_mut() {
                last.partial = cut_short;
            }
            let stats = &mut result.overall_stats;
            match ended {
                Some((outcome, status)) => (stats.outcome, stats.status) = (Some(outcome.key().into()), Some(status.key().into())),
                None => stats.in_progress = true,
            }
            result
        };
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join("result_files").join("before_todo_72.json");
        let older = TestRunResult::load_from_file(fixture.to_str().unwrap()).unwrap();
        let mut by_another = saved(clean(2), false, None);
        by_another.metadata.build = Some(another_build());
        let mut unknown = saved(clean(2), false, None);
        (unknown.overall_stats.in_progress, unknown.overall_stats.outcome) = (false, Some("aborted".into()));
        let cases = [
            ("finished, passed", saved(clean(3), false, Some((Completed, RunStatus::Passed)))),
            ("finished, failed", saved(run_with_errors(), false, Some((Completed, RunStatus::Failed)))),
            ("finished, a cycle limit of 0", saved(clean(0), false, Some((Completed, RunStatus::NotRun)))),
            ("at its time limit, one cycle", saved(clean(1), false, Some((TimeLimit, RunStatus::Passed)))),
            ("at its time limit, failed", saved(run_with_errors(), false, Some((TimeLimit, RunStatus::Failed)))),
            ("at a time limit of 0", saved(clean(0), false, Some((TimeLimit, RunStatus::NotRun)))),
            ("stopped in its fourth cycle", saved(clean(4), true, Some((Interrupted, RunStatus::Interrupted)))),
            ("stopped in its first cycle", saved(clean(1), true, Some((Interrupted, RunStatus::Interrupted)))),
            ("stopped, with errors", saved(run_with_errors(), true, Some((Interrupted, RunStatus::Failed)))),
            ("stopped before a test ran", saved(clean(0), false, Some((Interrupted, RunStatus::NotRun)))),
            ("halted in its second cycle", saved(run_with_errors(), true, Some((Halted, RunStatus::Failed)))),
            ("halted by its final seal check", saved(run_with_errors(), false, Some((Halted, RunStatus::Failed)))),
            ("never finished, by another build", by_another),
            ("never finished, its last saved cycle cut short", saved(run_with_errors(), true, None)),
            ("older than the outcome's record", older),
            ("keys this build doesn't know", unknown),
        ];
        let mut text = String::new();
        for (case, result) in cases {
            text += &format!("## {case}\n{}", head(&result, &create_overall_stats_summary_report(&result)));
        }
        crate::test_support::check("show_results_heads", &text);
    }

    /// A command is the first argument: each of its spellings names it, it takes the command line it needs, and anything else (a parameter,
    /// a run mode like `--ram-latency`, no argument) is none.
    #[test]
    fn the_first_argument_names_the_command() {
        let parse = |line: &[&str]| Command::parse(&args(line));
        for help in ["--help", "-h", "/?", "-?"] {
            assert!(matches!(parse(&["tmr.exe", help]), Some(Command::Help(program)) if program == "tmr.exe"), "{help}");
        }
        assert!(matches!(parse(&["tmr.exe", "-v"]), Some(Command::Version)));
        assert!(matches!(parse(&["tmr.exe", "--create-demo-configs"]), Some(Command::DemoConfigs)));
        assert!(matches!(parse(&["tmr.exe", "--debug-topology"]), Some(Command::DebugTopology)));
        assert!(matches!(parse(&["tmr.exe", "--show-topology"]), Some(Command::ShowTopology)));
        let line = ["tmr.exe", "--setup-large-pages"];
        assert!(matches!(parse(&line), Some(Command::SetupLargePages(given)) if given == args(&line)));
        let line = ["tmr.exe", "--setup-large-pages", "Group:Administrators"];
        assert!(matches!(parse(&line), Some(Command::SetupLargePages(given)) if given == args(&line)), "with a target");
        assert!(crate::logfile::wanted(&args(&line)), "--setup-large-pages keeps a log file");
        let line = ["tmr.exe", "--remove-large-pages", r"User:.\alex"];
        assert!(matches!(parse(&line), Some(Command::RemoveLargePages(given)) if given == args(&line)));
        assert!(crate::logfile::wanted(&args(&line)), "--remove-large-pages keeps a log file");
        let line = ["tmr.exe", "--calibrate-all", "maxpage=regular"];
        assert!(matches!(parse(&line), Some(Command::Calibrate(given)) if given == args(&line)));
        let line = ["tmr.exe", "--compare-results", "a.json", "b.json"];
        assert!(matches!(parse(&line), Some(Command::Compare(given)) if given == args(&line)));
        let line = ["tmr.exe", "--show-results", "a.json"];
        assert!(matches!(parse(&line), Some(Command::ShowResults(given)) if given == args(&line)));
        assert!(!crate::logfile::wanted(&args(&line)), "--show-results keeps no log file, as --compare-results");
        for none in [&["tmr.exe"][..], &["tmr.exe", "memory=50%", "--help"], &["tmr.exe", "--startup-debug"]] {
            assert!(parse(none).is_none(), "{none:?}");
        }
    }

    /// A calibration's save: `--yes` saves and `--no-save` keeps, each without asking and saying so after the question; with neither,
    /// `confirm` answers it. Both together are refused. With nothing stored yet it saves without a question, unless `--no-save`.
    #[test]
    fn a_calibrations_save_takes_the_command_lines_answer() {
        let choice = |line: &[&str]| SaveChoice::of(&args(line));
        assert_eq!(choice(&["tmr.exe", "--calibrate-seal"]), Ok(SaveChoice::Ask));
        assert_eq!(choice(&["tmr.exe", "--calibrate-seal", "--yes"]), Ok(SaveChoice::Save));
        assert_eq!(choice(&["tmr.exe", "--calibrate-all", "maxpage=regular", "--no-save"]), Ok(SaveChoice::Keep));
        assert!(choice(&["tmr.exe", "--calibrate-cache", "--yes", "--no-save"]).is_err());

        for (choice, confirms, answer, shown) in [
            (SaveChoice::Ask, true, true, ""),
            (SaveChoice::Ask, false, false, ""),
            (SaveChoice::Save, false, true, "\nSave new seal calibration? yes (--yes)\n"),
            (SaveChoice::Keep, true, false, "\nSave new seal calibration? no (--no-save)\n"),
        ] {
            let mut asked = Vec::new();
            let mut confirm = |question: &str| {
                asked.push(question.to_string());
                confirms
            };
            let mut save = Save { choice, confirm: &mut confirm };
            let mut out = Vec::new();
            assert_eq!(save.answer("Save new seal calibration?", &mut out).unwrap(), answer, "{choice:?}");
            assert_eq!(save.first(), choice != SaveChoice::Keep, "{choice:?}");
            assert_eq!(String::from_utf8(out).unwrap(), shown, "{choice:?}");
            assert_eq!(asked.len(), usize::from(choice == SaveChoice::Ask), "{choice:?}: confirm asked {asked:?}");
        }
    }
}
