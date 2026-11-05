//! The reports the window opens beside its results (TODO 109): a step's, and the run's final summary. Their tables are the engine's
//! (`tmr::frontend::Reporter`, the default formatting the console renders as text), kept here as data for the window to draw, so the two
//! show the same tables; a step's lines are its console report's, in the window's words. Plain Rust with no Reactor type, as `model.rs`.

use std::error::Error;

use tmr::frontend::{CpuPerformanceSummary, FinalTestSummaryReport, Renderer, Reporter, StepReport, TableData};

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// A part of a report, in the order the engine's reporter rendered it
#[derive(Clone, Debug)]
pub enum Block {
    Heading(String),
    /// A line as it is: a table's label, a note
    Text(String),
    Warning(String),
    Table(TableData),
    /// A rule between the final summary's tables
    Separator,
}

/// A report the window opens: its title, its verdict and whether it failed, its lines, then its tables
#[derive(Clone, Debug)]
pub struct Report {
    pub title: String,
    pub verdict: String,
    pub failed: bool,
    pub lines: Vec<String>,
    pub blocks: Vec<Block>,
}

impl Report {
    /// A step's: the console's step report (crates/tmr-cli's `render/run.rs`, `step_report`), its first line the verdict, and its threads'
    /// table and a latency step's latencies the engine's, a loaded latency test's by load
    pub fn of_step(report: &StepReport) -> Self {
        let StepReport { cycle, summary: s, wall, timed_secs, cycle_limit, stopped_by_time, config, threads, latency, .. } = report;
        let errors = report.errors();
        let failed = errors.total() > 0;
        let verdict = format!("{} · {:.1} s · {errors}", if failed { "❌ Failed" } else { "✅ Passed" }, wall.as_secs_f64());

        let ops_per_sec = if *timed_secs > 0.0 { s.operations as f64 / timed_secs } else { 0.0 };
        // A loaded latency test's headline, its rows being a load each (TODO 117)
        let latency_line = match (s.loaded.as_deref(), s.latency_p50_ns, s.latency_spread) {
            (Some(loaded), _, _) => format!(", {}", loaded.headline()),
            (None, Some(p50), Some(spread)) => format!(", P50 {p50:.1} ns, spread {spread:.2}x"),
            _ => String::new(),
        };
        // The bits that failed first, as the console's report has them (TODO 109)
        let mut lines: Vec<String> = report.failing_bits_line().into_iter().collect();
        lines.extend([
            format!(
                "Test: {timed_secs:.1} s, {:.2} GiB @ {} MiB/s, {} ops @ {} ops/s{latency_line}",
                s.bytes_processed as f64 / GIB,
                crate::model::thousands(s.throughput_mib_s),
                count(s.operations as f64),
                count(ops_per_sec)
            ),
            format!("Intermediate: {}", s.ops.line()),
        ]);
        let reps = if config.test_reps != 1 || config.verify_reps != 1 || config.write_read_cycles != 1 {
            format!(", test_reps={}, verify_reps={}, wrc={}", config.test_reps, config.verify_reps, config.write_read_cycles)
        } else {
            String::new()
        };
        lines.push(format!(
            "Config: parameter={}, mode={}, extent={}, chunk={}{reps}",
            config.parameter, config.mode, config.extent, config.chunk
        ));
        // Which limit ended it, as the console's 🔴 marks it
        let done = s.cycles_completed.1;
        if done > 0 || cycle_limit.is_some() {
            let ended = if *stopped_by_time {
                ", ended by the time limit"
            } else if cycle_limit.is_some_and(|limit| done >= limit) {
                ", ended by its cycle limit"
            } else {
                ""
            };
            lines.push(format!("Cycles: {done}/{}{ended}", cycle_limit.map_or("∞".to_string(), |limit| limit.to_string())));
        }

        let blocks = blocks(|r| {
            // A loaded latency test's are its loaders', none when it has none (TODO 117)
            if s.loaded.is_none() || !threads.thread_timings.is_empty() {
                r.report_thread_timing(threads)?;
            }
            match (s.loaded.as_deref(), latency) {
                (Some(loaded), _) => r.report_loaded(loaded),
                (None, Some(latency)) => r.report_latency_summary(latency),
                (None, None) => Ok(()),
            }
        });
        Report { title: format!("Cycle {cycle} · step {} · {}", s.step, s.label), verdict, failed, lines, blocks }
    }

