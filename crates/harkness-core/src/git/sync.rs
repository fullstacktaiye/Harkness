//! Fetch, pull and push.
//!
//! The three operations that reach a remote, and with them the one operation
//! that can destroy work belonging to someone who is not the caller. The
//! guardrails are therefore here, in the core, as typed refusals: nothing that
//! calls Harkness gets to decide for itself whether a force push is allowed,
//! because the window and the agent share this code and only this code.
//!
//! The network work always shells out to system Git through the shared runner.
//! `git2` answers the local questions around it — which remote, which upstream,
//! which default branch, what moved — and never the network ones: libgit2 is
//! built with `default-features = false` and therefore has neither an HTTPS nor
//! an SSH transport compiled in. Enabling them would bundle OpenSSL, break the
//! credential-helper delegation this design rests on, and break the macOS and
//! Windows builds.

use std::{collections::BTreeMap, path::Path};

use git2::{ErrorCode, Oid, Repository};

use crate::{
    catalog::entry::GitStatus,
    git::{
        GitError, RepositoryLock,
        runner::{Cancellation, GitAccess, GitCommand},
        status,
    },
};

/// The remote a repository means when nothing else names one.
const DEFAULT_REMOTE: &str = "origin";

/// What one fetch should do.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FetchOptions {
    /// The remote to contact. `None` resolves one, preferring the upstream of
    /// the checked-out branch.
    pub remote: Option<String>,
    /// Delete the remote-tracking refs whose branches the remote no longer has.
    pub prune: bool,
}

/// How a pull reconciles the local branch with its upstream.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PullStrategy {
    /// Advance the branch only when the upstream already contains it, and fail
    /// as [`GitError::NonFastForward`] otherwise.
    ///
    /// The default, because it is the only strategy that cannot rewrite or
    /// invent history on a caller's behalf.
    #[default]
    FastForwardOnly,
    /// Merge the upstream, creating a merge commit when the histories have
    /// diverged.
    Merge,
    /// Replay the local commits on top of the upstream.
    Rebase,
}

/// What one pull should do.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PullOptions {
    /// Where the upstream branch is fetched from. `None` uses the remote the
    /// branch tracks.
    pub remote: Option<String>,
    /// How to reconcile the two histories.
    pub strategy: PullStrategy,
}

/// What one push should do.
///
/// There is deliberately no `force` field. The only override is
/// [`force_with_lease`], which refuses to overwrite a remote branch that has
/// moved since it was last fetched; no value of this type can make the runner
/// emit a bare `--force`.
///
/// [`force_with_lease`]: PushOptions::force_with_lease
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PushOptions {
    /// The remote to push to. `None` resolves one, preferring the upstream of
    /// the branch being pushed.
    pub remote: Option<String>,
    /// Configure the branch to track what this push creates.
    ///
    /// Without it, a branch that tracks nothing is refused as
    /// [`GitError::NoUpstream`] rather than quietly creating a remote branch.
    pub set_upstream: bool,
    /// Overwrite the remote branch, but only while it still points at the
    /// commit this repository last fetched.
    pub force_with_lease: bool,
    /// Allow the push when the branch is the remote's default branch.
    ///
    /// Without it, such a push is refused as [`GitError::DefaultBranchPush`],
    /// and a remote whose default branch cannot be determined is refused as
    /// [`GitError::DefaultBranchUnknown`] rather than guessed at.
    pub allow_default_branch: bool,
}

/// What one fetch produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FetchOutcome {
    /// The remote that was contacted.
    pub remote: String,
    /// Whether any remote-tracking ref of that remote moved.
    pub updated: bool,
    /// The repository once the fetch had run. The divergence reported here is
    /// what the fetch was for.
    pub status: Option<GitStatus>,
}

/// What one pull produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullOutcome {
    /// The remote that was contacted.
    pub remote: String,
    /// The branch that was pulled.
    pub branch: String,
    /// How the two histories were reconciled.
    pub strategy: PullStrategy,
    /// Whether the branch moved, as opposed to already being up to date.
    pub updated: bool,
    /// The repository once the pull had run.
    pub status: Option<GitStatus>,
}

/// What one push produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PushOutcome {
    /// The remote that was pushed to.
    pub remote: String,
    /// The branch that was pushed, under the same name on the remote.
    pub branch: String,
    /// Whether this push is what gave the branch its upstream.
    pub upstream_configured: bool,
    /// Whether the remote-tracking ref moved, as opposed to the remote already
    /// having everything.
    pub updated: bool,
    /// The repository once the push had run.
    pub status: Option<GitStatus>,
}

/// Updates the remote-tracking refs of one remote.
///
/// Touches no branch and no working tree, so it is the one synchronizing
/// operation with nothing to refuse.
pub(crate) fn fetch(
    git_executable: &Path,
    root: &Path,
    _lock: &RepositoryLock,
    options: &FetchOptions,
    cancellation: &Cancellation,
    on_progress: impl FnMut(String),
) -> Result<FetchOutcome, GitError> {
    let repository = open(root)?;
    let branch = branch_if_any(&repository, root)?;
    let upstream = match &branch {
        Some(branch) => upstream(&repository, root, branch)?,
        None => None,
    };
    let remote = resolve_remote(
        &repository,
        root,
        options.remote.as_deref(),
        upstream.as_ref(),
    )?;

    let before = tracking_refs(&repository, root, &remote)?;
    run(
        git_executable,
        root,
        &fetch_arguments(&remote, options),
        cancellation,
        on_progress,
    )?;
    let after = tracking_refs(&open(root)?, root, &remote)?;

    Ok(FetchOutcome {
        remote,
        updated: before != after,
        status: current_status(root),
    })
}

