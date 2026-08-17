//! The durable project catalog file and its schema.

pub(crate) mod entry;
pub(crate) mod lock;

use std::{
    collections::HashMap,
    fs,
    io::{self, Write},
    path::Path,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tempfile::NamedTempFile;

use crate::{
    catalog::entry::{Project, ProjectSource},
    editor::EditorConfiguration,
    project::ProjectError,
};

/// The newest catalog schema understood by this Harkness build.
pub(crate) const CATALOG_VERSION: u32 = 4;
/// The first schema that can represent managed worktrees.
pub(crate) const WORKTREE_CATALOG_VERSION: u32 = 2;
/// The first schema that can represent global editor configuration.
pub(crate) const EDITOR_CATALOG_VERSION: u32 = 3;
/// The first schema that can represent explicit project checks.
pub(crate) const CHECK_CATALOG_VERSION: u32 = 4;
/// The oldest catalog schema this build can load without losing data.
pub(crate) const MINIMUM_SUPPORTED_CATALOG_VERSION: u32 = 1;

#[derive(Clone, Debug, Default)]
pub(crate) struct Catalog {
    pub(crate) projects: Vec<Project>,
    pub(crate) editor: Option<EditorConfiguration>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogWire {
    version: u32,
    projects: Vec<Project>,
    #[serde(default)]
    editor: Option<EditorConfiguration>,
}

/// The strict body shared by the released v1 and v2 schemas.
///
/// Keeping this separate from [`CatalogWire`] prevents a new build from
/// accepting v3-only data under an older version number.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyCatalogWire {
    version: u32,
    projects: Vec<Project>,
}

/// The forward-compatible prefix of every catalog file.
#[derive(Deserialize)]
struct CatalogVersion {
    version: u32,
}

/// Reads a catalog file, treating a missing file as an empty catalog.
pub(crate) fn read_catalog(catalog_path: &Path) -> Result<Catalog, ProjectError> {
    match catalog_path.try_exists() {
        Ok(false) => Ok(Catalog::default()),
        Ok(true) => {
            let bytes = fs::read(catalog_path).map_err(|source| ProjectError::CatalogRead {
                path: catalog_path.to_path_buf(),
                source,
            })?;
            // Read the version before the body: a future schema would fail to
            // deserialize as a v1 catalog, and reporting that as "malformed"
            // would hide the one cause the user can act on.
            let probe: CatalogVersion = serde_json::from_slice(&bytes).map_err(|source| {
                ProjectError::MalformedCatalog {
                    path: catalog_path.to_path_buf(),
                    source,
                }
            })?;
            if probe.version < MINIMUM_SUPPORTED_CATALOG_VERSION {
                return Err(ProjectError::CatalogVersionTooOld {
                    found: probe.version,
                    minimum: MINIMUM_SUPPORTED_CATALOG_VERSION,
                });
            }
            if probe.version > CATALOG_VERSION {
                return Err(ProjectError::CatalogVersionTooNew {
                    found: probe.version,
                    maximum: CATALOG_VERSION,
                });
            }

            let mut body: Value = serde_json::from_slice(&bytes).map_err(|source| {
                ProjectError::MalformedCatalog {
                    path: catalog_path.to_path_buf(),
                    source,
                }
            })?;
            if probe.version == 1 {
                normalize_legacy_managed_rows(&mut body);
            }
            if probe.version < CHECK_CATALOG_VERSION {
                reject_checks_before_v4(catalog_path, &body)?;
            }
            let catalog = if probe.version < EDITOR_CATALOG_VERSION {
                let wire: LegacyCatalogWire = serde_json::from_value(body).map_err(|source| {
                    ProjectError::MalformedCatalog {
                        path: catalog_path.to_path_buf(),
                        source,
                    }
                })?;
                debug_assert_eq!(wire.version, probe.version);
                Catalog {
                    projects: wire.projects,
                    editor: None,
                }
            } else {
                let wire: CatalogWire = serde_json::from_value(body).map_err(|source| {
                    ProjectError::MalformedCatalog {
                        path: catalog_path.to_path_buf(),
                        source,
                    }
                })?;
                debug_assert_eq!(wire.version, probe.version);
                Catalog {
                    projects: wire.projects,
                    editor: wire.editor,
                }
            };
            validate_catalog(catalog_path, &catalog)?;
            Ok(catalog)
        }
        Err(source) => Err(ProjectError::CatalogRead {
            path: catalog_path.to_path_buf(),
            source,
        }),
    }
}

fn reject_checks_before_v4(catalog_path: &Path, body: &Value) -> Result<(), ProjectError> {
    let carries_checks = body
        .get("projects")
        .and_then(Value::as_array)
        .is_some_and(|projects| {
            projects.iter().any(|project| {
                // An explicit null is the absence of checks, not a v4 field: it
                // is what `#[serde(default)] Option` reads as `None`, which is
                // the state this version is allowed to hold. An empty array is
                // not the same thing — that is an explicit "run nothing",
                // distinguishable from the built-in defaults, and it does
                // require v4.
                project
                    .get("checks")
                    .is_some_and(|checks| !checks.is_null())
            })
        });
    if carries_checks {
        return Err(invalid_catalog(
            catalog_path,
            "project checks require catalog version 4".to_owned(),
        ));
    }
    Ok(())
}

/// Before source-specific Rust types existed, a v1 managed row could omit its
/// optional remote. Such a row was never safely deletable as managed storage.
/// Load it as a local project so the rest of the user's catalog remains
/// available and the same refusal is preserved. Version 2 keeps rejecting the
/// shape: a current writer may not silently lose same-version data.
fn normalize_legacy_managed_rows(body: &mut Value) {
    let Some(projects) = body.get_mut("projects").and_then(Value::as_array_mut) else {
        return;
    };
    for project in projects {
        let Some(project) = project.as_object_mut() else {
            continue;
        };
        if project.get("source").and_then(Value::as_str) == Some("managed_repository")
            && project.get("remote").is_none()
        {
            project.insert("source".to_owned(), Value::String("local".to_owned()));
        }
    }
}

fn validate_catalog(catalog_path: &Path, catalog: &Catalog) -> Result<(), ProjectError> {
    let mut entries = HashMap::with_capacity(catalog.projects.len());
    for project in &catalog.projects {
        if entries.insert(project.id, project).is_some() {
            return Err(invalid_catalog(
                catalog_path,
                format!("project identifier {} appears more than once", project.id),
            ));
        }
        match &project.source {
            ProjectSource::Local => {}
            ProjectSource::ManagedRepository { remote } if remote.trim().is_empty() => {
                return Err(invalid_catalog(
                    catalog_path,
                    format!("managed project {} has an empty remote", project.id),
                ));
            }
            ProjectSource::ManagedRepository { .. } => {}
            ProjectSource::Worktree {
                worktree_branch, ..
            } if worktree_branch
                .as_deref()
                .is_some_and(|branch| branch.trim().is_empty()) =>
            {
                return Err(invalid_catalog(
                    catalog_path,
                    format!("worktree {} has an empty branch", project.id),
                ));
            }
            ProjectSource::Worktree { .. } => {}
        }
    }

    for project in &catalog.projects {
        let ProjectSource::Worktree { parent, .. } = &project.source else {
            continue;
        };
        let Some(parent_entry) = entries.get(parent) else {
            return Err(invalid_catalog(
                catalog_path,
                format!("worktree {} refers to missing parent {parent}", project.id),
            ));
        };
        if matches!(parent_entry.source, ProjectSource::Worktree { .. }) {
            return Err(invalid_catalog(
                catalog_path,
                format!(
                    "worktree {} names worktree {parent} as its parent",
                    project.id
                ),
            ));
        }
    }
    Ok(())
}

fn invalid_catalog(catalog_path: &Path, reason: String) -> ProjectError {
    ProjectError::InvalidCatalog {
        path: catalog_path.to_path_buf(),
        reason,
    }
}

#[derive(Serialize)]
struct PersistedCatalog<'a> {
    version: u32,
    projects: &'a [Project],
    #[serde(skip_serializing_if = "Option::is_none")]
    editor: &'a Option<EditorConfiguration>,
}

