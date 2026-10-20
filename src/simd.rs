// SIMD capability detection
pub fn detect_simd_capabilities() -> String {
    let mut capabilities = Vec::new();

    if is_x86_feature_detected!("sse") {
        capabilities.push("SSE");
    }
    if is_x86_feature_detected!("sse2") {
        capabilities.push("SSE2");
    }
    if is_x86_feature_detected!("sse3") {
        capabilities.push("SSE3");
    }
    if is_x86_feature_detected!("sse4.1") {
        capabilities.push("SSE4.1");
    }
    if is_x86_feature_detected!("sse4.2") {
        capabilities.push("SSE4.2");
    }
    if is_x86_feature_detected!("avx") {
        capabilities.push("AVX");
    }
    if is_x86_feature_detected!("avx2") {
        capabilities.push("AVX2");
    }
    if is_x86_feature_detected!("avx512f") {
        capabilities.push("AVX-512F");
    }
    if is_x86_feature_detected!("avx512bw") {
        capabilities.push("AVX-512BW");
    }
    if is_x86_feature_detected!("avx512vl") {
        capabilities.push("AVX-512VL");
    }

    if capabilities.is_empty() {
        "None detected".to_string()
    } else {
        capabilities.join(", ")
    }
}

/// A SIMD width: what `width=` gives every test that picks its width at run time (TODO 99), and
/// what such a test runs at.
///
/// Those tests are every `-Auto` test (resolved by name, `runner::resolve_auto_dispatch_test`,
/// and in their own dispatchers), Mem-Sequence and the Bench pairs that name no width; the seal
/// takes it too unless `seal-width=` sets its own (`seal::width_setting`). A test named for a
/// width (`-128`, `-256`, `-512`) keeps that one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Width {
    W128,
    W256,
    W512,
}

impl Width {
    /// Parses `width=`: `auto`, `128`, `256` or `512`. `Ok(None)` is auto.
    pub fn parse(s: &str) -> Result<Option<Width>, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(None),
            "128" => Ok(Some(Width::W128)),
            "256" => Ok(Some(Width::W256)),
            "512" => Ok(Some(Width::W512)),
            other => Err(format!("width '{other}': use auto, 128, 256 or 512")),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Width::W128 => "128",
            Width::W256 => "256",
            Width::W512 => "512",
        }
    }

    pub fn bits(self) -> u32 {
        match self {
            Width::W128 => 128,
            Width::W256 => 256,
            Width::W512 => 512,
        }
    }

    /// The widest width this CPU has: what auto runs. 128 is a fallback the startup gate (AVX2)
    /// never lets run.
    pub fn widest() -> Width {
        Isa::detect().widest()
    }

    /// This width, or why this CPU can't run it.
    pub fn check(self) -> Result<Width, String> {
        Isa::detect().check(self)
    }

    /// The seal's width at this one.
    pub fn seal(self) -> crate::seal::SealWidth {
        match self {
            Width::W128 => crate::seal::SealWidth::W128,
            Width::W256 => crate::seal::SealWidth::W256,
            Width::W512 => crate::seal::SealWidth::W512,
        }
    }
}

/// The instruction sets a width needs, as a CPU reports them: AVX-512 (its F, which every 512-bit
/// dispatcher has checked) and AVX2.
#[derive(Debug, Clone, Copy)]
struct Isa {
    avx2: bool,
    avx512: bool,
}

impl Isa {
    fn detect() -> Isa {
        Isa { avx2: is_x86_feature_detected!("avx2"), avx512: is_x86_feature_detected!("avx512f") }
    }

    fn widest(self) -> Width {
        match self {
            Isa { avx512: true, .. } => Width::W512,
            Isa { avx2: true, .. } => Width::W256,
            _ => Width::W128,
        }
    }

    fn check(self, width: Width) -> Result<Width, String> {
        match width {
            Width::W512 if !self.avx512 => Err("width 512 needs AVX-512, which this CPU lacks".to_string()),
            Width::W256 if !self.avx2 => Err("width 256 needs AVX2, which this CPU lacks".to_string()),
            w => Ok(w),
        }
    }
}

