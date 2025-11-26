//! TSC (Time Stamp Counter) detection and calibration module
//!
//! Provides multiple methods for detecting TSC frequency with cross-validation.
//! Uses quanta-style statistical calibration with Welford's algorithm for
//! accurate frequency measurement on AMD and other CPUs without CPUID frequency leaves.
//!
//! TSC is x86_64 specific - ARM has different timing mechanisms (CNTVCT_EL0).

use std::time::{Duration, Instant};

/// TSC detection method used
#[derive(Debug, Clone, PartialEq)]
pub enum TscDetectionMethod {
    /// CPUID leaf 0x15 - Crystal frequency × ratio (Intel)
    CpuidLeaf15,
    /// CPUID leaf 0x16 - Processor frequency info (Intel)
    CpuidLeaf16,
    /// Windows QueryPerformanceFrequency
    WindowsQpc,
    /// Timing-based calibration (100ms measurement)
    TimingCalibration,
    /// Multiple methods agreed
    Validated { primary: Box<TscDetectionMethod>, secondary: Box<TscDetectionMethod> },
    /// Not available (non-x86_64 platform)
    NotAvailable,
}

impl std::fmt::Display for TscDetectionMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TscDetectionMethod::CpuidLeaf15 => write!(f, "CPUID 0x15"),
            TscDetectionMethod::CpuidLeaf16 => write!(f, "CPUID 0x16"),
            TscDetectionMethod::WindowsQpc => write!(f, "Windows QPC"),
            TscDetectionMethod::TimingCalibration => write!(f, "Timing calibration"),
            TscDetectionMethod::Validated { primary, .. } => write!(f, "{} (validated)", primary),
            TscDetectionMethod::NotAvailable => write!(f, "Not available"),
        }
    }
}

/// Calibration statistics using Welford's online algorithm
/// for computing running mean and variance in a single pass
#[derive(Debug, Clone)]
pub struct CalibrationStats {
    /// Number of samples collected
    pub samples: u32,
    /// Running mean of frequency measurements (GHz)
    pub mean_ghz: f64,
    /// Running M2 for variance calculation (Welford's algorithm)
    m2: f64,
    /// Total calibration time
    pub calibration_time_ms: u64,
    /// Whether calibration converged early
    pub converged: bool,
}

impl Default for CalibrationStats {
    fn default() -> Self {
        Self {
            samples: 0,
            mean_ghz: 0.0,
            m2: 0.0,
            calibration_time_ms: 0,
            converged: false,
        }
    }
}

impl CalibrationStats {
    /// Add a new sample using Welford's online algorithm
    fn add_sample(&mut self, ghz: f64) {
        self.samples += 1;
        let delta = ghz - self.mean_ghz;
        self.mean_ghz += delta / self.samples as f64;
        let delta2 = ghz - self.mean_ghz;
        self.m2 += delta * delta2;
    }

    /// Get the sample variance
    fn variance(&self) -> f64 {
        if self.samples < 2 {
            return f64::MAX;
        }
        self.m2 / (self.samples - 1) as f64
    }

    /// Get standard deviation in GHz
    pub fn std_dev_ghz(&self) -> f64 {
        self.variance().sqrt()
    }

    /// Get standard error of the mean in GHz
    pub fn std_error_ghz(&self) -> f64 {
        if self.samples < 2 {
            return f64::MAX;
        }
        self.std_dev_ghz() / (self.samples as f64).sqrt()
    }

    /// Get standard error in nanoseconds (for convergence check)
    /// Since frequency is in GHz (cycles/ns), error in ns = error_ghz / mean_ghz² × some_factor
    /// But for simplicity, we express error as a ratio
    pub fn relative_error(&self) -> f64 {
        if self.mean_ghz == 0.0 {
            return f64::MAX;
        }
        self.std_error_ghz() / self.mean_ghz
    }
}

/// TSC information including frequency and detection metadata
#[derive(Debug, Clone)]
pub struct TscInfo {
    /// TSC frequency in GHz (cycles per nanosecond)
    pub frequency_ghz: f64,
    /// Detection method used
    pub detection_method: TscDetectionMethod,
    /// Whether TSC is invariant (constant rate regardless of power state)
    pub is_invariant: bool,
    /// Confidence level (0.0 - 1.0) based on validation
    pub confidence: f64,
    /// Optional: frequency from secondary method for comparison
    pub secondary_frequency_ghz: Option<f64>,
    /// Calibration statistics (if timing calibration was used)
    pub calibration_stats: Option<CalibrationStats>,
}

