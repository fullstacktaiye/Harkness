//! A lazy, read-only filesystem model for the project shell's native tree.
//!
//! The model is a [`QAbstractItemModel`] implemented in Rust through cxx-qt
//! inheritance. It lists exactly one directory level per `fetchMore` call, so
//! expanding a node never walks more of the tree than becomes visible.
//! [`harkness_core::list_directory`] supplies the entries, which means `.git`
//! internals never appear and symlinked directories are inert leaves.

#[cxx_qt::bridge]
pub mod ffi {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qbytearray.h");
        type QByteArray = cxx_qt_lib::QByteArray;

        include!("cxx-qt-lib/qhash_i32_QByteArray.h");
        type QHash_i32_QByteArray = cxx_qt_lib::QHash<cxx_qt_lib::QHashPair_i32_QByteArray>;

        include!("cxx-qt-lib/qmodelindex.h");
        type QModelIndex = cxx_qt_lib::QModelIndex;

        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;

        include!("cxx-qt-lib/qvariant.h");
        type QVariant = cxx_qt_lib::QVariant;

        include!("cxx-qt-lib/qtypes.h");
        type quintptr = cxx_qt_lib::quintptr;

        include!("filetreemodelbase.h");
        type FileTreeModelBase;

        #[rust_name = "make_index"]
        fn makeIndex(self: &FileTreeModelBase, row: i32, column: i32, id: quintptr) -> QModelIndex;

        #[rust_name = "begin_insert"]
        fn beginInsert(
            self: Pin<&mut FileTreeModelBase>,
            parent: &QModelIndex,
            first: i32,
            last: i32,
        );

        #[rust_name = "end_insert"]
        fn endInsert(self: Pin<&mut FileTreeModelBase>);

        #[rust_name = "begin_reset"]
        fn beginReset(self: Pin<&mut FileTreeModelBase>);

        #[rust_name = "end_reset"]
        fn endReset(self: Pin<&mut FileTreeModelBase>);
    }

    extern "RustQt" {
        /// The tree model. `root` is read for diagnostics only; QML drives the
        /// model through [`setRoot`](Self::set_root), which resets it.
        #[qobject]
        #[qml_element]
        #[base = FileTreeModelBase]
        #[qproperty(QString, root, READ)]
        type FileTreeModel = super::FileTreeModelRust;

        #[cxx_override]
        fn data(self: &FileTreeModel, index: &QModelIndex, role: i32) -> QVariant;

        #[cxx_override]
        #[cxx_name = "rowCount"]
        fn row_count(self: &FileTreeModel, parent: &QModelIndex) -> i32;

        #[cxx_override]
        #[cxx_name = "columnCount"]
        fn column_count(self: &FileTreeModel, parent: &QModelIndex) -> i32;

        #[cxx_override]
        fn index(self: &FileTreeModel, row: i32, column: i32, parent: &QModelIndex) -> QModelIndex;

        #[cxx_override]
        fn parent(self: &FileTreeModel, child: &QModelIndex) -> QModelIndex;

        #[cxx_override]
        #[cxx_name = "hasChildren"]
        fn has_children(self: &FileTreeModel, parent: &QModelIndex) -> bool;

        #[cxx_override]
        #[cxx_name = "canFetchMore"]
        fn can_fetch_more(self: &FileTreeModel, parent: &QModelIndex) -> bool;

        #[cxx_override]
        #[cxx_name = "fetchMore"]
        fn fetch_more(self: Pin<&mut FileTreeModel>, parent: &QModelIndex);

        #[cxx_override]
        #[cxx_name = "roleNames"]
        fn role_names(self: &FileTreeModel) -> QHash_i32_QByteArray;

        /// Points the model at a new directory, discarding all fetched nodes.
        #[qinvokable]
        #[cxx_name = "setRoot"]
        fn set_root(self: Pin<&mut FileTreeModel>, path: &QString);
    }
}

use std::{collections::HashMap, pin::Pin};

use cxx_qt::{CxxQtType, casting::Upcast};
use cxx_qt_lib::{QByteArray, QHash, QHashPair_i32_QByteArray, QModelIndex, QString, QVariant};