/// The width given and where (`width=`, or a config's `system.width`); `None` is auto.
static WIDTH: std::sync::OnceLock<(Option<Width>, &'static str)> = std::sync::OnceLock::new();

/// Sets the run's width once, `source` saying where it came from: the command line's `width=`
/// first, then a config's, which a `width=` given (`auto` too) keeps out. A width this CPU lacks
/// is refused, before anything runs.
pub fn set_width(width: Option<Width>, source: &'static str) -> Result<(), String> {
    if WIDTH.get().is_some() {
        return Ok(());
    }
    if let Some(w) = width {
        w.check().map_err(|e| format!("{source}: {e}"))?;
    }
    let _ = WIDTH.set((width, source));
    Ok(())
}

/// The width given and where, `None` under auto.
pub fn width_given() -> Option<(Width, &'static str)> {
    #[cfg(test)]
    if let Some(given) = TEST_WIDTH.with(|w| w.get()) {
        return given.map(|w| (w, "a test's width"));
    }
    WIDTH.get().and_then(|&(width, source)| width.map(|w| (w, source)))
}

/// The width every test that picks its width at run time runs at: the one given, else the widest
/// this CPU has. Read once a test function call, outside its hot loops.
pub fn test_width() -> Width {
    width_given().map_or_else(Width::widest, |(w, _)| w)
}

#[cfg(test)]
thread_local! {
    /// A test's width on its own thread, in place of the run's (`TestWidth`)
    static TEST_WIDTH: std::cell::Cell<Option<Option<Width>>> = const { std::cell::Cell::new(None) };
}

/// While it lives, `width_given` and `test_width` on this thread give `width` (`None`: auto): the
/// run's is set once a process, so tests can't set it.
#[cfg(test)]
pub(crate) struct TestWidth {
    before: Option<Option<Width>>,
}

#[cfg(test)]
impl TestWidth {
    #[must_use = "the width holds only while the guard lives"]
    pub(crate) fn set(width: Option<Width>) -> Self {
        TestWidth { before: TEST_WIDTH.with(|w| w.replace(Some(width))) }
    }
}

#[cfg(test)]
impl Drop for TestWidth {
    fn drop(&mut self) {
        let before = self.before;
        TEST_WIDTH.with(|w| w.set(before));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_parses_auto_and_the_three_widths_and_refuses_the_rest() {
        assert_eq!(Width::parse("auto"), Ok(None));
        assert_eq!(Width::parse(" AUTO "), Ok(None));
        assert_eq!(Width::parse("128"), Ok(Some(Width::W128)));
        assert_eq!(Width::parse("256"), Ok(Some(Width::W256)));
        assert_eq!(Width::parse("512"), Ok(Some(Width::W512)));
        for bad in ["", "64", "1024", "scalar", "avx2", "512b"] {
            assert!(Width::parse(bad).is_err(), "{bad:?} parsed");
        }
        for w in [Width::W128, Width::W256, Width::W512] {
            assert_eq!(Width::parse(w.name()), Ok(Some(w)));
            assert_eq!(w.name(), w.bits().to_string());
            assert_eq!(w.seal().name(), w.name());
        }
    }

    /// A width the CPU lacks is refused, saying what it needs; auto is the widest there is.
    #[test]
    fn a_width_the_cpu_lacks_is_refused_and_auto_is_the_widest() {
        let avx2 = Isa { avx2: true, avx512: false };
        let avx512 = Isa { avx2: true, avx512: true };
        let neither = Isa { avx2: false, avx512: false };
        assert_eq!(avx512.widest(), Width::W512);
        assert_eq!(avx2.widest(), Width::W256);
        assert_eq!(neither.widest(), Width::W128);
        for w in [Width::W128, Width::W256, Width::W512] {
            assert_eq!(avx512.check(w), Ok(w));
        }
        assert_eq!(avx2.check(Width::W256), Ok(Width::W256));
        assert_eq!(avx2.check(Width::W128), Ok(Width::W128));
        assert!(avx2.check(Width::W512).unwrap_err().contains("AVX-512"));
        assert!(neither.check(Width::W256).unwrap_err().contains("AVX2"));
        // This CPU: every width up to its widest runs, and nothing wider
        let widest = Width::widest();
        for w in [Width::W128, Width::W256, Width::W512] {
            assert_eq!(w.check().is_ok(), w <= widest, "{w:?} on a CPU whose widest is {widest:?}");
        }
    }

    #[test]
    fn a_test_width_holds_on_its_thread_while_its_guard_lives() {
        let auto = test_width();
        {
            let _width = TestWidth::set(Some(Width::W128));
            assert_eq!(test_width(), Width::W128);
            assert_eq!(width_given().map(|(w, _)| w), Some(Width::W128));
            {
                let _auto = TestWidth::set(None);
                assert_eq!(test_width(), Width::widest());
                assert_eq!(width_given(), None);
            }
            assert_eq!(test_width(), Width::W128);
        }
        assert_eq!(test_width(), auto);
    }
}