//! Descriptor-held filesystem reads for observation tools.

use std::ffi::OsString;
#[cfg(not(unix))]
use std::fs;
use std::fs::{File, Metadata};
use std::path::{Component, Path, PathBuf};

use harkness_core::{compare_directory_entries, directory_entry_is_visible};
#[cfg(unix)]
use std::ffi::{CStr, CString, OsStr};
#[cfg(unix)]
use std::mem::MaybeUninit;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(windows)]
use std::os::windows::ffi::OsStringExt;
#[cfg(windows)]
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

use crate::tool::{ExecutionContext, ToolError};
use crate::trust::ContainedPath;

/// One exact directory entry retained by a bounded descriptor walk.
pub(super) struct SafeDirectoryEntry {
    pub name: OsString,
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_file: bool,
    pub is_symlink: bool,
}

/// A bounded direct-child listing.
pub(super) struct SafeDirectoryListing {
    pub entries: Vec<SafeDirectoryEntry>,
    pub truncated: bool,
}

/// Opens and verifies one regular file, returning metadata from the held file.
pub(super) fn open_regular(path: &ContainedPath) -> Result<(File, Metadata), ToolError> {
    let fresh = path.revalidate().map_err(ToolError::from)?;
    open_regular_platform(fresh.as_path(), path)
}

#[cfg(all(test, unix))]
fn open_regular_after_revalidation(
    path: &ContainedPath,
    after_revalidation: impl FnOnce(),
) -> Result<(File, Metadata), ToolError> {
    let fresh = path.revalidate().map_err(ToolError::from)?;
    after_revalidation();
    open_regular_platform(fresh.as_path(), path)
}

/// Lists at most `maximum` children from a held directory descriptor.
pub(super) fn list_directory(
    directory: &ContainedPath,
    maximum: usize,
    context: &ExecutionContext,
) -> Result<SafeDirectoryListing, ToolError> {
    let fresh = directory.revalidate().map_err(ToolError::from)?;
    list_directory_platform(fresh.as_path(), maximum, context)
}

/// Refuses a lexical path containing a symlink in any existing component.
pub(super) fn ensure_no_symlink_components(path: &Path) -> Result<(), ToolError> {
    ensure_no_symlink_components_platform(path)
}

