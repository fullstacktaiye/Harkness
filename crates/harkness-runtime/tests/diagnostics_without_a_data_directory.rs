//! Initializing with nowhere to write, which must still leave a working process.
//!
//! Its own binary for the same reason `diagnostics.rs` is: the subscriber is
//! process-global, so the degraded arrangement can only be observed by a process
//! that has not already installed a working one.

use harkness_runtime::observe;

#[test]
fn no_data_directory_degrades_to_stderr_rather_than_failing() {
    let outcome = observe::init(None, observe::Options::default());

    let observe::InitOutcome::StderrOnly { reason } = &outcome else {
        panic!("no data directory means no file: {outcome:?}");
    };
    assert!(
        reason.contains("data directory"),
        "the reason has to be relayable to a user asking where their logs went: {reason}"
    );
    assert!(outcome.describe().contains("stderr only"));

    // The point of degrading rather than failing: instrumentation still works,
    // and nothing that calls it has to know which arrangement it got.
    tracing::info!(run_id = "none", "a line with nowhere to be filed");

    // And the guard still holds, so a later caller that *does* have a data
    // directory is told the arrangement is already made rather than silently
    // replacing it.
    assert_eq!(
        observe::init(None, observe::Options::default()),
        observe::InitOutcome::AlreadyInitialized
    );
}
