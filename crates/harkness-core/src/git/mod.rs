//! System Git integration.
//!
//! The facade the Git command runner, the per-repository lock, and the clone
//! implementation land behind. Until then it reserves the error type only; the
//! clone that Harkness runs today still lives beside [`ProjectService`].
//!
//! [`ProjectService`]: crate::ProjectService

/// Failures raised by Git operations.
///
/// Uninhabited for now: the variants arrive with the command runner, which is
/// also what gives this type its first caller.
#[expect(dead_code, reason = "reserved for the Git command runner")]
#[derive(Debug)]
#[non_exhaustive]
pub enum GitError {}