impl Default for TscInfo {
    fn default() -> Self {
        Self {
            frequency_ghz: 0.0,
            detection_method: TscDetectionMethod::NotAvailable,
            is_invariant: false,
            confidence: 0.0,
            secondary_frequency_ghz: None,
            calibration_stats: None,
        }
    }
}

impl TscInfo {
    /// Detect TSC frequency using multiple methods with validation
    #[cfg(target_arch = "x86_64")]
    pub fn detect() -> Self {
        log::info!("Detecting TSC frequency...");

        // Check if TSC is invariant (important for reliable timing)
        let is_invariant = check_tsc_invariant();
        if is_invariant {
            log::info!("  TSC: Invariant TSC detected (constant rate)");
        } else {
            log::warn!("  TSC: Non-invariant TSC - frequency may vary with power state");
        }

        // Try multiple detection methods
        let cpuid_15_result = detect_via_cpuid_leaf_15();
        let cpuid_16_result = detect_via_cpuid_leaf_16();
        let (timing_freq, timing_stats) = detect_via_timing_calibration();

        // Log all results
        if let Some((freq, _)) = cpuid_15_result {
            log::info!("  TSC: CPUID 0x15 reports {:.3} GHz", freq);
        }
        if let Some((freq, _)) = cpuid_16_result {
            log::info!("  TSC: CPUID 0x16 reports {:.3} GHz", freq);
        }

        let convergence_str = if timing_stats.converged { "converged" } else { "completed" };
        log::info!(
            "  TSC: Calibration {} at {:.6} GHz ({} samples in {}ms, σ={:.6} GHz)",
            convergence_str,
            timing_freq,
            timing_stats.samples,
            timing_stats.calibration_time_ms,
            timing_stats.std_dev_ghz()
        );

        // Determine best result with validation
        let (frequency_ghz, detection_method, confidence, secondary) =
            validate_and_select(cpuid_15_result, cpuid_16_result, timing_freq, &timing_stats);

        log::info!("  TSC: Selected {:.3} GHz via {} (confidence: {:.0}%)",
                   frequency_ghz, detection_method, confidence * 100.0);

        TscInfo {
            frequency_ghz,
            detection_method,
            is_invariant,
            confidence,
            secondary_frequency_ghz: secondary,
            calibration_stats: Some(timing_stats),
        }
    }

    #[cfg(not(target_arch = "x86_64"))]
    pub fn detect() -> Self {
        log::warn!("TSC frequency detection not available on non-x86_64 platforms");
        Self::default()
    }
}

/// Check if TSC is invariant (CPUID.80000007H:EDX[8])
#[cfg(target_arch = "x86_64")]
fn check_tsc_invariant() -> bool {
    use raw_cpuid::CpuId;

    let cpuid = CpuId::new();
    if let Some(adv_pm) = cpuid.get_advanced_power_mgmt_info() {
        adv_pm.has_invariant_tsc()
    } else {
        false
    }
}

/// Detect TSC via CPUID leaf 0x15 (Time Stamp Counter/Core Crystal Clock)
/// Available on Intel Skylake+ and some newer AMD
/// Formula: TSC_freq = (ECX * EBX) / EAX
#[cfg(target_arch = "x86_64")]
fn detect_via_cpuid_leaf_15() -> Option<(f64, TscDetectionMethod)> {
    use raw_cpuid::CpuId;

    let cpuid = CpuId::new();
    let tsc_info = cpuid.get_tsc_info()?;

    let denominator = tsc_info.denominator() as u64;
    let numerator = tsc_info.numerator() as u64;

    if denominator == 0 || numerator == 0 {
        log::debug!("CPUID 0x15: denominator={}, numerator={} - invalid", denominator, numerator);
        return None;
    }

    // Try to get crystal frequency from the leaf itself
    let crystal_hz = tsc_info.nominal_frequency() as u64;
    if crystal_hz == 0 {
        // Crystal frequency not provided - need to derive or skip
        log::debug!("CPUID 0x15: Crystal frequency not provided (returned 0)");
        return None;
    }

    // TSC frequency = (crystal * numerator) / denominator
    let tsc_hz = (crystal_hz * numerator) / denominator;
    let tsc_ghz = tsc_hz as f64 / 1_000_000_000.0;

    log::debug!("CPUID 0x15: crystal={}Hz, num={}, denom={} -> TSC={:.3}GHz",
                crystal_hz, numerator, denominator, tsc_ghz);

    Some((tsc_ghz, TscDetectionMethod::CpuidLeaf15))
}

