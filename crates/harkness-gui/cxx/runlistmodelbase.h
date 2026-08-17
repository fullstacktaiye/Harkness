// SPDX-License-Identifier: MIT
#pragma once

#include <QAbstractListModel>

// Base class for the Rust-implemented RunListModel.
//
// QAbstractListModel keeps the mutation notifications protected, so a Rust
// implementation cannot reach them through CXX. These thin public wrappers
// forward to the protected members; the class carries no Q_OBJECT of its own
// and needs no moc pass. It lives at global scope because cxx-qt names base
// classes unqualified.
//
// Run history only ever grows at the tail a page at a time, so a listing needs
// appends and a reset and nothing else; the timeline and approval bases beside
// this one carry the removal and dataChanged() wrappers their models do use.
class RunListModelBase : public QAbstractListModel
{
public:
  using QAbstractListModel::QAbstractListModel;

  void beginInsert(int first, int last)
  {
    beginInsertRows(QModelIndex(), first, last);
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