    /// The run's final summary: the engine's tables, the overview's verdict first, then the run's work by thread, CPU and core, `work`
    /// (`cpu_performance`; the console prints it just before the summary)
    pub fn of_summary(report: &FinalTestSummaryReport, work: Vec<Block>) -> Self {
        // The overview's: every error fails the run, WHEA's among them
        let errors = report.errors();
        let failed = errors.total() > 0;
        let whea = if report.whea_monitored { "" } else { ", WHEA not monitored" };
        let cycles = report.cycles_completed;
        let verdict = format!(
            "{} · {cycles} cycle{} in {} · {errors}{whea}",
            if failed { "❌ Failed" } else { "✅ Passed" },
            if cycles == 1 { "" } else { "s" },
            report.total_runtime
        );
        let mut blocks = blocks(|r| r.report_final_summary(report));
        // Its rules go between tables: none above the first
        if matches!(blocks.first(), Some(Block::Separator)) {
            blocks.remove(0);
        }
        if !work.is_empty() {
            blocks.push(Block::Separator);
            blocks.push(Block::Heading("The run's work, by thread, CPU and core".to_string()));
            blocks.extend(work);
        }
        Report { title: "Final summary".to_string(), verdict, failed, lines: Vec::new(), blocks }
    }
}

/// The run's work by thread, by CPU and by physical core: the engine's tables, each that has rows (TODO 109)
pub fn cpu_performance(summary: &CpuPerformanceSummary) -> Vec<Block> {
    blocks(|r| {
        if !summary.by_thread.threads.is_empty() {
            r.report_performance_by_thread(&summary.by_thread)?;
        }
        if !summary.by_cpu.cpus.is_empty() {
            r.report_performance_by_cpu(&summary.by_cpu)?;
        }
        if !summary.by_core.cores.is_empty() {
            r.report_performance_by_physical_core(&summary.by_core)?;
        }
        Ok(())
    })
}

/// `{:.2}` with B, M or K, as the console's step report counts operations: `1.00M`
fn count(n: f64) -> String {
    match n {
        n if n >= 1e9 => format!("{:.2}B", n / 1e9),
        n if n >= 1e6 => format!("{:.2}M", n / 1e6),
        n if n >= 1e3 => format!("{:.2}K", n / 1e3),
        n => format!("{n:.0}"),
    }
}

/// The engine's reports, rendered as blocks
#[derive(Default)]
pub(crate) struct Blocks(Vec<Block>);

type Rendered = Result<(), Box<dyn Error>>;

impl Blocks {
    fn push(&mut self, block: Block) -> Rendered {
        self.0.push(block);
        Ok(())
    }
}

impl Renderer for Blocks {
    fn render_table(&mut self, table: &TableData) -> Rendered {
        self.push(Block::Table(table.clone()))
    }

    fn render_heading(&mut self, text: &str) -> Rendered {
        self.push(Block::Heading(text.to_string()))
    }

    /// An empty line spaces the console's tables; the window spaces its blocks itself
    fn render_text(&mut self, text: &str) -> Rendered {
        if text.trim().is_empty() { Ok(()) } else { self.push(Block::Text(text.to_string())) }
    }

    fn render_info(&mut self, text: &str) -> Rendered {
        self.render_text(text)
    }

    fn render_warning(&mut self, text: &str) -> Rendered {
        self.push(Block::Warning(text.to_string()))
    }

    fn render_error(&mut self, text: &str) -> Rendered {
        self.push(Block::Warning(text.to_string()))
    }

    fn render_success(&mut self, text: &str) -> Rendered {
        self.render_text(text)
    }

    fn render_separator(&mut self) -> Rendered {
        self.push(Block::Separator)
    }
}

