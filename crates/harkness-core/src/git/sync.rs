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
//! what moved — and never the network ones: libgit2 is built with
//! `default-features = false` and therefore has neither an HTTPS nor an SSH
//! transport compiled in. Enabling them would bundle OpenSSL, break the
//! credential-helper delegation this design rests on, and break the macOS and
//! Windows builds.
//!
//! Two rules shape everything below, and both exist because a guardrail that
//! holds only under a default configuration is not a guardrail:
//!
//! - **Nothing is inferred from local state that a remote can contradict.**
//!   Which branch a remote calls its default is asked of the remote, not read
//!   from the ref a clone wrote months ago; what a push did to it is read from
//!   Git's report of that push, not guessed at by comparing tracking refs that
//!   a repository need not even have.
//! - **Every option that widens a command is pinned, in both directions.** A
//!   typed option that says "do not prune" emits `--no-prune` rather than
//!   nothing, because emitting nothing hands the decision to whatever
//!   `fetch.prune` happens to say. The same reasoning puts `--no-follow-tags`
//!   on a push that promised to publish one branch. The settings with no flag
//!   to pin them with are pinned by the runner's hermetic policy instead.
//!
//! The default-branch refusal is defense in depth and nothing more. It closes
//! the window between one client's check and its own push, and it cannot close
//! the window against anyone else: only branch protection configured on the
//! server is atomic with the write it guards.

use std::{collections::BTreeMap, path::Path};

use git2::{ErrorCode, Oid, Repository};

use crate::{
    catalog::entry::GitStatus,
    git::{
        GitError, LOCAL_REMOTE, RepositoryLock, head_branch, recorded_default_branch,
        resolve_remote,
        runner::{Cancellation, GitAccess, GitCommand, GitOutput},
        status,
    },
};

/// What one fetch should do.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FetchOptions {
    /// The remote to contact. `None` resolves one, preferring the upstream of
    /// the checked-out branch.
    pub remote: Option<String>,
    /// Delete the remote-tracking refs whose branches the remote no longer has.
    ///
    /// Decides the question outright in both directions: `false` is "do not
    /// prune" and not "say nothing about pruning", so a repository configured
    /// with `fetch.prune` or `remote.<name>.prune` still keeps its stale
    /// tracking refs when a caller asked for them to be kept. Tags are never
    /// pruned either way; this option is about branches the remote deleted.
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
/// No value of this type can widen the push beyond one branch either. Tags,
/// submodule commits and any other ref Git can be configured to carry along
/// are excluded by the arguments rather than by the configuration, so the
/// promise this type makes is the promise the invocation keeps.
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
    ///
    /// Answered by comparing `refs/remotes/<remote>/*` either side of the
    /// fetch, so a repository whose refspec writes somewhere else reports
    /// `false` even where objects arrived. Nothing outside this repository
    /// depends on the answer, which is why the same shortcut is not taken for
    /// a push: see [`PushOutcome::update`].
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

/// What a push did to the branch on the remote.
///
/// Read from Git's own report of the push. The obvious alternative — comparing
/// the remote-tracking refs either side of it — answers a different question:
/// a remote reached through a custom refspec, or configured with no fetch
/// mapping at all, has no tracking ref to move, so a real publication would
/// look exactly like a push that did nothing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RefUpdate {
    /// The remote already pointed at the commit that was pushed.
    Unchanged,
    /// The branch did not exist on the remote until this push created it.
    Created,
    /// The remote branch advanced, keeping every commit it already had.
    FastForward,
    /// The remote branch was moved to a commit that does not contain what it
    /// held, discarding those commits. Only [`PushOptions::force_with_lease`]
    /// can reach this.
    Forced,
    /// Git reported the push as successful, but said nothing attributable to
    /// this branch.
    ///
    /// Counted as a change by [`PushOutcome::updated`], because a report that
    /// cannot be read is not evidence that nothing happened — and treating it
    /// as such is the specific mistake this enum replaced.
    Unknown,
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
    /// What the push did to that branch on the remote.
    pub update: RefUpdate,
    /// The repository once the push had run.
    pub status: Option<GitStatus>,
}

impl PushOutcome {
    /// Whether the remote changed.
    ///
    /// A method rather than a field, so the one answer that has to be
    /// conservative — [`RefUpdate::Unknown`] — cannot be stored as `false` by
    /// anything constructing this type.
    #[must_use]
    pub fn updated(&self) -> bool {
        !matches!(self.update, RefUpdate::Unchanged)
    }
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
        upstream.as_ref().map(|upstream| upstream.remote.as_str()),
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

/// Refuses an operation the repository is in no state to start.
///
/// Asked before Git is spawned so that a repository someone left mid-merge
/// reports what is actually wrong with it. Left to Git, the refusal would
/// arrive as a generic failure indistinguishable from one this call caused.
fn refuse_while_pending(repository: &Repository, root: &Path) -> Result<(), GitError> {
    match status::pending(repository) {
        None => Ok(()),
        Some(pending) => Err(GitError::OperationInProgress {
            path: root.to_path_buf(),
            pending,
        }),
    }
}

/// Fetches the checked-out branch's upstream and reconciles the branch with it.
///
/// The one operation here that writes to the working tree, and therefore the
/// one whose failures are not all equivalent to "nothing happened": see
/// [`interrupted`].
pub(crate) fn pull(
    git_executable: &Path,
    root: &Path,
    _lock: &RepositoryLock,
    options: &PullOptions,
    cancellation: &Cancellation,
    on_progress: impl FnMut(String),
) -> Result<PullOutcome, GitError> {
    let repository = open(root)?;
    refuse_while_pending(&repository, root)?;
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
        Some(&upstream.remote),
    )?;

