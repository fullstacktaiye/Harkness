//! A keyed list model for the source-control view's changed files.
//!
//! The Changes list used to bind a `ListView` straight to the `QVariantList`
//! that `HarknessBackend.git` carries. A plain list has no row identity, so
//! every projection of the working tree — including one that had not changed
//! at all — destroyed and rebuilt every delegate, throwing away the scroll
//! position and the hover state with them.
//!
//! This model reconciles instead. Rows are keyed by the backend's `pathId`
//! token, which is stable for as long as a path keeps its place in the status
//! projection, so a commit that clears three of five files removes exactly
//! three rows and leaves the other two delegates alone. Only an order change
//! or a duplicate key falls back to a reset.
//!
//! The whole status entry travels as one `entry` role rather than being
//! unpacked into a role per field: the map is already the contract QML binds
//! to, and keeping it whole means the model never has to be taught a new field
//! when the projection grows one.

#[cxx_qt::bridge]
pub mod ffi {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qbytearray.h");
        type QByteArray = cxx_qt_lib::QByteArray;

        include!("cxx-qt-lib/qhash_i32_QByteArray.h");
        type QHash_i32_QByteArray = cxx_qt_lib::QHash<cxx_qt_lib::QHashPair_i32_QByteArray>;

        include!("cxx-qt-lib/qmodelindex.h");
        type QModelIndex = cxx_qt_lib::QModelIndex;

        include!("cxx-qt-lib/qvariant.h");
        type QVariant = cxx_qt_lib::QVariant;

        include!("cxx-qt-lib/core/qlist/qlist_QVariant.h");
        type QList_QVariant = cxx_qt_lib::QList<QVariant>;

        include!("listmodelbase.h");
        type ChangesModelBase;

        #[rust_name = "begin_insert"]
        fn beginInsert(self: Pin<&mut ChangesModelBase>, first: i32, last: i32);

        #[rust_name = "end_insert"]
        fn endInsert(self: Pin<&mut ChangesModelBase>);

        #[rust_name = "begin_remove"]
        fn beginRemove(self: Pin<&mut ChangesModelBase>, first: i32, last: i32);

        #[rust_name = "end_remove"]
        fn endRemove(self: Pin<&mut ChangesModelBase>);

        #[rust_name = "begin_reset"]
        fn beginReset(self: Pin<&mut ChangesModelBase>);

        #[rust_name = "end_reset"]
        fn endReset(self: Pin<&mut ChangesModelBase>);

        #[rust_name = "emit_changed"]
        fn changed(self: Pin<&mut ChangesModelBase>, first: i32, last: i32);
    }

    extern "RustQt" {
        /// The changed-file list. QML writes the backend's status entries to
        /// [`entries`](Self::entries) and the model turns each assignment into
        /// the smallest set of row notifications that describes it.
        #[qobject]
        #[qml_element]
        #[base = ChangesModelBase]
        #[qproperty(QList_QVariant, entries, READ = entries, WRITE = set_entries, NOTIFY)]
        type ChangesModel = super::ChangesModelRust;

        #[cxx_override]
        fn data(self: &ChangesModel, index: &QModelIndex, role: i32) -> QVariant;

        #[cxx_override]
        #[cxx_name = "rowCount"]
        fn row_count(self: &ChangesModel, parent: &QModelIndex) -> i32;

        #[cxx_override]
        #[cxx_name = "roleNames"]
        fn role_names(self: &ChangesModel) -> QHash_i32_QByteArray;
    }

    extern "RustQt" {
        /// The rows as QML last supplied them.
        fn entries(self: &ChangesModel) -> QList_QVariant;

        /// Reconciles the model against a new status projection.
        #[cxx_name = "setEntries"]
        fn set_entries(self: Pin<&mut ChangesModel>, value: &QList_QVariant);
    }
}

use std::pin::Pin;

use cxx_qt::{CxxQtType, casting::Upcast};
use cxx_qt_lib::{
    QByteArray, QHash, QHashPair_i32_QByteArray, QList, QModelIndex, QString, QVariant,
};

use super::reconcile::{Edit, Keyed, plan};

/// `Qt::DisplayRole`, so a row reads as its path in accessibility tooling.
const DISPLAY_ROLE: i32 = 0;
/// `Qt::UserRole + 1` and up: the roles QML delegates bind to.
const ENTRY_ROLE: i32 = 257;
const PATH_ID_ROLE: i32 = 258;

fn model_roles() -> QHash<QHashPair_i32_QByteArray> {
    let mut roles = QHash::<QHashPair_i32_QByteArray>::default();
    roles.insert(DISPLAY_ROLE, QByteArray::from("display"));
    roles.insert(ENTRY_ROLE, QByteArray::from("entry"));
    roles.insert(PATH_ID_ROLE, QByteArray::from("pathId"));
    roles
}

/// One status entry, with the token that gives it identity across refreshes.
#[derive(Clone, Debug, PartialEq)]
struct ChangeRow {
    key: String,
    value: QVariant,
}

