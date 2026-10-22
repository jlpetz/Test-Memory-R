//! tmr.exe (TODO 29): what must run before anything else, the crash hooks, the choice of frontend
//! and the CPU gate, then the frontend. That is the CLI (`tmr_cli`), or in a build with the `gui`
//! feature the window (`tmr_gui`, TODO 7), which a double-click or `--gui` opens (`launch.rs`).
//! `--gui` and `--cli` are this exe's own: they pick the frontend and are taken out of the arguments
//! before the session sees them. The engine is the `tmr` library.

// The CPU gate detects CLFLUSHOPT, which std has behind a nightly gate
#![feature(clflushopt_target_feature)]

#[cfg(feature = "gui")]
mod fatal_box; // the window's message boxes, the crash box among them (TODO 29 S6)
mod launch; // which frontend a launch runs, and what it does about the console (TODO 29 S6)

use std::process::exit;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(feature = "gui")]
use fatal_box::message_box;
use launch::Mode;
use tmr::frontend::RunStatus;
use windows::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS;

/// Set once the launch is the window's (`launch::decide`). The panic hook and the CPU gate then
/// show their text in a message box as well, and a fatal fault nothing handles has the crash box
/// (`fatal_box`): a double-click gives the window no console to read it on, and one launched from a
/// shell isn't watched there. Never set in a build without the window.
static GUI_MODE: AtomicBool = AtomicBool::new(false);

/// A fatal error's `text` in a message box, in the window's mode only. Blocks the calling thread
/// until it is dismissed.
fn show_fatal(text: &str) {
    if GUI_MODE.load(Ordering::Relaxed) {
        message_box(text);
    }
}

/// A build without the window is never in its mode
#[cfg(not(feature = "gui"))]
fn message_box(_text: &str) {}