    let before = head_commit(&repository);
    let arguments = pull_arguments(&remote, &upstream.branch, options);
    run(git_executable, root, &arguments, cancellation, on_progress)
        .map_err(|error| interrupted(root, &arguments, error))?;
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
/// Every refusal is evaluated before anything is written, so a refused push
/// never changes the remote. All but one are settled without contacting it:
/// the default-branch guardrail costs a `ls-remote`, because the remote is the
/// only thing that knows which branch it currently calls its default.
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
    // Before the upstream question, because a branch with no commits has
    // nothing to publish whatever it tracks, and Git's own diagnostic for it
    // talks about a refspec the caller never wrote.
    if head_commit(&repository).is_none() {
        return Err(GitError::UnbornBranch {
            path: root.to_path_buf(),
            branch,
        });
    }
    let upstream = upstream(&repository, root, &branch)?;
    let remote = resolve_remote(
        &repository,
        root,
        options.remote.as_deref(),
        upstream.as_ref().map(|upstream| upstream.remote.as_str()),
    )?;
    if remote == LOCAL_REMOTE {
        return Err(GitError::LocalUpstreamUnsupported { branch });
    }

    // Creating a remote branch is a deliberate act. A branch that tracks
    // nothing is refused until the caller says it means to publish it.
    if upstream.is_none() && !options.set_upstream {
        return Err(GitError::NoUpstream { branch });
    }
    if !options.allow_default_branch {
        // Only asked when the guardrail is live: a caller that has already
        // allowed the default branch is not made to prove which one it is,
        // and pays for no round trip proving it.
        let default = default_branch(git_executable, root, &repository, &remote, cancellation)?;
        if default == branch {
            return Err(GitError::DefaultBranchPush { remote, branch });
        }
    }

    let reported = run(
        git_executable,
        root,
        &push_arguments(&remote, &branch, options),
        cancellation,
        on_progress,
    )?;

    let update = push_report(&reported.stdout, &branch);
    Ok(PushOutcome {
        remote,
        branch,
        upstream_configured: options.set_upstream && upstream.is_none(),
        update,
        status: current_status(root),
    })
}

/// `git fetch` and its options.
///
/// Pruning is stated either way. Emitting nothing for `prune: false` would
/// leave the answer to `fetch.prune`, and to `remote.<name>.prune`, which
/// overrides even that — so a caller that asked for its stale tracking refs to
/// be kept would watch them be deleted. Tags are never pruned, because this
/// option is about branches; submodules are never recursed, because this
/// operation is about one repository.
fn fetch_arguments(remote: &str, options: &FetchOptions) -> Vec<String> {
    let mut arguments = vec![owned("fetch"), owned("--progress")];
    arguments.push(owned(if options.prune {
        "--prune"
    } else {
        "--no-prune"
    }));
    arguments.extend([owned("--no-prune-tags"), owned("--no-recurse-submodules")]);
    arguments.extend([owned("--"), remote.to_owned()]);
    arguments
}

/// `git pull` and its options.
///
/// The remote and its branch are always named. Left implicit, the reconciliation
/// would depend on the user's `pull.rebase`, and Harkness would be reporting a
/// [`PullStrategy`] it had not actually used.
///
/// `--no-prune` for the reason it appears on a fetch, and more so: deleting
/// tracking refs is not something anyone asked a pull to do. Autostash has no
/// flag old enough to rely on for both strategies and is pinned by the runner
/// instead.
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
    arguments.extend([owned("--no-prune"), owned("--no-recurse-submodules")]);
    arguments.extend([owned("--"), remote.to_owned(), branch.to_owned()]);
    arguments
}

/// `git push` and its options.
///
/// `--force` appears nowhere and cannot be reached: [`PushOptions`] has no
/// field that emits it, and `--force-with-lease` is a different argument that
/// refuses the overwrite Git's plain force would have performed silently.
///
/// The other three arguments are what make "publishes one branch" true rather
/// than merely intended. `--no-follow-tags` because `push.followTags` would
/// otherwise publish every annotated tag the branch reaches;
/// `--recurse-submodules=no` because `push.recurseSubmodules` would otherwise
/// push commits to other repositories on other remotes; and `--porcelain`
/// because the outcome has to report what Git did rather than what a local ref
/// suggests it did.
fn push_arguments(remote: &str, branch: &str, options: &PushOptions) -> Vec<String> {
    let mut arguments = vec![owned("push"), owned("--progress"), owned("--porcelain")];
    if options.set_upstream {
        arguments.push(owned("--set-upstream"));
    }
    if options.force_with_lease {
        arguments.push(owned("--force-with-lease"));
    }
    arguments.extend([owned("--no-follow-tags"), owned("--recurse-submodules=no")]);
    // The branch is named on both sides, so the destination cannot depend on
    // the user's `push.default`, and the default-branch refusal above therefore
    // decides about the ref this actually writes.
    arguments.extend([owned("--"), remote.to_owned(), branch.to_owned()]);
    arguments
}

/// Runs one network invocation and restates its failure as a typed one.
///
/// Standard output counts as diagnostic here for all three verbs, because for
/// all three the interesting half of a failure lands there: the porcelain
/// rejection of a push, and the `CONFLICT` lines of a merge.
fn run(
    git_executable: &Path,
    root: &Path,
    arguments: &[String],
    cancellation: &Cancellation,
    on_progress: impl FnMut(String),
) -> Result<GitOutput, GitError> {
    GitCommand::new(git_executable, root, GitAccess::Network)
        .args(arguments)
        .diagnose_with_stdout()
        .run_with_progress(cancellation, on_progress)
        .map_err(classify)
}

/// Substrings Git uses when it rejects a push, or refuses to fast-forward.
///
/// Matched once, here, so that a caller matches on a variant instead of on
/// standard error.
/// The push markers are the reasons Git prints in its `--porcelain` report,
/// which is a machine format rather than prose; the pull marker is prose, but
/// prose in a locale the runner pins.
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
/// Reading Git's prose is not the first choice; it is what is left once the
/// alternatives run out. A rejected push is reported in `--porcelain` output
/// that never arrives, because the command failed. So the markers above are
/// matched instead, and they are matched against English on purpose: the
/// runner pins the diagnostic locale precisely so that this recognition works
/// on a machine whose user reads Git in another language.
///
/// Anything unrecognized stays [`GitError::Failed`] with its output intact: a
/// wrong guess would be worse than the diagnostic Git already wrote. Some
/// failures are unrecognizable in principle rather than by omission — a
/// private repository answers an unauthenticated request with "repository not
/// found", which is the same thing it says about a repository that really does
/// not exist, and no amount of matching can tell those apart from here.
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

