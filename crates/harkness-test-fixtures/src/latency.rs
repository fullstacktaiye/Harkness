//! Recording and enforcement shared by every latency target in the workspace.
//!
//! A latency target is an `#[ignore]`d test that measures one operation against
//! a budget the issue that introduced it published. Two things about them are
//! easy to get wrong in isolation, and both are decided here instead:
//!
//! - **A debug build measures the optimizer being off.** Enforcing a budget
//!   there turns a slow machine into a red test that says nothing about the
//!   code, so the threshold binds only when `debug_assertions` is off. A debug
//!   run still executes the measurement and still records it — it just asserts
//!   completion rather than timing.
//! - **A failed threshold has to be diagnosable from the log alone.** CI runs
//!   these on a runner nobody can inspect afterwards, so every measurement
//!   prints the machine it was taken on beside the number, in one line a script
//!   can parse.
//!
//! The recorded line is stable and machine-readable:
//!
//! ```text
//! harkness-latency target=<name> measured_ns=<n> budget_ns=<n> profile=<debug|release> enforced=<bool> os=<os> arch=<arch> cpus=<n>
//! ```
//!
//! Nanoseconds rather than milliseconds because the budgets span five orders of
//! magnitude — ten microseconds per streamed event, 1.5 seconds per inventory
//! walk — and a millisecond column records the small ones as zero.
//!
//! libtest captures a passing test's output, so the job that runs these passes
//! `--nocapture`; `.github/scripts/run-ignored-exact-test.sh` is what does that.

use std::time::Duration;

/// Whether a measured budget is enforced in this build.
///
/// False in a debug build, where the number measures the optimizer being off.
#[must_use]
pub fn enforced() -> bool {
    !cfg!(debug_assertions)
}

/// The profile word the recorded line carries.
#[must_use]
pub fn profile() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
}

/// Records one measurement and enforces its budget in a release build.
///
/// `target` names the measurement rather than the test, so a renamed test keeps
/// its history: `store::persist_state_change_batch`, not
/// `persisting_a_state_change_batch_meets_the_latency_target`.
///
/// # Panics
///
/// In a release build, when `measured` is not below `budget`. In a debug build
/// this only records — reaching it at all is the assertion.
pub fn record(target: &str, measured: Duration, budget: Duration) {
    let cpus = std::thread::available_parallelism().map_or(0, |cpus| cpus.get());
    println!(
        "harkness-latency target={target} measured_ns={} budget_ns={} profile={} enforced={} \
         os={} arch={} cpus={cpus}",
        measured.as_nanos(),
        budget.as_nanos(),
        profile(),
        enforced(),
        std::env::consts::OS,
        std::env::consts::ARCH,
    );

    assert!(
        !(enforced() && measured >= budget),
        "{target} took {measured:?}, over its {budget:?} budget, on {} / {} with {cpus} cpus",
        std::env::consts::OS,
        std::env::consts::ARCH,
    );
}

#[cfg(test)]
mod tests {
    use super::{Duration, enforced, profile, record};

    /// The two accessors are read by different callers — one decides whether to
    /// assert, the other labels the recorded line — so a build where they
    /// disagreed would record numbers under a profile that was not enforcing
    /// them.
    #[test]
    fn the_profile_word_and_the_enforcement_flag_cannot_disagree() {
        assert_eq!(enforced(), profile() == "release");
        assert!(matches!(profile(), "debug" | "release"));
    }

    #[test]
    fn a_measurement_inside_its_budget_is_recorded_in_either_profile() {
        record(
            "fixture::inside_budget",
            Duration::ZERO,
            Duration::from_secs(1),
        );
    }

    #[test]
    #[cfg_attr(not(debug_assertions), should_panic(expected = "over its"))]
    fn an_overrun_binds_only_where_the_number_means_something() {
        // The same call is a failure in release and a recorded number in debug,
        // which is the whole rule this module exists to hold.
        record(
            "fixture::over_budget",
            Duration::from_secs(60),
            Duration::ZERO,
        );
    }
}
