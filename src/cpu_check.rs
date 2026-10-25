//! Whether this CPU can run TMR (TODO 104): the engine is built for x86-64-v3 (AVX2), and emits CLFLUSHOPT, which no baseline level
//! holds, in its cache-flush path. A CPU without either would hit an illegal-instruction fault mid-test, so a frontend checks first:
//! tmr.exe before anything else, and `session::run` refuses to start without them. This leaves out pre-2015 CPUs (Intel before Skylake,
//! Haswell and Broadwell among them, which have AVX2 but no CLFLUSHOPT; AMD before Excavator and Zen), all of the DDR3 and DDR4 era.

/// The features this CPU lacks, of those TMR needs: none when it can run TMR.
pub fn missing() -> Vec<&'static str> {
    let mut missing = Vec::new();
    if !is_x86_feature_detected!("avx2") {
        missing.push("AVX2");
    }
    // clflushopt's detection is behind the unstable `clflushopt_target_feature` gate (rustc PR #157098), at the crate root
    if !is_x86_feature_detected!("clflushopt") {
        missing.push("CLFLUSHOPT");
    }
    missing
}

/// Why this CPU can't run TMR, for the frontend to show; `None` when it can.
pub fn refusal() -> Option<String> {
    let missing = missing();
    (!missing.is_empty()).then(|| {
        format!(
            "this CPU is missing required instruction set features: {}\n\
             TMR is built for x86-64-v3 (AVX2) + CLFLUSHOPT — a 2015-or-newer CPU\n\
             (Intel Skylake+ / AMD Excavator+ / any Zen). Pre-2015 CPUs are not supported.",
            missing.join(", ")
        )
    })
}
