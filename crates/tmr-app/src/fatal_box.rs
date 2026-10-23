//! The window's message boxes (TODO 29 S6): a fatal error's text, which the console has too, in a
//! box, since a double-click gives the window no console to read it on. The panic hook and the CPU
//! gate show theirs as they happen (`message_box`). A fault's box is the crash box (`arm`).
//!
//! The crash box shows only a fault that nothing handles: it is the process's unhandled-exception
//! filter, which Windows calls once no handler took the fault. The vectored handler, which prints
//! the console's FATAL lines, sees every fault first, those a DLL catches itself too (a graphics
//! driver probing memory), and a box there would cry FATAL for them and hold the thread that
//! faulted until it was dismissed.
//!
//! And it never waits without a limit. A fault inside the process heap, or under the loader lock,
//! can leave a box unable to open, and the process must still end, so Windows Error Reporting takes
//! its dump. So the box has a thread of its own, started with the window, not in the fault: starting
//! a thread takes the loader lock and the heap. The filter writes the text into a static buffer,
//! with no allocation, wakes that thread, and waits up to `SHOW_WAIT` for the box to show, which a
//! CBT hook on the thread sees as the box's activation. Shown, the filter waits until it is
//! dismissed; not shown in time, it gives up on it. Either way the fault then ends the process, as
//! it does without the window.

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::fmt::{self, Write};
use std::ptr::null_mut;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::Thread;
use std::time::{Duration, Instant};

use windows::Win32::System::Diagnostics::Debug::{EXCEPTION_POINTERS, LPTOP_LEVEL_EXCEPTION_FILTER, SetUnhandledExceptionFilter};
use windows::Win32::System::Threading::{GetCurrentThread, GetCurrentThreadId, SetThreadPriority, Sleep, THREAD_PRIORITY_TIME_CRITICAL};

use crate::Fault;

// user32's: the `windows` crate has them behind a feature nothing else takes, and enabling it
// would build the crate again, for the engine too
#[link(name = "user32", kind = "raw-dylib")]
unsafe extern "system" {
    fn MessageBoxW(owner: *mut c_void, text: *const u16, caption: *const u16, kind: u32) -> i32;
    fn SetWindowsHookExW(id: i32, hook: Option<HookProc>, module: *mut c_void, thread: u32) -> *mut c_void;
    fn CallNextHookEx(hook: *mut c_void, code: i32, wparam: usize, lparam: isize) -> isize;
}

type HookProc = unsafe extern "system" fn(code: i32, wparam: usize, lparam: isize) -> isize;

const MB_ICONERROR: u32 = 0x10;
const MB_SETFOREGROUND: u32 = 0x1_0000;
const MB_TOPMOST: u32 = 0x4_0000;
const WH_CBT: i32 = 5;
const HCBT_ACTIVATE: i32 = 5;
const EXCEPTION_CONTINUE_SEARCH: i32 = 0;

const CAPTION: &str = "Test Memory R (TMR)";

/// `text` in a message box. Blocks the calling thread until it is dismissed.
pub fn message_box(text: &str) {
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (text, caption) = (wide(text), wide(CAPTION));
    // SAFETY: both are NUL-terminated UTF-16 that outlive the call, and there is no owner window
    unsafe { MessageBoxW(null_mut(), text.as_ptr(), caption.as_ptr(), MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST) };
}

/// How long a fault waits for its box to show before it ends the process without one. A box shows
/// in milliseconds; its thread outranks the workers (`arm`).
const SHOW_WAIT: Duration = Duration::from_secs(10);

/// How often a fault waiting on its box looks whether it has shown, or been dismissed
const POLL: Duration = Duration::from_millis(50);

/// The crash box's text's room, in UTF-16 units, the NUL that ends it included
const TEXT_UNITS: usize = 512;

/// The crash box's text. Written once, by the fault that claimed it (`CLAIMED`), before `READY`, and
/// read by the box's thread after it.
struct Text(UnsafeCell<[u16; TEXT_UNITS]>);

