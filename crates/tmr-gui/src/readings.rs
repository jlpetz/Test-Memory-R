//! The window's line for timings and temperatures. The reading itself is the engine's (`tmr::external_line`), so a results file and the
//! header name the same thing.

/// One line for the header, or empty when neither source had a fresh reading.
pub fn line() -> String {
    tmr::external_line()
}