impl ChangeRow {
    fn from_entry(entry: &QVariant) -> Self {
        Self {
            key: entry_field(entry, "pathId").unwrap_or_default(),
            value: entry.clone(),
        }
    }

    fn path(&self) -> String {
        entry_field(&self.value, "path").unwrap_or_default()
    }
}

impl Keyed for ChangeRow {
    /// The backend's `pathId`, which is stable for as long as a path keeps its
    /// place in the status projection.
    fn key(&self) -> &str {
        &self.key
    }
}

fn entry_field(entry: &QVariant, key: &str) -> Option<String> {
    entry
        .value::<cxx_qt_lib::QMap<cxx_qt_lib::QMapPair_QString_QVariant>>()?
        .get(&QString::from(key))?
        .value::<QString>()
        .map(|value| value.to_string())
}

#[derive(Default)]
pub struct ChangesModelRust {
    rows: Vec<ChangeRow>,
}

impl ffi::ChangesModel {
    fn data(&self, index: &QModelIndex, role: i32) -> QVariant {
        let rust = self.rust();
        if !index.is_valid() {
            return QVariant::default();
        }
        let Ok(row) = usize::try_from(index.row()) else {
            return QVariant::default();
        };
        let Some(entry) = rust.rows.get(row) else {
            return QVariant::default();
        };
        match role {
            DISPLAY_ROLE => QVariant::from(&QString::from(entry.path().as_str())),
            ENTRY_ROLE => entry.value.clone(),
            PATH_ID_ROLE => QVariant::from(&QString::from(entry.key.as_str())),
            _ => QVariant::default(),
        }
    }

    fn row_count(&self, parent: &QModelIndex) -> i32 {
        // A list model has rows only below its invisible root.
        if parent.is_valid() {
            return 0;
        }
        i32::try_from(self.rust().rows.len()).unwrap_or(i32::MAX)
    }

    fn role_names(&self) -> QHash<QHashPair_i32_QByteArray> {
        model_roles()
    }

    fn entries(&self) -> QList<QVariant> {
        self.rust()
            .rows
            .iter()
            .map(|row| row.value.clone())
            .collect()
    }

    fn set_entries(mut self: Pin<&mut Self>, value: &QList<QVariant>) {
        let incoming = value.iter().map(ChangeRow::from_entry).collect::<Vec<_>>();
        if self.as_ref().rust().rows == incoming {
            // The identical projection Git reports on an unchanged poll. Say
            // nothing: a notification here is what rebuilt the list on a timer.
            return;
        }
        match plan(&self.as_ref().rust().rows, &incoming) {
            Some(edits) => {
                for edit in edits {
                    self.as_mut().apply(edit);
                }
            }
            None => {
                {
                    let base: Pin<&mut ffi::ChangesModelBase> = self.as_mut().upcast_pin();
                    base.begin_reset();
                }
                self.as_mut().rust_mut().get_mut().rows = incoming;
                {
                    let base: Pin<&mut ffi::ChangesModelBase> = self.as_mut().upcast_pin();
                    base.end_reset();
                }
            }
        }
        self.as_mut().entries_changed();
    }

