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

    /// The width a test's name says: one ending in 128, 256 or 512 (`Mem-StuckBit512`,
    /// `Spd-L1-Read-256`, `Bench-Init-Seal-TMR-512`). `None` for a name with no width.
    pub fn of_name(name: &str) -> Option<Width> {
        [Width::W128, Width::W256, Width::W512].into_iter().find(|w| name.ends_with(w.name()))
    }

    /// The seal's width at this one.
    pub fn seal(self) -> crate::seal::SealWidth {
        match self {
            Width::W128 => crate::seal::SealWidth::W128,
            Width::W256 => crate::seal::SealWidth::W256,
            Width::W512 => crate::seal::SealWidth::W512,
        }
    }

    /// The width a seal width is; `None` for `Auto`, which is none until resolved.
    pub fn of_seal(width: crate::seal::SealWidth) -> Option<Width> {
        match width {
            crate::seal::SealWidth::W128 => Some(Width::W128),
            crate::seal::SealWidth::W256 => Some(Width::W256),
            crate::seal::SealWidth::W512 => Some(Width::W512),
            crate::seal::SealWidth::Auto => None,
        }
    }
}

/// Whether this CPU has AVX-512 as every 512-bit kernel is compiled for it: F, BW, CD, DQ and VL,
/// their `#[target_feature]` set. The one test for it, behind `Width::check` (and so the run's
/// choice of tests, `runner::leave_out_what_the_cpu_lacks`) and the seal's widths.
pub fn has_avx512() -> bool {
    is_x86_feature_detected!("avx512f") && is_x86_feature_detected!("avx512bw") && is_x86_feature_detected!("avx512cd")
        && is_x86_feature_detected!("avx512dq") && is_x86_feature_detected!("avx512vl")
}

/// The instruction sets a width needs, as a CPU reports them: AVX-512 (`has_avx512`) and AVX2.
#[derive(Debug, Clone, Copy)]
struct Isa {
    avx2: bool,
    avx512: bool,
}

