//! Managed-repository cloning.
//!
//! Clone is kept behind the same runner as every other production Git spawn,
//! while its caller chooses the checkout destination explicitly.

use std::path::Path;

use crate::{
    GitError,
    runner::{Cancellation, GitAccess, GitCommand},
};

/// Clones `remote` into an explicit checkout destination.
///
/// `remote` and `destination` reach Git exactly as the caller supplied them.
/// The `--` boundary prevents either from being interpreted as an option.
/// Callers are responsible for validating the remote before invoking this
/// primitive. Relative destinations resolve against `working_directory`; an
/// absolute destination is used as written.
///
/// No timeout: a first clone of a large repository is legitimately slow, and
/// `cancellation` already stops it on the only terms a user cares about.
///
pub(crate) fn run(
    git_executable: &Path,
    working_directory: &Path,
    remote: &str,
    destination: &Path,
    cancellation: &Cancellation,
    on_progress: &mut impl FnMut(String),
) -> Result<(), GitError> {
    GitCommand::new(git_executable, working_directory, GitAccess::Network)
        .args(["clone", "--progress", "--", remote])
        .arg(destination)
        .run_with_progress(cancellation, on_progress)
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::{
        Cancellation, GitService,
        testing::{Fixture, initialize_repository},
    };

    #[test]
    fn clone_uses_the_explicit_checkout_destination() {
        let fixture = Fixture::new();
        let source = fixture.directory("clone-source");
        initialize_repository(&source);
        let working_directory = fixture.directory("clone-working-directory");
        let destination = Path::new("chosen-checkout");

        GitService::new(&working_directory, &fixture.data_dir)
            .clone_to(
                source.to_str().unwrap(),
                destination,
                &Cancellation::default(),
                |_| {},
            )
            .unwrap();

        assert!(working_directory.join(destination).join(".git").is_dir());
        assert!(!working_directory.join("checkout").exists());
    }

    #[test]
    fn clone_accepts_an_absolute_checkout_destination() {
        let fixture = Fixture::new();
        let source = fixture.directory("absolute-clone-source");
        initialize_repository(&source);
        let working_directory = fixture.directory("absolute-clone-working-directory");
        let destination = fixture.root.path().join("absolute-chosen-checkout");

        GitService::new(&working_directory, &fixture.data_dir)
            .clone_to(
                source.to_str().unwrap(),
                &destination,
                &Cancellation::default(),
                |_| {},
            )
            .unwrap();

        assert!(destination.join(".git").is_dir());
        assert!(!working_directory.join("absolute-chosen-checkout").exists());
    }
}