/// What `report` renders, as blocks
pub(crate) fn blocks(report: impl FnOnce(&mut Reporter<Blocks>) -> Rendered) -> Vec<Block> {
    let mut reporter = Reporter::new(Blocks::default());
    // Rendering into blocks can't fail: a report's own error leaves what it rendered before it
    let _ = report(&mut reporter);
    reporter.into_renderer().0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tables' titles and headings, in order
    fn outline(report: &Report) -> Vec<String> {
        report
            .blocks
            .iter()
            .map(|block| match block {
                Block::Heading(text) | Block::Text(text) | Block::Warning(text) => text.clone(),
                Block::Table(table) => format!("table {}", table.title.as_deref().unwrap_or("")),
                Block::Separator => "—".to_string(),
            })
            .collect()
    }

    /// The final summary's tables are the console's, every one the fixture has, with no rule above the first; its verdict fails it on
    /// errors or WHEA, and says when WHEA wasn't watched
    #[test]
    fn the_final_summary_is_the_consoles_tables() {
        let summary = tmr::test_support::final_summary_report();
        let report = Report::of_summary(&summary, Vec::new());
        assert_eq!(outline(&report), [
            "table Final Test Summary - Overview",
            "—",
            "table Final Test Summary - Per-Test Performance",
            "—",
            "table Final Test Summary - Intermediate Operations",
            "—",
            "table Errors by Step",
            "—",
            "table Failing Bits"
        ]);
        assert!(report.failed);
        assert!(report.verdict.starts_with("❌ Failed · 2 cycles in "), "{}", report.verdict);

        let clean =
            FinalTestSummaryReport { total_errors: 0, seal: None, whea_total: 0, whea_corrected: 0, whea_monitored: false, ..summary };
        let report = Report::of_summary(&clean, vec![Block::Text("the work".to_string())]);
        assert!(!report.failed);
        let outline = outline(&report);
        assert_eq!(outline[outline.len() - 3..], ["—", "The run's work, by thread, CPU and core", "the work"], "the run's work last");
        assert!(
            report.verdict.starts_with("✅ Passed · ") && report.verdict.ends_with("0 errors (0 test + 0 WHEA), WHEA not monitored"),
            "{}",
            report.verdict
        );
    }

    /// TODO 117: a loaded latency step's report has its headline on its Test: line and its rows in place of a latency table, after its
    /// loaders' timing table: the engine's lines and table, as the console's (`test_support::loaded_step_report`, crates/tmr-cli's events
    /// golden)
    #[test]
    fn a_loaded_steps_report_is_its_rows() {
        let report = Report::of_step(&tmr::test_support::loaded_step_report());
        assert_eq!(report.title, "Cycle 1 · step 2 · Lat-Loaded-DRAM");
        assert!(
            report.lines[0].starts_with("Test: 9.6 s, 91.00 GiB @ 9,707 MiB/s, ")
                && report.lines[0].ends_with(" ops/s, Under load: writes 451.7 ns @ 16117 MiB/s, reads 264.8 ns @ 27180 MiB/s"),
            "{}",
            report.lines[0]
        );
        assert!(!report.lines.iter().any(|line| line.contains("P50")), "{:?}", report.lines);
        let outline = outline(&report);
        assert_eq!((outline.len(), outline[0].as_str(), outline[3].as_str()), (4, "table ", "table "), "{outline:?}");
        assert!(
            outline[1] == "🔍 Under load: thread 0 (CPU 2, core 1, node 0) measures; 7 threads load, one a core; 8 sit out"
                && outline[2].starts_with("   1.00 GiB chain in 128 B cells; 256-bit loaders in 4 KiB blocks; 1.0 s windows"),
            "the engine's header lines: {outline:?}"
        );
        let Some(Block::Table(rows)) = report.blocks.last() else { panic!("{:?}", report.blocks) };
        assert_eq!(rows.rows.iter().map(|row| row[0].as_str()).collect::<Vec<_>>(), ["idle", "full"]);
        let Some(Block::Table(threads)) = report.blocks.first() else { panic!("{:?}", report.blocks) };
        assert_eq!(threads.rows.len(), 8, "the 7 loaders and their average");
    }

    #[test]
    fn operations_count_as_the_console_does() {
        assert_eq!([999.0, 1_000.0, 2_500_000.0, 3e9].map(count), ["999", "1.00K", "2.50M", "3.00B"]);
    }
}