#[cfg(unix)]
fn ensure_no_symlink_components_platform(path: &Path) -> Result<(), ToolError> {
    if !path.is_absolute() {
        return Err(ToolError::ForbiddenPath {
            path: path.to_path_buf(),
            reason: "symlink-safe traversal requires an absolute path".to_owned(),
        });
    }
    let components = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut directory = open_directory_absolute(Path::new("/"))?;
    for (index, name) in components.iter().enumerate() {
        let name_c = c_component(name)?;
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        let inspected = unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                name_c.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if inspected != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(());
            }
            return Err(open_error(path, error));
        }
        let stat = unsafe { stat.assume_init() };
        if stat.st_mode & libc::S_IFMT == libc::S_IFLNK {
            return Err(ToolError::ForbiddenPath {
                path: path.to_path_buf(),
                reason: "workspace search does not follow symlinks".to_owned(),
            });
        }
        if index + 1 < components.len() {
            let next = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name_c.as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                )
            };
            if next < 0 {
                return Err(open_error(path, std::io::Error::last_os_error()));
            }
            directory = unsafe { File::from_raw_fd(next) };
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_no_symlink_components_platform(path: &Path) -> Result<(), ToolError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        // A prefix and a root are not filesystem entries and cannot be links.
        // Asking about one is not merely redundant: a contained path is
        // canonical, so on Windows the first component is the verbatim prefix
        // `\\?\C:`, which names the *volume device*. Opening that succeeds and
        // then querying its attributes fails with ERROR_INVALID_FUNCTION, so
        // every search would refuse itself before reading a single entry.
        // `PathBoundary::escaping_symlink` skips them for the same reason.
        if matches!(component, Component::Prefix(_) | Component::RootDir) {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ToolError::ForbiddenPath {
                    path: path.to_path_buf(),
                    reason: "workspace search does not follow symlinks".to_owned(),
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(ToolError::execution_failed(error)),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn open_regular_platform(
    absolute: &Path,
    capability: &ContainedPath,
) -> Result<(File, Metadata), ToolError> {
    let (parent, name) = open_parent(absolute)?;
    let name = c_component(name)?;
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        return Err(open_error(absolute, std::io::Error::last_os_error()));
    }
    let file = unsafe { File::from_raw_fd(descriptor) };
    let metadata = file.metadata().map_err(ToolError::execution_failed)?;
    if !metadata.file_type().is_file() {
        return Err(ToolError::execution_failed(format!(
            "{} is not a regular file",
            absolute.display()
        )));
    }
    // The descriptor traversal is authoritative; this final check also keeps
    // the ContainedPath capability live at the operation boundary.
    capability.revalidate().map_err(ToolError::from)?;
    Ok((file, metadata))
}

#[cfg(unix)]
fn list_directory_platform(
    absolute: &Path,
    maximum: usize,
    context: &ExecutionContext,
) -> Result<SafeDirectoryListing, ToolError> {
    let directory = open_directory_absolute(absolute)?;
    let duplicate = unsafe { libc::dup(directory.as_raw_fd()) };
    if duplicate < 0 {
        return Err(ToolError::execution_failed(std::io::Error::last_os_error()));
    }
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        unsafe { libc::close(duplicate) };
        return Err(ToolError::execution_failed(std::io::Error::last_os_error()));
    }
    let mut entries = Vec::with_capacity(maximum);
    let mut truncated = false;
    let result = (|| {
        loop {
            context.check_still_permitted()?;
            // `readdir` returns NULL both at end of stream and on failure, and
            // only `errno` distinguishes them — which means clearing `errno`
            // first, and its location is `__errno_location` on glibc and
            // `__error` on Darwin. Telling an `EIO` part-way through a
            // directory from a genuine end of stream is worth having, but not
            // at the cost of hand-rolled per-platform `unsafe` in the middle of
            // the containment path. Tracked rather than guessed at.
            let raw = unsafe { libc::readdir(stream) };
            if raw.is_null() {
                break;
            }
            let name = unsafe { CStr::from_ptr((*raw).d_name.as_ptr()) }.to_bytes();
            if matches!(name, b"." | b"..") || !directory_entry_is_visible(OsStr::from_bytes(name))
            {
                continue;
            }
            if entries.len() == maximum {
                truncated = true;
                break;
            }
            let name_c = CString::new(name).map_err(ToolError::execution_failed)?;
            let mut stat = MaybeUninit::<libc::stat>::uninit();
            let inspected = unsafe {
                libc::fstatat(
                    directory.as_raw_fd(),
                    name_c.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if inspected != 0 {
                return Err(ToolError::execution_failed(std::io::Error::last_os_error()));
            }
            let stat = unsafe { stat.assume_init() };
            let kind = stat.st_mode & libc::S_IFMT;
            let name = OsString::from_vec(name.to_vec());
            entries.push(SafeDirectoryEntry {
                path: absolute.join(&name),
                name,
                is_dir: kind == libc::S_IFDIR,
                is_file: kind == libc::S_IFREG,
                is_symlink: kind == libc::S_IFLNK,
            });
        }
        Ok(())
    })();
    unsafe { libc::closedir(stream) };
    result?;
    sort_entries(&mut entries);
    Ok(SafeDirectoryListing { entries, truncated })
}

#[cfg(unix)]
fn open_parent(absolute: &Path) -> Result<(File, &OsStr), ToolError> {
    let name = absolute
        .file_name()
        .ok_or_else(|| ToolError::ForbiddenPath {
            path: absolute.to_path_buf(),
            reason: "a readable file must have a final path component".to_owned(),
        })?;
    let parent = absolute.parent().ok_or_else(|| ToolError::ForbiddenPath {
        path: absolute.to_path_buf(),
        reason: "a readable file must have a parent directory".to_owned(),
    })?;
    Ok((open_directory_absolute(parent)?, name))
}

#[cfg(unix)]
fn open_directory_absolute(path: &Path) -> Result<File, ToolError> {
    if !path.is_absolute() {
        return Err(ToolError::ForbiddenPath {
            path: path.to_path_buf(),
            reason: "descriptor traversal requires an absolute contained path".to_owned(),
        });
    }
    let root = CString::new("/").expect("the root path contains no NUL");
    let descriptor = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    if descriptor < 0 {
        return Err(ToolError::execution_failed(std::io::Error::last_os_error()));
    }
    let mut directory = unsafe { File::from_raw_fd(descriptor) };
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                let name = c_component(name)?;
                let next = unsafe {
                    libc::openat(
                        directory.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                    )
                };
                if next < 0 {
                    return Err(open_error(path, std::io::Error::last_os_error()));
                }
                directory = unsafe { File::from_raw_fd(next) };
            }
            _ => {
                return Err(ToolError::ForbiddenPath {
                    path: path.to_path_buf(),
                    reason: "descriptor traversal accepts only normal absolute components"
                        .to_owned(),
                });
            }
        }
    }
    Ok(directory)
}

