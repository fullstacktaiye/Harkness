// SPDX-License-Identifier: MIT
#pragma once

#include <QAbstractListModel>

// Base classes for the Rust-implemented flat list models.
//
// QAbstractListModel keeps the mutation notifications and dataChanged()
// protected, so a Rust implementation cannot reach them through CXX. These thin
// public wrappers forward to the protected members; the class carries no
// Q_OBJECT of its own and needs no moc pass. They live at global scope because
// cxx-qt names base classes unqualified.
//
// The wrappers are written once here and the per-model classes below are empty
// subclasses. cxx requires an extern C++ type to be declared by exactly one
// bridge, so each model still needs a distinct base *name* — but nothing needs
// a distinct base *body*, and four copies of beginInsertRows() would be four
// places to fix when one of them turns out to be wrong.
class ListModelBase : public QAbstractListModel
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

  void beginRemove(int first, int last)
  {
    beginRemoveRows(QModelIndex(), first, last);
  }

  void endRemove()
  {
    endRemoveRows();
  }

  void beginReset()
  {
    beginResetModel();
  }

  void endReset()
  {
    endResetModel();
  }

  // A list model is one column, so a changed run is a single contiguous span.
  void changed(int first, int last)
  {
    Q_EMIT dataChanged(index(first, 0), index(last, 0));
  }
};

// The source-control view's changed files.
class ChangesModelBase : public ListModelBase
{
public:
  using ListModelBase::ListModelBase;
};

// Newest-first run history.
class RunListModelBase : public ListModelBase
{
public:
  using ListModelBase::ListModelBase;
};

// One run's event log.
class RunTimelineModelBase : public ListModelBase
{
public:
  using ListModelBase::ListModelBase;
};

// The approvals waiting to be answered.
class ApprovalModelBase : public ListModelBase
{
public:
  using ListModelBase::ListModelBase;
};
