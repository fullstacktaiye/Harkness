//! The canonical Git remote identity used to deduplicate managed clones.

use std::fs;

use crate::project::ProjectError;

/// Normalizes a remote into the canonical identity used for deduplication.
///
/// Accepts GitHub HTTPS/SSH/SCP-style remotes. This is the same validation
/// [`ProjectService::import_repository`] applies in production, exposed so
/// front ends can validate a form before starting a clone.
///
/// [`ProjectService::import_repository`]: crate::ProjectService::import_repository
pub fn normalize_remote(remote: &str) -> Result<String, ProjectError> {
    normalize_remote_with_local(remote, false)
}

pub(crate) fn normalize_remote_with_local(
    remote: &str,
    allow_local: bool,
) -> Result<String, ProjectError> {
    let remote = remote.trim();
    let invalid = || ProjectError::InvalidRemote {
        remote: remote.to_owned(),
    };
    let local = |path: &str| {
        fs::canonicalize(path)
            .map(|path| format!("file://{}", path.display()))
            .map_err(|_| invalid())
    };

    let Some(path) = remote
        .strip_prefix("https://github.com/")
        .or_else(|| remote.strip_prefix("http://github.com/"))
        .or_else(|| remote.strip_prefix("ssh://git@github.com/"))
        .or_else(|| remote.strip_prefix("git@github.com:"))
    else {
        return if allow_local {
            local(remote.strip_prefix("file://").unwrap_or(remote))
        } else {
            Err(invalid())
        };
    };

    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    let (Some(owner), Some(repository)) = (
        parts.next().filter(|part| !part.is_empty()),
        parts.next().filter(|part| !part.is_empty()),
    ) else {
        return Err(invalid());
    };
    if parts.next().is_some() {
        return Err(invalid());
    }
    Ok(format!(
        "github.com/{}/{}",
        owner.to_ascii_lowercase(),
        repository.to_ascii_lowercase()
    ))
}

pub(crate) fn repository_name(normalized_remote: &str) -> String {
    normalized_remote
        .rsplit('/')
        .next()
        .unwrap_or(normalized_remote)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use crate::project::ProjectError;

    #[test]
    fn github_https_and_ssh_remotes_share_a_normalized_identity() {
        let expected = "github.com/example/project";
        assert_eq!(
            super::normalize_remote("https://github.com/Example/Project.git").unwrap(),
            expected
        );
        assert_eq!(
            super::normalize_remote("git@github.com:example/project.git").unwrap(),
            expected
        );
        assert_eq!(
            super::normalize_remote("ssh://git@github.com/EXAMPLE/PROJECT/").unwrap(),
            expected
        );
    }

    #[test]
    fn malformed_github_remotes_are_rejected() {
        for remote in [
            "",
            "https://github.com/",
            "https://github.com/only-owner",
            "https://github.com/owner/repository/extra",
            "https://gitlab.com/owner/repository",
            "not a remote at all //",
        ] {
            assert!(
                matches!(
                    super::normalize_remote(remote),
                    Err(ProjectError::InvalidRemote { .. })
                ),
                "expected '{remote}' to be rejected"
            );
        }
    }
}