/// Restates a failed pull as one that has to be recovered from.
///
/// The distinction the caller needs is not why the pull failed but whether the
/// repository still looks the way it did beforehand. A conflicting merge, a
/// rebase that stopped part-way, a cancellation that killed Git between two
/// writes: all of them leave the index and the working tree changed, and all of
/// them arrive here as an ordinary error that a front end would otherwise treat
/// as "nothing happened".
///
/// The original failure is kept as the source, so nothing is lost — a
/// cancellation still reads as a cancellation — and the state the repository
/// was actually left in travels with it.
fn interrupted(root: &Path, arguments: &[String], error: GitError) -> GitError {
    // Deliberately tolerant: this runs on a path that has already failed, and
    // a repository too broken to open is described by the failure it already
    // has rather than replaced by a worse one about the description.
    let pending = Repository::open(root)
        .ok()
        .as_ref()
        .and_then(status::pending);
    let Some(pending) = pending else {
        return error;
    };
    GitError::Interrupted {
        command: arguments.join(" "),
        path: root.to_path_buf(),
        pending,
        status: current_status(root).map(Box::new),
        source: Box::new(error),
    }
}

/// Reads `git push --porcelain` and reports what it did to one branch.
///
/// The format is Git's machine-readable one and stable: `To <url>` first, then
/// one `<flag>\t<from>:<to>\t<summary>` line per ref, then `Done`. Only the
/// line whose destination is this branch is consulted, so a push that somehow
/// carried anything else cannot be mistaken for it.
fn push_report(stdout: &[u8], branch: &str) -> RefUpdate {
    let destination = format!("refs/heads/{branch}");
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| {
            let (flag, rest) = line.split_once('\t')?;
            let (_, updated) = rest.split('\t').next()?.split_once(':')?;
            (updated == destination).then(|| ref_update(flag))
        })
        .next()
        .unwrap_or(RefUpdate::Unknown)
}