/// A fatal fault, as the crash hooks report it: the vectored handler's console lines, and in the
/// window's mode the crash box's text.
struct Fault {
    label: &'static str,
    code: i32,
    ip: usize,
    /// From the module base (works regardless of ASLR)
    rva: usize,
    module_base: usize,
    /// For an access violation, its access type and target address
    access: Option<(&'static str, usize)>,
}

impl Fault {
    /// The fault `info` holds, when it is one the hooks report: an access violation, an illegal
    /// instruction, a stack overflow or a stack buffer overrun. `None` for the rest (a debugger's
    /// breakpoint and the like), and for a null pointer.
    ///
    /// # Safety
    /// `info` is null or the exception pointers Windows passed the handler.
    unsafe fn of(info: *const EXCEPTION_POINTERS) -> Option<Fault> {
        use windows::Win32::Foundation::{
            EXCEPTION_ACCESS_VIOLATION, STATUS_ILLEGAL_INSTRUCTION,
            STATUS_STACK_OVERFLOW, STATUS_STACK_BUFFER_OVERRUN,
        };

        if info.is_null() { return None; }
        let ptrs = unsafe { &*info };
        if ptrs.ExceptionRecord.is_null() { return None; }
        let record = unsafe { &*ptrs.ExceptionRecord };

        let code = record.ExceptionCode;
        let ip = record.ExceptionAddress as usize;

        // Only fatal exceptions — let debugger breakpoints etc. pass through
        let label = if code == EXCEPTION_ACCESS_VIOLATION {
            "ACCESS_VIOLATION"
        } else if code == STATUS_ILLEGAL_INSTRUCTION {
            "ILLEGAL_INSTRUCTION"
        } else if code == STATUS_STACK_OVERFLOW {
            "STACK_OVERFLOW"
        } else if code == STATUS_STACK_BUFFER_OVERRUN {
            "STACK_BUFFER_OVERRUN"
        } else {
            return None; // Not our problem
        };

        // Compute RVA from module base (works regardless of ASLR)
        let module_base = unsafe {
            windows::Win32::System::LibraryLoader::GetModuleHandleA(None)
                .map(|h| h.0 as usize).unwrap_or(0)
        };
        let rva = if module_base > 0 && ip >= module_base { ip - module_base } else { ip };

        // For access violations, ExceptionInformation[0] = access type, [1] = target address
        let access = (code == EXCEPTION_ACCESS_VIOLATION && record.NumberParameters >= 2).then(|| {
            let access_type = match record.ExceptionInformation[0] {
                0 => "READ from",
                1 => "WRITE to",
                8 => "DEP violation at",
                _ => "UNKNOWN access at",
            };
            (access_type, record.ExceptionInformation[1])
        });
        Some(Fault { label, code: code.0, ip, rva, module_base, access })
    }
}

/// Vectored Exception Handler — catches hardware crashes (access violations, illegal
/// instructions, stack overflows) that Rust's panic handler cannot see.
/// Logs the faulting instruction, target address, and access type before the process dies.
/// It allocates nothing, which a fault in the heap may need. It runs first-chance, so it also prints
/// for a fault that a DLL then handles itself; the window's crash box shows only a fault nothing
/// handles (`fatal_box`).
unsafe extern "system" fn crash_exception_handler(info: *mut EXCEPTION_POINTERS) -> i32 {
    // SAFETY: Windows passes a vectored handler the fault's exception pointers
    let Some(fault) = (unsafe { Fault::of(info) }) else {
        return 0; // EXCEPTION_CONTINUE_SEARCH: not our problem — pass to next handler
    };
    eprintln!("\n!!! FATAL: {} (0x{:08X}) at IP={:#x} (RVA={:#x})", fault.label, fault.code, fault.ip, fault.rva);
    if let Some((access_type, target_addr)) = fault.access {
        eprintln!("!!!   {} address {:#x}", access_type, target_addr);
    }
    eprintln!("!!! Module base: {:#x}", fault.module_base);

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
        let text = format!(
            "FATAL: this CPU is missing required instruction set features: {}\n\
             TMR is built for x86-64-v3 (AVX2) + CLFLUSHOPT — a 2015-or-newer CPU\n\
             (Intel Skylake+ / AMD Excavator+ / any Zen). Pre-2015 CPUs are not supported.",
            missing.join(", ")
        );
        eprintln!("{text}");
        show_fatal(&text);
        exit(RunStatus::NotRun.exit_code());
    }
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
        let text = format!("PANIC at {}: {}", location, msg);
        eprintln!("\n!!! {text}");
        log::error!("{text}");
        show_fatal(&text);
    }));

    // 2. Windows VEH — catches hardware exceptions (segfaults, illegal instructions)
    unsafe {
        windows::Win32::System::Diagnostics::Debug::AddVectoredExceptionHandler(
            1, // first=1: our handler runs BEFORE any other handlers
            Some(crash_exception_handler),
        );
    }

    // 3. The frontend, decided before anything prints, so the CPU gate below knows whether to show
    //    its text in a message box as well, and has a console to print it on when the CLI needs one
    //    opened. In the window's mode the crash box's thread starts now (`fatal_box`).
    let mut args: Vec<String> = std::env::args().collect();
    let asked = launch::take_mode(&mut args);
    let launch = launch::decide(&launch::probe(asked, args.len() > 1));
    GUI_MODE.store(launch.mode == Mode::Gui, Ordering::Relaxed);
    #[cfg(feature = "gui")]
    if launch.mode == Mode::Gui {
        fatal_box::arm();
    }
    if launch.alloc_console {
        launch::alloc_console();
    }

    // 4. CPU capability gate — TMR is built for x86-64-v3 (AVX2) and requires CLFLUSHOPT
    //    (not part of any psABI level, so the compiler baseline can't guarantee it). Reject
    //    unsupported CPUs cleanly here rather than letting them #UD-fault mid-test.
    require_cpu_features();

    // The console Windows opened for the window is left only now, so the gate's text above was on it
    if launch.free_console {
        launch::free_console();
    }
    if let Some(note) = launch.note {
        eprintln!("{note}");
    }

    // The window shows the session, its setup and its run, and its log records go to the log
    // file, the warnings and errors to the window too (tmr_gui's logger)
    #[cfg(feature = "gui")]
    if launch.mode == Mode::Gui {
        let closed = tmr_gui::main(args);
        if let Some(error) = &closed.error {
            eprintln!("{error}");
            show_fatal(error);
        }
        exit(closed.exit_code);
    }

    exit(tmr_cli::main(args));
}
