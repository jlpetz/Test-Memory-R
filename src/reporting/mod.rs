/// Reporting module for clean separation of data, formatting, and rendering
/// 
/// This module provides a three-layer architecture:
/// 1. Models - Pure data structures representing what to display
/// 2. Formatters - Logic for transforming and formatting data
/// 3. Renderers - Output mechanisms (console, GUI, JSON, etc.)
pub mod models;
pub mod formatters;
pub mod renderers;
pub mod converters;
pub mod system_info_builder;
#[cfg(test)]
mod tests;

use crate::Result;
use self::models::*;
use self::formatters::ReportFormatter;
use self::renderers::Renderer;

/// Main reporter that combines formatting and rendering
pub struct Reporter<R: Renderer> {
    formatter: Box<dyn ReportFormatter>,
    renderer: R,
}

impl<R: Renderer> Reporter<R> {
    pub fn new(formatter: Box<dyn ReportFormatter>, renderer: R) -> Self {
        Self { formatter, renderer }
    }


    /// Report system information
    pub fn report_system_info(&mut self, report: &SystemInfoReport) -> Result<()> {
        self.renderer.render_heading("System Information")?;

        // CPU info
        let cpu_table = self.formatter.prepare_cpu_info_table(&report.cpu_info);
        self.renderer.render_table(&cpu_table)?;

        // Cache info
        let cache_table = self.formatter.prepare_cache_info_table(&report.cache_info);
        self.renderer.render_table(&cache_table)?;

        // TSC calibration info
        let tsc_table = self.formatter.prepare_tsc_info_table(&report.tsc_info);
        self.renderer.render_table(&tsc_table)?;

        Ok(())
    }

    /// Report thread timing deviations
    pub fn report_thread_timing(&mut self, report: &ThreadTimingReport) -> Result<()> {
        let table = self.formatter.prepare_thread_timing_table(report);
        self.renderer.render_table(&table)?;
        Ok(())
    }
    
    /// Report performance by thread
    pub fn report_performance_by_thread(&mut self, report: &PerformanceByThreadReport) -> Result<()> {
        let table = self.formatter.prepare_performance_by_thread_table(report);
        self.renderer.render_table(&table)?;
        Ok(())
    }
    
    /// Report performance by CPU
    pub fn report_performance_by_cpu(&mut self, report: &PerformanceByCpuReport) -> Result<()> {
        let table = self.formatter.prepare_performance_by_cpu_table(report);
        self.renderer.render_table(&table)?;
        Ok(())
    }
    
    /// Report performance by physical core
    pub fn report_performance_by_physical_core(&mut self, report: &PerformanceByPhysicalCoreReport) -> Result<()> {
        let table = self.formatter.prepare_performance_by_physical_core_table(report);
        self.renderer.render_table(&table)?;
        Ok(())
    }
    
    /// Report CPU topology
    pub fn report_cpu_topology(&mut self, report: &CpuTopologyReport) -> Result<()> {
        let table = self.formatter.prepare_cpu_topology_table(report);
        self.renderer.render_table(&table)?;
        Ok(())
    }
    
    
    /// Report consolidated memory status and allocation plan
    pub fn report_consolidated_memory(&mut self, report: &ConsolidatedMemoryReport) -> Result<()> {
        let table = self.formatter.prepare_consolidated_memory_table(report);
        self.renderer.render_table(&table)?;
        
        // Render warnings if any
        for warning in &report.warnings {
            self.renderer.render_warning(warning)?;
        }
        
        Ok(())
    }
    
    /// Report test configuration
    pub fn report_test_configuration(&mut self, report: &TestConfigurationReport) -> Result<()> {
        let table = self.formatter.prepare_test_configuration_table(report);
        self.renderer.render_table(&table)?;
        for note in &report.notes {
            println!("{note}");
        }
        Ok(())
    }
    
    /// Report cycle completion
    #[expect(dead_code, reason = "TODO #29: the per-cycle report is to be revived, not deleted (from TODO #69 F)")]
    pub fn report_cycle(&mut self, report: &CycleReport) -> Result<()> {
        let table = self.formatter.prepare_cycle_report_table(report);
        self.renderer.render_table(&table)?;
        Ok(())
    }
    
    /// Report final test summary
    pub fn report_final_summary(&mut self, report: &FinalTestSummaryReport) -> Result<()> {
        self.renderer.render_separator()?;
        
        // Render overview table
        let overview_table = self.formatter.prepare_final_summary_overview_table(report);
        self.renderer.render_table(&overview_table)?;
        
        // Only show per-test performance if there are tests
        if !report.per_test_summaries.is_empty() {
            self.renderer.render_separator()?;
            let performance_table = self.formatter.prepare_final_summary_performance_table(report);
            self.renderer.render_table(&performance_table)?;
            // The work around the tests, by operation (TODO 90)
            self.renderer.render_separator()?;
            let table = self.formatter.prepare_intermediate_table(report);
            self.renderer.render_table(&table)?;
        }

        // Where in the sequence it failed, only when something did (TODO 74)
        let final_check_errors = report.seal.as_ref().map_or(0, |s| s.final_check_errors);
        if !report.errors_by_step.is_empty() || final_check_errors > 0 {
            self.renderer.render_separator()?;
            let table = self.formatter.prepare_errors_by_step_table(report);
            self.renderer.render_table(&table)?;
        }
        // Which bits failed, only when any did (TODO 97)
        if !report.failing_bits.is_empty() || !report.seal_failing_bits.is_empty() {
            self.renderer.render_separator()?;
            let table = self.formatter.prepare_failing_bits_table(report);
            self.renderer.render_table(&table)?;
        }
        
        Ok(())
    }
    
    /// Report latency test summary results (multi-threaded)
    pub fn report_latency_summary(&mut self, report: &LatencyTestSummaryReport) -> Result<()> {
        // Simple label - test name and extent are already shown in the main test header and config line
        println!("🔍 Latency");

        // Per-level detailed tables (per-thread breakdown with ALL row)
        for level in &report.levels_tested {
            let per_thread_table = self.formatter.prepare_latency_per_thread_table(level);
            self.renderer.render_table(&per_thread_table)?;
        }

        Ok(())
    }

    /// The allocation report (TODO 81): by node, the requests, by thread, each thread's requests,
    /// and with more than one thread, how evenly they were served.
    pub fn report_allocation(&mut self, report: &AllocationReport) -> Result<()> {
        let mut tables = vec![
            self.formatter.prepare_allocation_nodes_table(report),
            self.formatter.prepare_allocation_commits_table(report),
            self.formatter.prepare_allocation_threads_table(report),
            self.formatter.prepare_thread_commits_table(report),
        ];
        if report.threads.len() > 1 {
            tables.push(self.formatter.prepare_fairness_by_thread_table(report));
            tables.push(self.formatter.prepare_fairness_summary_table(report));
        }
        for table in &tables {
            println!();
            self.renderer.render_table(table)?;
        }
        Ok(())
    }
}

/// Convenience function to create a console reporter with default settings
pub fn create_console_reporter() -> Reporter<renderers::ConsoleRenderer> {
    let formatter = Box::new(formatters::DefaultFormatter::new());
    let renderer = renderers::ConsoleRenderer::new();
    Reporter::new(formatter, renderer)
}