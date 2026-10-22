//! tmr.exe (TODO 29): what must run before anything else, the crash hooks and the CPU gate, then a
//! frontend. That is the CLI, or with `--gui` in a build with the `gui` feature, the window (TODO
//! 7). `--gui` and `--cli` are this exe's own: they pick the frontend and are taken out of the
//! arguments before the session sees them. The engine and the CLI are the `tmr` library.

// The CPU gate detects CLFLUSHOPT, which std has behind a nightly gate
#![feature(clflushopt_target_feature)]

use std::process::exit;

use tmr::frontend::RunStatus;

/// Vectored Exception Handler — catches hardware crashes (access violations, illegal
/// instructions, stack overflows) that Rust's panic handler cannot see.
/// Logs the faulting instruction, target address, and access type before the process dies.
unsafe extern "system" fn crash_exception_handler(
    info: *mut windows::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS,
) -> i32 {
    use windows::Win32::Foundation::{
        EXCEPTION_ACCESS_VIOLATION, STATUS_ILLEGAL_INSTRUCTION,
        STATUS_STACK_OVERFLOW, STATUS_STACK_BUFFER_OVERRUN,
    };

    if info.is_null() { return 0; } // EXCEPTION_CONTINUE_SEARCH
    let ptrs = unsafe { &*info };
    if ptrs.ExceptionRecord.is_null() { return 0; }
    let record = unsafe { &*ptrs.ExceptionRecord };

    let code = record.ExceptionCode;
    let ip = record.ExceptionAddress as usize;

    // Only handle fatal exceptions — let debugger breakpoints etc. pass through
    let label = if code == EXCEPTION_ACCESS_VIOLATION {
        "ACCESS_VIOLATION"
    } else if code == STATUS_ILLEGAL_INSTRUCTION {
        "ILLEGAL_INSTRUCTION"
    } else if code == STATUS_STACK_OVERFLOW {
        "STACK_OVERFLOW"
    } else if code == STATUS_STACK_BUFFER_OVERRUN {
        "STACK_BUFFER_OVERRUN"
    } else {
        return 0; // Not our problem — pass to next handler
    };

    // Compute RVA from module base (works regardless of ASLR)
    let module_base = unsafe {
        windows::Win32::System::LibraryLoader::GetModuleHandleA(None)
            .map(|h| h.0 as usize).unwrap_or(0)
    };
    let rva = if module_base > 0 && ip >= module_base { ip - module_base } else { ip };

    // For access violations, ExceptionInformation[0] = access type, [1] = target address
    if code == EXCEPTION_ACCESS_VIOLATION && record.NumberParameters >= 2 {
        let access_type = match record.ExceptionInformation[0] {
            0 => "READ from",
            1 => "WRITE to",
            8 => "DEP violation at",
            _ => "UNKNOWN access at",
        };
        let target_addr = record.ExceptionInformation[1];
        eprintln!("\n!!! FATAL: {} (0x{:08X}) at IP={:#x} (RVA={:#x})", label, code.0, ip, rva);
        eprintln!("!!!   {} address {:#x}", access_type, target_addr);
    } else {
        eprintln!("\n!!! FATAL: {} (0x{:08X}) at IP={:#x} (RVA={:#x})", label, code.0, ip, rva);
    }
    eprintln!("!!! Module base: {:#x}", module_base);

    // Return EXCEPTION_CONTINUE_SEARCH (0) — let the default handler terminate the process
    // after we've logged. Using 1 (EXCEPTION_CONTINUE_EXECUTION) would loop forever.
    0
}

/// Verify the running CPU has the instruction-set features TMR is built against.
///
/// TMR compiles at the `x86-64-v3` baseline (AVX2) and emits CLFLUSHOPT unconditionally
/// in the cache-flush path. CLFLUSHOPT is a standalone CPUID feature (not part of any
/// psABI microarch level), so it must be checked at runtime. Missing either feature means
/// the binary would hit an illegal-instruction fault during testing — so we detect it up
/// front, explain why, and exit cleanly. This excludes pre-2015 CPUs (Intel pre-Skylake,
/// incl. Haswell/Broadwell which have AVX2 but no CLFLUSHOPT; AMD pre-Excavator/pre-Zen),
/// all of which are DDR3/DDR4-era and out of scope for a DDR5 tester.
fn require_cpu_features() {
    let mut missing: Vec<&str> = Vec::new();
    if !is_x86_feature_detected!("avx2") {
        missing.push("AVX2");
    }
    // clflushopt detection landed in std behind the unstable `clflushopt_target_feature`
    // gate (rustc PR #157098) — gated at the crate root, above.
    if !is_x86_feature_detected!("clflushopt") {
        missing.push("CLFLUSHOPT");
    }

    if !missing.is_empty() {
        eprintln!("FATAL: this CPU is missing required instruction set features: {}", missing.join(", "));
        eprintln!("TMR is built for x86-64-v3 (AVX2) + CLFLUSHOPT — a 2015-or-newer CPU");
        eprintln!("(Intel Skylake+ / AMD Excavator+ / any Zen). Pre-2015 CPUs are not supported.");
        exit(RunStatus::NotRun.exit_code());
    }
}

