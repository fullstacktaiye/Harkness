// SPDX-License-Identifier: MIT
#pragma once

#include <QtCore/QString>
#include <QtGui/QClipboard>
#include <QtGui/QGuiApplication>

namespace harkness {

/// Puts `text` on the clipboard byte for byte.
///
/// QtQuick's only clipboard writer is `TextEdit::copy`, which carries the text
/// through a `QTextDocument` first: that turns every CRLF into a block break
/// and serialises it back out as an LF. The review surface copies diff lines
/// whose terminator can be the whole change under review, so a writer that
/// rewrites terminators is the one thing it cannot use.
///
/// Called before `QGuiApplication` exists — which the GUI never does, and a
/// test binary could — this does nothing rather than crashing.
inline void setClipboardText(const QString &text)
{
    if (auto *clipboard = QGuiApplication::clipboard()) {
        clipboard->setText(text);
    }
}

} // namespace harkness
