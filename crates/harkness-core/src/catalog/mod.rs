//! The durable project catalog file and its schema.

pub(crate) mod entry;
pub(crate) mod lock;

use std::{
    fs,
    io::{self, Write},
    path::Path,
};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::{catalog::entry::Project, project::ProjectError};

/// The catalog schema version written by this Harkness build.
pub const CATALOG_VERSION: u32 = 2;
/// The oldest catalog schema this build can load without losing data.
pub(crate) const MINIMUM_SUPPORTED_CATALOG_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Catalog {
    pub(crate) version: u32,
    pub(crate) projects: Vec<Project>,
}

/// The forward-compatible prefix of every catalog file.
#[derive(Deserialize)]
struct CatalogVersion {
    version: u32,
}

impl Default for Catalog {
    fn default() -> Self {
        Self {
            version: CATALOG_VERSION,
            projects: Vec::new(),
        }
    }
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
            if !(MINIMUM_SUPPORTED_CATALOG_VERSION..=CATALOG_VERSION).contains(&probe.version) {
                return Err(ProjectError::UnsupportedCatalogVersion {
                    found: probe.version,
                    expected: CATALOG_VERSION,
                });
            }

            let mut catalog: Catalog = serde_json::from_slice(&bytes).map_err(|source| {
                ProjectError::MalformedCatalog {
                    path: catalog_path.to_path_buf(),
                    source,
                }
            })?;
            // The v1-to-v2 migration is additive: the new optional fields
            // deserialize to `None`. Normalize only the in-memory version so a
            // read-only client leaves the older file untouched.
            catalog.version = CATALOG_VERSION;
            Ok(catalog)
        }
        Err(source) => Err(ProjectError::CatalogRead {
            path: catalog_path.to_path_buf(),
            source,
        }),
    }
}

pub(crate) fn persist_catalog(
    data_dir: &Path,
    catalog_path: &Path,
    catalog: &Catalog,
) -> io::Result<()> {
    fs::create_dir_all(data_dir)?;
    let mut temporary = NamedTempFile::new_in(data_dir)?;
    serde_json::to_writer_pretty(&mut temporary, catalog).map_err(io::Error::other)?;
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
