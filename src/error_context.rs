//! Where in the run a worker thread is, for its error lines (TODO 74): set once per step, outside
//! every hot loop, and read only by the cold paths that log a bad word.

use std::cell::RefCell;

thread_local! {
    static CONTEXT: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Set this thread's context, e.g. `step 7 (Test 12, Mem-SimpleV2), cycle 2, thread 3`.
pub fn set(context: String) {
    CONTEXT.with(|c| *c.borrow_mut() = context);
}

/// This thread's context, empty when none is set (unit tests, calibration).
pub fn get() -> String {
    CONTEXT.with(|c| c.borrow().clone())
}

/// The start of an error line from `who`: `Error found by Mem-SimpleV2 in step 7 (Test 12,
/// Mem-SimpleV2), cycle 2, thread 3`, or without a context, `Error found by Mem-SimpleV2`.
pub fn found_by(who: &str) -> String {
    let context = get();
    if context.is_empty() {
        format!("Error found by {who}")
    } else {
        format!("Error found by {who} in {context}")
    }
}

/// The same for the seal, whose `what` says where it ran: `Error found by the seal check before
/// step 7 (Test 12, Mem-SimpleV2), cycle 2, thread 3`.
pub fn found_by_the(what: &str) -> String {
    let context = get();
    if context.is_empty() {
        format!("Error found by the {what}")
    } else {
        format!("Error found by the {what} {context}")
    }
}