/// `Qt::DisplayRole`, so delegates see the entry name as `model.display`.
const DISPLAY_ROLE: i32 = 0;
/// `Qt::UserRole + 1` and up: the roles QML delegates bind to.
const FILE_NAME_ROLE: i32 = 257;
const FILE_PATH_ROLE: i32 = 258;
const IS_DIRECTORY_ROLE: i32 = 259;
const EXPANDABLE_ROLE: i32 = 260;

fn model_roles() -> QHash<QHashPair_i32_QByteArray> {
    let mut roles = QHash::<QHashPair_i32_QByteArray>::default();
    roles.insert(DISPLAY_ROLE, QByteArray::from("display"));
    roles.insert(FILE_NAME_ROLE, QByteArray::from("fileName"));
    roles.insert(FILE_PATH_ROLE, QByteArray::from("filePath"));
    roles.insert(IS_DIRECTORY_ROLE, QByteArray::from("isDirectory"));
    roles.insert(EXPANDABLE_ROLE, QByteArray::from("expandable"));
    roles
}

/// The internal id of the invisible root node. Valid indexes always carry an
/// id of 1 or higher; an invalid [`QModelIndex`] addresses this node.
const ROOT_ID: u64 = 0;

struct Node {
    /// `None` for the invisible root, which is not a filesystem entry.
    entry: Option<harkness_core::DirEntry>,
    /// Child node ids, or `None` while the directory has not been listed.
    children: Option<Vec<u64>>,
    /// Guards against a re-entrant fetch without changing row or child state
    /// before beginInsertRows() starts the model mutation.
    fetching: bool,
    parent: Option<u64>,
}

pub struct FileTreeModelRust {
    root: QString,
    nodes: HashMap<u64, Node>,
    next_id: u64,
}

impl Default for FileTreeModelRust {
    fn default() -> Self {
        let mut nodes = HashMap::new();
        nodes.insert(
            ROOT_ID,
            Node {
                entry: None,
                children: None,
                fetching: false,
                parent: None,
            },
        );
        Self {
            root: QString::default(),
            nodes,
            next_id: 1,
        }
    }
}

impl FileTreeModelRust {
    /// The model-internal id an index addresses; the invisible root stands in
    /// for any invalid index.
    fn node_id(index: &QModelIndex) -> u64 {
        if index.is_valid() {
            index.internal_id() as u64
        } else {
            ROOT_ID
        }
    }

    fn node(&self, index: &QModelIndex) -> Option<&Node> {
        self.nodes.get(&Self::node_id(index))
    }

    fn directory_path(&self, node: &Node) -> String {
        node.entry
            .as_ref()
            .map(|entry| entry.path.display().to_string())
            .unwrap_or_else(|| self.root.to_string())
    }

    /// Whether the node may contain children once fetched. Only the invisible
    /// root and real directories can; files and symlinked directories cannot.
    fn is_expandable(node: &Node) -> bool {
        node.entry
            .as_ref()
            .map(|entry| entry.expandable)
            .unwrap_or(true)
    }
}

impl ffi::FileTreeModel {
    fn data(&self, index: &QModelIndex, role: i32) -> QVariant {
        let rust = self.rust();
        let Some(node) = rust.node(index) else {
            return QVariant::default();
        };
        let Some(entry) = &node.entry else {
            return QVariant::default();
        };

        match role {
            DISPLAY_ROLE | FILE_NAME_ROLE => QVariant::from(&QString::from(entry.name.as_str())),
            FILE_PATH_ROLE => {
                QVariant::from(&QString::from(entry.path.display().to_string().as_str()))
            }
            IS_DIRECTORY_ROLE => QVariant::from(&entry.is_dir),
            EXPANDABLE_ROLE => QVariant::from(&entry.expandable),
            _ => QVariant::default(),
        }
    }

    fn row_count(&self, parent: &QModelIndex) -> i32 {
        // Qt expects no rows below a non-zero column of a single-column model.
        if parent.is_valid() && parent.column() > 0 {
            return 0;
        }
        let rust = self.rust();
        rust.node(parent)
            .and_then(|node| node.children.as_ref())
            .map_or(0, |children| children.len() as i32)
    }

