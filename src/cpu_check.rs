//! Whether this CPU can run TMR (TODO 104): the engine is built for x86-64-v3 (AVX2, BMI1 and 2, FMA, F16C, LZCNT, MOVBE and the levels
//! below), and emits CLFLUSHOPT, which no baseline level holds, in its cache-flush path. A CPU without one of them would hit an
//! illegal-instruction fault mid-test, so a frontend checks first: tmr.exe before anything but its crash hooks and its choice of frontend,
//! and `session::run` refuses to start without them. This leaves out Intel before Skylake (2015; Haswell and Broadwell have AVX2 but no
//! CLFLUSHOPT) and AMD before Zen (2017; Excavator has AVX2 but no CLFLUSHOPT), all of the DDR3 and DDR4 era.
//!
//! **The check reads CPUID itself**, and XCR0 (XGETBV) for whether the OS saves the AVX registers, which AVX needs as much as the CPU's
//! bit. `is_x86_feature_detected!` won't do: it expands to `cfg!(target_feature = ..) || <the run-time check>`, so for a feature the
//! build enables it is a constant `true`, and for x86-64-v3's own it answered yes on every CPU (only CLFLUSHOPT's half really checked).
//!
//! **Which CPUs reach it.** Only code compiled in TMR's crates is x86-64-v3's: the standard library's own functions keep the baseline they
//! were built with (fat LTO doesn't recompile them), but what is inlined into TMR's code, or a generic instantiated in its crates, is v3.
//! `tmr::main` saves a register with a VEX move in its first instructions, so a CPU without AVX, or an OS without the AVX state, faults
//! there, before the crash hooks; only a launcher built for a lower baseline could tell it so. A CPU with AVX reaches the check and its
//! message: in the release asm (2026-10-26), nothing on the way there or in the message uses AVX2, BMI, FMA, F16C, LZCNT or MOVBE, but
//! std's command-line parser's fill of a run of 128 or more backslashes. That holds because the gate runs before the window's crash box
//! arms, and it and a refused `--gui` show their box without converting to UTF-16 (`fatal_box::ascii_box`): the conversion is a generic
//! whose size hint's divide by 3 is BMI2's MULX. So Sandy and Ivy Bridge and AMD's Bulldozer to Steamroller are told they lack AVX2 and the
//! rest, and Haswell, Broadwell and Excavator that they lack CLFLUSHOPT.

use std::arch::x86_64::{__cpuid_count, _xgetbv};

/// The CPUID words and XCR0 the check reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Cpuid {
    /// Leaf 1's ECX
    leaf1_ecx: u32,
    /// Leaf 7 (subleaf 0)'s EBX; 0 when the CPU has no leaf 7
    leaf7_ebx: u32,
    /// Leaf 0x8000_0001's ECX; 0 when the CPU has no such leaf
    ext1_ecx: u32,
    /// XCR0, the state components the OS saves; 0 when the OS hasn't turned XSAVE on (leaf 1 ECX bit 27, OSXSAVE), when XGETBV faults
    xcr0: u64,
}

/// Where CPUID gives a feature
#[derive(Debug, Clone, Copy)]
enum At {
    Leaf1Ecx(u32),
    Leaf7Ebx(u32),
    Ext1Ecx(u32),
    /// The OS saves the SSE and AVX registers (XCR0 bits 1 and 2), having turned XSAVE on
    OsAvxState,
}

/// A feature TMR is built for: its name in the refusal, where CPUID gives it (the Intel SDM's and AMD APM's CPUID chapters), and in a test
/// build its target feature's name (`rustc --print cfg -C target-cpu=x86-64-v3`), for the test that every one the build enables is here.
struct Feature {
    name: &'static str,
    at: At,
    #[cfg(test)]
    rustc: &'static str,
}

/// A row of `FEATURES`: the refusal's name, the target feature's, where CPUID gives it
macro_rules! feature {
    ($name:literal, $rustc:literal, $at:expr) => {
        Feature {
            name: $name,
            at: $at,
            #[cfg(test)]
            rustc: $rustc,
        }
    };
}