#[cfg(unix)]
fn c_component(name: &OsStr) -> Result<CString, ToolError> {
    CString::new(name.as_bytes()).map_err(|_| ToolError::ForbiddenPath {
        path: PathBuf::from(name),
        reason: "a filesystem path contains a NUL byte".to_owned(),
    })
}

#[cfg(unix)]
fn open_error(path: &Path, error: std::io::Error) -> ToolError {
    if error.kind() == std::io::ErrorKind::NotFound {
        ToolError::NotFound {
            path: path.to_path_buf(),
        }
    } else {
        ToolError::ForbiddenPath {
            path: path.to_path_buf(),
            reason: format!("the path could not be opened without following links: {error}"),
        }
    }
}

#[cfg(windows)]
fn open_regular_platform(
    absolute: &Path,
    capability: &ContainedPath,
) -> Result<(File, Metadata), ToolError> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, GetFinalPathNameByHandleW,
    };

    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(absolute)
        .map_err(ToolError::execution_failed)?;
    let metadata = file.metadata().map_err(ToolError::execution_failed)?;
    if metadata.file_attributes()
        & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
        != 0
        || !metadata.is_file()
    {
        return Err(ToolError::execution_failed(format!(
            "{} is not a non-reparse regular file",
            absolute.display()
        )));
    }
    let after = capability.revalidate().map_err(ToolError::from)?;
    let required = unsafe {
        GetFinalPathNameByHandleW(file.as_raw_handle().cast(), std::ptr::null_mut(), 0, 0)
    };
    if required == 0 {
        return Err(ToolError::execution_failed(std::io::Error::last_os_error()));
    }
    let mut buffer = vec![0_u16; required as usize];
    let written = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle().cast(),
            buffer.as_mut_ptr(),
            required,
            0,
        )
    };
    if written == 0 || written >= required {
        return Err(ToolError::execution_failed(std::io::Error::last_os_error()));
    }
    buffer.truncate(written as usize);
    let handle_path = PathBuf::from(OsString::from_wide(&buffer));
    if after.as_path() != absolute || handle_path != after.as_path() {
        return Err(ToolError::ForbiddenPath {
            path: absolute.to_path_buf(),
            reason: "the readable path changed while it was being opened".to_owned(),
        });
    }
    // OPEN_REPARSE_POINT protects only the leaf, not ancestors. Comparing the
    // path resolved from the held handle with the freshly contained target
    // catches swap-out/open/swap-back attacks. Windows still lacks the Unix
    // held-parent walk here, so exotic namespace/volume identity behavior is
    // retained as a documented platform TOCTOU residual.
    Ok((file, metadata))
}