impl Isa {
    fn detect() -> Isa {
        Isa { avx2: is_x86_feature_detected!("avx2"), avx512: has_avx512() }
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
type WidthSetting = std::sync::OnceLock<(Option<Width>, &'static str)>;

/// The run's width setting (`set_width`).
static WIDTH: WidthSetting = std::sync::OnceLock::new();

/// Sets the run's width once, `source` saying where it came from: the command line's `width=`
/// first, then a config's, which a `width=` given (`auto` too) keeps out. A width this CPU lacks
/// is refused, before anything runs.
pub fn set_width(width: Option<Width>, source: &'static str) -> Result<(), String> {
    set_width_on(&WIDTH, Width::check, width, source)
}

/// `set_width` on `setting`, `check` saying whether the CPU has a width (`Width::check`): the first
/// width set holds, a later one isn't checked, and a width the CPU lacks is refused, leaving
/// `setting` unset.
fn set_width_on(setting: &WidthSetting, check: impl Fn(Width) -> Result<Width, String>, width: Option<Width>,
                source: &'static str) -> Result<(), String> {
    if setting.get().is_some() {
        return Ok(());
    }
    if let Some(w) = width {
        check(w).map_err(|e| format!("{source}: {e}"))?;
    }
    let _ = setting.set((width, source));
    Ok(())
}

/// The width given and where, `None` under auto.
pub fn width_given() -> Option<(Width, &'static str)> {
    #[cfg(any(test, feature = "test-support"))]
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

/// Where the run's width came from, as the results file records it: `width=`, the config's, or
/// `auto` when none was given.
pub fn width_source() -> &'static str {
    width_given().map_or("auto", |(_, source)| source)
}

/// `$f`, the function a run-time dispatcher calls at the run's width (`test_width`), called as the
/// dispatcher calls it: `crate::dispatched!(f)(args)`. In a unit test build it is noted first, on
/// the thread's record while a test takes one (`dispatches`), so the test sees which width's
/// function the dispatch took. The binary's is `$f` alone.
#[cfg(not(test))]
#[macro_export]
macro_rules! dispatched {
    ($f:expr) => {
        $f
    };
}

#[cfg(test)]
#[macro_export]
macro_rules! dispatched {
    ($f:expr) => {{
        let f = $f;
        $crate::simd::note_dispatch(f as *const ());
        f
    }};
}

#[cfg(test)]
thread_local! {
    /// The functions this thread's dispatchers called, while a test records them (`dispatches`)
    static DISPATCHED: std::cell::RefCell<Option<Vec<*const ()>>> = const { std::cell::RefCell::new(None) };
}

/// Notes `f`, the function a dispatcher is calling (`dispatched!`), on this thread's record if a
/// test takes one.
#[cfg(test)]
pub(crate) fn note_dispatch(f: *const ()) {
    DISPATCHED.with_borrow_mut(|record| if let Some(record) = record { record.push(f) });
}

/// Runs `f`, and returns its value and the functions the dispatchers it ran called on this thread,
/// in order (`dispatched!`): a test runs a test function in `f`, and sees which width's function
/// its dispatch took (TODO 99).
#[cfg(test)]
pub(crate) fn dispatches<R>(f: impl FnOnce() -> R) -> (R, Vec<*const ()>) {
    /// Ends the record however `f` ends, so a failed test leaves none on its thread
    struct Stop;
    impl Drop for Stop {
        fn drop(&mut self) {
            DISPATCHED.with_borrow_mut(|record| *record = None);
        }
    }
    DISPATCHED.with_borrow_mut(|record| {
        assert!(record.is_none(), "one record a thread at a time");
        *record = Some(Vec::new());
    });
    let stop = Stop;
    let value = f();
    let called = DISPATCHED.with_borrow_mut(Option::take).expect("the record `f` ran with");
    drop(stop);
    (value, called)
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    /// A test's width on its own thread, in place of the run's (`TestWidth`)
    static TEST_WIDTH: std::cell::Cell<Option<Option<Width>>> = const { std::cell::Cell::new(None) };
}

/// While it lives, `width_given` and `test_width` on this thread give `width` (`None`: auto): the
/// run's is set once a process, so tests can't set it. The CLI's tests too, under the
/// `test-support` feature: its goldens of the plan are at 256 bits (`test_support`, TODO 29).
#[cfg(any(test, feature = "test-support"))]
pub(crate) struct TestWidth {
    before: Option<Option<Width>>,
}

#[cfg(any(test, feature = "test-support"))]
impl TestWidth {
    #[must_use = "the width holds only while the guard lives"]
    pub(crate) fn set(width: Option<Width>) -> Self {
        TestWidth { before: TEST_WIDTH.with(|w| w.replace(Some(width))) }
    }
}

#[cfg(any(test, feature = "test-support"))]
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

    /// TODO 99: the run's width is the first one set, `width=`'s over the config's (`auto` too),
    /// and a width the CPU lacks is refused, with where it came from and what it needs, leaving
    /// the width unset; a config's that `width=` keeps out isn't checked. `set_width` on a setting
    /// of the test's own, for a CPU with AVX2 alone.
    #[test]
    fn the_first_width_set_holds_and_one_the_cpu_lacks_is_refused() {
        let check = |w| Isa { avx2: true, avx512: false }.check(w);
        let setting = WidthSetting::new();
        let refused = set_width_on(&setting, check, Some(Width::W512), "width=").unwrap_err();
        assert!(refused.starts_with("width=: ") && refused.contains("AVX-512"), "{refused}");
        assert_eq!(setting.get(), None, "a refused width isn't the run's");
        set_width_on(&setting, check, Some(Width::W128), "the config's width").unwrap();
        assert_eq!(setting.get(), Some(&(Some(Width::W128), "the config's width")));

        for (given, config) in [(Some(Width::W256), Some(Width::W128)), (None, Some(Width::W256)), (Some(Width::W128), Some(Width::W512))] {
            let setting = WidthSetting::new();
            set_width_on(&setting, check, given, "width=").unwrap();
            set_width_on(&setting, check, config, "the config's width").unwrap_or_else(|e| panic!("{config:?} after {given:?}: {e}"));
            assert_eq!(setting.get(), Some(&(given, "width=")), "{given:?} then the config's {config:?}");
        }
    }

    #[test]
    fn a_test_width_holds_on_its_thread_while_its_guard_lives() {
        let auto = test_width();
        {
            let _width = TestWidth::set(Some(Width::W128));
            assert_eq!(test_width(), Width::W128);
            assert_eq!(width_given().map(|(w, _)| w), Some(Width::W128));
            assert_eq!(width_source(), "a test's width");
            {
                let _auto = TestWidth::set(None);
                assert_eq!(test_width(), Width::widest());
                assert_eq!(width_given(), None);
                assert_eq!(width_source(), "auto");
            }
            assert_eq!(test_width(), Width::W128);
        }
        assert_eq!(test_width(), auto);
    }

    #[test]
    fn a_seal_width_is_its_width_and_auto_none() {
        for w in [Width::W128, Width::W256, Width::W512] {
            assert_eq!(Width::of_seal(w.seal()), Some(w));
        }
        assert_eq!(Width::of_seal(crate::seal::SealWidth::Auto), None);
    }
}