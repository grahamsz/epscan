//! A Ctrl-c the rest of the program can check for and stop at its own next
//! safe point, instead of the default that kills wherever it happens to be.

use std::{
    io::{IsTerminal, Write, stderr},
    sync::atomic::{AtomicBool, Ordering},
};

static REQUESTED: AtomicBool = AtomicBool::new(false);

/// Catch Ctrl-c and note it happened
pub fn install() -> epscan::Result<()> {
    ctrlc::set_handler(|| {
        if !REQUESTED.swap(true, Ordering::SeqCst) {
            let mut err = stderr().lock();
            // The terminal has already echoed `^C` onto whatever line the bars
            // were drawing. Wind back over it so the notice replaces it rather
            // than hanging off the end of a half-drawn bar
            if err.is_terminal() {
                let _ = err.write_all(b"\r\x1b[2K");
            }
            let _ = writeln!(
                err,
                "stopping at the next safe point, this can take a moment"
            );
        }
    })
    .map_err(|error| std::io::Error::other(error).into())
}

/// Whether the operator has asked to stop
pub fn requested() -> bool {
    REQUESTED.load(Ordering::SeqCst)
}

/// The cancellation flag shared with the scanner session
pub fn flag() -> &'static AtomicBool {
    &REQUESTED
}
