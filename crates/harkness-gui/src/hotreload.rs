// SPDX-License-Identifier: MIT
//! Development-only QML hot reload.
//!
//! The mechanism, and why it needs an URL interceptor rather than a second
//! import path, is documented in `cxx/qmlhotreload.h`. This module is the
//! bridge to it plus the policy deciding when it applies.

use std::{env, path::PathBuf, pin::Pin};

use cxx_qt_lib::{QQmlApplicationEngine, QString, QUrl};

#[cxx_qt::bridge]
mod ffi {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;

        include!("cxx-qt-lib/qurl.h");
        type QUrl = cxx_qt_lib::QUrl;

        include!("cxx-qt-lib/qqmlapplicationengine.h");
        type QQmlApplicationEngine = cxx_qt_lib::QQmlApplicationEngine;
    }

    #[namespace = "harkness"]
    unsafe extern "C++" {
        include!("qmlhotreload.h");

        #[rust_name = "install_qml_hot_reload"]
        fn installQmlHotReload(
            engine: Pin<&mut QQmlApplicationEngine>,
            module_prefix: &QString,
            source_dir: &QString,
            root_url: &QUrl,
        ) -> bool;
    }
}

/// Where this binary's QML came from. An installed copy has been moved away
/// from its sources, so this path simply does not exist there and the reload
/// declines to install itself.
const BUILT_QML_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/qml");

/// Turns the reload off when set to a false-ish value.
const ENABLE_VARIABLE: &str = "HARKNESS_QML_HOT_RELOAD";
/// Points the reload at a working copy other than the one this binary was
/// built from.
const SOURCE_DIRECTORY_VARIABLE: &str = "HARKNESS_QML_SOURCE_DIR";

/// Redirects the engine's lookups of the module's own QML at the working copy
/// on disk, and rebuilds the window whenever a file there changes. Returns
/// whether it applied.
///
/// Call before the engine loads anything: the interceptor only affects URLs
/// resolved after it is installed.
pub(crate) fn install(engine: Pin<&mut QQmlApplicationEngine>, root_url: &str) -> bool {
    let (Some(prefix), Some(source_dir)) = (module_prefix(root_url), source_dir()) else {
        return false;
    };
    ffi::install_qml_hot_reload(
        engine,
        &QString::from(prefix),
        &QString::from(&source_dir.to_string_lossy().into_owned()),
        &QUrl::from(root_url),
    )
}

/// The resource directory `root_url` names, which is the prefix every one of
/// the module's QML files shares. Derived rather than spelled out a second
/// time, so it cannot drift away from the URL actually loaded.
fn module_prefix(root_url: &str) -> Option<&str> {
    let path = root_url.strip_prefix("qrc:")?;
    let file_name = path.rfind('/')?;
    Some(&path[..=file_name])
}

/// The directory to watch, or `None` when the reload does not apply: it is
/// switched off explicitly, or this build has no working copy to read.
fn source_dir() -> Option<PathBuf> {
    if matches!(
        env::var(ENABLE_VARIABLE).as_deref(),
        Ok("0") | Ok("false") | Ok("no")
    ) {
        return None;
    }
    let directory = env::var_os(SOURCE_DIRECTORY_VARIABLE)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(BUILT_QML_DIR));
    directory.is_dir().then_some(directory)
}

#[cfg(test)]
pub(crate) mod tests {
    // Reached through the parent module rather than through `crate`, because
    // `tests/qml_smoke.rs` mounts `main.rs` as a submodule of its own crate.
    use super::{super::MAIN_QML_URL, *};

    /// A prefix that stopped matching the loaded URL would silently disable
    /// the redirect rather than fail, so it is checked rather than assumed.
    ///
    /// Driven from `tests/qml_smoke.rs`: these run in a harness-free target
    /// that never collects `#[test]` items, the same way `main`'s own tests do.
    #[allow(dead_code)]
    pub(crate) fn module_prefix_is_the_root_url_directory() {
        assert_eq!(
            module_prefix(MAIN_QML_URL),
            Some("/qt/qml/io/github/fullstacktaiye/harkness/qml/")
        );
        // A root loaded from anywhere but the resource system is already being
        // read from disk, so there is nothing to redirect.
        assert_eq!(module_prefix("file:///tmp/Main.qml"), None);
    }
}