/// Detect TSC via CPUID leaf 0x16 (Processor Frequency Information)
/// Returns base frequency in MHz - available on Intel and some AMD
#[cfg(target_arch = "x86_64")]
fn detect_via_cpuid_leaf_16() -> Option<(f64, TscDetectionMethod)> {
    use raw_cpuid::CpuId;

    let cpuid = CpuId::new();
    let freq_info = cpuid.get_processor_frequency_info()?;

    let base_mhz = freq_info.processor_base_frequency();
    if base_mhz == 0 {
        log::debug!("CPUID 0x16: Base frequency is 0");
        return None;
    }

    let tsc_ghz = base_mhz as f64 / 1000.0;

    log::debug!("CPUID 0x16: base={}MHz, max={}MHz -> TSC={:.3}GHz",
                base_mhz, freq_info.processor_max_frequency(), tsc_ghz);

    Some((tsc_ghz, TscDetectionMethod::CpuidLeaf16))
}

/// Detect TSC via statistical timing calibration (quanta-style)
///
/// Uses Welford's algorithm for online mean/variance calculation with:
/// - Up to 200ms wall time limit
/// - Early exit on convergence (500+ samples, relative error < 0.001%)
/// - ~1μs busy-loop samples for high precision
///
/// Most reliable method that works on all x86_64 CPUs (especially AMD)
#[cfg(target_arch = "x86_64")]
fn detect_via_timing_calibration() -> (f64, CalibrationStats) {
    use std::arch::x86_64::_rdtsc;

    // Calibration parameters
    // Longer samples (2ms) reduce OS timing overhead and give more stable readings
    // At 2ms per sample, we can get ~100 samples in 200ms which is plenty for convergence
    const MAX_CALIBRATION_TIME: Duration = Duration::from_millis(200);
    const MIN_SAMPLES: u32 = 100;  // Require more samples for better statistics
    const TARGET_RELATIVE_ERROR: f64 = 0.00001; // 0.001% relative error (quanta's original threshold)
    const SAMPLE_DURATION_US: u64 = 2000; // 2ms per sample - longer for better stability

    let mut stats = CalibrationStats::default();
    let calibration_start = Instant::now();

    loop {
        // Take a single measurement
        unsafe {
            let start_tsc = _rdtsc();
            let start_time = Instant::now();

            // Busy-wait for sample duration (more accurate than sleep for short durations)
            while start_time.elapsed() < Duration::from_micros(SAMPLE_DURATION_US) {
                std::hint::spin_loop();
            }

            let end_tsc = _rdtsc();
            let elapsed_ns = start_time.elapsed().as_nanos() as f64;

            if elapsed_ns > 0.0 {
                let cycles = (end_tsc - start_tsc) as f64;
                let ghz = cycles / elapsed_ns; // GHz = cycles/nanosecond

                // Filter out obvious outliers (should be 0.5-10 GHz range)
                if ghz > 0.5 && ghz < 10.0 {
                    stats.add_sample(ghz);
                }
            }
        }

        let elapsed = calibration_start.elapsed();

        // Check for early convergence (quanta-style)
        if stats.samples >= MIN_SAMPLES {
            let rel_error = stats.relative_error();
            if rel_error < TARGET_RELATIVE_ERROR {
                stats.converged = true;
                stats.calibration_time_ms = elapsed.as_millis() as u64;
                log::debug!(
                    "TSC calibration converged after {} samples in {}ms (error: {:.6}%)",
                    stats.samples,
                    stats.calibration_time_ms,
                    rel_error * 100.0
                );
                break;
            }
        }

        // Check for time limit
        if elapsed >= MAX_CALIBRATION_TIME {
            stats.calibration_time_ms = elapsed.as_millis() as u64;
            log::debug!(
                "TSC calibration completed after {} samples in {}ms (error: {:.6}%)",
                stats.samples,
                stats.calibration_time_ms,
                stats.relative_error() * 100.0
            );
            break;
        }
    }

    (stats.mean_ghz, stats)
}