/// x86-64-v3's features above x86-64's own (which every x86-64 CPU has: x87, FXSR, SSE, SSE2), in the levels' order, and CLFLUSHOPT
#[rustfmt::skip]
const FEATURES: &[Feature] = &[
    // x86-64-v2
    feature!("CMPXCHG16B", "cmpxchg16b", At::Leaf1Ecx(13)),
    feature!("LAHF/SAHF", "lahfsahf", At::Ext1Ecx(0)),
    feature!("POPCNT", "popcnt", At::Leaf1Ecx(23)),
    feature!("SSE3", "sse3", At::Leaf1Ecx(0)),
    feature!("SSSE3", "ssse3", At::Leaf1Ecx(9)),
    feature!("SSE4.1", "sse4.1", At::Leaf1Ecx(19)),
    feature!("SSE4.2", "sse4.2", At::Leaf1Ecx(20)),
    // x86-64-v3
    feature!("AVX", "avx", At::Leaf1Ecx(28)),
    feature!("the OS's AVX support (XSAVE saving the AVX registers)", "", At::OsAvxState),
    feature!("AVX2", "avx2", At::Leaf7Ebx(5)),
    feature!("BMI1", "bmi1", At::Leaf7Ebx(3)),
    feature!("BMI2", "bmi2", At::Leaf7Ebx(8)),
    feature!("F16C", "f16c", At::Leaf1Ecx(29)),
    feature!("FMA", "fma", At::Leaf1Ecx(12)),
    feature!("LZCNT", "lzcnt", At::Ext1Ecx(5)),
    feature!("MOVBE", "movbe", At::Leaf1Ecx(22)),
    feature!("XSAVE", "xsave", At::Leaf1Ecx(26)),
    // In no level: the cache-flush path's (`tests::flush_range_to_dram`)
    feature!("CLFLUSHOPT", "clflushopt", At::Leaf7Ebx(23)),
];

/// Leaf 1 ECX's OSXSAVE: the OS has turned XSAVE on, so XGETBV runs
const OSXSAVE: u32 = 1 << 27;
/// XCR0's SSE and AVX state components
const XCR0_SSE_AVX: u64 = 0b110;

/// This CPU's words.
fn read() -> Cpuid {
    // A leaf above the highest the CPU has is asked for only after leaf 0 or 0x8000_0000 says it is there
    let max = __cpuid_count(0, 0).eax;
    let leaf1 = __cpuid_count(1, 0);
    let leaf7_ebx = if max >= 7 { __cpuid_count(7, 0).ebx } else { 0 };
    let max_ext = __cpuid_count(0x8000_0000, 0).eax;
    let ext1_ecx = if max_ext >= 0x8000_0001 { __cpuid_count(0x8000_0001, 0).ecx } else { 0 };
    // SAFETY: XGETBV faults unless the OS has turned XSAVE on, which OSXSAVE says it has
    let xcr0 = if leaf1.ecx & OSXSAVE != 0 { unsafe { _xgetbv(0) } } else { 0 };
    Cpuid { leaf1_ecx: leaf1.ecx, leaf7_ebx, ext1_ecx, xcr0 }
}

/// Whether `cpu` has `at`
fn has(cpu: Cpuid, at: At) -> bool {
    match at {
        At::Leaf1Ecx(bit) => cpu.leaf1_ecx & (1 << bit) != 0,
        At::Leaf7Ebx(bit) => cpu.leaf7_ebx & (1 << bit) != 0,
        At::Ext1Ecx(bit) => cpu.ext1_ecx & (1 << bit) != 0,
        At::OsAvxState => cpu.leaf1_ecx & OSXSAVE != 0 && cpu.xcr0 & XCR0_SSE_AVX == XCR0_SSE_AVX,
    }
}

/// The features `cpu` lacks, of those TMR needs, in `FEATURES`' order
fn missing_in(cpu: Cpuid) -> Vec<&'static str> {
    FEATURES.iter().filter(|f| !has(cpu, f.at)).map(|f| f.name).collect()
}

/// The features this CPU lacks, of those TMR needs: none when it can run TMR.
pub fn missing() -> Vec<&'static str> {
    missing_in(read())
}

