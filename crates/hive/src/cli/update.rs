//! `hive update`: the self-update verb. The logic is `crate::update`; this
//! is the exit lane, whose codes `--check` is read for.

use crate::update::{self, RealIo};

/// Exit codes are the contract: without `--check`, 0 whenever nothing is
/// wrong (installed, already latest, ahead of the release) and non-zero on
/// any failure; with it, 0 = no update, 1 = update available, 2 = the
/// query or the tag could not be read.
pub(crate) fn update_cmd(check: bool, force: bool) -> ! {
    match update::run(&RealIo, check, force) {
        Ok(outcome) => update::print_and_exit(outcome),
        Err(failure) => {
            eprintln!("Error: {}", failure.message);
            std::process::exit(failure.code)
        }
    }
}
