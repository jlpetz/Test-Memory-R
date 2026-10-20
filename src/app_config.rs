//! TMR Application Configuration
//!
//! Persistent configuration file (`tmr-cfg.json`) stored next to the executable.
//! Holds machine identity, calibration results, UI preferences, and session state.
//! All fields are optional — missing fields use defaults, unknown fields are preserved.

use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use crate::cache::SystemInfo;
use crate::calibration::{CalibrationResults, SealCalibration};
use crate::smbios::{SmbiosData, MemoryModule};

/// Readable CPU + BIOS identity snapshot stored alongside the hash.
/// When machine_id mismatches, these fields let you see exactly what changed.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MachineIdentity {
    pub cpu_vendor: String,
    pub cpu_brand: String,
    pub cpu_family: u32,
    pub cpu_model: u32,
    pub cpu_stepping: u32,
    /// CPU base frequency in MHz (CPUID leaf 0x16 preferred, registry ~MHz fallback)
    pub cpu_base_mhz: u32,
    /// CPU microcode revision (e.g. "0x010003E0")
    pub cpu_microcode: String,
    pub physical_cores: usize,
    pub bios_vendor: String,
    pub bios_version: String,
    /// BIOS release date in ISO format (YYYY-MM-DD)
    pub bios_date: String,
    pub system_manufacturer: String,
    pub system_product: String,
}

impl MachineIdentity {
    /// Snapshot the readable identity of the current machine.
    ///
    /// Shared by [`AppConfig::update_identity`] (which stores it to gate calibration reuse) and
    /// [`crate::run_context::RunIdentity`] (which stamps it onto result files) so the two can never
    /// disagree about what "this machine" means.
    pub fn detect(system_info: &SystemInfo, smbios: &SmbiosData) -> Self {
        Self {
            cpu_vendor: system_info.cpu_vendor.clone(),
            cpu_brand: system_info.cpu_brand.clone(),
            cpu_family: system_info.cpu_family,
            cpu_model: system_info.cpu_model,
            cpu_stepping: system_info.cpu_stepping,
            cpu_base_mhz: AppConfig::get_cpu_base_frequency_mhz(smbios),
            cpu_microcode: smbios.cpu_microcode.clone(),
            physical_cores: system_info.physical_cores,
            bios_vendor: smbios.bios.vendor.clone(),
            bios_version: smbios.bios.version.clone(),
            bios_date: smbios.bios.release_date.clone(),
            system_manufacturer: smbios.system.manufacturer.clone(),
            system_product: smbios.system.product_name.clone(),
        }
    }
}

/// The machine a calibration was measured on, as `AppConfig::is_calibration_valid` checks it: its
/// machine ID (`AppConfig::generate_machine_id`: CPU, cache, BIOS), system UUID, memory
/// configuration (`AppConfig::generate_memory_id`) and TSC frequency. The config's identity is the
/// cache calibrations' (`AppConfig::fingerprint`); a seal calibration keeps its own (TODO 99,
/// `SealCalibration::fingerprint`), so saving one kind never makes the other, measured on other
/// hardware, pass as this machine's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MachineFingerprint {
    pub machine_id: Option<String>,
    pub system_uuid: Option<String>,
    pub memory_id: Option<String>,
    pub tsc_frequency_ghz: Option<f64>,
}

impl MachineFingerprint {
    /// This machine's.
    pub fn detect(system_info: &SystemInfo, smbios: &SmbiosData) -> Self {
        MachineFingerprint {
            machine_id: Some(AppConfig::generate_machine_id(system_info, smbios)),
            system_uuid: (!smbios.system.uuid.is_empty()).then(|| smbios.system.uuid.clone()),
            memory_id: Some(AppConfig::generate_memory_id(smbios)),
            tsc_frequency_ghz: Some(system_info.cache_info.tsc_frequency_ghz),
        }
    }