/// Validate results and select best frequency
/// Returns (frequency, method, confidence, optional_secondary)
#[cfg(target_arch = "x86_64")]
fn validate_and_select(
    cpuid_15: Option<(f64, TscDetectionMethod)>,
    cpuid_16: Option<(f64, TscDetectionMethod)>,
    timing: f64,
    timing_stats: &CalibrationStats,
) -> (f64, TscDetectionMethod, f64, Option<f64>) {
    const TOLERANCE: f64 = 0.02; // 2% tolerance for validation

    // Helper to check if two frequencies match within tolerance
    let frequencies_match = |a: f64, b: f64| -> bool {
        let diff = (a - b).abs();
        let avg = (a + b) / 2.0;
        diff / avg < TOLERANCE
    };

    // Calculate calibration confidence based on convergence and sample count
    // Converged with many samples = higher confidence
    let calibration_confidence = if timing_stats.converged {
        // Converged: base 0.95, up to 0.99 based on sample count
        let sample_bonus = (timing_stats.samples as f64 / 5000.0).min(0.04);
        0.95 + sample_bonus
    } else {
        // Didn't converge: base 0.85, scaled by how close we got
        let error_factor = (1.0 - timing_stats.relative_error() * 1000.0).max(0.0);
        0.85 + error_factor * 0.05
    };

    // Case 1: CPUID 0x15 available and matches timing
    if let Some((freq_15, method_15)) = cpuid_15.clone() {
        if frequencies_match(freq_15, timing) {
            return (
                freq_15,
                TscDetectionMethod::Validated {
                    primary: Box::new(method_15),
                    secondary: Box::new(TscDetectionMethod::TimingCalibration),
                },
                0.99, // High confidence - hardware + measurement agree
                Some(timing),
            );
        } else {
            log::warn!("TSC: CPUID 0x15 ({:.3} GHz) differs from calibration ({:.3} GHz) by {:.1}%",
                       freq_15, timing, ((freq_15 - timing).abs() / timing) * 100.0);
        }
    }

    // Case 2: CPUID 0x16 available and matches timing
    if let Some((freq_16, method_16)) = cpuid_16.clone() {
        if frequencies_match(freq_16, timing) {
            return (
                freq_16,
                TscDetectionMethod::Validated {
                    primary: Box::new(method_16),
                    secondary: Box::new(TscDetectionMethod::TimingCalibration),
                },
                0.97, // Good confidence - leaf 0x16 may report nominal, not actual
                Some(timing),
            );
        } else {
            log::warn!("TSC: CPUID 0x16 ({:.3} GHz) differs from calibration ({:.3} GHz) by {:.1}%",
                       freq_16, timing, ((freq_16 - timing).abs() / timing) * 100.0);
        }
    }

    // Case 3: CPUID values match each other (even if different from timing)
    if let (Some((freq_15, _)), Some((freq_16, _))) = (cpuid_15.clone(), cpuid_16.clone()) {
        if frequencies_match(freq_15, freq_16) {
            log::info!("TSC: CPUID 0x15 and 0x16 agree, using hardware value");
            return (
                freq_15,
                TscDetectionMethod::CpuidLeaf15,
                0.92,
                Some(timing),
            );
        }
    }

    // Case 4: Fall back to timing calibration with dynamic confidence
    // For AMD and other CPUs without CPUID frequency leaves, calibration is authoritative
    if let Some((freq_15, _)) = cpuid_15 {
        // If within 5%, note the discrepancy but trust calibration
        let diff_pct = ((freq_15 - timing).abs() / timing) * 100.0;
        if diff_pct < 5.0 {
            return (
                timing,
                TscDetectionMethod::TimingCalibration,
                calibration_confidence.min(0.90), // Cap at 0.90 when CPUID differs
                Some(freq_15),
            );
        }
    }

    // Pure timing calibration - confidence based on statistical quality
    (
        timing,
        TscDetectionMethod::TimingCalibration,
        calibration_confidence,
        cpuid_16.map(|(f, _)| f),
    )
}

#[cfg(not(target_arch = "x86_64"))]
fn check_tsc_invariant() -> bool {
    false
}

#[cfg(not(target_arch = "x86_64"))]
fn detect_via_cpuid_leaf_15() -> Option<(f64, TscDetectionMethod)> {
    None
}

#[cfg(not(target_arch = "x86_64"))]
fn detect_via_cpuid_leaf_16() -> Option<(f64, TscDetectionMethod)> {
    None
}

#[cfg(not(target_arch = "x86_64"))]
fn detect_via_timing_calibration() -> (f64, CalibrationStats) {
    (0.0, CalibrationStats::default())
}

#[cfg(not(target_arch = "x86_64"))]
fn validate_and_select(
    _cpuid_15: Option<(f64, TscDetectionMethod)>,
    _cpuid_16: Option<(f64, TscDetectionMethod)>,
    _timing: f64,
    _timing_stats: &CalibrationStats,
) -> (f64, TscDetectionMethod, f64, Option<f64>) {
    (0.0, TscDetectionMethod::NotAvailable, 0.0, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tsc_detection() {
        let info = TscInfo::detect();
        println!("TSC Info: {:?}", info);

        #[cfg(target_arch = "x86_64")]
        {
            assert!(info.frequency_ghz > 0.0, "TSC frequency should be positive");
            assert!(info.frequency_ghz < 10.0, "TSC frequency should be reasonable (<10 GHz)");
        }
    }
}