    /// Applies one planned edit, wrapping the row mutation in the notification
    /// Qt requires for it.
    fn apply(mut self: Pin<&mut Self>, edit: Edit<ChangeRow>) {
        match edit {
            Edit::Remove { first, last } => {
                {
                    let base: Pin<&mut ffi::ChangesModelBase> = self.as_mut().upcast_pin();
                    base.begin_remove(first as i32, last as i32);
                }
                self.as_mut().rust_mut().get_mut().rows.drain(first..=last);
                let base: Pin<&mut ffi::ChangesModelBase> = self.as_mut().upcast_pin();
                base.end_remove();
            }
            Edit::Insert { first, rows } => {
                let last = first + rows.len() - 1;
                {
                    let base: Pin<&mut ffi::ChangesModelBase> = self.as_mut().upcast_pin();
                    base.begin_insert(first as i32, last as i32);
                }
                self.as_mut()
                    .rust_mut()
                    .get_mut()
                    .rows
                    .splice(first..first, rows);
                let base: Pin<&mut ffi::ChangesModelBase> = self.as_mut().upcast_pin();
                base.end_insert();
            }
            Edit::Update { first, rows } => {
                let last = first + rows.len() - 1;
                self.as_mut()
                    .rust_mut()
                    .get_mut()
                    .rows
                    .splice(first..=last, rows);
                let base: Pin<&mut ffi::ChangesModelBase> = self.as_mut().upcast_pin();
                base.emit_changed(first as i32, last as i32);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use cxx_qt_lib::{QByteArray, QMap, QMapPair_QString_QVariant, QString, QVariant};

    use super::super::reconcile::{Edit, plan};
    use super::{ChangeRow, DISPLAY_ROLE, ENTRY_ROLE, PATH_ID_ROLE, model_roles};

    fn entry(path_id: &str, path: &str, unstaged: &str) -> QVariant {
        let mut map = QMap::<QMapPair_QString_QVariant>::default();
        map.insert(
            QString::from("pathId"),
            QVariant::from(&QString::from(path_id)),
        );
        map.insert(QString::from("path"), QVariant::from(&QString::from(path)));
        map.insert(
            QString::from("unstaged"),
            QVariant::from(&QString::from(unstaged)),
        );
        QVariant::from(&map)
    }

    fn row(path_id: &str, path: &str, unstaged: &str) -> ChangeRow {
        ChangeRow::from_entry(&entry(path_id, path, unstaged))
    }

    #[test]
    fn qml_roles_have_stable_names() {
        let roles = model_roles();

        for (role, name) in [
            (DISPLAY_ROLE, "display"),
            (ENTRY_ROLE, "entry"),
            (PATH_ID_ROLE, "pathId"),
        ] {
            assert_eq!(roles.get(&role), Some(QByteArray::from(name)));
        }
    }

    #[test]
    fn a_row_takes_its_identity_from_the_backend_token() {
        let row = row("path-1", "src/main.rs", "modified");

        assert_eq!(row.key, "path-1");
        assert_eq!(row.path(), "src/main.rs");
    }

    #[test]
    fn an_unchanged_projection_plans_no_edits() {
        let rows = vec![
            row("path-1", "a.txt", "modified"),
            row("path-2", "b.txt", ""),
        ];

        assert_eq!(plan(&rows, &rows.clone()), Some(Vec::new()));
    }

    #[test]
    fn a_committed_file_is_removed_without_touching_its_neighbours() {
        let current = vec![
            row("path-1", "a.txt", "modified"),
            row("path-2", "b.txt", "modified"),
            row("path-3", "c.txt", "modified"),
        ];
        let incoming = vec![current[0].clone(), current[2].clone()];

        assert_eq!(
            plan(&current, &incoming),
            Some(vec![Edit::Remove { first: 1, last: 1 }])
        );
    }

    #[test]
    fn adjacent_removals_are_reported_as_one_run() {
        let current = vec![
            row("path-1", "a.txt", "modified"),
            row("path-2", "b.txt", "modified"),
            row("path-3", "c.txt", "modified"),
        ];
        let incoming = vec![current[2].clone()];

        assert_eq!(
            plan(&current, &incoming),
            Some(vec![Edit::Remove { first: 0, last: 1 }])
        );
    }

    #[test]
    fn a_new_file_is_inserted_at_its_place_in_the_listing() {
        let current = vec![
            row("path-1", "a.txt", "modified"),
            row("path-3", "c.txt", "modified"),
        ];
        let inserted = row("path-4", "b.txt", "untracked");
        let incoming = vec![current[0].clone(), inserted.clone(), current[1].clone()];

        assert_eq!(
            plan(&current, &incoming),
            Some(vec![Edit::Insert {
                first: 1,
                rows: vec![inserted],
            }])
        );
    }

    #[test]
    fn a_changed_row_updates_in_place() {
        let current = vec![
            row("path-1", "a.txt", "modified"),
            row("path-2", "b.txt", "modified"),
        ];
        let changed = row("path-2", "b.txt", "deleted");
        let incoming = vec![current[0].clone(), changed.clone()];

        assert_eq!(
            plan(&current, &incoming),
            Some(vec![Edit::Update {
                first: 1,
                rows: vec![changed],
            }])
        );
    }

    #[test]
    fn removals_insertions_and_updates_compose_in_application_order() {
        let current = vec![
            row("path-1", "a.txt", "modified"),
            row("path-2", "b.txt", "modified"),
            row("path-3", "c.txt", "modified"),
        ];
        let changed = row("path-3", "c.txt", "deleted");
        let added = row("path-4", "d.txt", "untracked");
        let incoming = vec![current[0].clone(), changed.clone(), added.clone()];

        let edits = plan(&current, &incoming).expect("the order is unchanged");

        assert_eq!(
            edits,
            vec![
                Edit::Remove { first: 1, last: 1 },
                Edit::Update {
                    first: 1,
                    rows: vec![changed],
                },
                Edit::Insert {
                    first: 2,
                    rows: vec![added],
                },
            ]
        );
    }

    #[test]
    fn a_reordered_listing_falls_back_to_a_reset() {
        let current = vec![
            row("path-1", "a.txt", "modified"),
            row("path-2", "b.txt", "modified"),
        ];
        let incoming = vec![current[1].clone(), current[0].clone()];

        assert_eq!(plan(&current, &incoming), None);
    }

    #[test]
    fn ambiguous_keys_fall_back_to_a_reset() {
        let untokenized = ChangeRow::from_entry(&QVariant::from(
            &QMap::<QMapPair_QString_QVariant>::default(),
        ));
        let incoming = vec![untokenized.clone(), untokenized];

        assert_eq!(plan(&[], &incoming), None);
    }
}