    /// Whether it is this machine's now. Granular: the machine ID must be there and match; the
    /// system UUID, memory configuration and TSC frequency must match where they were recorded.
    /// Each mismatch is logged, `what` naming the calibration it is for.
    pub fn matches(&self, system_info: &SystemInfo, smbios: &SmbiosData, what: &str) -> bool {
        let mut valid = true;

        // Check machine_id (CPU + cache + BIOS)
        match &self.machine_id {
            Some(stored_id) => {
                let current_id = AppConfig::generate_machine_id(system_info, smbios);
                if stored_id != &current_id {
                    log::warn!("{what}: machine ID mismatch (CPU/cache/BIOS changed): stored={}, current={}",
                        stored_id, current_id);
                    valid = false;
                }
            }
            None => {
                log::info!("{what}: no machine ID recorded, so not validated");
                return false;
            }
        }

        // Check system UUID (motherboard/VM instance identity)
        if let (Some(stored_uuid), Some(current_uuid)) = (&self.system_uuid, &Some(smbios.system.uuid.clone()))
            && !current_uuid.is_empty() && stored_uuid != current_uuid {
                log::warn!("{what}: system UUID mismatch (VM migration or motherboard swap): stored={}, current={}",
                    stored_uuid, current_uuid);
                valid = false;
            }

        // Check memory configuration (DIMM swaps, XMP changes)
        if let Some(stored_mem_id) = &self.memory_id {
            let current_mem_id = AppConfig::generate_memory_id(smbios);
            if stored_mem_id != &current_mem_id {
                log::warn!("{what}: memory configuration changed (DIMM swap, XMP change, or capacity change): stored={}, current={}",
                    stored_mem_id, current_mem_id);
                valid = false;
            }
        }

        // Check TSC frequency — both values should be snapped to nearest MHz now,
        // but older configs may have raw floats. Use 0.01% tolerance (100ppm) to
        // filter measurement noise while catching real frequency changes (e.g. VM migration).
        if let Some(stored_tsc) = self.tsc_frequency_ghz {
            let current_tsc = system_info.cache_info.tsc_frequency_ghz;
            if stored_tsc > 0.0 {
                let drift_pct = ((current_tsc - stored_tsc) / stored_tsc * 100.0).abs();
                if drift_pct > 0.01 {
                    log::warn!("{what}: TSC frequency changed: stored={:.3} GHz, current={:.3} GHz (drift: {:.4}%)",
                        stored_tsc, current_tsc, drift_pct);
                    valid = false;
                }
            }
        }

        valid
    }
}

/// Application-wide persistent configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    /// Machine fingerprint — invalidates calibration when hardware changes.
    /// Generated from CPU vendor + brand + family/model/stepping + cache sizes + base freq + BIOS.
    #[serde(default)]
    pub machine_id: Option<String>,

    /// Readable identity fields for diagnosing what changed when machine_id mismatches.
    #[serde(default)]
    pub machine_identity: Option<MachineIdentity>,

    /// SMBIOS Type 1 System UUID (motherboard/VM instance identity).
    /// Separate from machine_id for granular mismatch reporting (e.g. VM migration).
    #[serde(default)]
    pub system_uuid: Option<String>,

    /// Memory configuration fingerprint — hash of per-DIMM serial, part, manufacturer, speed, capacity.
    /// Detects DIMM swaps, XMP changes, capacity changes.
    #[serde(default)]
    pub memory_id: Option<String>,

    /// Per-DIMM memory module details from SMBIOS Type 17.
    /// Stored for display; the hash is in `memory_id`.
    #[serde(default)]
    pub memory_modules: Option<Vec<MemoryModule>>,

    /// TSC frequency at full precision (GHz) when calibration was performed.
    /// Any change from this value triggers a warning (clock drift, VM migration, etc.).
    #[serde(default)]
    pub tsc_frequency_ghz: Option<f64>,

    /// Standard calibration results (from --calibrate-cache)
    #[serde(default)]
    pub calibration: Option<CalibrationResults>,

    /// Extended calibration results (from --calibrate-cache-ext) — preferred over standard
    #[serde(default)]
    pub calibration_extended: Option<CalibrationResults>,

    /// The seal's width, measured (from --calibrate-seal or --calibrate-all, TODO 99): what
    /// `seal-width=auto` runs on the machine it was measured on, which it keeps its own fingerprint
    /// of (`valid_seal_calibration`); the identity above is the cache calibrations'
    #[serde(default)]
    pub seal_calibration: Option<SealCalibration>,

    /// Last loaded test config file path (for TM5-style "reload last config" behavior)
    #[serde(default)]
    pub last_config: Option<String>,

    /// Default UI mode: "cli" or "gui" (future)
    #[serde(default)]
    pub ui_mode: Option<String>,

    /// Auto-start testing on launch without user prompt
    #[serde(default)]
    pub auto_start: Option<bool>,
}