/// Takes `--gui` and `--cli` out of `args`, and says whether the window was asked for: the last of
/// them given decides, and without either it is the CLI.
fn take_mode(args: &mut Vec<String>) -> bool {
    let mut gui = false;
    args.retain(|arg| match arg.as_str() {
        "--gui" => {
            gui = true;
            false
        }
        "--cli" => {
            gui = false;
            false
        }
        _ => true,
    });
    gui
}

/// The first Windows build the window runs on: Windows 11 and Server 2025 (TODO 7). Below it the
/// Windows App Runtime's bootstrap has no `TryCreatePackageDependency` (build.rs).
#[cfg(feature = "gui")]
const FIRST_GUI_BUILD: u32 = 22000;

/// The Windows build, from `RtlGetVersion`, which unlike `GetVersionEx` doesn't take the answer
/// from the exe's manifest. `None` if it fails.
#[cfg(feature = "gui")]
fn windows_build() -> Option<u32> {
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
    // ntdll's: the `windows` crate has it only behind a WDK feature the engine doesn't take
    #[link(name = "ntdll", kind = "raw-dylib")]
    unsafe extern "system" {
        fn RtlGetVersion(info: *mut OSVERSIONINFOW) -> i32;
    }
    let mut info = OSVERSIONINFOW { dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32, ..Default::default() };
    // SAFETY: `info` is an OSVERSIONINFOW with its size set, valid for the call, which only writes it
    (unsafe { RtlGetVersion(&mut info) } == 0).then_some(info.dwBuildNumber)
}

fn main() {
    // --- Crash handling: install BEFORE anything else ---

    // 1. Rust panic hook — ensures panic messages reach stderr + log before abort
    std::panic::set_hook(Box::new(|info| {
        let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic".to_string()
        };
        let location = info.location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown location".to_string());
        eprintln!("\n!!! PANIC at {}: {}", location, msg);
        log::error!("PANIC at {}: {}", location, msg);
    }));

    // 2. Windows VEH — catches hardware exceptions (segfaults, illegal instructions)
    unsafe {
        windows::Win32::System::Diagnostics::Debug::AddVectoredExceptionHandler(
            1, // first=1: our handler runs BEFORE any other handlers
            Some(crash_exception_handler),
        );
    }

    // 3. CPU capability gate — TMR is built for x86-64-v3 (AVX2) and requires CLFLUSHOPT
    //    (not part of any psABI level, so the compiler baseline can't guarantee it). Reject
    //    unsupported CPUs cleanly here rather than letting them #UD-fault mid-test.
    require_cpu_features();

    let mut args: Vec<String> = std::env::args().collect();
    let gui = take_mode(&mut args);

    // The S3 preview (TODO 29): launched from a shell, the window shows the run, and the session's
    // setup still prints to this console and asks its question there
    #[cfg(feature = "gui")]
    if gui {
        match windows_build() {
            Some(build) if build >= FIRST_GUI_BUILD => exit(tmr_gui::main(args)),
            build => eprintln!(
                "Note: the window needs Windows build {FIRST_GUI_BUILD} or later (Windows 11, Server 2025), and this is {}: \
                 running the CLI",
                build.map_or("unknown".to_string(), |build| build.to_string())
            ),
        }
    }
    #[cfg(not(feature = "gui"))]
    if gui {
        eprintln!("Note: this tmr.exe was built without the window (cargo build --features tmr-app/gui): running the CLI");
    }

    exit(tmr::cli::main(args));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--gui` and `--cli` pick the frontend, the last one given deciding, and the session never sees
    /// them
    #[test]
    fn the_mode_flags_pick_the_frontend_and_leave_the_arguments() {
        let take = |args: &[&str]| {
            let mut args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            let gui = take_mode(&mut args);
            (gui, args)
        };
        let args = |args: &[&str]| args.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        assert_eq!(take(&["tmr", "memory=50%"]), (false, args(&["tmr", "memory=50%"])));
        assert_eq!(take(&["tmr", "--gui", "memory=50%"]), (true, args(&["tmr", "memory=50%"])));
        assert_eq!(take(&["tmr", "--gui", "cycles=1", "--cli"]), (false, args(&["tmr", "cycles=1"])));
        assert_eq!(take(&["tmr", "--cli", "--gui"]), (true, args(&["tmr"])));
    }
}