// SAFETY: one writer, the thread that set CLAIMED, and its write is published by READY's release;
// the one reader, the box's thread, reads only after acquiring READY
unsafe impl Sync for Text {}

static TEXT: Text = Text(UnsafeCell::new([0; TEXT_UNITS]));

/// A fault has taken the text: a second one, on another thread, waits on the same box
static CLAIMED: AtomicBool = AtomicBool::new(false);
/// The text is written, for the box's thread
static READY: AtomicBool = AtomicBool::new(false);
/// The box has been activated: it is on the screen (the CBT hook)
static SHOWN: AtomicBool = AtomicBool::new(false);
/// The box has been dismissed
static DISMISSED: AtomicBool = AtomicBool::new(false);

/// The box's thread, which a fault wakes, and its id: a fault on that thread can't wait for its box
static BOX_THREAD: OnceLock<Thread> = OnceLock::new();
static BOX_THREAD_ID: AtomicU32 = AtomicU32::new(0);
/// The filter installed before this one (the C runtime's, for C++ exceptions), which this one hands on to
static PREVIOUS: OnceLock<LPTOP_LEVEL_EXCEPTION_FILTER> = OnceLock::new();

/// Starts the crash box's thread, in the window's mode only. It hooks its own windows' activation,
/// then installs the filter and waits for a fault. At `THREAD_PRIORITY_TIME_CRITICAL`, above the
/// workers' High, so a box can show while they run on every CPU; it is idle until then. When the hook
/// can't be set, a box couldn't say it had shown, so there is no filter and a fault ends the process
/// at once, as it does in the CLI.
pub fn arm() {
    let caption: Vec<u16> = CAPTION.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = std::thread::Builder::new().name("tmr-crash-box".to_string()).spawn(move || {
        // SAFETY: no pointers: this thread's own priority, and its id
        let (_, id) = unsafe { (SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL), GetCurrentThreadId()) };
        // SAFETY: a hook on this thread alone, which needs no module, and `watch` only looks
        if unsafe { SetWindowsHookExW(WH_CBT, Some(watch), null_mut(), id) }.is_null() {
            return;
        }
        let _ = BOX_THREAD.set(std::thread::current());
        BOX_THREAD_ID.store(id, Ordering::Release);
        // SAFETY: `filter` is an unhandled-exception filter, for the life of the process
        let _ = PREVIOUS.set(unsafe { SetUnhandledExceptionFilter(Some(filter)) });
        while !READY.load(Ordering::Acquire) {
            std::thread::park();
        }
        // SAFETY: the text ends with a NUL (its last unit is never written) and isn't written after
        // READY; the caption is NUL-terminated and outlives the call
        unsafe { MessageBoxW(null_mut(), TEXT.0.get().cast(), caption.as_ptr(), MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST) };
        DISMISSED.store(true, Ordering::Release);
    });
}

/// The box's thread's CBT hook: its box being activated is its box on the screen.
unsafe extern "system" fn watch(code: i32, wparam: usize, lparam: isize) -> isize {
    if code == HCBT_ACTIVATE {
        SHOWN.store(true, Ordering::Release);
    }
    // SAFETY: the hook's own arguments, passed on as a hook must
    unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) }
}

/// The unhandled-exception filter: a fatal fault's lines in the box, waited for as the module says
/// (but not by the box's own thread, which would wait on itself), then the filter before it, and
/// Windows' own handling, which ends the process.
unsafe extern "system" fn filter(info: *const EXCEPTION_POINTERS) -> i32 {
    // SAFETY: Windows passes a filter the fault's exception pointers
    if let Some(fault) = unsafe { Fault::of(info) } {
        if !CLAIMED.swap(true, Ordering::AcqRel) {
            // SAFETY: CLAIMED makes this the only writer, and the box's thread reads only after READY
            let units = unsafe { &mut *TEXT.0.get() };
            let _ = fault.box_text(&mut Utf16 { units, len: 0 });
            READY.store(true, Ordering::Release);
            if let Some(thread) = BOX_THREAD.get() {
                thread.unpark();
            }
        }
        // SAFETY: no arguments
        if unsafe { GetCurrentThreadId() } != BOX_THREAD_ID.load(Ordering::Acquire) {
            wait_for_the_box();
        }
    }
    match PREVIOUS.get().copied().flatten() {
        // SAFETY: the filter Windows had before this one, with the pointers it was given
        Some(previous) => unsafe { previous(info) },
        None => EXCEPTION_CONTINUE_SEARCH,
    }
}