#[cfg(not(any(unix, windows)))]
fn open_regular_platform(
    absolute: &Path,
    capability: &ContainedPath,
) -> Result<(File, Metadata), ToolError> {
    // Platforms without descriptor-relative traversal retain the capability
    // check on both sides of open and derive type information from the held
    // file. This is a documented best effort; supported Unix and Windows
    // targets use the stronger implementations above.
    let file = File::open(absolute).map_err(ToolError::execution_failed)?;
    let metadata = file.metadata().map_err(ToolError::execution_failed)?;
    if !metadata.is_file() {
        return Err(ToolError::execution_failed(format!(
            "{} is not a regular file",
            absolute.display()
        )));
    }
    capability.revalidate().map_err(ToolError::from)?;
    Ok((file, metadata))
}

#[cfg(not(unix))]
fn list_directory_platform(
    absolute: &Path,
    maximum: usize,
    context: &ExecutionContext,
) -> Result<SafeDirectoryListing, ToolError> {
    // The supported Unix path uses readdir on a held descriptor. On other
    // targets, stop at the sentinel and re-check cancellation for every entry.
    let mut entries = Vec::with_capacity(maximum);
    let mut truncated = false;
    for entry in fs::read_dir(absolute).map_err(ToolError::execution_failed)? {
        context.check_still_permitted()?;
        let entry = entry.map_err(ToolError::execution_failed)?;
        let name = entry.file_name();
        if !directory_entry_is_visible(&name) {
            continue;
        }
        if entries.len() == maximum {
            truncated = true;
            break;
        }
        let kind = entry.file_type().map_err(ToolError::execution_failed)?;
        entries.push(SafeDirectoryEntry {
            name,
            path: entry.path(),
            is_dir: kind.is_dir(),
            is_file: kind.is_file(),
            is_symlink: kind.is_symlink(),
        });
    }
    sort_entries(&mut entries);
    Ok(SafeDirectoryListing { entries, truncated })
}

fn sort_entries(entries: &mut [SafeDirectoryEntry]) {
    entries.sort_by(|left, right| {
        compare_directory_entries(left.is_dir, &left.name, right.is_dir, &right.name)
    });
}

#[cfg(all(test, unix))]
mod tests {
    use std::ffi::CString;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
    use std::time::{Duration, Instant};

    use super::{open_regular, open_regular_after_revalidation};
    use crate::domain::{RunId, StepId, ToolCallId};
    use crate::tool::ExecutionContext;

    #[test]
    fn held_parent_walk_refuses_an_ancestor_swapped_after_revalidation() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let ancestor = workspace.path().join("ancestor");
        fs::create_dir(&ancestor).unwrap();
        fs::write(ancestor.join("file.txt"), b"inside").unwrap();
        fs::write(outside.path().join("file.txt"), b"outside-secret").unwrap();
        let context = ExecutionContext::detached(
            RunId::new(),
            StepId::new(),
            ToolCallId::new(),
            workspace.path(),
        )
        .unwrap();
        let contained = context.resolve("ancestor/file.txt").unwrap();
        let held = workspace.path().join("ancestor-original");

        let error = open_regular_after_revalidation(&contained, || {
            fs::rename(&ancestor, &held).unwrap();
            symlink(outside.path(), &ancestor).unwrap();
        })
        .unwrap_err();

        assert!(matches!(error.kind(), "forbidden_path" | "symlink_escapes"));
    }

    #[test]
    fn fifo_is_rejected_without_waiting_for_a_writer() {
        let workspace = tempfile::tempdir().unwrap();
        let fifo = workspace.path().join("pipe");
        let encoded = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(encoded.as_ptr(), 0o600) }, 0);
        let context = ExecutionContext::detached(
            RunId::new(),
            StepId::new(),
            ToolCallId::new(),
            workspace.path(),
        )
        .unwrap();
        let contained = context.resolve("pipe").unwrap();
        let started = Instant::now();

        assert!(open_regular(&contained).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
