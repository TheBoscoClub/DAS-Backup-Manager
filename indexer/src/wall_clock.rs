//! A wall-clock bound for code a mutant can turn into an infinite loop.
//!
//! Compiled only with the `test-overrides` feature, which no shipped build
//! enables (the crate's dev-dependency on itself turns it on for tests), so
//! nothing here exists in a release binary (bd DAS-Backup-Manager-2kb1.16).
//!
//! cargo-mutants reports a mutant that makes a test hang as TIMEOUT, which
//! keeps the weekly run red; a mutant that makes the test binary fail is
//! caught. A hung call cannot be failed from inside its own thread, and the
//! test harness waits for every test, so the bound is a watchdog: arm it
//! before the call, drop the guard after; when the limit passes first, it
//! names the call on stderr and aborts the test process, which a mutation
//! run counts as a failed test.

use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

/// Far longer than any bounded call takes (milliseconds), short enough to end a looping
/// mutant well before cargo-mutants' own timeout.
pub const LIMIT: Duration = Duration::from_secs(10);

/// Dropping it disarms the watchdog (the sender is held only for its drop).
#[must_use = "the bound ends when the guard is dropped"]
pub struct Bound(#[allow(dead_code)] Sender<()>);

/// Arm a watchdog on `what`: abort the process if the guard outlives `limit`.
pub fn arm(what: &'static str, limit: Duration) -> Bound {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        if let Err(RecvTimeoutError::Timeout) = rx.recv_timeout(limit) {
            eprintln!("wall-clock bound: {what} did not return within {limit:?}; aborting");
            std::process::abort();
        }
    });
    Bound(tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dropped_guard_disarms_the_watchdog() {
        let bound = arm("a call that returns", Duration::from_millis(50));
        drop(bound);
        // Had the watchdog seen a timeout the process would be gone by now.
        std::thread::sleep(Duration::from_millis(300));
    }
}