/// Fetches the checked-out branch's upstream and reconciles the branch with it.
pub(crate) fn pull(
    git_executable: &Path,
    root: &Path,
    _lock: &RepositoryLock,
    options: &PullOptions,
    cancellation: &Cancellation,
    on_progress: impl FnMut(String),
) -> Result<PullOutcome, GitError> {
    let repository = open(root)?;
    let branch = current_branch(&repository, root)?;
    // Resolved rather than left to Git, so a branch that tracks nothing is one
    // typed refusal instead of a paragraph of hints on standard error.
    let upstream = upstream(&repository, root, &branch)?.ok_or(GitError::NoUpstream {
        branch: branch.clone(),
    })?;
    let remote = resolve_remote(
        &repository,
        root,
        options.remote.as_deref(),
        Some(&upstream),
    )?;

    let before = head_commit(&repository);
    run(
        git_executable,
        root,
        &pull_arguments(&remote, &upstream.branch, options),
        cancellation,
        on_progress,
    )?;
    let after = head_commit(&open(root)?);

    Ok(PullOutcome {
        remote,
        branch,
        strategy: options.strategy,
        updated: before != after,
        status: current_status(root),
    })
}

/// Publishes the checked-out branch to a remote, under the same name.
///
/// Both refusals are evaluated before Git is spawned, so a refused push never
/// contacts the remote at all.
pub(crate) fn push(
    git_executable: &Path,
    root: &Path,
    _lock: &RepositoryLock,
    options: &PushOptions,
    cancellation: &Cancellation,
    on_progress: impl FnMut(String),
) -> Result<PushOutcome, GitError> {
    let repository = open(root)?;
    let branch = current_branch(&repository, root)?;
    let upstream = upstream(&repository, root, &branch)?;
    let remote = resolve_remote(
        &repository,
        root,
        options.remote.as_deref(),
        upstream.as_ref(),
    )?;

    // Creating a remote branch is a deliberate act. A branch that tracks
    // nothing is refused until the caller says it means to publish it.
    if upstream.is_none() && !options.set_upstream {
        return Err(GitError::NoUpstream { branch });
    }
    if !options.allow_default_branch {
        // Only asked when the guardrail is live: a caller that has already
        // allowed the default branch is not made to prove which one it is.
        let default = default_branch(&repository, root, &remote)?;
        if default == branch {
            return Err(GitError::DefaultBranchPush { remote, branch });
        }
    }

    let before = tracking_refs(&repository, root, &remote)?;
    run(
        git_executable,
        root,
        &push_arguments(&remote, &branch, options),
        cancellation,
        on_progress,
    )?;
    let after = tracking_refs(&open(root)?, root, &remote)?;

    Ok(PushOutcome {
        remote,
        branch,
        upstream_configured: options.set_upstream && upstream.is_none(),
        updated: before != after,
        status: current_status(root),
    })
}

/// `git fetch` and its options.
fn fetch_arguments(remote: &str, options: &FetchOptions) -> Vec<String> {
    let mut arguments = vec![owned("fetch"), owned("--progress")];
    if options.prune {
        arguments.push(owned("--prune"));
    }
    arguments.extend([owned("--"), remote.to_owned()]);
    arguments
}

/// `git pull` and its options.
///
/// The remote and its branch are always named. Left implicit, the reconciliation
/// would depend on the user's `pull.rebase`, and Harkness would be reporting a
/// [`PullStrategy`] it had not actually used.
fn pull_arguments(remote: &str, branch: &str, options: &PullOptions) -> Vec<String> {
    let mut arguments = vec![owned("pull"), owned("--progress")];
    match options.strategy {
        PullStrategy::FastForwardOnly => arguments.push(owned("--ff-only")),
        // `--ff` so a user's `merge.ff = only` cannot turn the strategy that
        // exists to merge into the one that refuses to, and `--no-edit`
        // because a merge commit must never wait on an editor a front end has
        // no terminal to show.
        PullStrategy::Merge => {
            arguments.extend([owned("--no-rebase"), owned("--ff"), owned("--no-edit")])
        }
        PullStrategy::Rebase => arguments.push(owned("--rebase")),
    }
    arguments.extend([owned("--"), remote.to_owned(), branch.to_owned()]);
    arguments
}

/// `git push` and its options.
///
/// `--force` appears nowhere and cannot be reached: [`PushOptions`] has no
/// field that emits it, and `--force-with-lease` is a different argument that
/// refuses the overwrite Git's plain force would have performed silently.
fn push_arguments(remote: &str, branch: &str, options: &PushOptions) -> Vec<String> {
    let mut arguments = vec![owned("push"), owned("--progress")];
    if options.set_upstream {
        arguments.push(owned("--set-upstream"));
    }
    if options.force_with_lease {
        arguments.push(owned("--force-with-lease"));
    }
    // The branch is named on both sides, so the destination cannot depend on
    // the user's `push.default`, and the default-branch refusal above therefore
    // decides about the ref this actually writes.
    arguments.extend([owned("--"), remote.to_owned(), branch.to_owned()]);
    arguments
}

/// Runs one network invocation and restates its failure as a typed one.
fn run(
    git_executable: &Path,
    root: &Path,
    arguments: &[String],
    cancellation: &Cancellation,
    on_progress: impl FnMut(String),
) -> Result<(), GitError> {
    GitCommand::new(git_executable, root, GitAccess::Network)
        .args(arguments)
        .run_with_progress(cancellation, on_progress)
        .map(|_| ())
        .map_err(classify)
}

