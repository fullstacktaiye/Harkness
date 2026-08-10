// SPDX-License-Identifier: MIT
#pragma once

#include <QtCore/QDebug>
#include <QtCore/QDir>
#include <QtCore/QFileInfo>
#include <QtCore/QFileSystemWatcher>
#include <QtCore/QObject>
#include <QtCore/QPointer>
#include <QtCore/QString>
#include <QtCore/QStringList>
#include <QtCore/QTimer>
#include <QtCore/QUrl>
#include <QtCore/QVariantMap>
#include <QtQml/QQmlAbstractUrlInterceptor>
#include <QtQml/QQmlApplicationEngine>

// Development-only QML hot reload.
//
// The QML files are compiled into the static module as resources, so a running
// binary reads them from `qrc:` and never looks at the working copy. Both
// halves of the reload follow from that one fact: an URL interceptor points
// every request inside the module's `qml/` prefix at the file on disk, and a
// watcher on that directory throws away the component cache and rebuilds the
// window whenever one of those files changes.
//
// Only the module's `.qml` files are redirected. The `qmldir` beside them stays
// in the resource, so the type registry — including the Rust-backed types the
// module registers — is exactly the one the build produced.
namespace harkness {

class QmlSourceInterceptor final : public QQmlAbstractUrlInterceptor
{
public:
  QmlSourceInterceptor(QString prefix, QString sourceDir)
    : m_prefix(std::move(prefix))
    , m_sourceDir(std::move(sourceDir))
  {
  }

  QUrl intercept(const QUrl& url, DataType type) override
  {
    Q_UNUSED(type)
    if (url.scheme() != QStringLiteral("qrc"))
      return url;
    const QString path = url.path();
    if (!path.startsWith(m_prefix))
      return url;
    const QString candidate =
      m_sourceDir + QLatin1Char('/') + path.mid(m_prefix.size());
    // A file the working copy does not have is still served from the resource,
    // so a stale build stays loadable instead of failing to resolve a type.
    if (!QFileInfo::exists(candidate))
      return url;
    return QUrl::fromLocalFile(candidate);
  }

private:
  QString m_prefix;
  QString m_sourceDir;
};

class QmlHotReloader final : public QObject
{
public:
  QmlHotReloader(QQmlApplicationEngine& engine,
                 const QString& prefix,
                 QString sourceDir,
                 QUrl rootUrl)
    : QObject(&engine)
    , m_engine(engine)
    , m_interceptor(prefix, sourceDir)
    , m_sourceDir(std::move(sourceDir))
    , m_rootUrl(std::move(rootUrl))
  {
    m_engine.addUrlInterceptor(&m_interceptor);

    m_debounce.setSingleShot(true);
    // Editors write a file in several steps; reloading on the first of them
    // would parse a half-written document.
    m_debounce.setInterval(150);
    QObject::connect(
      &m_debounce, &QTimer::timeout, this, [this]() { reload(); });

    // A rename-into-place drops the watch on the old inode, so every event
    // re-arms the whole watch list rather than trusting it to survive.
    QObject::connect(&m_watcher,
                     &QFileSystemWatcher::fileChanged,
                     this,
                     [this](const QString&) { schedule(); });
    QObject::connect(&m_watcher,
                     &QFileSystemWatcher::directoryChanged,
                     this,
                     [this](const QString&) { schedule(); });

    QObject::connect(&m_engine,
                     &QQmlApplicationEngine::objectCreated,
                     this,
                     [this](QObject* object, const QUrl&) {
                       if (object != nullptr)
                         m_root = object;
                     });

    watchSources();
  }

private:
  void schedule()
  {
    watchSources();
    m_debounce.start();
  }

  void watchSources()
  {
    QStringList paths;
    paths << m_sourceDir;
    const QDir directory(m_sourceDir);
    const QStringList filters{ QStringLiteral("*.qml"), QStringLiteral("*.js") };
    for (const QFileInfo& entry :
         directory.entryInfoList(filters, QDir::Files, QDir::Name))
      paths << entry.absoluteFilePath();

    const QStringList watched = m_watcher.files() + m_watcher.directories();
    for (const QString& path : paths) {
      if (!watched.contains(path))
        m_watcher.addPath(path);
    }
  }

  void reload()
  {
    QObject* const previous = m_root.data();

    // The window is rebuilt from scratch, so the one piece of state worth
    // carrying across is which project was open: without it every reload drops
    // the developer back on the launcher, which is what restarting already did.
    QVariantMap initial;
    if (previous != nullptr) {
      const QString opened = previous->property("openedProjectId").toString();
      if (!opened.isEmpty())
        initial.insert(QStringLiteral("restoreProjectId"), opened);
    }

    m_engine.clearComponentCache();
    m_engine.setInitialProperties(initial);
    m_engine.load(m_rootUrl);

    // The replacement is built before the old window is dropped, so a file
    // saved half-written leaves the running window alone: the engine prints
    // the parse error and there is still something on screen to fix it from.
    // Dropping the old window first would instead close the last window in the
    // application, which takes the process down with it.
    if (m_root.data() == previous) {
      qWarning().noquote()
        << "harkness-gui: QML reload failed; keeping the running window";
      return;
    }
    // Deleted outright rather than deferred: the two windows overlap until it
    // goes. QQmlApplicationEngine drops destroyed roots from its own list.
    delete previous;
    qInfo().noquote() << "harkness-gui: reloaded QML from" << m_sourceDir;
  }

  QQmlApplicationEngine& m_engine;
  QmlSourceInterceptor m_interceptor;
  QFileSystemWatcher m_watcher;
  QTimer m_debounce;
  QPointer<QObject> m_root;
  QString m_sourceDir;
  QUrl m_rootUrl;
};

/// Installs the interceptor and the watcher on `engine`, redirecting resource
/// paths under `modulePrefix` to `sourceDir`. Returns false, having changed
/// nothing, when `sourceDir` is not a directory — which is what an installed
/// build sees.
///
/// Must be called before the engine loads anything, and the engine takes
/// ownership of what it creates.
inline bool
installQmlHotReload(QQmlApplicationEngine& engine,
                    const QString& modulePrefix,
                    const QString& sourceDir,
                    const QUrl& rootUrl)
{
  const QFileInfo info(sourceDir);
  if (!info.isDir())
    return false;
  const QString resolved = info.absoluteFilePath();
  new QmlHotReloader(engine, modulePrefix, resolved, rootUrl);
  qInfo().noquote() << "harkness-gui: QML hot reload watching" << resolved;
  return true;
}

} // namespace harkness