/// Why this CPU can't run TMR, for the frontend to show; `None` when it can.
pub fn refusal() -> Option<String> {
    let missing = missing();
    (!missing.is_empty()).then(|| refusal_text(&missing))
}

/// The refusal for the features `missing`: ASCII, for tmr.exe's ANSI box (`fatal_box::ascii_box`)
fn refusal_text(missing: &[&str]) -> String {
    format!(
        "this CPU is missing required instruction set features: {}\n\
         TMR is built for x86-64-v3 (AVX2, BMI1/2, FMA) + CLFLUSHOPT: an Intel Skylake (2015) or AMD Zen (2017)\n\
         or newer CPU. Older CPUs are not supported.",
        missing.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A CPU with every feature and the OS's AVX support, as `FEATURES` places them
    fn full() -> Cpuid {
        let mut cpu = Cpuid { xcr0: 0b111, ..Default::default() };
        cpu.leaf1_ecx |= OSXSAVE;
        for f in FEATURES {
            match f.at {
                At::Leaf1Ecx(bit) => cpu.leaf1_ecx |= 1 << bit,
                At::Leaf7Ebx(bit) => cpu.leaf7_ebx |= 1 << bit,
                At::Ext1Ecx(bit) => cpu.ext1_ecx |= 1 << bit,
                At::OsAvxState => {}
            }
        }
        cpu
    }

    /// `cpu` without the features named
    fn without(mut cpu: Cpuid, names: &[&str]) -> Cpuid {
        for f in FEATURES.iter().filter(|f| names.contains(&f.name)) {
            match f.at {
                At::Leaf1Ecx(bit) => cpu.leaf1_ecx &= !(1 << bit),
                At::Leaf7Ebx(bit) => cpu.leaf7_ebx &= !(1 << bit),
                At::Ext1Ecx(bit) => cpu.ext1_ecx &= !(1 << bit),
                At::OsAvxState => cpu.xcr0 = 0b011,
            }
        }
        cpu
    }

    /// The machine the tests run on runs x86-64-v3 code, so it has every feature (CLFLUSHOPT too: the flush tests use it)
    #[test]
    fn this_machine_has_every_feature() {
        assert_eq!(missing(), Vec::<&str>::new(), "{:x?}", read());
    }

    /// The real words, not a constant: each of the three matches what std's run-time detection says for features the build doesn't
    /// enable, which std reads from CPUID alone (no OS state), as this does
    #[test]
    fn the_words_are_this_cpus() {
        let cpu = read();
        assert_ne!(cpu, Cpuid::default());
        assert_eq!(has(cpu, At::Leaf1Ecx(1)), is_x86_feature_detected!("pclmulqdq"), "leaf 1 ECX");
        assert_eq!(has(cpu, At::Leaf1Ecx(25)), is_x86_feature_detected!("aes"), "leaf 1 ECX");
        assert_eq!(has(cpu, At::Leaf7Ebx(23)), is_x86_feature_detected!("clflushopt"), "leaf 7 EBX");
        assert_eq!(has(cpu, At::Leaf7Ebx(19)), is_x86_feature_detected!("adx"), "leaf 7 EBX");
        assert_eq!(has(cpu, At::Ext1Ecx(6)), is_x86_feature_detected!("sse4a"), "leaf 0x8000_0001 ECX");
    }

    /// The CPUID bits as the SDM gives them, independently of the table
    #[test]
    fn the_bits_are_the_sdms() {
        let bits: Vec<(&str, At)> = FEATURES.iter().map(|f| (f.name, f.at)).collect();
        let at = |name: &str| bits.iter().find(|(n, _)| *n == name).map(|&(_, a)| format!("{a:?}")).expect(name);
        for (name, want) in [
            ("CMPXCHG16B", "Leaf1Ecx(13)"),
            ("SSE3", "Leaf1Ecx(0)"),
            ("SSSE3", "Leaf1Ecx(9)"),
            ("SSE4.1", "Leaf1Ecx(19)"),
            ("SSE4.2", "Leaf1Ecx(20)"),
            ("AVX2", "Leaf7Ebx(5)"),
            ("BMI1", "Leaf7Ebx(3)"),
            ("BMI2", "Leaf7Ebx(8)"),
            ("CLFLUSHOPT", "Leaf7Ebx(23)"),
            ("AVX", "Leaf1Ecx(28)"),
            ("FMA", "Leaf1Ecx(12)"),
            ("F16C", "Leaf1Ecx(29)"),
            ("MOVBE", "Leaf1Ecx(22)"),
            ("POPCNT", "Leaf1Ecx(23)"),
            ("XSAVE", "Leaf1Ecx(26)"),
            ("LZCNT", "Ext1Ecx(5)"),
            ("LAHF/SAHF", "Ext1Ecx(0)"),
        ] {
            assert_eq!(at(name), want, "{name}");
        }
        assert_eq!(bits.iter().filter(|(_, a)| !matches!(a, At::OsAvxState)).count(), 17, "a row this test doesn't restate");
    }

    /// The refusal is ASCII whatever is missing: tmr.exe shows it in an ANSI box
    #[test]
    fn the_refusal_is_ascii() {
        let all: Vec<&str> = FEATURES.iter().map(|f| f.name).collect();
        assert!(refusal_text(&all).is_ascii());
    }

    /// CPUs TMR refuses, by what they lack, and one it runs on
    #[test]
    fn a_cpu_is_refused_for_what_it_lacks() {
        assert_eq!(missing_in(full()), Vec::<&str>::new());
        // Haswell and Broadwell: x86-64-v3, but no CLFLUSHOPT
        assert_eq!(missing_in(without(full(), &["CLFLUSHOPT"])), ["CLFLUSHOPT"]);
        // CLFLUSHOPT, but none of AVX, AVX2, BMI, FMA or F16C. Real ones differ (a Skylake Pentium sets BMI's bits by an erratum, a
        // Goldmont Atom lacks LZCNT too), and tmr.exe faults before the check on a CPU without AVX (the module doc)
        let pentium = without(full(), &["AVX", "AVX2", "BMI1", "BMI2", "FMA", "F16C"]);
        assert_eq!(missing_in(pentium), ["AVX", "AVX2", "BMI1", "BMI2", "F16C", "FMA"]);
        // Ivy Bridge: AVX and F16C, none of the rest of v3, no CLFLUSHOPT
        let ivy = without(full(), &["AVX2", "BMI1", "BMI2", "FMA", "LZCNT", "MOVBE", "CLFLUSHOPT"]);
        assert_eq!(missing_in(ivy), ["AVX2", "BMI1", "BMI2", "FMA", "LZCNT", "MOVBE", "CLFLUSHOPT"]);
        // Every feature the CPU has, but an OS that doesn't save the AVX registers (XCR0 without bit 2), or hasn't turned XSAVE on
        let os_off = "the OS's AVX support (XSAVE saving the AVX registers)";
        assert_eq!(missing_in(Cpuid { xcr0: 0b011, ..full() }), [os_off]);
        assert_eq!(missing_in(Cpuid { leaf1_ecx: full().leaf1_ecx & !OSXSAVE, ..full() }), [os_off]);
        // A CPU with no leaf 7 or no extended leaf reads them as 0
        let old = Cpuid { leaf7_ebx: 0, ext1_ecx: 0, ..full() };
        assert_eq!(missing_in(old), ["LAHF/SAHF", "AVX2", "BMI1", "BMI2", "LZCNT", "CLFLUSHOPT"]);
    }

    /// Every target feature this build enables, above x86-64's own, is checked (`TMR_TARGET_FEATURES`, from build.rs): a baseline moved
    /// up (to x86-64-v4, say) without its features here fails this
    #[test]
    fn every_feature_the_build_enables_is_checked() {
        const X86_64: [&str; 4] = ["x87", "fxsr", "sse", "sse2"];
        let checked: Vec<&str> = FEATURES.iter().map(|f| f.rustc).filter(|r| !r.is_empty()).collect();
        let enabled: Vec<&str> = env!("TMR_TARGET_FEATURES").split(',').filter(|f| !f.is_empty() && !X86_64.contains(f)).collect();
        assert!(!enabled.is_empty(), "build.rs gave no target features");
        for f in &enabled {
            assert!(checked.contains(f), "the build enables {f}, which cpu_check doesn't check");
        }
    }
}