/// Substrings Git uses when it rejects a push, or refuses to fast-forward.
///
/// Matched once, here, so that a caller matches on a variant instead of on
/// standard error.
const NON_FAST_FORWARD_MARKERS: [&str; 5] = [
    // `git push`, on the rejected ref and in the hint that follows it.
    "non-fast-forward",
    "fetch first",
    "updates were rejected because",
    // `--force-with-lease`, when the remote moved since the last fetch.
    "stale info",
    // `git pull --ff-only`, on diverged histories.
    "not possible to fast-forward",
];

/// Substrings Git uses when it could not authenticate to a remote.
///
/// `terminal prompts disabled` belongs here rather than anywhere else: it is
/// what the runner's `GIT_TERMINAL_PROMPT=0` turns a credential prompt into.
const AUTHENTICATION_MARKERS: [&str; 7] = [
    "authentication failed",
    "could not read username",
    "could not read password",
    "terminal prompts disabled",
    "invalid username or password",
    "permission denied (publickey",
    "support for password authentication was removed",
];

/// Recognizes the failures worth their own variant in Git's diagnostic.
///
/// Anything unrecognized stays [`GitError::Failed`] with its output intact: a
/// wrong guess would be worse than the diagnostic Git already wrote.
fn classify(error: GitError) -> GitError {
    let GitError::Failed { command, stderr } = error else {
        return error;
    };
    let diagnostic = stderr.to_lowercase();
    let reported = |markers: &[&str]| markers.iter().any(|marker| diagnostic.contains(marker));
    if reported(&AUTHENTICATION_MARKERS) {
        GitError::AuthenticationFailed { command, stderr }
    } else if reported(&NON_FAST_FORWARD_MARKERS) {
        GitError::NonFastForward { command, stderr }
    } else {
        GitError::Failed { command, stderr }
    }
}

/// A branch's configured upstream, as `git pull` resolves it.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Upstream {
    /// The remote named by `branch.<name>.remote`.
    remote: String,
    /// The branch on that remote, from `branch.<name>.merge`.
    branch: String,
}

/// Reads the upstream configured for `branch`, if it has one.
///
/// Read from configuration rather than from the remote-tracking ref, because
/// that pair is exactly what Git itself would fetch and merge, and it stays
/// correct for a repository whose refspec is not the conventional one.
fn upstream(
    repository: &Repository,
    root: &Path,
    branch: &str,
) -> Result<Option<Upstream>, GitError> {
    let configuration = repository
        .config()
        .map_err(|source| inspection(root, source))?;
    let (Some(remote), Some(merge)) = (
        configured(&configuration, root, &format!("branch.{branch}.remote"))?,
        configured(&configuration, root, &format!("branch.{branch}.merge"))?,
    ) else {
        return Ok(None);
    };
    Ok(Some(Upstream {
        remote,
        branch: merge
            .strip_prefix("refs/heads/")
            .unwrap_or(&merge)
            .to_owned(),
    }))
}

/// Reads one configuration value, distinguishing absent from unreadable.
fn configured(
    configuration: &git2::Config,
    root: &Path,
    key: &str,
) -> Result<Option<String>, GitError> {
    // `get_string` rather than `get_str`: libgit2 refuses to hand out a
    // borrowed string from a live configuration, only from a snapshot.
    match configuration.get_string(key) {
        Ok(value) if value.is_empty() => Ok(None),
        Ok(value) => Ok(Some(value)),
        Err(error) if error.code() == ErrorCode::NotFound => Ok(None),
        Err(source) => Err(inspection(root, source)),
    }
}

/// Chooses the remote to contact.
///
/// A named remote must exist; nothing else is ever silently substituted for it.
/// With none named, a branch that tracks something decides, and a branch whose
/// remote no longer exists is refused rather than redirected — a branch that
/// tracks `upstream` must not be published to `origin` because `upstream` was
/// deleted from the configuration. Only a branch that tracks nothing at all
/// falls back, to `origin` and then to a repository's single remote.
fn resolve_remote(
    repository: &Repository,
    root: &Path,
    requested: Option<&str>,
    upstream: Option<&Upstream>,
) -> Result<String, GitError> {
    let configured = repository
        .remotes()
        .map_err(|source| inspection(root, source))?;
    // A remote whose name is not UTF-8 is skipped rather than guessed at; it
    // can still be reached by naming it, since that name came from the caller.
    let names = || configured.iter().filter_map(|name| name.ok().flatten());
    let known = |candidate: &str| names().any(|name| name == candidate);

    if let Some(requested) = requested {
        return if known(requested) {
            Ok(requested.to_owned())
        } else {
            Err(GitError::NoRemote {
                remote: Some(requested.to_owned()),
            })
        };
    }
    if let Some(upstream) = upstream {
        return if known(&upstream.remote) {
            Ok(upstream.remote.clone())
        } else {
            Err(GitError::NoRemote {
                remote: Some(upstream.remote.clone()),
            })
        };
    }
    if known(DEFAULT_REMOTE) {
        return Ok(DEFAULT_REMOTE.to_owned());
    }
    let mut only = names();
    match (only.next(), only.next()) {
        (Some(only), None) => Ok(only.to_owned()),
        _ => Err(GitError::NoRemote { remote: None }),
    }
}

