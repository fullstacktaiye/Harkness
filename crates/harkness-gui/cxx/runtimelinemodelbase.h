// SPDX-License-Identifier: MIT
#pragma once

#include <QAbstractListModel>

// Base class for the Rust-implemented RunTimelineModel.
//
// QAbstractListModel keeps the mutation notifications and dataChanged()
// protected, so a Rust implementation cannot reach them through CXX. These
// thin public wrappers forward to the protected members; the class carries no
// Q_OBJECT of its own and needs no moc pass. It lives at global scope because
// cxx-qt names base classes unqualified.
//
// A timeline uses every one of them: a live batch appends, an older page
// prepends, passing the retained-row bound removes from the head, selecting
// another run resets, and loading one event's payload changes that row alone.
class RunTimelineModelBase : public QAbstractListModel
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