const CONFIG_FILENAME: &str = "tmr-cfg.json";

impl AppConfig {
    /// Load config from a JSON file. Returns Default if file doesn't exist.
    pub fn load(path: &std::path::Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                match serde_json::from_str(&contents) {
                    Ok(config) => {
                        log::info!("Loaded app config from {}", path.display());
                        config
                    }
                    Err(e) => {
                        log::warn!("Failed to parse {}: {} — using defaults", path.display(), e);
                        Self::default()
                    }
                }
            }
            Err(_) => {
                log::info!("No config file at {} — using defaults", path.display());
                Self::default()
            }
        }
    }

    /// Save config to a JSON file (pretty-printed)
    pub fn save(&self, path: &std::path::Path) -> Result<(), String> {
        // Ensure parent directory exists
        if let Some(parent) = path.parent()
            && !parent.exists() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed to create directory {}: {}", parent.display(), e))?;
            }

        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize config: {}", e))?;

        std::fs::write(path, json)
            .map_err(|e| format!("Failed to write {}: {}", path.display(), e))?;

        log::info!("Saved app config to {}", path.display());
        Ok(())
    }

    /// Get the default config file path (next to the executable)
    pub fn default_path() -> PathBuf {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join(CONFIG_FILENAME)))
            .unwrap_or_else(|| PathBuf::from(CONFIG_FILENAME))
    }

    /// Generate a machine ID from system information + BIOS info.
    /// Includes CPU vendor, brand, family/model/stepping, microcode, base frequency,
    /// cache sizes, core count, and BIOS vendor/version/date.
    /// Deterministic: same hardware always produces the same ID.
    pub fn generate_machine_id(system_info: &SystemInfo, smbios: &SmbiosData) -> String {
        let mut hasher = DefaultHasher::new();
        // CPU identity
        system_info.cpu_vendor.hash(&mut hasher);
        system_info.cpu_brand.hash(&mut hasher);
        system_info.cpu_family.hash(&mut hasher);
        system_info.cpu_model.hash(&mut hasher);
        system_info.cpu_stepping.hash(&mut hasher);
        smbios.cpu_microcode.hash(&mut hasher);
        // Cache topology
        system_info.cache_info.per_core_l1d.hash(&mut hasher);
        system_info.cache_info.per_core_l2.hash(&mut hasher);
        system_info.cache_info.l3_cache.hash(&mut hasher);
        system_info.physical_cores.hash(&mut hasher);
        // CPU base frequency: CPUID leaf 0x16 preferred, registry ~MHz fallback
        let base_mhz = Self::get_cpu_base_frequency_mhz(smbios);
        base_mhz.hash(&mut hasher);
        // BIOS identity (firmware updates change behavior)
        smbios.bios.vendor.hash(&mut hasher);
        smbios.bios.version.hash(&mut hasher);
        smbios.bios.release_date.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }

    /// Get CPU base frequency in MHz.
    /// Tries CPUID leaf 0x16 first (nominal, power-state independent).
    /// Falls back to registry ~MHz (which reflects the OS-detected frequency).
    fn get_cpu_base_frequency_mhz(smbios: &SmbiosData) -> u32 {
        use raw_cpuid::CpuId;
        let cpuid = CpuId::new();
        let cpuid_mhz = cpuid.get_processor_frequency_info()
            .map(|f| f.processor_base_frequency() as u32)
            .unwrap_or(0);
        if cpuid_mhz > 0 {
            cpuid_mhz
        } else {
            // Fallback: registry ~MHz (set by Windows from TSC/ACPI during boot)
            smbios.cpu_base_mhz
        }
    }

    /// Generate memory configuration fingerprint from SMBIOS data.
    /// Hash of per-DIMM serial, part, manufacturer, speed, capacity.
    pub fn generate_memory_id(smbios: &SmbiosData) -> String {
        smbios.memory_fingerprint()
    }

    /// Get the best available calibration data.
    /// Prefers extended over standard (extended has finer-grain verification).
    pub fn get_best_calibration(&self) -> Option<&CalibrationResults> {
        self.calibration_extended.as_ref().or(self.calibration.as_ref())
    }

    /// The machine the config's identity names, which the cache calibrations were measured on.
    pub fn fingerprint(&self) -> MachineFingerprint {
        MachineFingerprint {
            machine_id: self.machine_id.clone(),
            system_uuid: self.system_uuid.clone(),
            memory_id: self.memory_id.clone(),
            tsc_frequency_ghz: self.tsc_frequency_ghz,
        }
    }

    /// Check if stored calibration is valid for the current system: whether the config's identity
    /// is this machine's (`MachineFingerprint::matches`: machine_id, system_uuid, memory_id, TSC
    /// frequency, each mismatch logged). The cache calibrations go by it; a seal calibration by its
    /// own (`valid_seal_calibration`).
    pub fn is_calibration_valid(&self, system_info: &SystemInfo, smbios: &SmbiosData) -> bool {
        self.fingerprint().matches(system_info, smbios, "Calibration")
    }

    /// Update all identity fields to match the current system
    pub fn update_identity(&mut self, system_info: &SystemInfo, smbios: &SmbiosData) {
        let MachineFingerprint { machine_id, system_uuid, memory_id, tsc_frequency_ghz } =
            MachineFingerprint::detect(system_info, smbios);
        self.machine_id = machine_id;
        self.machine_identity = Some(MachineIdentity::detect(system_info, smbios));
        self.system_uuid = system_uuid;
        self.memory_id = memory_id;
        self.memory_modules = if smbios.memory_modules.is_empty() {
            None
        } else {
            Some(smbios.memory_modules.clone())
        };
        self.tsc_frequency_ghz = tsc_frequency_ghz;
    }

    /// Update the machine ID to match the current system (legacy compat — prefer update_identity)
    pub fn update_machine_id(&mut self, system_info: &SystemInfo, smbios: &SmbiosData) {
        self.update_identity(system_info, smbios);
    }

    /// Stores a seal calibration measured on this machine, stamped with this machine's fingerprint
    /// (`valid_seal_calibration`). The config's identity is left alone: it is the cache
    /// calibrations', and taking this machine's would pass theirs, measured on other hardware, off
    /// as this machine's (TODO 99).
    pub fn set_seal_calibration(&mut self, mut seal: SealCalibration, system_info: &SystemInfo, smbios: &SmbiosData) {
        seal.fingerprint = MachineFingerprint::detect(system_info, smbios);
        self.seal_calibration = Some(seal);
    }

    /// The seal calibration a run on this machine takes, if any: one whose own fingerprint is this
    /// machine's now, whatever the config's identity (which a cache calibration's save rewrites).
    pub fn valid_seal_calibration(&self, system_info: &SystemInfo, smbios: &SmbiosData) -> Option<&SealCalibration> {
        self.seal_calibration.as_ref()
            .filter(|seal| seal.fingerprint.matches(system_info, smbios, "Seal calibration"))
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = AppConfig::default();
        assert!(config.machine_id.is_none());
        assert!(config.calibration.is_none());
        assert!(config.calibration_extended.is_none());
        assert!(config.last_config.is_none());
        assert!(config.get_best_calibration().is_none());
    }

    #[test]
    fn test_roundtrip_empty_config() {
        let config = AppConfig::default();
        let json = serde_json::to_string_pretty(&config).unwrap();
        let loaded: AppConfig = serde_json::from_str(&json).unwrap();
        assert!(loaded.machine_id.is_none());
        assert!(loaded.calibration.is_none());
    }

    #[test]
    fn test_roundtrip_with_fields() {
        let config = AppConfig {
            machine_id: Some("abc123".to_string()),
            last_config: Some("test.json".to_string()),
            auto_start: Some(true),
            ..Default::default()
        };

        let json = serde_json::to_string_pretty(&config).unwrap();
        let loaded: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.machine_id.as_deref(), Some("abc123"));
        assert_eq!(loaded.last_config.as_deref(), Some("test.json"));
        assert_eq!(loaded.auto_start, Some(true));
    }

    #[test]
    fn test_unknown_fields_preserved() {
        // JSON with an extra field not in the struct
        let json = r#"{"machine_id": "test", "some_future_field": 42}"#;
        // Should parse without error (serde default behavior with #[serde(default)])
        let config: AppConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.machine_id.as_deref(), Some("test"));
    }

    #[test]
    fn test_get_best_calibration_prefers_extended() {
        use crate::calibration::{CalibrationResults, CpuSignature};

        let sig = CpuSignature {
            vendor: "Test".to_string(),
            brand: "Test".to_string(),
            l1d_size: 32 * 1024,
            l2_size: 512 * 1024,
            l3_size: 32 * 1024 * 1024,
            ..Default::default()
        };

        let mut config = AppConfig::default();

        // No calibration
        assert!(config.get_best_calibration().is_none());

        // Only standard
        let mut standard = CalibrationResults::new(sig.clone());
        standard.total_time_ms = 1000;
        config.calibration = Some(standard);
        assert_eq!(config.get_best_calibration().unwrap().total_time_ms, 1000);

        // Extended available — should be preferred
        let mut extended = CalibrationResults::new(sig);
        extended.total_time_ms = 5000;
        config.calibration_extended = Some(extended);
        assert_eq!(config.get_best_calibration().unwrap().total_time_ms, 5000);
    }

    #[test]
    fn test_load_missing_file() {
        let config = AppConfig::load(std::path::Path::new("nonexistent_tmr_cfg.json"));
        assert!(config.machine_id.is_none());
    }

    #[test]
    fn test_roundtrip_identity_fields() {
        use crate::smbios::MemoryModule;

        let config = AppConfig {
            machine_id: Some("abc123def456".to_string()),
            system_uuid: Some("12345678-1234-1234-1234-123456789ABC".to_string()),
            memory_id: Some("fedcba9876543210".to_string()),
            tsc_frequency_ghz: Some(2.496012345),
            memory_modules: Some(vec![
                {
                    let cap: u64 = 32 * 1024 * 1024 * 1024;
                    MemoryModule {
                        locator: "DIMM_A1".to_string(),
                        manufacturer: "UnknownMemoryMaker".to_string(),
                        serial_number: "12345678".to_string(),
                        part_number: "123456789-12345".to_string(),
                        speed_mts: 4800,
                        memory_type: 0x22,
                        capacity_bytes: cap,
                        capacity_human: crate::smbios::format_capacity(cap),
                    }
                },
            ]),
            ..Default::default()
        };

        let json = serde_json::to_string_pretty(&config).unwrap();
        let loaded: AppConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(loaded.system_uuid.as_deref(), Some("12345678-1234-1234-1234-123456789ABC"));
        assert_eq!(loaded.memory_id.as_deref(), Some("fedcba9876543210"));
        assert_eq!(loaded.tsc_frequency_ghz, Some(2.496012345));
        assert_eq!(loaded.memory_modules.as_ref().unwrap().len(), 1);
        assert_eq!(loaded.memory_modules.as_ref().unwrap()[0].locator, "DIMM_A1");
        assert_eq!(loaded.memory_modules.as_ref().unwrap()[0].speed_mts, 4800);
    }

    /// A seal calibration that chose `width_bits`, measured nowhere yet (`set_seal_calibration`
    /// stamps the machine).
    fn seal_calibration(width_bits: u32) -> SealCalibration {
        use crate::calibration::SealWidthTiming;
        let timing = |bits, ms| SealWidthTiming { bits, fill_mib_s: 20000.0, check_mib_s: 30000.0, ms, ms_min: ms - 1.0, ms_max: ms + 1.0 };
        SealCalibration {
            timestamp: chrono::Utc::now(),
            fingerprint: MachineFingerprint::default(),
            pattern: "tmr".to_string(),
            buffer_bytes: 512 << 20,
            page_size: "large".to_string(),
            runs: 5,
            widths: vec![timing(128, 40.0), timing(256, 40.5)],
            width_bits,
            fastest_bits: 128,
        }
    }

    fn this_machine() -> (&'static SystemInfo, &'static SmbiosData) {
        (crate::tests::get_system_info(), crate::smbios::get_smbios())
    }

    /// The width `auto` resolves to with `config`'s seal calibration, on a CPU with AVX-512 (where
    /// the rule is 256 and every width runs), and where it came from.
    fn resolve(config: &AppConfig, width: crate::seal::SealWidth) -> (crate::seal::SealWidth, crate::seal::SealWidthSource) {
        let (system_info, smbios) = this_machine();
        let calibrated = config.valid_seal_calibration(system_info, smbios).and_then(SealCalibration::width);
        width.resolve_on(calibrated, true, true, "seal-width=").unwrap()
    }

    /// TODO 99: `seal-width=auto` runs the seal calibration measured on this machine, stamped with
    /// this machine's fingerprint when it is saved, else the rule. A width set wins over a
    /// calibrated one.
    #[test]
    fn auto_takes_this_machines_seal_calibration_and_a_setting_wins() {
        use crate::seal::SealWidth::{Auto, W128, W256, W512};
        use crate::seal::SealWidthSource::{Calibrated, Rule, Setting};
        let (system_info, smbios) = this_machine();

        // Absent: the rule, on a config that is this machine's
        let mut config = AppConfig::default();
        config.update_identity(system_info, smbios);
        assert_eq!(resolve(&config, Auto), (W256, Rule));
        assert_eq!(resolve(&config, W512), (W512, Setting("seal-width=")));

        // Present, measured here: the calibrated width, which a setting overrides
        config.set_seal_calibration(seal_calibration(128), system_info, smbios);
        assert_eq!(config.seal_calibration.as_ref().unwrap().fingerprint, MachineFingerprint::detect(system_info, smbios));
        assert_eq!(resolve(&config, Auto), (W128, Calibrated));
        assert_eq!(resolve(&config, W256), (W256, Setting("seal-width=")));
        // A cache calibration saved since, on the same machine, keeps it
        config.update_machine_id(system_info, smbios);
        assert_eq!(resolve(&config, Auto), (W128, Calibrated));
        // ...as does a config whose identity names no machine (no cache calibration saved yet)
        let fresh = AppConfig { seal_calibration: config.seal_calibration.clone(), ..Default::default() };
        assert_eq!(resolve(&fresh, Auto), (W128, Calibrated));
    }

    /// TODO 99: saving a seal calibration leaves the config's identity, the cache calibrations',
    /// alone: one measured on other hardware stays stale when the seal is calibrated here (the
    /// reviewer's case: a CPU swap, then only `--calibrate-seal`, or `--calibrate-all` with the
    /// cache save declined).
    #[test]
    fn a_seal_save_leaves_a_stale_cache_calibration_stale() {
        use crate::calibration::{CalibrationResults, CpuSignature};
        let (system_info, smbios) = this_machine();
        let mut config = AppConfig { calibration: Some(CalibrationResults::new(CpuSignature::default())), ..Default::default() };
        config.update_identity(system_info, smbios);
        config.machine_id = Some("another machine".to_string());
        assert!(!config.is_calibration_valid(system_info, smbios));
        let identity = serde_json::to_value(&config).unwrap();

        config.set_seal_calibration(seal_calibration(128), system_info, smbios);
        assert!(config.valid_seal_calibration(system_info, smbios).is_some(), "the seal's, measured here");
        assert!(!config.is_calibration_valid(system_info, smbios), "the cache calibration from other hardware revived");
        let mut after = serde_json::to_value(&config).unwrap();
        after.as_object_mut().unwrap().insert("seal_calibration".to_string(), serde_json::Value::Null);
        assert_eq!(after, identity, "the seal's save touched the rest of the config");
    }

    /// TODO 99: a seal calibration is this machine's only while all of its fingerprint is (each of
    /// what the cache calibration's check compares: machine ID, memory, system UUID, TSC); a cache
    /// calibration saved here, which rewrites the config's identity, doesn't revive one measured on
    /// other hardware, or one saved without a fingerprint.
    #[test]
    fn a_cache_save_doesnt_revive_a_seal_calibration_from_other_hardware() {
        use crate::calibration::{CalibrationResults, CpuSignature};
        use crate::seal::SealWidth::{Auto, W128, W256};
        use crate::seal::SealWidthSource::{Calibrated, Rule};
        let (system_info, smbios) = this_machine();
        let here = MachineFingerprint::detect(system_info, smbios);
        let tsc = here.tsc_frequency_ghz.unwrap();
        let mut elsewhere = vec![
            ("machine ID", MachineFingerprint { machine_id: Some("another machine".to_string()), ..here.clone() }),
            ("memory", MachineFingerprint { memory_id: Some("other DIMMs".to_string()), ..here.clone() }),
            ("TSC", MachineFingerprint { tsc_frequency_ghz: Some(tsc + 0.1), ..here.clone() }),
            ("none", MachineFingerprint::default()),
        ];
        // A system UUID is compared only where this machine reports one
        if here.system_uuid.is_some() {
            elsewhere.push(("system UUID", MachineFingerprint { system_uuid: Some("another board".to_string()), ..here.clone() }));
        }
        for (what, fingerprint) in elsewhere {
            let mut config = AppConfig::default();
            config.set_seal_calibration(seal_calibration(128), system_info, smbios);
            assert_eq!(resolve(&config, Auto), (W128, Calibrated), "{what}: measured here");
            config.seal_calibration.as_mut().unwrap().fingerprint = fingerprint;
            assert_eq!(resolve(&config, Auto), (W256, Rule), "{what}: from other hardware");

            config.update_machine_id(system_info, smbios);
            config.calibration = Some(CalibrationResults::new(CpuSignature::default()));
            assert!(config.is_calibration_valid(system_info, smbios));
            assert_eq!(resolve(&config, Auto), (W256, Rule), "{what}: revived by a cache save");
        }
    }

    /// TODO 99: a config written before seal calibrations loads, with none; one with a seal
    /// calibration round-trips it. A seal calibration saved without a fingerprint (one that kept
    /// only a machine ID) loads, keeping the rest of the config, and is stale.
    #[test]
    fn a_config_without_a_seal_calibration_loads_and_one_round_trips() {
        use crate::calibration::{CalibrationResults, CpuSignature};
        let (system_info, smbios) = this_machine();
        let mut config = AppConfig {
            calibration: Some(CalibrationResults::new(CpuSignature::default())),
            ..Default::default()
        };
        config.update_identity(system_info, smbios);
        config.set_seal_calibration(seal_calibration(256), system_info, smbios);
        let mut json: serde_json::Value = serde_json::to_value(&config).unwrap();
        let loaded: AppConfig = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(loaded.seal_calibration, config.seal_calibration);
        assert_eq!(loaded.valid_seal_calibration(system_info, smbios).and_then(SealCalibration::width),
                   Some(crate::seal::SealWidth::W256));

        // A seal calibration without a fingerprint
        let seal = json["seal_calibration"].as_object_mut().unwrap();
        seal.remove("fingerprint");
        seal.insert("machine_id".to_string(), serde_json::Value::String(AppConfig::generate_machine_id(system_info, smbios)));
        let unstamped: AppConfig = serde_json::from_str(&json.to_string()).unwrap();
        assert_eq!(unstamped.seal_calibration.as_ref().map(|s| &s.fingerprint), Some(&MachineFingerprint::default()));
        assert!(unstamped.valid_seal_calibration(system_info, smbios).is_none());
        assert!(unstamped.is_calibration_valid(system_info, smbios) && unstamped.calibration.is_some());

        // Before seal calibrations: no such key
        json.as_object_mut().unwrap().remove("seal_calibration");
        let older: AppConfig = serde_json::from_str(&json.to_string()).unwrap();
        assert!(older.seal_calibration.is_none() && older.calibration.is_some());
        assert!(older.is_calibration_valid(system_info, smbios));
        assert!(older.valid_seal_calibration(system_info, smbios).is_none());
    }

    #[test]
    fn test_backward_compat_old_config_no_identity_fields() {
        // Simulates loading a config file from before identity fields were added
        let json = r#"{"machine_id": "old_style_id", "calibration": null}"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.machine_id.as_deref(), Some("old_style_id"));
        assert!(config.system_uuid.is_none());
        assert!(config.memory_id.is_none());
        assert!(config.memory_modules.is_none());
        assert!(config.tsc_frequency_ghz.is_none());
    }
}