    fn column_count(&self, _parent: &QModelIndex) -> i32 {
        1
    }

    fn index(&self, row: i32, column: i32, parent: &QModelIndex) -> QModelIndex {
        if row < 0 || column != 0 {
            return QModelIndex::default();
        }
        let rust = self.rust();
        rust.node(parent)
            .and_then(|node| node.children.as_ref())
            .and_then(|children| children.get(row as usize))
            .map_or_else(QModelIndex::default, |&child| {
                let base: &ffi::FileTreeModelBase = self.upcast();
                base.make_index(row, column, (child as usize).into())
            })
    }

    fn parent(&self, child: &QModelIndex) -> QModelIndex {
        let rust = self.rust();
        let Some(node) = rust.node(child) else {
            return QModelIndex::default();
        };
        let Some(parent_id) = node.parent else {
            return QModelIndex::default();
        };

        // Children of the invisible root are top-level indexes. Qt requires
        // their parent() to be invalid; manufacturing an index with id zero
        // makes the view believe there is another visible hierarchy level.
        if parent_id == ROOT_ID {
            return QModelIndex::default();
        }

        // The parent's row is its position within the grandparent's children.
        let row = rust
            .nodes
            .get(&parent_id)
            .and_then(|parent| parent.parent)
            .and_then(|grandparent_id| rust.nodes.get(&grandparent_id))
            .and_then(|grandparent| grandparent.children.as_ref())
            .and_then(|children| children.iter().position(|&id| id == parent_id))
            .map_or(0, |position| position as i32);
        let base: &ffi::FileTreeModelBase = self.upcast();
        base.make_index(row, 0, (parent_id as usize).into())
    }

    fn has_children(&self, parent: &QModelIndex) -> bool {
        let rust = self.rust();
        let Some(node) = rust.node(parent) else {
            return false;
        };
        match &node.children {
            Some(children) => !children.is_empty(),
            None => FileTreeModelRust::is_expandable(node),
        }
    }

    fn can_fetch_more(&self, parent: &QModelIndex) -> bool {
        let rust = self.rust();
        rust.node(parent).is_some_and(|node| {
            node.children.is_none()
                && !node.fetching
                && FileTreeModelRust::is_expandable(node)
                && !rust.root.is_empty()
        })
    }

    fn fetch_more(mut self: Pin<&mut Self>, parent: &QModelIndex) {
        let (node_id, path) = {
            let pinned = self.as_ref();
            let rust = pinned.rust();
            let Some(node) = rust.node(parent) else {
                return;
            };
            if node.children.is_some() || node.fetching || !FileTreeModelRust::is_expandable(node) {
                return;
            }
            (
                FileTreeModelRust::node_id(parent),
                rust.directory_path(node),
            )
        };

        // Read before the insert notification: a failed listing simply marks
        // the node fetched and empty rather than leaving Qt mid-notification.
        let entries = harkness_core::list_directory(&path).unwrap_or_default();
        let count = entries.len();
        let mut children = Vec::with_capacity(count);
        if count > 0 {
            // beginInsertRows() may synchronously ask canFetchMore() again.
            // Guard that re-entry separately: changing `children` here would
            // also change rowCount()/hasChildren() before Qt's insertion
            // protocol begins and confuse TreeView's flattening proxy.
            self.as_mut()
                .rust_mut()
                .get_mut()
                .nodes
                .get_mut(&node_id)
                .expect("fetched node must exist")
                .fetching = true;
            {
                let base: Pin<&mut ffi::FileTreeModelBase> = self.as_mut().upcast_pin();
                base.begin_insert(parent, 0, count as i32 - 1);
            }
            let rust = self.as_mut().rust_mut().get_mut();
            for entry in entries {
                let id = rust.next_id;
                rust.next_id += 1;
                rust.nodes.insert(
                    id,
                    Node {
                        entry: Some(entry),
                        children: None,
                        fetching: false,
                        parent: Some(node_id),
                    },
                );
                children.push(id);
            }
            let node = rust
                .nodes
                .get_mut(&node_id)
                .expect("fetched node must exist");
            node.children = Some(children);
            node.fetching = false;
            let base: Pin<&mut ffi::FileTreeModelBase> = self.as_mut().upcast_pin();
            base.end_insert();
        } else {
            let node = self
                .as_mut()
                .rust_mut()
                .get_mut()
                .nodes
                .get_mut(&node_id)
                .expect("fetched node must exist");
            node.children = Some(children);
            node.fetching = false;
        }
    }