/// Up to `SHOW_WAIT` for the box to show, then, if it did, until it is dismissed. No allocation and
/// no lock: the heap's, or the loader's, may be the fault's. Hence kernel32's `Sleep`, where std's
/// sleep makes a timer each time.
fn wait_for_the_box() {
    // SAFETY: no pointers
    let poll = || unsafe { Sleep(POLL.as_millis() as u32) };
    let asked = Instant::now();
    while !SHOWN.load(Ordering::Acquire) {
        if asked.elapsed() >= SHOW_WAIT {
            return;
        }
        poll();
    }
    while !DISMISSED.load(Ordering::Acquire) {
        poll();
    }
}

impl Fault {
    /// The vectored handler's FATAL lines, without their `!!!`, for the box
    fn box_text(&self, out: &mut impl Write) -> fmt::Result {
        write!(out, "FATAL: {} (0x{:08X}) at IP={:#x} (RVA={:#x})", self.label, self.code, self.ip, self.rva)?;
        if let Some((access_type, target_addr)) = self.access {
            write!(out, "\n{} address {:#x}", access_type, target_addr)?;
        }
        write!(out, "\nModule base: {:#x}", self.module_base)
    }
}

/// Writes UTF-16 into `units` from `len` on, with no allocation, leaving the last unit for the NUL
/// that ends the text, and drops what doesn't fit.
struct Utf16<'a> {
    units: &'a mut [u16],
    len: usize,
}

impl Write for Utf16<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for unit in s.encode_utf16() {
            if self.len + 1 >= self.units.len() {
                break;
            }
            self.units[self.len] = unit;
            self.len += 1;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The box's text is the console's FATAL lines without their `!!!`, cut off with its NUL kept when
    /// it doesn't fit
    #[test]
    fn the_box_text_is_the_fatal_lines_cut_to_fit() {
        let fault = Fault {
            label: "ACCESS_VIOLATION",
            code: 0xC000_0005_u32 as i32,
            ip: 0x7ff6_1234_5678,
            rva: 0x5678,
            module_base: 0x7ff6_1234_0000,
            access: Some(("WRITE to", 0x10)),
        };
        let text = |units: &[u16]| String::from_utf16(&units[..units.iter().position(|&u| u == 0).unwrap()]).unwrap();
        let mut units = [0u16; TEXT_UNITS];
        fault.box_text(&mut Utf16 { units: &mut units, len: 0 }).unwrap();
        assert_eq!(
            text(&units),
            "FATAL: ACCESS_VIOLATION (0xC0000005) at IP=0x7ff612345678 (RVA=0x5678)\n\
                                  WRITE to address 0x10\nModule base: 0x7ff612340000"
        );

        let mut units = [0u16; 8];
        fault.box_text(&mut Utf16 { units: &mut units, len: 0 }).unwrap();
        assert_eq!((text(&units).as_str(), units[7]), ("FATAL: ", 0), "cut off, the NUL kept");

        let fault = Fault { label: "STACK_OVERFLOW", code: 0xC000_00FD_u32 as i32, access: None, ..fault };
        let mut units = [0u16; TEXT_UNITS];
        fault.box_text(&mut Utf16 { units: &mut units, len: 0 }).unwrap();
        assert_eq!(text(&units), "FATAL: STACK_OVERFLOW (0xC00000FD) at IP=0x7ff612345678 (RVA=0x5678)\nModule base: 0x7ff612340000");
    }
}
