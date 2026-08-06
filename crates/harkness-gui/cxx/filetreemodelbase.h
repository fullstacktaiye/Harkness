// SPDX-License-Identifier: MIT
#pragma once

#include <QAbstractItemModel>

// Base class for the Rust-implemented FileTreeModel.
//
// QAbstractItemModel keeps index creation and population notifications
// protected, so a Rust override of index() or fetchMore() cannot reach them
// through CXX. These thin public wrappers forward to the protected members;
// the class carries no Q_OBJECT of its own and needs no moc pass. It lives at
// global scope because cxx-qt names base classes unqualified.
class FileTreeModelBase : public QAbstractItemModel
{
public:
  using QAbstractItemModel::QAbstractItemModel;

  QModelIndex makeIndex(int row, int column, quintptr id) const
  {
    return createIndex(row, column, id);
  }

  void beginInsert(const QModelIndex& parent, int first, int last)
  {
    beginInsertRows(parent, first, last);
  }

  void endInsert()
  {
    endInsertRows();
  }

  void beginReset()
  {
    beginResetModel();
  }

  void endReset()
  {
    endResetModel();
  }
};