    fn role_names(&self) -> QHash<QHashPair_i32_QByteArray> {
        model_roles()
    }

    fn set_root(mut self: Pin<&mut Self>, path: &QString) {
        if *self.as_ref().root() == *path {
            return;
        }
        // Populate the first level atomically with the reset. Qt Quick's
        // TreeView maintains an internal flattened proxy; fetching the
        // invisible root through rowsInserted can make that proxy retain the
        // pre- and post-fetch root rows. Descendant directories still use
        // fetchMore(), so traversal remains one-level-at-a-time and lazy.
        let entries = harkness_core::list_directory(path.to_string()).unwrap_or_default();
        {
            let base: Pin<&mut ffi::FileTreeModelBase> = self.as_mut().upcast_pin();
            base.begin_reset();
        }
        {
            let rust = self.as_mut().rust_mut().get_mut();
            rust.nodes.clear();
            rust.next_id = 1;
            let mut children = Vec::with_capacity(entries.len());
            for entry in entries {
                let id = rust.next_id;
                rust.next_id += 1;
                rust.nodes.insert(
                    id,
                    Node {
                        entry: Some(entry),
                        children: None,
                        fetching: false,
                        parent: Some(ROOT_ID),
                    },
                );
                children.push(id);
            }
            rust.nodes.insert(
                ROOT_ID,
                Node {
                    entry: None,
                    children: Some(children),
                    fetching: false,
                    parent: None,
                },
            );
            rust.root = path.clone();
        }
        {
            let base: Pin<&mut ffi::FileTreeModelBase> = self.as_mut().upcast_pin();
            base.end_reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use cxx_qt_lib::QByteArray;

    use super::{
        DISPLAY_ROLE, EXPANDABLE_ROLE, FILE_NAME_ROLE, FILE_PATH_ROLE, FileTreeModelRust,
        IS_DIRECTORY_ROLE, Node, ROOT_ID, model_roles,
    };

    #[test]
    fn default_model_has_an_unfetched_invisible_root() {
        let model = FileTreeModelRust::default();
        let root = &model.nodes[&ROOT_ID];

        assert!(root.entry.is_none());
        assert!(root.children.is_none());
        assert_eq!(model.next_id, 1);
        assert!(FileTreeModelRust::is_expandable(root));
    }

    #[test]
    fn qml_roles_have_stable_names() {
        let roles = model_roles();

        for (role, name) in [
            (DISPLAY_ROLE, "display"),
            (FILE_NAME_ROLE, "fileName"),
            (FILE_PATH_ROLE, "filePath"),
            (IS_DIRECTORY_ROLE, "isDirectory"),
            (EXPANDABLE_ROLE, "expandable"),
        ] {
            assert_eq!(roles.get(&role), Some(QByteArray::from(name)));
        }
    }

    #[cfg(unix)]
    #[test]
    fn real_directories_expand_but_symlinked_ones_do_not() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap();
        fs::create_dir(root.path().join("real")).unwrap();
        symlink(root.path().join("real"), root.path().join("linked")).unwrap();

        let entries = harkness_core::list_directory(root.path()).unwrap();
        let expandable: Vec<bool> = entries
            .into_iter()
            .map(|entry| {
                FileTreeModelRust::is_expandable(&Node {
                    entry: Some(entry),
                    children: None,
                    fetching: false,
                    parent: Some(ROOT_ID),
                })
            })
            .collect();

        // Both entries are listed, but only the real directory may expand.
        assert_eq!(expandable.len(), 2);
        assert_eq!(expandable.iter().filter(|&&flag| flag).count(), 1);
    }
}