/// Reads the remote's default branch from `refs/remotes/<remote>/HEAD`.
///
/// That ref is what `git clone` and `git remote set-head` record, and it is the
/// only local evidence of which branch the remote considers its default. A
/// repository that has never recorded it is [`GitError::DefaultBranchUnknown`]:
/// assuming `main` would let a push through on exactly the repositories where
/// the guardrail could not be checked.
fn default_branch(repository: &Repository, root: &Path, remote: &str) -> Result<String, GitError> {
    let unknown = || GitError::DefaultBranchUnknown {
        remote: remote.to_owned(),
    };
    let head = match repository.find_reference(&format!("refs/remotes/{remote}/HEAD")) {
        Ok(head) => head,
        Err(error) if error.code() == ErrorCode::NotFound => return Err(unknown()),
        Err(source) => return Err(inspection(root, source)),
    };
    head.symbolic_target()
        .map_err(|source| inspection(root, source))?
        .and_then(|target| target.strip_prefix(&format!("refs/remotes/{remote}/")))
        .filter(|branch| !branch.is_empty())
        .map(str::to_owned)
        .ok_or_else(unknown)
}

/// Names the checked-out branch, refusing anything a push cannot be about.
///
/// An unborn branch keeps its name: it has no commits to push, but it is still
/// the branch the default-branch refusal has to be evaluated against.
fn current_branch(repository: &Repository, root: &Path) -> Result<String, GitError> {
    match repository.head() {
        Ok(head) if head.is_branch() => head
            .shorthand()
            .map(str::to_owned)
            .map_err(|source| inspection(root, source)),
        Ok(head) => Err(GitError::DetachedHead {
            path: root.to_path_buf(),
            detail: head.target().map_or_else(
                || "HEAD resolves to no commit".to_owned(),
                |commit| format!("HEAD is detached at {commit}"),
            ),
        }),
        Err(error) if error.code() == ErrorCode::UnbornBranch => unborn_branch(repository, root),
        Err(source) => Err(inspection(root, source)),
    }
}