pub(crate) fn persist_catalog(
    data_dir: &Path,
    catalog_path: &Path,
    catalog: &Catalog,
) -> io::Result<()> {
    fs::create_dir_all(data_dir)?;
    let mut temporary = NamedTempFile::new_in(data_dir)?;
    // Persist the oldest schema that can represent every field: explicit
    // checks require v4, editor configuration requires v3, a worktree requires
    // v2, and an ordinary project remains v1-compatible. Removing the last
    // newer field naturally restores the corresponding downgrade path.
    let version = if catalog
        .projects
        .iter()
        .any(|project| project.checks.is_some())
    {
        CHECK_CATALOG_VERSION
    } else if catalog.editor.is_some() {
        EDITOR_CATALOG_VERSION
    } else if catalog
        .projects
        .iter()
        .any(|project| matches!(project.source, ProjectSource::Worktree { .. }))
    {
        WORKTREE_CATALOG_VERSION
    } else {
        MINIMUM_SUPPORTED_CATALOG_VERSION
    };
    let persisted = PersistedCatalog {
        version,
        projects: &catalog.projects,
        editor: &catalog.editor,
    };
    serde_json::to_writer_pretty(&mut temporary, &persisted).map_err(io::Error::other)?;
    temporary.write_all(b"\n")?;
    temporary.as_file_mut().sync_all()?;
    temporary
        .persist(catalog_path)
        .map_err(|error| error.error)?;

    // The file's contents are already durable; what the rename still needs is a
    // sync of the directory holding the new entry. Windows has no equivalent
    // handle to sync, so this is a Unix-only step.
    #[cfg(unix)]
    fs::File::open(data_dir)?.sync_all()?;

    Ok(())
}