/// Reads one porcelain status flag.
///
/// `!` and `-` are absent because they cannot occur here: a rejected ref fails
/// the command, and nothing in Harkness deletes one.
fn ref_update(flag: &str) -> RefUpdate {
    match flag {
        " " => RefUpdate::FastForward,
        "+" => RefUpdate::Forced,
        "*" => RefUpdate::Created,
        "=" => RefUpdate::Unchanged,
        _ => RefUpdate::Unknown,
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

/// Names the branch the remote currently calls its default.
///
/// Asked of the remote, every time, immediately before the push it guards. The
/// obvious cheaper answer is `refs/remotes/<remote>/HEAD`, which `git clone`
/// writes once and an ordinary `git fetch` never revisits — so a project that
/// renamed its default branch after this clone was made would have its new
/// default pushed to while the guardrail was busy protecting the old one. A
/// stale guardrail is worse than an expensive one.
///
/// The recorded ref remains the fallback, for the servers and protocol versions
/// that advertise no symbolic HEAD. When neither answers,
/// [`GitError::DefaultBranchUnknown`] refuses: assuming `main` would wave the
/// push through on exactly the remotes where the guardrail could not be
/// evaluated.
///
/// None of this is atomic, and cannot be. Between this answer and the push, the
/// remote's default branch can change; only branch protection configured on the
/// server closes that window.
fn default_branch(
    git_executable: &Path,
    root: &Path,
    repository: &Repository,
    remote: &str,
    cancellation: &Cancellation,
) -> Result<String, GitError> {
    match advertised_default_branch(git_executable, root, remote, cancellation)? {
        Some(branch) => Ok(branch),
        None => recorded_default_branch(repository, root, remote)?.ok_or_else(|| {
            GitError::DefaultBranchUnknown {
                remote: remote.to_owned(),
            }
        }),
    }
}

/// Asks the remote which branch its HEAD names.
///
/// `ls-remote` is read-only and reaches the remote through the same runner,
/// credentials and cancellation as the push itself, so a remote that cannot be
/// reached fails here as the same typed error it would have failed with a
/// moment later.
fn advertised_default_branch(
    git_executable: &Path,
    root: &Path,
    remote: &str,
    cancellation: &Cancellation,
) -> Result<Option<String>, GitError> {
    let reported = GitCommand::new(git_executable, root, GitAccess::Network)
        .args(["ls-remote", "--symref", "--", remote, "HEAD"])
        .run(cancellation)
        .map_err(classify)?;
    Ok(String::from_utf8_lossy(&reported.stdout)
        .lines()
        .find_map(advertised_head))
}

/// Reads one `ref: refs/heads/<branch>\tHEAD` line of `ls-remote --symref`.
///
/// A HEAD pointing outside `refs/heads/`, or at a branch the remote does not
/// have, is no answer at all and falls through to the recorded ref.
fn advertised_head(line: &str) -> Option<String> {
    let (target, name) = line.strip_prefix("ref: ")?.split_once('\t')?;
    if name.trim() != "HEAD" {
        return None;
    }
    let branch = target.strip_prefix("refs/heads/")?;
    (!branch.is_empty()).then(|| branch.to_owned())
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
        Err(error) if error.code() == ErrorCode::UnbornBranch => head_branch(repository, root),
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
    use std::{fs, path::Path, thread};

    use git2::Repository;

    use super::{
        FetchOptions, PullOptions, PullStrategy, PushOptions, RefUpdate, advertised_head, classify,
        fetch_arguments, pull_arguments, push_arguments, push_report,
    };
    use crate::{
        catalog::entry::UpstreamStatus,
        git::{Cancellation, GitError, GitService, PendingOperation},
        testing::{
            Fixture, PROCESS_PROJECT_ROOT_ENV, PROCESS_READY_FILE_ENV, commit_all, git,
            initialize_repository, remote_with_clone, spawn_child, wait_for_child_signal,
            wait_for_file,
        },
    };

    /// The refusal that has to hold structurally rather than by review: no
    /// combination of options may produce Git's unconditional force, and none
    /// may drop the arguments that keep the push to the one branch it names.
    #[test]
    fn no_push_options_value_produces_a_bare_force_or_a_wider_push() {
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
                    for narrowing in ["--no-follow-tags", "--recurse-submodules=no", "--porcelain"]
                    {
                        assert!(
                            flags.iter().any(|flag| flag == narrowing),
                            "{options:?} dropped {narrowing}: {arguments:?}"
                        );
                    }
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
        // Pruning is stated in both directions, so neither answer is left to
        // whatever the repository happens to be configured with.
        assert_eq!(
            fetch_arguments("origin", &FetchOptions::default()),
            [
                "fetch",
                "--progress",
                "--no-prune",
                "--no-prune-tags",
                "--no-recurse-submodules",
                "--",
                "origin"
            ]
        );
        assert_eq!(
            fetch_arguments(
                "upstream",
                &FetchOptions {
                    remote: Some("upstream".to_owned()),
                    prune: true,
                }
            ),
            [
                "fetch",
                "--progress",
                "--prune",
                "--no-prune-tags",
                "--no-recurse-submodules",
                "--",
                "upstream"
            ]
        );

        let strategy = |strategy| PullOptions {
            remote: None,
            strategy,
        };
        assert_eq!(
            pull_arguments("origin", "main", &strategy(PullStrategy::FastForwardOnly)),
            [
                "pull",
                "--progress",
                "--ff-only",
                "--no-prune",
                "--no-recurse-submodules",
                "--",
                "origin",
                "main"
            ]
        );
        assert_eq!(
            pull_arguments("origin", "main", &strategy(PullStrategy::Merge)),
            [
                "pull",
                "--progress",
                "--no-rebase",
                "--ff",
                "--no-edit",
                "--no-prune",
                "--no-recurse-submodules",
                "--",
                "origin",
                "main"
            ]
        );
        assert_eq!(
            pull_arguments("origin", "main", &strategy(PullStrategy::Rebase)),
            [
                "pull",
                "--progress",
                "--rebase",
                "--no-prune",
                "--no-recurse-submodules",
                "--",
                "origin",
                "main"
            ]
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
            // The porcelain report, which is where a rejection now arrives.
            "!\trefs/heads/main:refs/heads/main\t[rejected] (non-fast-forward)",
            "!\trefs/heads/main:refs/heads/main\t[rejected] (stale info)",
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

        // Anything unrecognized keeps Git's own account of itself. The first is
        // unrecognizable in principle: a private repository answers an
        // unauthenticated request with exactly what a missing one answers.
        for stderr in [
            "remote: Repository not found.\nfatal: repository 'https://github.com/octocat/private.git/' not found",
            "fatal: repository 'https://example.invalid/' not found",
        ] {
            assert!(
                matches!(classify(failed(stderr)), GitError::Failed { .. }),
                "{stderr}"
            );
        }
        assert!(matches!(classify(GitError::Cancelled), GitError::Cancelled));
    }

    /// The parse that decides whether a caller is told the remote changed.
    #[test]
    fn a_push_report_names_what_git_did_to_the_branch() {
        let reported = |flag: &str, refspec: &str, summary: &str| {
            format!("To /remote.git\n{flag}\t{refspec}\t{summary}\nDone\n").into_bytes()
        };
        let pushed = |flag: &str, branch: &str, summary: &str| {
            reported(
                flag,
                &format!("refs/heads/{branch}:refs/heads/{branch}"),
                summary,
            )
        };

        assert_eq!(
            push_report(&pushed("*", "feature", "[new branch]"), "feature"),
            RefUpdate::Created
        );
        assert_eq!(
            push_report(&pushed(" ", "main", "3fa5e37..96052c8"), "main"),
            RefUpdate::FastForward
        );
        assert_eq!(
            push_report(
                &pushed("+", "main", "96052c8...bcdc257 (forced update)"),
                "main"
            ),
            RefUpdate::Forced
        );
        assert_eq!(
            push_report(&pushed("=", "main", "[up to date]"), "main"),
            RefUpdate::Unchanged
        );
        // A name with slashes in it is still one destination, matched whole.
        assert_eq!(
            push_report(
                &pushed(" ", "release/1.x", "a1b2c3d..e4f5a6b"),
                "release/1.x"
            ),
            RefUpdate::FastForward
        );

        // A report about some other ref is no report about this branch, and
        // neither is no report at all. Both are conservative rather than quiet.
        assert_eq!(
            push_report(
                &reported("*", "refs/tags/v1:refs/tags/v1", "[new tag]"),
                "main"
            ),
            RefUpdate::Unknown
        );
        assert_eq!(
            push_report(&pushed(" ", "other", "a1b2c3d..e4f5a6b"), "main"),
            RefUpdate::Unknown
        );
        assert_eq!(push_report(b"", "main"), RefUpdate::Unknown);
        assert_eq!(
            push_report(b"To /remote.git\nDone\n", "main"),
            RefUpdate::Unknown
        );
    }

    /// The parse the default-branch guardrail now rests on.
    #[test]
    fn an_advertised_head_is_read_only_when_it_names_a_branch() {
        assert_eq!(
            advertised_head("ref: refs/heads/main\tHEAD").as_deref(),
            Some("main")
        );
        assert_eq!(
            advertised_head("ref: refs/heads/release/1.x\tHEAD").as_deref(),
            Some("release/1.x")
        );

        for line in [
            // The advertisement of some other symbolic ref.
            "ref: refs/heads/main\trefs/remotes/upstream/HEAD",
            // An ordinary object line, which every `ls-remote` also prints.
            "09cc9789c2b6b922194394a54a19a5f660d0fa48\tHEAD",
            // A HEAD pointing outside the branch namespace, or nowhere at all.
            "ref: refs/pull/1/head\tHEAD",
            "ref: refs/heads/\tHEAD",
            "",
        ] {
            assert_eq!(advertised_head(line), None, "{line}");
        }
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

    /// `prune: false` has to mean "do not prune" rather than "say nothing",
    /// because the two settings that would otherwise answer are exactly the two
    /// a user who likes pruning has set.
    #[test]
    fn pruning_follows_the_option_rather_than_the_configuration() {
        let fixture = Fixture::new();
        let (_, clone) = remote_with_clone(&fixture, "pruning");
        let service = GitService::new(&clone, &fixture.data_dir);
        git(&clone, ["config", "fetch.prune", "true"]);
        git(&clone, ["config", "fetch.pruneTags", "true"]);
        // Overrides `fetch.prune` in Git's own precedence, so pinning only the
        // first would have left this one deciding.
        git(&clone, ["config", "remote.origin.prune", "true"]);
        let stale = |clone: &Path| {
            git(clone, ["update-ref", "refs/remotes/origin/stale", "HEAD"]);
            git(clone, ["tag", "orphan"]);
        };
        let survives = |clone: &Path, name: &str| {
            let refs = git(clone, ["for-each-ref", "--format=%(refname)"]);
            refs.lines().any(|reference| reference.ends_with(name))
        };
        stale(&clone);

        service
            .fetch(&FetchOptions::default(), &Cancellation::default(), |_| {})
            .unwrap();
        assert!(
            survives(&clone, "/stale"),
            "a fetch that was told not to prune pruned anyway"
        );

        // A pull fetches too, and nobody asked that fetch to delete anything.
        service
            .pull(&PullOptions::default(), &Cancellation::default(), |_| {})
            .unwrap();
        assert!(survives(&clone, "/stale"), "a pull pruned a tracking ref");

        service
            .fetch(
                &FetchOptions {
                    remote: None,
                    prune: true,
                },
                &Cancellation::default(),
                |_| {},
            )
            .unwrap();
        assert!(
            !survives(&clone, "/stale"),
            "a fetch that was told to prune kept a stale tracking ref"
        );
        assert!(
            survives(&clone, "/orphan"),
            "pruning branches deleted a tag as well"
        );
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
        assert_eq!(outcome.update, RefUpdate::Created);
        assert!(outcome.updated());
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
        assert_eq!(repeated.update, RefUpdate::Unchanged);
        assert!(!repeated.updated());
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
        let forced = service
            .push(&published(true), &Cancellation::default(), |_| {})
            .unwrap();
        assert_eq!(forced.update, RefUpdate::Forced);
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
            identify(&clone);
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

    /// The failure a front end must not read as "nothing happened": both
    /// reconciling strategies can stop half way, and both leave the working
    /// tree changed and Git waiting when they do.
    #[test]
    fn a_conflicting_pull_reports_the_state_it_left_behind() {
        let fixture = Fixture::new();
        let conflicted = |name: &str, strategy, pending, marker: &str| {
            let (remote, clone) = remote_with_clone(&fixture, name);
            let service = GitService::new(&clone, &fixture.data_dir);
            identify(&clone);
            // The same line of the same file, edited on both sides.
            write_in_remote(&fixture, &remote, "tracked.txt", "theirs\n");
            fs::write(clone.join("tracked.txt"), "ours\n").unwrap();
            commit_all(&Repository::open(&clone).unwrap(), "ours");

            let error = service
                .pull(
                    &PullOptions {
                        remote: None,
                        strategy,
                    },
                    &Cancellation::default(),
                    |_| {},
                )
                .unwrap_err();

            let GitError::Interrupted {
                pending: reported,
                status,
                source,
                ..
            } = &error
            else {
                panic!("{name}: {error:?}");
            };
            assert_eq!(*reported, pending, "{name}");
            assert!(
                status.as_ref().is_some_and(|status| status.dirty),
                "{name}: the interruption carried no changed state: {status:?}"
            );
            assert!(
                matches!(**source, GitError::Failed { .. }),
                "{name}: {source:?}"
            );
            assert!(
                clone.join(".git").join(marker).exists(),
                "{name}: Git left no {marker} to resolve"
            );

            // Nothing else runs against the repository until it is resolved,
            // and the refusal says so rather than looking like a fresh failure.
            let refused = service
                .pull(&PullOptions::default(), &Cancellation::default(), |_| {})
                .unwrap_err();
            assert!(
                matches!(&refused, GitError::OperationInProgress { pending: waiting, .. }
                    if *waiting == pending),
                "{name}: {refused:?}"
            );
            (clone, service)
        };

        let (merged, service) = conflicted(
            "conflicting-merge",
            PullStrategy::Merge,
            PendingOperation::Merge,
            "MERGE_HEAD",
        );
        git(&merged, ["merge", "--abort"]);
        let recovered = service
            .pull(&PullOptions::default(), &Cancellation::default(), |_| {})
            .unwrap_err();
        assert!(
            matches!(recovered, GitError::NonFastForward { .. }),
            "aborting left the repository looking busy: {recovered:?}"
        );

        let (rebased, service) = conflicted(
            "conflicting-rebase",
            PullStrategy::Rebase,
            PendingOperation::Rebase,
            "rebase-merge",
        );
        git(&rebased, ["rebase", "--abort"]);
        let recovered = service
            .pull(&PullOptions::default(), &Cancellation::default(), |_| {})
            .unwrap_err();
        assert!(
            matches!(recovered, GitError::NonFastForward { .. }),
            "aborting left the repository looking busy: {recovered:?}"
        );
    }

    /// Autostash is configuration that silently moves a user's uncommitted work
    /// into a stash nobody was told about, and puts it back only if the reapply
    /// happens to succeed. A pull that cannot run has to say so instead.
    #[test]
    fn a_dirty_working_tree_is_refused_rather_than_quietly_stashed() {
        let fixture = Fixture::new();
        let refused = |name: &str, strategy| {
            let (remote, clone) = remote_with_clone(&fixture, name);
            identify(&clone);
            git(&clone, ["config", "merge.autoStash", "true"]);
            git(&clone, ["config", "rebase.autoStash", "true"]);
            write_in_remote(&fixture, &remote, "tracked.txt", "theirs\n");
            fs::write(clone.join("tracked.txt"), "work in progress\n").unwrap();

            let error = GitService::new(&clone, &fixture.data_dir)
                .pull(
                    &PullOptions {
                        remote: None,
                        strategy,
                    },
                    &Cancellation::default(),
                    |_| {},
                )
                .unwrap_err();

            assert!(
                matches!(error, GitError::Failed { .. }),
                "{name}: {error:?}"
            );
            assert_eq!(
                fs::read_to_string(clone.join("tracked.txt")).unwrap(),
                "work in progress\n",
                "{name}: the uncommitted work did not survive"
            );
            assert!(
                git(&clone, ["stash", "list"]).trim().is_empty(),
                "{name}: the pull stashed the user's work"
            );
        };

        refused("autostashing-merge", PullStrategy::Merge);
        refused("autostashing-rebase", PullStrategy::Rebase);
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

        assert_eq!(allowed.update, RefUpdate::FastForward);
        assert!(allowed.updated());
        assert_ne!(git(&remote, ["rev-parse", "HEAD"]), remote_head);
    }

    /// The guardrail has to protect the branch the remote calls its default
    /// now, not the one it called its default when this clone was made.
    /// `refs/remotes/origin/HEAD` is written once, by `git clone`, and an
    /// ordinary fetch never revisits it.
    #[test]
    fn the_default_branch_a_remote_renamed_to_is_the_one_protected() {
        let fixture = Fixture::new();
        let (remote, clone) = remote_with_clone(&fixture, "renamed");
        let service = GitService::new(&clone, &fixture.data_dir);
        let published =
            |options: PushOptions| service.push(&options, &Cancellation::default(), |_| {});
        git(&clone, ["checkout", "-b", "feature"]);
        fs::write(clone.join("feature.txt"), "feature\n").unwrap();
        commit_all(&Repository::open(&clone).unwrap(), "feature commit");
        published(PushOptions {
            set_upstream: true,
            ..PushOptions::default()
        })
        .expect("a branch that is not the default is publishable");

        // The project renames its default branch, as a hosting service does
        // when a repository switches from `main` to a release branch.
        git(&remote, ["symbolic-ref", "HEAD", "refs/heads/feature"]);
        service
            .fetch(&FetchOptions::default(), &Cancellation::default(), |_| {})
            .unwrap();
        assert_eq!(
            git(&clone, ["symbolic-ref", "refs/remotes/origin/HEAD"]).trim(),
            "refs/remotes/origin/main",
            "the recorded default was refreshed, so this test proves nothing"
        );

        let published_head = git(&remote, ["rev-parse", "refs/heads/feature"]);
        fs::write(clone.join("more.txt"), "more\n").unwrap();
        commit_all(&Repository::open(&clone).unwrap(), "more");

        let refused = published(PushOptions::default()).unwrap_err();

        assert!(
            matches!(&refused, GitError::DefaultBranchPush { remote, branch }
                if remote == "origin" && branch == "feature"),
            "{refused:?}"
        );
        assert_eq!(
            git(&remote, ["rev-parse", "refs/heads/feature"]),
            published_head,
            "the refused push reached the remote"
        );

        // And the branch that used to be the default is no longer protected,
        // which is what makes this the live answer rather than a wider refusal.
        git(&clone, ["checkout", "main"]);
        fs::write(clone.join("main.txt"), "main\n").unwrap();
        commit_all(&Repository::open(&clone).unwrap(), "main commit");
        let outcome = published(PushOptions::default()).expect("main is no longer the default");
        assert_eq!(outcome.update, RefUpdate::FastForward);
    }

    /// A remote whose default branch cannot be determined is its own refusal:
    /// guessing `main` would wave the push through on exactly the repositories
    /// where the guardrail could not be evaluated.
    #[test]
    fn an_undeterminable_default_branch_is_refused_distinctly() {
        let fixture = Fixture::new();
        let (remote, clone) = remote_with_clone(&fixture, "headless");
        let service = GitService::new(&clone, &fixture.data_dir);
        // A HEAD naming a branch the remote does not have is advertised as no
        // symbolic HEAD at all, which is also what a server too old to
        // advertise one looks like from here.
        git(
            &remote,
            ["symbolic-ref", "HEAD", "refs/heads/never-created"],
        );

        // What the clone recorded still answers, so the guardrail still holds.
        let refused = service
            .push(&PushOptions::default(), &Cancellation::default(), |_| {})
            .unwrap_err();
        assert!(
            matches!(&refused, GitError::DefaultBranchPush { branch, .. } if branch == "main"),
            "{refused:?}"
        );

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

    /// The outcome has to describe the remote, and a remote-tracking ref is not
    /// the remote. A repository with no fetch mapping has none to compare, so
    /// the comparison this replaced reported every publication as a no-op.
    #[test]
    fn a_push_outcome_survives_a_repository_with_no_tracking_ref() {
        let fixture = Fixture::new();
        let (remote, clone) = remote_with_clone(&fixture, "untracked");
        let service = GitService::new(&clone, &fixture.data_dir);
        let published = PushOptions {
            allow_default_branch: true,
            ..PushOptions::default()
        };
        git(
            &clone,
            ["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"],
        );
        git(&clone, ["config", "--unset", "remote.origin.fetch"]);
        git(&clone, ["update-ref", "-d", "refs/remotes/origin/main"]);
        fs::write(clone.join("local.txt"), "local\n").unwrap();
        commit_all(&Repository::open(&clone).unwrap(), "local commit");

        let outcome = service
            .push(&published, &Cancellation::default(), |_| {})
            .unwrap();

        assert_eq!(outcome.update, RefUpdate::FastForward);
        assert!(outcome.updated());
        assert_eq!(
            git(&remote, ["rev-parse", "HEAD"]),
            git(&clone, ["rev-parse", "HEAD"]),
            "the remote did not receive the commit this reported publishing"
        );
        assert!(
            Repository::open(&clone)
                .unwrap()
                .find_reference("refs/remotes/origin/main")
                .is_err(),
            "a tracking ref reappeared, so the comparison would have worked and \
             this test proves nothing"
        );

        // And an unchanged remote is still reported as unchanged, so the
        // conservative answer is not simply always "something happened".
        let repeated = service
            .push(&published, &Cancellation::default(), |_| {})
            .unwrap();
        assert_eq!(repeated.update, RefUpdate::Unchanged);
        assert!(!repeated.updated());
    }

    /// Branch names are a namespace, not an identifier: the report has to be
    /// attributed by the whole destination ref.
    #[test]
    fn a_branch_in_a_nested_namespace_is_published_and_attributed() {
        let fixture = Fixture::new();
        let (remote, clone) = remote_with_clone(&fixture, "namespaced");
        let service = GitService::new(&clone, &fixture.data_dir);
        git(&clone, ["checkout", "-b", "release/1.x/backport"]);
        fs::write(clone.join("backport.txt"), "backport\n").unwrap();
        commit_all(&Repository::open(&clone).unwrap(), "backport");

        let outcome = service
            .push(
                &PushOptions {
                    set_upstream: true,
                    ..PushOptions::default()
                },
                &Cancellation::default(),
                |_| {},
            )
            .unwrap();

        assert_eq!(outcome.branch, "release/1.x/backport");
        assert_eq!(outcome.update, RefUpdate::Created);
        assert!(
            Repository::open(&remote)
                .unwrap()
                .find_reference("refs/heads/release/1.x/backport")
                .is_ok()
        );
    }

    /// "Publishes one branch" has to survive the configuration of a user who
    /// likes their tags and submodules to travel with a push. Both settings are
    /// ordinary, both widen what reaches the remote, and neither is in
    /// `PushOptions`.
    #[test]
    fn a_branch_push_carries_nothing_a_configuration_would_have_added() {
        let fixture = Fixture::new();
        let (remote, clone) = remote_with_clone(&fixture, "widening");
        let service = GitService::new(&clone, &fixture.data_dir);
        identify(&clone);
        git(&clone, ["config", "push.followTags", "true"]);
        git(&clone, ["config", "push.recurseSubmodules", "on-demand"]);
        git(&clone, ["config", "push.default", "matching"]);
        git(&clone, ["checkout", "-b", "feature"]);
        fs::write(clone.join("feature.txt"), "feature\n").unwrap();
        commit_all(&Repository::open(&clone).unwrap(), "feature commit");
        git(&clone, ["tag", "--annotate", "v1", "--message", "release"]);

        let outcome = service
            .push(
                &PushOptions {
                    set_upstream: true,
                    ..PushOptions::default()
                },
                &Cancellation::default(),
                |_| {},
            )
            .unwrap();

        assert_eq!(outcome.update, RefUpdate::Created);
        assert_eq!(
            git(&remote, ["for-each-ref", "--format=%(refname)"])
                .lines()
                .collect::<Vec<_>>(),
            ["refs/heads/feature", "refs/heads/main"],
            "the push published a ref nobody asked it to"
        );
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

    /// `.` is what Git writes in `branch.<name>.remote` for a branch built on
    /// another local branch. Pulling one is ordinary; pushing one is not a
    /// thing, and saying so beats claiming the remote is misconfigured.
    #[test]
    fn a_branch_tracking_the_repository_itself_pulls_but_does_not_push() {
        let fixture = Fixture::new();
        let root = fixture.directory("local-upstream");
        initialize_repository(&root);
        let service = GitService::new(&root, &fixture.data_dir);
        git(&root, ["checkout", "-b", "feature"]);
        git(&root, ["checkout", "main"]);
        fs::write(root.join("later.txt"), "later\n").unwrap();
        commit_all(&Repository::open(&root).unwrap(), "later");
        git(&root, ["checkout", "feature"]);
        git(&root, ["config", "branch.feature.remote", "."]);
        git(&root, ["config", "branch.feature.merge", "refs/heads/main"]);

        let outcome = service
            .pull(&PullOptions::default(), &Cancellation::default(), |_| {})
            .unwrap();

        assert_eq!(outcome.remote, ".");
        assert!(outcome.updated);
        assert!(root.join("later.txt").exists());

        let refused = service
            .push(&PushOptions::default(), &Cancellation::default(), |_| {})
            .unwrap_err();

        assert!(
            matches!(&refused, GitError::LocalUpstreamUnsupported { branch } if branch == "feature"),
            "{refused:?}"
        );
    }

    #[test]
    fn an_unborn_branch_has_no_commits_to_push() {
        let fixture = Fixture::new();
        let root = fixture.directory("unborn");
        Repository::init(&root)
            .unwrap()
            .set_head("refs/heads/main")
            .unwrap();

        let refused = GitService::new(&root, &fixture.data_dir)
            .push(
                &PushOptions {
                    set_upstream: true,
                    allow_default_branch: true,
                    ..PushOptions::default()
                },
                &Cancellation::default(),
                |_| {},
            )
            .unwrap_err();

        assert!(
            matches!(&refused, GitError::UnbornBranch { branch, .. } if branch == "main"),
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

    /// Cancellation has to reach Git while Git is the thing taking the time.
    /// Checking the token before spawning would pass this only for a caller who
    /// had already given up before it asked.
    #[cfg(unix)]
    #[test]
    fn cancelling_stops_each_synchronizing_verb_while_git_runs() {
        let fixture = Fixture::new();
        let (_, clone) = remote_with_clone(&fixture, "cancelled");
        let running = fixture.root.path().join("git-is-running");
        let hanging_git = fixture.shim(
            "hanging-git",
            &format!(
                "#!/bin/sh\n\
                 echo running > '{}'\n\
                 while true; do sleep 0.05; done\n",
                running.display()
            ),
        );
        let service = GitService::new(&clone, &fixture.data_dir).with_git_executable(hanging_git);
        let cancelled = |attempt: &(dyn Fn(&Cancellation) -> Result<(), GitError> + Sync)| {
            let _ = fs::remove_file(&running);
            let cancellation = Cancellation::default();
            thread::scope(|scope| {
                let running_verb = scope.spawn(|| attempt(&cancellation));
                wait_for_file(&running);
                cancellation.cancel();
                running_verb.join().unwrap().unwrap_err()
            })
        };

        for error in [
            cancelled(&|cancellation| {
                service
                    .fetch(&FetchOptions::default(), cancellation, |_| {})
                    .map(|_| ())
            }),
            cancelled(&|cancellation| {
                service
                    .pull(&PullOptions::default(), cancellation, |_| {})
                    .map(|_| ())
            }),
            cancelled(&|cancellation| {
                service
                    .push(
                        &PushOptions {
                            allow_default_branch: true,
                            ..PushOptions::default()
                        },
                        cancellation,
                        |_| {},
                    )
                    .map(|_| ())
            }),
        ] {
            assert!(matches!(error, GitError::Cancelled), "{error:?}");
        }
    }

    /// The hermetic policy, asserted where it is actually applied rather than
    /// where it is written. Git opens `/dev/tty` for credentials even with
    /// stdin closed, so a front end with no terminal would hang forever without
    /// the first of these; the rest are what keep a typed option's promise from
    /// depending on a configuration file. Exit statuses in the nineties are
    /// ones no Git verb produces, so a failure here is unambiguous.
    #[cfg(unix)]
    #[test]
    fn every_network_invocation_carries_the_hermetic_policy() {
        let fixture = Fixture::new();
        let (_, clone) = remote_with_clone(&fixture, "policed");
        let invoked = fixture.root.path().join("policed-invocations");
        let asserting_git = fixture.shim(
            "policy-asserting-git",
            &format!(
                "#!/bin/sh\n\
                 test \"$GIT_TERMINAL_PROMPT\" = 0 || exit 97\n\
                 test \"$LC_ALL\" = C || exit 96\n\
                 test \"$GIT_EDITOR\" = harkness-has-no-editor || exit 95\n\
                 test \"$GIT_SEQUENCE_EDITOR\" = harkness-has-no-editor || exit 94\n\
                 echo \"$*\" >> '{}'\n",
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

        /// The first argument Git would read as a verb, past the policy.
        fn verb(invocation: &str) -> &str {
            let mut arguments = invocation.split_whitespace();
            while let Some(argument) = arguments.next() {
                match argument {
                    "-c" => {
                        arguments.next();
                    }
                    argument if argument.starts_with('-') => {}
                    argument => return argument,
                }
            }
            ""
        }

        let invocations = fs::read_to_string(&invoked).unwrap();
        for invocation in invocations.lines() {
            for pinned in [
                "--no-pager",
                "-c merge.autoStash=false",
                "-c rebase.autoStash=false",
                "-c submodule.recurse=false",
            ] {
                assert!(
                    invocation.contains(pinned),
                    "'{pinned}' is missing from '{invocation}'"
                );
            }
        }
        // Without this the shim's exit statuses could never fire, and the test
        // would pass by never having run anything.
        assert_eq!(
            invocations.lines().map(verb).collect::<Vec<_>>(),
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
    ///
    /// Nothing is asserted about what the remote contains. It is a public
    /// repository anyone can commit to through a pull request, and a test about
    /// credentials that fails because someone landed a commit is a test about
    /// nothing. What is asserted is that the transport worked and that the
    /// repository it left behind is internally consistent.
    #[test]
    #[ignore = "requires network access and credentials for a public GitHub repository"]
    fn github_fetch_and_pull_round_trip() {
        let fixture = Fixture::new();
        let clone = github_clone(&fixture, "hello-world");
        let service = GitService::new(&clone, &fixture.data_dir);

        let fetched = service
            .fetch(&FetchOptions::default(), &Cancellation::default(), |_| {})
            .unwrap();
        let pulled = service
            .pull(&PullOptions::default(), &Cancellation::default(), |_| {})
            .unwrap();

        assert_eq!(fetched.remote, "origin");
        assert_eq!(pulled.remote, "origin");
        assert_eq!(pulled.strategy, PullStrategy::FastForwardOnly);
        assert_eq!(
            pulled.status.as_ref().unwrap().branch.as_deref(),
            Some(pulled.branch.as_str())
        );
        assert_eq!(
            divergence(&pulled.status.unwrap()),
            Some((0, 0)),
            "a branch that has just been fast-forwarded is level with its upstream"
        );
    }

    /// The default-branch guardrail, against a remote that really answers.
    ///
    /// Reaches the network and writes nothing: the refusal is decided from the
    /// remote's own advertisement of its HEAD, and the push it refuses never
    /// runs. Nothing else proves that `ls-remote --symref` answers the same way
    /// from a hosting service as it does from the bare directories every other
    /// test here uses, and the guardrail is now built on that answer.
    #[test]
    #[ignore = "requires network access and credentials for a public GitHub repository"]
    fn github_default_branch_refusal_asks_the_remote() {
        let fixture = Fixture::new();
        let clone = github_clone(&fixture, "protected-hello-world");
        let branch = git(&clone, ["rev-parse", "--abbrev-ref", "HEAD"])
            .trim()
            .to_owned();

        let refused = GitService::new(&clone, &fixture.data_dir)
            .push(&PushOptions::default(), &Cancellation::default(), |_| {})
            .unwrap_err();

        assert!(
            matches!(&refused, GitError::DefaultBranchPush { remote, branch: refused_branch }
                if remote == "origin" && *refused_branch == branch),
            "{refused:?}"
        );
    }

    /// Clones the public repository the network tests share.
    #[cfg(test)]
    fn github_clone(fixture: &Fixture, name: &str) -> std::path::PathBuf {
        let clone = fixture.root.path().join(name);
        git(
            fixture.root.path(),
            [
                "clone",
                "--",
                "git@github.com:octocat/Hello-World.git",
                clone.to_str().unwrap(),
            ],
        );
        clone
    }

    /// Gives a clone an identity, for the fixtures that commit through Git
    /// rather than through libgit2. A machine running the suite need not have
    /// one configured.
    fn identify(clone: &Path) {
        git(clone, ["config", "user.name", "Harkness Tests"]);
        git(clone, ["config", "user.email", "tests@harkness.invalid"]);
    }

    /// Commits a file in the bare remote, through a throwaway clone of it.
    fn commit_in_remote(fixture: &Fixture, remote: &Path, file: &str) {
        write_in_remote(fixture, remote, file, "from the remote\n");
    }

    /// Commits `contents` at `file` in the bare remote, replacing what is there.
    fn write_in_remote(fixture: &Fixture, remote: &Path, file: &str, contents: &str) {
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
        fs::write(contributor.join(file), contents).unwrap();
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