/// Names the checked-out branch, or reports that there is none.
///
/// A fetch is about a remote rather than about a branch, so a detached head is
/// an ordinary answer there rather than a refusal.
fn branch_if_any(repository: &Repository, root: &Path) -> Result<Option<String>, GitError> {
    match current_branch(repository, root) {
        Ok(branch) => Ok(Some(branch)),
        Err(GitError::DetachedHead { .. }) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Reads the branch name HEAD points at before the first commit exists.
fn unborn_branch(repository: &Repository, root: &Path) -> Result<String, GitError> {
    repository
        .find_reference("HEAD")
        .and_then(|head| {
            head.symbolic_target()
                .map(|target| target.map(str::to_owned))
        })
        .map_err(|source| inspection(root, source))?
        .and_then(|target| {
            target
                .strip_prefix("refs/heads/")
                .filter(|branch| !branch.is_empty())
                .map(str::to_owned)
        })
        .ok_or_else(|| GitError::DetachedHead {
            path: root.to_path_buf(),
            detail: "HEAD names no branch".to_owned(),
        })
}

/// Snapshots the remote-tracking refs of one remote.
///
/// Compared either side of an invocation, this is how "did anything actually
/// move" is answered without parsing Git's human-readable report of it.
fn tracking_refs(
    repository: &Repository,
    root: &Path,
    remote: &str,
) -> Result<BTreeMap<String, Oid>, GitError> {
    let prefix = format!("refs/remotes/{remote}/");
    let mut tracked = BTreeMap::new();
    for reference in repository
        .references()
        .map_err(|source| inspection(root, source))?
    {
        let reference = reference.map_err(|source| inspection(root, source))?;
        // A ref whose name is not UTF-8, or which is symbolic like
        // `refs/remotes/<remote>/HEAD`, is skipped consistently on both sides
        // of the comparison and so cannot make one look like a change.
        let Ok(name) = reference.name() else {
            continue;
        };
        if let Some(target) = reference.target()
            && name.starts_with(&prefix)
        {
            tracked.insert(name.to_owned(), target);
        }
    }
    Ok(tracked)
}

fn head_commit(repository: &Repository) -> Option<Oid> {
    repository.head().ok().and_then(|head| head.target())
}

/// Describes the repository after an operation that already succeeded.
///
/// Degrades to `None` rather than failing: the fetch, pull or push has happened
/// by now, and reporting it as an error because the description afterwards
/// could not be read would be a lie about what the repository contains.
fn current_status(root: &Path) -> Option<GitStatus> {
    status::inspect(root).unwrap_or_default()
}

fn open(root: &Path) -> Result<Repository, GitError> {
    match Repository::open(root) {
        Ok(repository) => Ok(repository),
        Err(error) if error.code() == ErrorCode::NotFound => Err(GitError::NotARepository {
            path: root.to_path_buf(),
        }),
        Err(source) => Err(inspection(root, source)),
    }
}

fn inspection(root: &Path, source: git2::Error) -> GitError {
    GitError::Inspection {
        path: root.to_path_buf(),
        source,
    }
}

fn owned(argument: &str) -> String {
    argument.to_owned()
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use git2::Repository;

    use super::{
        FetchOptions, PullOptions, PullStrategy, PushOptions, classify, fetch_arguments,
        pull_arguments, push_arguments,
    };
    use crate::{
        catalog::entry::UpstreamStatus,
        git::{Cancellation, GitError, GitService},
        testing::{
            Fixture, PROCESS_PROJECT_ROOT_ENV, PROCESS_READY_FILE_ENV, commit_all, git,
            initialize_repository, remote_with_clone, spawn_child, wait_for_child_signal,
        },
    };

    /// The refusal that has to hold structurally rather than by review: no
    /// combination of options may produce Git's unconditional force.
    #[test]
    fn no_push_options_value_produces_a_bare_force() {
        for set_upstream in [false, true] {
            for force_with_lease in [false, true] {
                for allow_default_branch in [false, true] {
                    let options = PushOptions {
                        remote: Some("origin".to_owned()),
                        set_upstream,
                        force_with_lease,
                        allow_default_branch,
                    };

                    let arguments = push_arguments("origin", "main", &options);

                    // Everything Git reads as an option ends at `--`; the three
                    // arguments after it are data whatever they are called.
                    let separator = arguments.iter().position(|argument| argument == "--");
                    let flags = &arguments[..separator.expect("the refspec is separated")];
                    assert!(
                        !flags
                            .iter()
                            .any(|flag| flag == "--force" || flag == "-f" || flag == "--f"),
                        "{options:?} produced {arguments:?}"
                    );
                    assert_eq!(
                        flags.iter().any(|flag| flag == "--force-with-lease"),
                        force_with_lease,
                        "{options:?} produced {arguments:?}"
                    );
                    assert_eq!(
                        flags.iter().any(|flag| flag == "--set-upstream"),
                        set_upstream,
                        "{options:?} produced {arguments:?}"
                    );
                    assert_eq!(
                        &arguments[separator.unwrap()..],
                        ["--", "origin", "main"],
                        "{options:?} produced {arguments:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn fetch_and_pull_arguments_carry_their_options() {
        assert_eq!(
            fetch_arguments("origin", &FetchOptions::default()),
            ["fetch", "--progress", "--", "origin"]
        );
        assert_eq!(
            fetch_arguments(
                "upstream",
                &FetchOptions {
                    remote: Some("upstream".to_owned()),
                    prune: true,
                }
            ),
            ["fetch", "--progress", "--prune", "--", "upstream"]
        );

        let strategy = |strategy| PullOptions {
            remote: None,
            strategy,
        };
        assert_eq!(
            pull_arguments("origin", "main", &strategy(PullStrategy::FastForwardOnly)),
            ["pull", "--progress", "--ff-only", "--", "origin", "main"]
        );
        assert_eq!(
            pull_arguments("origin", "main", &strategy(PullStrategy::Merge)),
            [
                "pull",
                "--progress",
                "--no-rebase",
                "--ff",
                "--no-edit",
                "--",
                "origin",
                "main"
            ]
        );
        assert_eq!(
            pull_arguments("origin", "main", &strategy(PullStrategy::Rebase)),
            ["pull", "--progress", "--rebase", "--", "origin", "main"]
        );
    }

    /// Real diagnostics, so that a caller never has to match on them itself.
    #[test]
    fn git_diagnostics_become_typed_failures() {
        let failed = |stderr: &str| GitError::Failed {
            command: "push".to_owned(),
            stderr: stderr.to_owned(),
        };

        for stderr in [
            " ! [rejected]        main -> main (non-fast-forward)",
            " ! [rejected]        main -> main (fetch first)",
            " ! [rejected]        main -> main (stale info)",
            "hint: Updates were rejected because the tip of your current branch is behind",
            "fatal: Not possible to fast-forward, aborting.",
        ] {
            assert!(
                matches!(classify(failed(stderr)), GitError::NonFastForward { .. }),
                "{stderr}"
            );
        }

        for stderr in [
            "fatal: could not read Username for 'https://github.com': terminal prompts disabled",
            "remote: Support for password authentication was removed on August 13, 2021.\n\
             fatal: Authentication failed for 'https://github.com/octocat/Hello-World.git/'",
            "git@github.com: Permission denied (publickey).\n\
             fatal: Could not read from remote repository.",
            "remote: Invalid username or password.",
        ] {
            assert!(
                matches!(
                    classify(failed(stderr)),
                    GitError::AuthenticationFailed { .. }
                ),
                "{stderr}"
            );
        }

        // Anything unrecognized keeps Git's own account of itself.
        assert!(matches!(
            classify(failed(
                "fatal: repository 'https://example.invalid/' not found"
            )),
            GitError::Failed { .. }
        ));
        assert!(matches!(classify(GitError::Cancelled), GitError::Cancelled));
    }

    #[test]
    fn a_fetch_updates_tracking_refs_and_reports_the_new_divergence() {
        let fixture = Fixture::new();
        let (remote, clone) = remote_with_clone(&fixture, "fetched");
        let service = GitService::new(&clone, &fixture.data_dir);
        commit_in_remote(&fixture, &remote, "remote.txt");

        let before = service.status().unwrap().unwrap();
        assert_eq!(divergence(&before), Some((0, 0)));

        let outcome = service
            .fetch(&FetchOptions::default(), &Cancellation::default(), |_| {})
            .unwrap();

        assert_eq!(outcome.remote, "origin");
        assert!(outcome.updated);
        assert_eq!(divergence(&outcome.status.unwrap()), Some((0, 1)));
        assert_eq!(
            divergence(&service.status().unwrap().unwrap()),
            Some((0, 1)),
            "the tracking ref did not survive the fetch"
        );

        // Nothing changed in the remote since, so the second fetch moves no ref.
        let repeated = service
            .fetch(&FetchOptions::default(), &Cancellation::default(), |_| {})
            .unwrap();
        assert!(!repeated.updated);
    }

    #[test]
    fn a_fast_forward_pull_advances_the_branch() {
        let fixture = Fixture::new();
        let (remote, clone) = remote_with_clone(&fixture, "pulled");
        let service = GitService::new(&clone, &fixture.data_dir);
        commit_in_remote(&fixture, &remote, "remote.txt");

        let outcome = service
            .pull(&PullOptions::default(), &Cancellation::default(), |_| {})
            .unwrap();

        assert_eq!(outcome.remote, "origin");
        assert_eq!(outcome.branch, "main");
        assert_eq!(outcome.strategy, PullStrategy::FastForwardOnly);
        assert!(outcome.updated);
        assert!(clone.join("remote.txt").exists());
        assert_eq!(divergence(&outcome.status.unwrap()), Some((0, 0)));

        let repeated = service
            .pull(&PullOptions::default(), &Cancellation::default(), |_| {})
            .unwrap();
        assert!(!repeated.updated, "an up-to-date pull reported a change");
    }

    /// The default strategy must fail rather than invent a merge commit, and it
    /// must leave the branch exactly where it was.
    #[test]
    fn a_diverged_pull_is_refused_instead_of_merged() {
        let fixture = Fixture::new();
        let (remote, clone) = remote_with_clone(&fixture, "diverged");
        let service = GitService::new(&clone, &fixture.data_dir);
        commit_in_remote(&fixture, &remote, "remote.txt");
        fs::write(clone.join("local.txt"), "local\n").unwrap();
        commit_all(&Repository::open(&clone).unwrap(), "local commit");
        let head = git(&clone, ["rev-parse", "HEAD"]);

        let error = service
            .pull(&PullOptions::default(), &Cancellation::default(), |_| {})
            .unwrap_err();

        assert!(
            matches!(error, GitError::NonFastForward { .. }),
            "{error:?}"
        );
        assert_eq!(git(&clone, ["rev-parse", "HEAD"]), head);
        assert_eq!(git(&clone, ["rev-list", "--count", "HEAD"]).trim(), "2");
        assert_eq!(
            divergence(&service.status().unwrap().unwrap()),
            Some((1, 1)),
            "the pull's fetch did not record the divergence"
        );
    }

    #[test]
    fn a_push_without_an_upstream_is_refused_until_one_is_requested() {
        let fixture = Fixture::new();
        let (remote, clone) = remote_with_clone(&fixture, "unpublished");
        let service = GitService::new(&clone, &fixture.data_dir);
        git(&clone, ["checkout", "-b", "feature"]);
        fs::write(clone.join("feature.txt"), "feature\n").unwrap();
        commit_all(&Repository::open(&clone).unwrap(), "feature commit");

        let refused = service
            .push(&PushOptions::default(), &Cancellation::default(), |_| {})
            .unwrap_err();

        assert!(
            matches!(&refused, GitError::NoUpstream { branch } if branch == "feature"),
            "{refused:?}"
        );
        assert!(
            Repository::open(&remote)
                .unwrap()
                .find_reference("refs/heads/feature")
                .is_err(),
            "the refused push reached the remote"
        );

        let mut progress = Vec::new();
        let outcome = service
            .push(
                &PushOptions {
                    set_upstream: true,
                    ..PushOptions::default()
                },
                &Cancellation::default(),
                |message| progress.push(message),
            )
            .unwrap();

        assert_eq!(outcome.remote, "origin");
        assert_eq!(outcome.branch, "feature");
        assert!(outcome.upstream_configured);
        assert!(outcome.updated);
        assert!(
            Repository::open(&remote)
                .unwrap()
                .find_reference("refs/heads/feature")
                .is_ok()
        );
        assert_eq!(
            git(&clone, ["rev-parse", "--abbrev-ref", "feature@{upstream}"]).trim(),
            "origin/feature"
        );
        assert!(
            !progress.is_empty(),
            "the push forwarded no progress at all"
        );

        // With the upstream now configured, the same push needs no request.
        let repeated = service
            .push(&PushOptions::default(), &Cancellation::default(), |_| {})
            .unwrap();
        assert!(!repeated.upstream_configured);
        assert!(!repeated.updated);
    }

    /// The lease is the whole of the override, and it is not Git's force: it
    /// overwrites the remote only while the remote still holds exactly what
    /// this repository last saw there.
    #[test]
    fn a_lease_push_rewrites_history_but_not_work_it_never_saw() {
        let fixture = Fixture::new();
        let (remote, clone) = remote_with_clone(&fixture, "leased");
        let service = GitService::new(&clone, &fixture.data_dir);
        let repository = Repository::open(&clone).unwrap();
        let published = |force_with_lease| PushOptions {
            force_with_lease,
            allow_default_branch: true,
            ..PushOptions::default()
        };
        fs::write(clone.join("first.txt"), "first\n").unwrap();
        commit_all(&repository, "first");
        service
            .push(&published(false), &Cancellation::default(), |_| {})
            .unwrap();

        // Replace the commit that was just published. The remote still holds
        // what this repository last fetched, so the lease is intact.
        git(&clone, ["reset", "--hard", "HEAD~1"]);
        fs::write(clone.join("second.txt"), "second\n").unwrap();
        commit_all(&repository, "second");

        let refused = service
            .push(&published(false), &Cancellation::default(), |_| {})
            .unwrap_err();
        assert!(
            matches!(refused, GitError::NonFastForward { .. }),
            "{refused:?}"
        );
        service
            .push(&published(true), &Cancellation::default(), |_| {})
            .unwrap();
        assert_eq!(
            git(&remote, ["rev-parse", "HEAD"]),
            git(&clone, ["rev-parse", "HEAD"])
        );

        // Someone else pushes and this repository never fetches it, so its
        // lease is stale and the same request is refused rather than deleting
        // work it has never seen.
        commit_in_remote(&fixture, &remote, "theirs.txt");
        let theirs = git(&remote, ["rev-parse", "HEAD"]);
        fs::write(clone.join("third.txt"), "third\n").unwrap();
        commit_all(&repository, "third");

        let refused = service
            .push(&published(true), &Cancellation::default(), |_| {})
            .unwrap_err();

        assert!(
            matches!(refused, GitError::NonFastForward { .. }),
            "{refused:?}"
        );
        assert_eq!(
            git(&remote, ["rev-parse", "HEAD"]),
            theirs,
            "the lease was not honored"
        );
    }

    /// The two strategies that are opt-ins, doing what the default refuses to.
    #[test]
    fn merge_and_rebase_pulls_reconcile_a_divergence() {
        let fixture = Fixture::new();
        let reconciled = |name: &str, strategy| {
            let (remote, clone) = remote_with_clone(&fixture, name);
            // Both strategies write a commit through Git rather than through
            // libgit2, and a machine running the suite need not have an
            // identity configured for that.
            git(&clone, ["config", "user.name", "Harkness Tests"]);
            git(&clone, ["config", "user.email", "tests@harkness.invalid"]);
            commit_in_remote(&fixture, &remote, "theirs.txt");
            fs::write(clone.join("ours.txt"), "ours\n").unwrap();
            commit_all(&Repository::open(&clone).unwrap(), "ours");

            let outcome = GitService::new(&clone, &fixture.data_dir)
                .pull(
                    &PullOptions {
                        remote: None,
                        strategy,
                    },
                    &Cancellation::default(),
                    |_| {},
                )
                .unwrap();
            assert!(outcome.updated);
            assert!(clone.join("theirs.txt").exists() && clone.join("ours.txt").exists());
            (clone, outcome)
        };

        let (merged, outcome) = reconciled("merging", PullStrategy::Merge);
        assert_eq!(outcome.strategy, PullStrategy::Merge);
        assert_eq!(
            git(&merged, ["rev-list", "--count", "--merges", "HEAD"]).trim(),
            "1",
            "a merge pull did not merge"
        );

        let (rebased, outcome) = reconciled("rebasing", PullStrategy::Rebase);
        assert_eq!(outcome.strategy, PullStrategy::Rebase);
        assert_eq!(
            git(&rebased, ["rev-list", "--count", "--merges", "HEAD"]).trim(),
            "0",
            "a rebase pull created a merge commit"
        );
        assert_eq!(
            divergence(&outcome.status.unwrap()),
            Some((1, 0)),
            "the replayed commit is ahead of the upstream and nothing is behind"
        );
    }

    #[test]
    fn a_push_to_the_default_branch_is_refused_and_names_the_override() {
        let fixture = Fixture::new();
        let (remote, clone) = remote_with_clone(&fixture, "protected");
        let service = GitService::new(&clone, &fixture.data_dir);
        fs::write(clone.join("local.txt"), "local\n").unwrap();
        commit_all(&Repository::open(&clone).unwrap(), "local commit");
        let remote_head = git(&remote, ["rev-parse", "HEAD"]);

        let refused = service
            .push(&PushOptions::default(), &Cancellation::default(), |_| {})
            .unwrap_err();

        assert!(
            matches!(&refused, GitError::DefaultBranchPush { remote, branch }
                if remote == "origin" && branch == "main"),
            "{refused:?}"
        );
        assert!(
            refused.to_string().contains("allow_default_branch"),
            "the refusal does not name the override: {refused}"
        );
        assert_eq!(
            git(&remote, ["rev-parse", "HEAD"]),
            remote_head,
            "the refused push reached the remote"
        );

        let allowed = service
            .push(
                &PushOptions {
                    allow_default_branch: true,
                    ..PushOptions::default()
                },
                &Cancellation::default(),
                |_| {},
            )
            .unwrap();

        assert!(allowed.updated);
        assert_ne!(git(&remote, ["rev-parse", "HEAD"]), remote_head);
    }

    /// A remote whose default branch was never recorded is its own refusal:
    /// guessing `main` would wave the push through on exactly the repositories
    /// where the guardrail could not be evaluated.
    #[test]
    fn an_undeterminable_default_branch_is_refused_distinctly() {
        let fixture = Fixture::new();
        let (_, clone) = remote_with_clone(&fixture, "headless");
        let service = GitService::new(&clone, &fixture.data_dir);
        Repository::open(&clone)
            .unwrap()
            .find_reference("refs/remotes/origin/HEAD")
            .unwrap()
            .delete()
            .unwrap();

        let refused = service
            .push(&PushOptions::default(), &Cancellation::default(), |_| {})
            .unwrap_err();

        assert!(
            matches!(&refused, GitError::DefaultBranchUnknown { remote } if remote == "origin"),
            "{refused:?}"
        );

        // Allowing the default branch means the question is never asked, so the
        // missing ref stops mattering.
        service
            .push(
                &PushOptions {
                    allow_default_branch: true,
                    ..PushOptions::default()
                },
                &Cancellation::default(),
                |_| {},
            )
            .unwrap();
    }

    #[test]
    fn a_remote_that_is_not_configured_is_named_in_the_refusal() {
        let fixture = Fixture::new();
        let (_, clone) = remote_with_clone(&fixture, "misnamed");
        let service = GitService::new(&clone, &fixture.data_dir);

        let refused = service
            .fetch(
                &FetchOptions {
                    remote: Some("upstream".to_owned()),
                    prune: false,
                },
                &Cancellation::default(),
                |_| {},
            )
            .unwrap_err();

        assert!(
            matches!(&refused, GitError::NoRemote { remote } if remote.as_deref() == Some("upstream")),
            "{refused:?}"
        );

        // A branch that tracks a remote which no longer exists is refused by
        // that name, never redirected to whichever remote happens to be left.
        git(&clone, ["config", "branch.main.remote", "elsewhere"]);
        let refused = service
            .fetch(&FetchOptions::default(), &Cancellation::default(), |_| {})
            .unwrap_err();

        assert!(
            matches!(&refused, GitError::NoRemote { remote } if remote.as_deref() == Some("elsewhere")),
            "{refused:?}"
        );

        let root = fixture.directory("remoteless");
        initialize_repository(&root);
        let refused = GitService::new(&root, &fixture.data_dir)
            .fetch(&FetchOptions::default(), &Cancellation::default(), |_| {})
            .unwrap_err();

        assert!(
            matches!(&refused, GitError::NoRemote { remote } if remote.is_none()),
            "{refused:?}"
        );
    }

    #[test]
    fn a_detached_head_has_no_branch_to_push() {
        let fixture = Fixture::new();
        let (_, clone) = remote_with_clone(&fixture, "detached");
        git(&clone, ["checkout", "--detach", "HEAD"]);

        let refused = GitService::new(&clone, &fixture.data_dir)
            .push(&PushOptions::default(), &Cancellation::default(), |_| {})
            .unwrap_err();

        assert!(
            matches!(refused, GitError::DetachedHead { .. }),
            "{refused:?}"
        );
    }

    /// The lock is taken for the whole of an operation, so a repository another
    /// process is working on refuses rather than racing it.
    #[test]
    fn a_fetch_refuses_while_another_process_holds_the_repository() {
        let fixture = Fixture::new();
        let (_, clone) = remote_with_clone(&fixture, "busy");
        let ready_file = fixture.root.path().join("sync-lock-held");
        let mut holder = spawn_child(&fixture.data_dir, "hold-repository-lock")
            .env(PROCESS_PROJECT_ROOT_ENV, &clone)
            .env(PROCESS_READY_FILE_ENV, &ready_file)
            .spawn()
            .unwrap();
        wait_for_child_signal(&mut holder, &ready_file);

        let error = GitService::new(&clone, &fixture.data_dir)
            .fetch(&FetchOptions::default(), &Cancellation::default(), |_| {})
            .unwrap_err();

        holder.kill().unwrap();
        holder.wait().unwrap();
        assert!(
            matches!(error, GitError::RepositoryBusy { .. }),
            "{error:?}"
        );
    }

    /// Git opens `/dev/tty` for credentials even with stdin closed, so a front
    /// end with no terminal would hang forever without this. Exit 97 is a
    /// status no Git verb produces, so the failure is unambiguous.
    #[cfg(unix)]
    #[test]
    fn every_network_invocation_disables_terminal_prompts() {
        let fixture = Fixture::new();
        let (_, clone) = remote_with_clone(&fixture, "prompted");
        let invoked = fixture.root.path().join("prompt-checked-verbs");
        let asserting_git = fixture.shim(
            "prompt-asserting-git",
            &format!(
                "#!/bin/sh\n\
                 test \"$GIT_TERMINAL_PROMPT\" = 0 || exit 97\n\
                 echo \"$1\" >> '{}'\n",
                invoked.display()
            ),
        );
        let service = GitService::new(&clone, &fixture.data_dir).with_git_executable(asserting_git);

        service
            .fetch(&FetchOptions::default(), &Cancellation::default(), |_| {})
            .unwrap();
        service
            .pull(&PullOptions::default(), &Cancellation::default(), |_| {})
            .unwrap();
        service
            .push(
                &PushOptions {
                    allow_default_branch: true,
                    ..PushOptions::default()
                },
                &Cancellation::default(),
                |_| {},
            )
            .unwrap();

        // Without this the shim's exit 97 could never fire, and the test would
        // pass by never having run anything.
        assert_eq!(
            fs::read_to_string(&invoked)
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            ["fetch", "pull", "push"]
        );
    }

    /// The one test here that contacts GitHub.
    ///
    /// Every other test uses a local remote, which exercises no credential
    /// helper at all: this is the only coverage proving that a real remote is
    /// still reached with the user's own SSH or HTTPS setup now that fetch and
    /// pull run through the generalized runner. Push is not covered, because
    /// nothing on a public repository is writable.
    #[test]
    #[ignore = "requires network access and credentials for a public GitHub repository"]
    fn github_fetch_and_pull_round_trip() {
        let fixture = Fixture::new();
        let clone = fixture.root.path().join("hello-world");
        git(
            fixture.root.path(),
            [
                "clone",
                "--",
                "git@github.com:octocat/Hello-World.git",
                clone.to_str().unwrap(),
            ],
        );
        let service = GitService::new(&clone, &fixture.data_dir);

        let fetched = service
            .fetch(&FetchOptions::default(), &Cancellation::default(), |_| {})
            .unwrap();
        let pulled = service
            .pull(&PullOptions::default(), &Cancellation::default(), |_| {})
            .unwrap();

        assert_eq!(fetched.remote, "origin");
        assert!(!fetched.updated, "a fresh clone had refs to update");
        assert_eq!(pulled.remote, "origin");
        assert!(!pulled.updated, "a fresh clone had commits to pull");
        assert_eq!(divergence(&pulled.status.unwrap()), Some((0, 0)));
    }

    /// Commits a file in the bare remote, through a throwaway clone of it.
    fn commit_in_remote(fixture: &Fixture, remote: &Path, file: &str) {
        let name = remote.file_name().unwrap().to_string_lossy();
        let contributor = fixture
            .root
            .path()
            .join(format!("contributor-{name}-{file}"));
        git(
            fixture.root.path(),
            [
                "clone",
                "--",
                remote.to_str().unwrap(),
                contributor.to_str().unwrap(),
            ],
        );
        fs::write(contributor.join(file), "from the remote\n").unwrap();
        commit_all(&Repository::open(&contributor).unwrap(), "remote commit");
        git(&contributor, ["push", "--", "origin", "main"]);
    }

    fn divergence(status: &crate::GitStatus) -> Option<(usize, usize)> {
        status
            .upstream
            .as_ref()
            .map(|UpstreamStatus { ahead, behind, .. }| (*ahead, *behind))
    }
}
