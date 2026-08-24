mod approval_model;
mod backend;
mod changes_model;
mod file_tree_model;
pub(crate) mod hotreload;
mod reconcile;
mod run_list_model;
mod run_timeline_model;
pub(crate) mod runs_backend;

use cxx_qt_lib::{QGuiApplication, QQmlApplicationEngine, QString, QUrl};

/// Root of the statically compiled QML module.
const MAIN_QML_URL: &str = "qrc:/qt/qml/io/github/fullstacktaiye/harkness/qml/Main.qml";

fn main() {
    // Force-links the statically compiled QML module so its types register.
    cxx_qt::init_qml_module!("io.github.fullstacktaiye.harkness");

    let mut app = QGuiApplication::new();
    let mut engine = QQmlApplicationEngine::new();

    QGuiApplication::set_desktop_file_name(&QString::from("io.github.fullstacktaiye.harkness"));
    if let Some(mut app) = app.as_mut() {
        app.as_mut()
            .set_application_name(&QString::from("io.github.fullstacktaiye.harkness"));
        app.as_mut()
            .set_application_display_name(&QString::from("Harkness"));
    }

    if let Some(mut engine) = engine.as_mut() {
        // Only applies to a binary still sitting beside the QML it was built
        // from; it has to be installed before the first load, because the
        // interceptor it adds is what redirects that load to disk.
        hotreload::install(engine.as_mut(), MAIN_QML_URL);
        engine.as_mut().load(&QUrl::from(MAIN_QML_URL));
    }

    if let Some(app) = app.as_mut() {
        app.exec();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{
        fs,
        path::Path,
        ptr,
        sync::atomic::{AtomicBool, AtomicPtr, Ordering},
    };

    use cxx_qt::QObject;
    use cxx_qt_lib::{QByteArray, QGuiApplication, QObjectExt, QQmlApplicationEngine, QUrl};
    use git2::{Repository, Signature};
    use tempfile::TempDir;

    /// The runs one seeded store holds, by what each of them is there to prove.
    #[allow(dead_code)]
    pub(crate) struct SeededRuns {
        failed: String,
        waiting: String,
        interrupted: String,
        running: String,
        paged: String,
        progressed: String,
        /// A run parked on a request that asked for a run-wide breadth, so the
        /// decision surface has two scopes to choose between.
        widened: String,
        /// The approval identifier of that request, which the approval page is
        /// opened by rather than found through.
        widened_approval: String,
        /// A run parked on a remote write, which the risk ceiling reduced to a
        /// single call however wide it asked.
        remote: String,
        remote_approval: String,
        /// A run parked on a request whose deadline for an answer has passed.
        lapsed: String,
        lapsed_approval: String,
        /// A run somebody refused, with the reason they typed.
        refused: String,
        refused_approval: String,
    }

    /// Events a run needs before its timeline can be said to page.
    ///
    /// Six pages of the model's own `TIMELINE_PAGE_SIZE`, so the newest page is
    /// full, backwards paging has somewhere to go, and the count is far enough
    /// above a screenful that a delegate count near it would mean the view had
    /// materialized the whole log.
    #[allow(dead_code)]
    const PAGED_RUN_EVENTS: usize = 1200;

    /// Progress ticks one call reports in a row.
    #[allow(dead_code)]
    const FOLDED_PROGRESS_TICKS: usize = 100;

    /// Records the runs the run-surface fixtures render.
    ///
    /// Written through the real store rather than through hand-built JSON: the
    /// point of these tests is that the pages render what the runtime persists,
    /// and a fixture that skipped the domain's validation could describe a run
    /// that cannot exist.
    #[allow(dead_code)]
    fn seed_run_fixtures(data_dir: &Path) -> SeededRuns {
        use harkness_runtime::approval::{
            ApprovalDecision, ApprovalRequest, ApprovalScope, DecidedVia, PendingApproval,
            WorkspaceBinding, canonical_input_hash,
        };
        use harkness_runtime::domain::{
            ExecutionState, Failure, Run, RunId, Step, Task, TaskId, ToolCall, ToolCallState,
        };
        use harkness_runtime::store::{EventKind, RunEvent, Store};
        use harkness_runtime::tool::{Capability, RiskLevel, ToolIdentity};
        use serde_json::json;
        use std::io::Write as _;
        use time::OffsetDateTime;

        let at =
            |seconds: i64| OffsetDateTime::from_unix_timestamp(1_755_000_000 + seconds).unwrap();
        let store = Store::open(data_dir).unwrap();
        let task = Task::with_id(
            TaskId::new(),
            "Check: cargo test --workspace",
            data_dir.join("workspace"),
            None,
            at(0),
        );
        store.insert_task(&task).unwrap();

        // --- A finished run that failed, with everything a page can show ----
        let failed = Run::with_id(RunId::new(), task.id(), at(1));
        store.insert_run(&failed).unwrap();
        store
            .transition_run(failed.id(), ExecutionState::Running, at(2))
            .unwrap();
        let step = Step::new(failed.id(), 0, "run the check", at(3));
        store.insert_step(&step).unwrap();
        let read = ToolCall::new(
            &step,
            "fs.read_file",
            "1.0.0",
            json!({"path": "Cargo.toml"}),
            at(4),
        );
        store.insert_tool_call(&read).unwrap();
        store
            .transition_tool_call(read.id(), ToolCallState::Running, at(5))
            .unwrap();
        store
            .succeed_tool_call(read.id(), json!({"bytes": 42}), at(6))
            .unwrap();
        let exec = ToolCall::new(
            &step,
            "process.exec",
            "1.0.0",
            json!({"argv": ["cargo", "test"]}),
            at(7),
        );
        store.insert_tool_call(&exec).unwrap();
        store
            .transition_tool_call(exec.id(), ToolCallState::Running, at(8))
            .unwrap();
        store
            .fail_tool_call(
                exec.id(),
                Failure::new("tool_failed", MARKUP_MESSAGE),
                at(9),
            )
            .unwrap();
        // Three artifacts: one small and textual, one nobody may render inline,
        // and one whose bytes are removed after the row is written.
        let mut log = store
            .create_artifact(failed.id(), "stdout.log", "text/plain", at(10))
            .unwrap()
            .for_step(step.id())
            .for_tool_call(exec.id());
        log.write_all(b"running 1 test\ntest failed\n").unwrap();
        let log = log.finish().unwrap();
        let mut binary = store
            .create_artifact(failed.id(), "core", "application/octet-stream", at(11))
            .unwrap();
        binary.write_all(&[0, 159, 146, 150]).unwrap();
        binary.finish().unwrap();
        let mut gone = store
            .create_artifact(failed.id(), "removed.log", "text/plain", at(12))
            .unwrap();
        gone.write_all(b"deleted out from under the row\n").unwrap();
        let gone = gone.finish().unwrap();
        fs::remove_file(
            data_dir
                .join("artifacts")
                .join(failed.id().to_string())
                .join(gone.id().to_string()),
        )
        .unwrap();
        store
            .append_events(
                failed.id(),
                [
                    RunEvent::new(EventKind::RunStateChanged, at(2))
                        .with_payload(json!({"state": "running"})),
                    RunEvent::new(EventKind::AgentAction, at(3))
                        .with_payload(json!({"action": "call_tool", "tool": "fs.read_file"})),
                    RunEvent::new(EventKind::StepStarted, at(3)).for_step(step.id()),
                    RunEvent::new(EventKind::PolicyDecision, at(4))
                        .for_step(step.id())
                        .for_tool_call(read.id())
                        .with_payload(json!({"verdict": "allow", "source": "built_in"})),
                    RunEvent::new(EventKind::ToolCallStateChanged, at(6))
                        .for_step(step.id())
                        .for_tool_call(read.id())
                        .with_payload(json!({"state": "succeeded"})),
                    RunEvent::new(EventKind::ToolProgress, at(8))
                        .for_step(step.id())
                        .for_tool_call(exec.id())
                        .with_payload(json!({"line": "compiling harkness-gui"})),
                    RunEvent::new(EventKind::ArtifactCreated, at(10))
                        .for_step(step.id())
                        .for_tool_call(exec.id())
                        .for_artifact(log.id())
                        .with_payload(json!({"name": "stdout.log", "bytes": 26})),
                    // The event the untrusted-text criterion is about: markup,
                    // an ampersand, and a control character, kept verbatim.
                    RunEvent::new(EventKind::Diagnostic, at(11))
                        .with_payload(json!({"message": MARKUP_MESSAGE})),
                    RunEvent::new(EventKind::SnapshotCaptured, at(12))
                        .with_payload(json!({"digest": "0f0f"})),
                    // A kind this build does not define, which must render as an
                    // entry rather than being dropped.
                    RunEvent::new(EventKind::parse("from_a_later_build"), at(12))
                        .with_payload(json!({"whatever": true})),
                    RunEvent::new(EventKind::StepFinished, at(13))
                        .for_step(step.id())
                        .with_payload(json!({"state": "failed"})),
                    RunEvent::new(EventKind::RunStateChanged, at(13))
                        .with_payload(json!({"state": "failed"})),
                ],
            )
            .unwrap();
        store
            .fail_step(
                step.id(),
                Failure::new("tool_failed", MARKUP_MESSAGE),
                at(13),
            )
            .unwrap();
        store
            .fail_run(
                failed.id(),
                Failure::new("tool_failed", MARKUP_MESSAGE),
                at(13),
            )
            .unwrap();

        // --- A run parked on an unanswered approval -------------------------
        let waiting = Run::with_id(RunId::new(), task.id(), at(20));
        store.insert_run(&waiting).unwrap();
        store
            .transition_run(waiting.id(), ExecutionState::Running, at(21))
            .unwrap();
        let waiting_step = Step::new(waiting.id(), 0, "run the check", at(22));
        store.insert_step(&waiting_step).unwrap();
        let waiting_call = ToolCall::new(
            &waiting_step,
            "process.exec",
            "1.0.0",
            json!({"argv": ["cargo", "test"]}),
            at(23),
        );
        store.insert_tool_call(&waiting_call).unwrap();
        store
            .transition_tool_call(waiting_call.id(), ToolCallState::AwaitingApproval, at(24))
            .unwrap();
        store
            .open_approval(
                ApprovalRequest::open(
                    PendingApproval::new(
                        waiting.id(),
                        waiting_call.id(),
                        ToolIdentity::parse("process.exec", "1.0.0").unwrap(),
                        canonical_input_hash(&json!({"argv": ["cargo", "test"]})).unwrap(),
                        WorkspaceBinding::new(None, data_dir.join("workspace")),
                        RiskLevel::Execute,
                        at(24),
                    )
                    .summarized_as("cargo test --workspace")
                    .with_capabilities([Capability::new("process.spawn").unwrap()]),
                )
                .unwrap(),
            )
            .unwrap();
        store
            .transition_run(waiting.id(), ExecutionState::WaitingForApproval, at(25))
            .unwrap();

        // --- A run whose owning process stopped mid-call --------------------
        let interrupted = Run::with_id(RunId::new(), task.id(), at(30));
        store.insert_run(&interrupted).unwrap();
        store
            .transition_run(interrupted.id(), ExecutionState::Running, at(31))
            .unwrap();
        let interrupted_step = Step::new(interrupted.id(), 0, "run the check", at(32));
        store.insert_step(&interrupted_step).unwrap();
        let interrupted_call = ToolCall::new(
            &interrupted_step,
            "process.exec",
            "1.0.0",
            json!({"argv": ["cargo", "test"]}),
            at(33),
        );
        store.insert_tool_call(&interrupted_call).unwrap();
        store
            .transition_tool_call(interrupted_call.id(), ToolCallState::Running, at(34))
            .unwrap();
        store
            .transition_tool_call(interrupted_call.id(), ToolCallState::Interrupted, at(35))
            .unwrap();
        store
            .append_event(
                interrupted.id(),
                RunEvent::new(EventKind::RunInterrupted, at(35))
                    .with_payload(json!({"reason": "lease_released"})),
            )
            .unwrap();
        store
            .transition_run(interrupted.id(), ExecutionState::Interrupted, at(35))
            .unwrap();

        // --- A run still executing, which nothing may re-attempt ------------
        let running = Run::with_id(RunId::new(), task.id(), at(40));
        store.insert_run(&running).unwrap();
        store
            .transition_run(running.id(), ExecutionState::Running, at(41))
            .unwrap();
        let running_step = Step::new(running.id(), 0, "run the check", at(42));
        store.insert_step(&running_step).unwrap();
        let running_call = ToolCall::new(
            &running_step,
            "process.exec",
            "1.0.0",
            json!({"argv": ["cargo", "test", "--workspace"]}),
            at(43),
        );
        store.insert_tool_call(&running_call).unwrap();
        store
            .transition_tool_call(running_call.id(), ToolCallState::Running, at(44))
            .unwrap();
        store
            .append_events(
                running.id(),
                [
                    RunEvent::new(EventKind::RunStateChanged, at(41))
                        .with_payload(json!({"state": "running"})),
                    RunEvent::new(EventKind::StepStarted, at(42)).for_step(running_step.id()),
                    RunEvent::new(EventKind::ToolCallStateChanged, at(44))
                        .for_step(running_step.id())
                        .for_tool_call(running_call.id())
                        .with_payload(json!({"state": "running"})),
                ],
            )
            .unwrap();
        store
            .append_events(
                running.id(),
                (0..8).map(|index| {
                    RunEvent::new(EventKind::ToolProgress, at(45))
                        .for_step(running_step.id())
                        .for_tool_call(running_call.id())
                        .with_payload(json!({"line": format!("Compiling harkness-crate-{index}")}))
                }),
            )
            .unwrap();

        // --- A run long enough that its timeline has to page ----------------
        let paged = Run::with_id(RunId::new(), task.id(), at(50));
        store.insert_run(&paged).unwrap();
        store
            .transition_run(paged.id(), ExecutionState::Running, at(51))
            .unwrap();
        store
            .append_events(
                paged.id(),
                (0..PAGED_RUN_EVENTS).map(|index| {
                    RunEvent::new(EventKind::Diagnostic, at(52))
                        .with_payload(json!({"index": index}))
                }),
            )
            .unwrap();
        store
            .transition_run(paged.id(), ExecutionState::Succeeded, at(53))
            .unwrap();

        // --- A run whose one call reported a hundred progress lines ---------
        let progressed = Run::with_id(RunId::new(), task.id(), at(60));
        store.insert_run(&progressed).unwrap();
        store
            .transition_run(progressed.id(), ExecutionState::Running, at(61))
            .unwrap();
        let progress_step = Step::new(progressed.id(), 0, "run the check", at(62));
        store.insert_step(&progress_step).unwrap();
        let progress_call = ToolCall::new(
            &progress_step,
            "process.exec",
            "1.0.0",
            json!({"argv": ["cargo", "build"]}),
            at(63),
        );
        store.insert_tool_call(&progress_call).unwrap();
        store
            .append_event(
                progressed.id(),
                RunEvent::new(EventKind::StepStarted, at(62)).for_step(progress_step.id()),
            )
            .unwrap();
        store
            .append_events(
                progressed.id(),
                (0..FOLDED_PROGRESS_TICKS).map(|index| {
                    RunEvent::new(EventKind::ToolProgress, at(63))
                        .for_step(progress_step.id())
                        .for_tool_call(progress_call.id())
                        .with_payload(json!({"line": format!("compiling crate {index}")}))
                }),
            )
            .unwrap();
        store
            .transition_run(progressed.id(), ExecutionState::Succeeded, at(64))
            .unwrap();

        // --- Four more parked runs, one per shape a decision can take ------
        //
        // Each is the same construction as `waiting` above and differs in
        // exactly the field the surface has to react to: the breadth asked
        // for, the risk ceiling that overrides it, a deadline that has passed,
        // and an answer that was actually given.
        let parked = |seconds: i64,
                      tool: &str,
                      risk: RiskLevel,
                      scope: ApprovalScope,
                      input: serde_json::Value,
                      summary: &str,
                      capability: &str,
                      expires: Option<i64>| {
            let run = Run::with_id(RunId::new(), task.id(), at(seconds));
            store.insert_run(&run).unwrap();
            store
                .transition_run(run.id(), ExecutionState::Running, at(seconds + 1))
                .unwrap();
            let step = Step::new(run.id(), 0, "do the work", at(seconds + 2));
            store.insert_step(&step).unwrap();
            let call = ToolCall::new(&step, tool, "1.0.0", input.clone(), at(seconds + 3));
            store.insert_tool_call(&call).unwrap();
            store
                .transition_tool_call(call.id(), ToolCallState::AwaitingApproval, at(seconds + 4))
                .unwrap();
            let mut request = PendingApproval::new(
                run.id(),
                call.id(),
                ToolIdentity::parse(tool, "1.0.0").unwrap(),
                canonical_input_hash(&input).unwrap(),
                WorkspaceBinding::new(None, data_dir.join("workspace")),
                risk,
                at(seconds + 4),
            )
            .requesting(scope)
            .summarized_as(summary)
            .with_capabilities([Capability::new(capability).unwrap()]);
            if let Some(deadline) = expires {
                request = request.expiring_at(at(deadline));
            }
            let opened = ApprovalRequest::open(request).unwrap();
            store.open_approval(opened.clone()).unwrap();
            store
                .transition_run(
                    run.id(),
                    ExecutionState::WaitingForApproval,
                    at(seconds + 5),
                )
                .unwrap();
            (run.id().to_string(), opened.id().to_string())
        };

        // A workspace write that asked to cover its capability for the whole
        // run. Nothing narrows it, so the surface has a real choice to offer —
        // and the input carries the markup the untrusted-text criterion is
        // about, so the raw rendering has something to fail to interpret.
        let (widened, widened_approval) = parked(
            70,
            "fs.apply_patch",
            RiskLevel::WorkspaceWrite,
            ApprovalScope::CapabilityForRun,
            json!({"path": "src/lib.rs", "patch": MARKUP_MESSAGE}),
            "Apply a patch to src/lib.rs (+12 -3)",
            "fs.write",
            None,
        );

        // A remote write that asked for the same breadth. The ceiling reduces
        // it to one call when the request is created, so the record keeps both
        // spellings and the surface renders no choice at all.
        let (remote, remote_approval) = parked(
            80,
            "git.push",
            RiskLevel::RemoteWrite,
            ApprovalScope::CapabilityForRun,
            json!({"remote": "origin", "branch": "main", "force": true}),
            "Force-push main to origin",
            "network",
            None,
        );

        // A deadline that has already passed. The row is still `pending` —
        // only a sweeper closes one — so this is the case where the clock and
        // the record disagree and the surface has to follow the clock.
        let (lapsed, lapsed_approval) = parked(
            90,
            "process.exec",
            RiskLevel::Execute,
            ApprovalScope::ExactCall,
            json!({"argv": ["cargo", "publish"]}),
            "cargo publish",
            "process.spawn",
            Some(95),
        );

        // --- A run somebody refused, with the reason they gave --------------
        //
        // Written through `Store::decide_approval`, which is the same call the
        // bridge makes, so the record and its `approval_decided` event are the
        // ones a real refusal produces rather than a hand-built row.
        let refused = Run::with_id(RunId::new(), task.id(), at(100));
        store.insert_run(&refused).unwrap();
        store
            .transition_run(refused.id(), ExecutionState::Running, at(101))
            .unwrap();
        let refused_step = Step::new(refused.id(), 0, "publish the crate", at(102));
        store.insert_step(&refused_step).unwrap();
        let refused_input = json!({"argv": ["cargo", "publish"]});
        let refused_call = ToolCall::new(
            &refused_step,
            "process.exec",
            "1.0.0",
            refused_input.clone(),
            at(103),
        );
        store.insert_tool_call(&refused_call).unwrap();
        store
            .transition_tool_call(refused_call.id(), ToolCallState::AwaitingApproval, at(104))
            .unwrap();
        let refused_request = ApprovalRequest::open(
            PendingApproval::new(
                refused.id(),
                refused_call.id(),
                ToolIdentity::parse("process.exec", "1.0.0").unwrap(),
                canonical_input_hash(&refused_input).unwrap(),
                WorkspaceBinding::new(None, data_dir.join("workspace")),
                RiskLevel::Execute,
                at(104),
            )
            .summarized_as("cargo publish")
            .with_capabilities([Capability::new("process.spawn").unwrap()]),
        )
        .unwrap();
        store.open_approval(refused_request.clone()).unwrap();
        store
            .decide_approval(
                refused_request.id(),
                ApprovalDecision::deny(refused_request.id(), DecidedVia::Gui, at(105))
                    .because(DENIAL_REASON),
            )
            .unwrap();
        store
            .reject_tool_call_approval(
                refused_call.id(),
                "gui",
                Failure::new("approval_denied", "a person refused this call"),
                at(106),
            )
            .unwrap();
        store
            .fail_run(
                refused.id(),
                Failure::new("approval_denied", "a person refused this call"),
                at(106),
            )
            .unwrap();

        drop(store);
        SeededRuns {
            failed: failed.id().to_string(),
            waiting: waiting.id().to_string(),
            interrupted: interrupted.id().to_string(),
            running: running.id().to_string(),
            paged: paged.id().to_string(),
            progressed: progressed.id().to_string(),
            widened,
            widened_approval,
            remote,
            remote_approval,
            lapsed,
            lapsed_approval,
            refused: refused.id().to_string(),
            refused_approval: refused_request.id().to_string(),
        }
    }

    /// The reason the seeded refusal records, which the surface has to show
    /// back verbatim.
    #[allow(dead_code)]
    const DENIAL_REASON: &str = "not on a release branch";

    /// A failure message carrying the three things a renderer must not act on:
    /// markup, an entity-shaped ampersand, and a control character.
    #[allow(dead_code)]
    const MARKUP_MESSAGE: &str = "<b>alert</b> & \u{7}the tool exited 1";

    /// Drives the run surfaces against a seeded store under a real event loop.
    ///
    /// Its own test binary rather than another block of `main_qml_loads`,
    /// because every read these pages perform is asynchronous by construction —
    /// a store open is a blocking call and belongs on a worker — so nothing here
    /// can be checked without `exec()` running. `default_branch_push_repro` is
    /// the same shape for the same reason.
    ///
    /// The fixture reports through `objectName`, the convention the other QML
    /// checks in this file use: a failure names every check that did not hold
    /// rather than stopping at the first.
    #[allow(dead_code)]
    pub(crate) fn run_surfaces() {
        let fixture = TempDir::new().unwrap();
        let data_dir = fixture.path().join("data");

        // SAFETY: set before any Qt object is constructed, and this binary runs
        // single-threaded with respect to Qt and environment usage.
        unsafe {
            std::env::set_var("QT_QPA_PLATFORM", "offscreen");
            std::env::set_var("QT_FORCE_STDERR_LOGGING", "1");
            std::env::set_var("QT_FATAL_WARNINGS", "1");
            std::env::set_var("HARKNESS_DATA_DIR", &data_dir);
        }
        let seeded = seed_run_fixtures(&data_dir);

        cxx_qt::init_qml_module!("io.github.fullstacktaiye.harkness");
        let mut app = QGuiApplication::new();
        let mut engine = QQmlApplicationEngine::new();

        static LOADED: AtomicBool = AtomicBool::new(false);
        static ROOT: AtomicPtr<QObject> = AtomicPtr::new(ptr::null_mut());
        if let Some(mut engine) = engine.as_mut() {
            let _connection = engine.as_mut().on_object_created(|_engine, object, _url| {
                LOADED.store(!object.is_null(), Ordering::SeqCst);
                ROOT.store(object, Ordering::SeqCst);
            });
            let qml = String::from_utf8(RUN_SURFACES_QML.to_vec())
                .unwrap()
                // Set by `docs/screenshots`' regeneration and by nothing else;
                // the checks below run either way, so a screenshot pass is the
                // same evidence with images taken along the way.
                .replace(
                    "__SCREENSHOT_DIR__",
                    &std::env::var("HARKNESS_RUN_SCREENSHOT_DIR")
                        .unwrap_or_default()
                        .replace('\\', "\\\\")
                        .replace('"', "\\\""),
                )
                .replace("__FAILED_RUN__", &seeded.failed)
                .replace("__WAITING_RUN__", &seeded.waiting)
                .replace("__INTERRUPTED_RUN__", &seeded.interrupted)
                .replace("__RUNNING_RUN__", &seeded.running)
                .replace("__PAGED_RUN__", &seeded.paged)
                .replace("__PROGRESSED_RUN__", &seeded.progressed)
                .replace("__WIDENED_RUN__", &seeded.widened)
                .replace("__WIDENED_APPROVAL__", &seeded.widened_approval)
                .replace("__REMOTE_RUN__", &seeded.remote)
                .replace("__REMOTE_APPROVAL__", &seeded.remote_approval)
                .replace("__LAPSED_RUN__", &seeded.lapsed)
                .replace("__LAPSED_APPROVAL__", &seeded.lapsed_approval)
                .replace("__REFUSED_RUN__", &seeded.refused)
                .replace("__REFUSED_APPROVAL__", &seeded.refused_approval)
                .replace("__DENIAL_REASON__", DENIAL_REASON);
            engine.as_mut().load_data(
                &QByteArray::from(qml.as_bytes()),
                &QUrl::from("qrc:/RunSurfaces.qml"),
            );
        }
        assert!(
            LOADED.load(Ordering::SeqCst),
            "the run-surface fixture failed to load; see QML warnings above"
        );
        if let Some(app) = app.as_mut() {
            app.exec();
        }
        let name = unsafe { ROOT.load(Ordering::SeqCst).as_ref() }
            .map(|object| object.object_name().to_string())
            .unwrap_or_default();
        assert_eq!(name, "RunSurfacesPassed", "run-surface checks failed");
    }

    /// The run-surface fixture, with one run identifier substituted per marker.
    #[allow(dead_code)]
    const RUN_SURFACES_QML: &[u8] = br#"
import QtQuick
import org.kde.kirigami as Kirigami
import io.github.fullstacktaiye.harkness

Kirigami.ApplicationWindow {
    id: window

    objectName: "RunSurfacesPending"
    visible: true
    width: 1180
    height: 820

    pageStack.columnView.columnResizeMode: Kirigami.ColumnView.SingleColumn
    pageStack.globalToolBar.style: Kirigami.ApplicationHeaderStyle.None

    property var failures: []
    property int phase: 0
    property int settled: 0
    property int pendingGrabs: 0
    property bool grabbing: false
    property var openedPage: null
    property string excerptableArtifact: ""
    property bool overlapIssued: false
    property bool narrowed: false
    property bool cancelPressed: false
    property int cancelPolls: 0
    /// The review surface currently built, or null. Held rather than declared,
    /// so a phase can destroy one and prove that destroying it decides nothing.
    property var approvalPage: null
    property bool scopeWidened: false
    /// Milliseconds between pressing Cancel and the control saying so.
    property real cancelResponse: -1
    property bool approvalPressed: false
    property bool denyIssued: false

    readonly property int wideWidth: 1180
    /// Narrower than the two halves of the detail page's body can both be, so
    /// the split has to turn rather than clip one of them.
    readonly property int narrowWidth: Kirigami.Units.gridUnit * 30

    /// Where to write the pages as images, or empty to write none.
    ///
    /// A screenshot pass runs the same checks; it only waits a few frames
    /// longer at each page so what it grabs is laid out rather than half bound.
    readonly property string screenshotDir: "__SCREENSHOT_DIR__"
    readonly property bool capturing: screenshotDir.length > 0

    /// Whether this page has been written out, and true at once when nothing is
    /// being captured.
    ///
    /// Called last in a phase, after its checks: `grabToImage` renders on a
    /// later frame, so a phase that moved on as soon as it asked for an image
    /// would save the page it moved *to*.
    function captured(name, item) {
        if (!capturing)
            return true;
        if (!grabbing) {
            grabbing = true;
            pendingGrabs += 1;
            item.grabToImage(function (result) {
                if (!result.saveToFile(screenshotDir + "/" + name + ".png"))
                    check("wroteTheScreenshot_" + name, false);
                pendingGrabs -= 1;
            });
            return false;
        }
        if (pendingGrabs > 0)
            return false;
        grabbing = false;
        return true;
    }

    /// Whether the page in front of the fixture has had time to lay out.
    ///
    /// Only a capturing pass waits: an ordinary run has nothing to look at and
    /// pays seven times this delay for nothing.
    function composed() {
        if (!capturing)
            return true;
        if (settled < 6) {
            settled += 1;
            return false;
        }
        return true;
    }

    readonly property string failedRun: "__FAILED_RUN__"
    readonly property string waitingRun: "__WAITING_RUN__"
    readonly property string interruptedRun: "__INTERRUPTED_RUN__"
    readonly property string runningRun: "__RUNNING_RUN__"
    readonly property string pagedRun: "__PAGED_RUN__"
    readonly property string progressedRun: "__PROGRESSED_RUN__"
    readonly property string widenedRun: "__WIDENED_RUN__"
    readonly property string widenedApproval: "__WIDENED_APPROVAL__"
    readonly property string remoteRun: "__REMOTE_RUN__"
    readonly property string remoteApproval: "__REMOTE_APPROVAL__"
    readonly property string lapsedRun: "__LAPSED_RUN__"
    readonly property string lapsedApproval: "__LAPSED_APPROVAL__"
    readonly property string refusedRun: "__REFUSED_RUN__"
    readonly property string refusedApproval: "__REFUSED_APPROVAL__"
    readonly property string denialReason: "__DENIAL_REASON__"

    /// Whether the review surface is built and has read its own run.
    readonly property bool approvalReady: window.approvalPage !== null
        && window.approvalPage.ready && window.approvalPage.runReady

    readonly property var detail: detailPage
    readonly property bool detailReady: detail.runId === window.currentRun && detail.ready
        && detail.timelineView.count > 0

    function check(name, passed) {
        // A phase is re-entered while it waits for its image to be written, so
        // a failing check is recorded once rather than once per poll.
        if (!passed && failures.indexOf(name) === -1)
            failures.push(name);
    }

    function finish() {
        poll.stop();
        deadline.stop();
        window.objectName = failures.length === 0
            ? "RunSurfacesPassed"
            : "RunSurfacesFailed[" + failures.join(",") + "]";
        Qt.quit();
    }

    /// The run the detail page is pointed at.
    ///
    /// Every route to a run - the shell's Runs view, the launcher's list, and
    /// a re-attempt link - goes through `showRun`, so one detail page shows
    /// whichever run was named last. Main.qml pushes a page per run instead;
    /// what both share, and what the criterion is about, is that naming a run
    /// from either entry point lands on one RunDetailPage for that run rather
    /// than on two.
    property string currentRun: ""

    function showRun(runId) {
        const id = String(runId || "");
        if (id.length === 0)
            return;
        window.currentRun = id;
    }

    /// Main.qml's `showApproval` for this fixture's benefit.
    ///
    /// Built rather than declared, because what the closing-never-grants
    /// criterion is about is the page going away: an `ApprovalPage` that was
    /// only hidden would still be holding its bridge, and hiding is not what a
    /// reader does when they navigate back.
    function showApproval(approvalId, runId, seed) {
        closeApproval();
        window.approvalPage = approvalPageComponent.createObject(approvalHost, {
            "approvalId": String(approvalId),
            "runId": String(runId),
            "seed": seed !== undefined ? seed : null
        });
    }

    /// Leaves the review surface, which is all that leaving it does.
    function closeApproval() {
        if (window.approvalPage !== null) {
            window.approvalPage.destroy();
            window.approvalPage = null;
        }
    }

    pageStack.initialPage: Kirigami.Page {
        padding: 0

        RunState {
            id: vocabulary
        }

        // The launcher's recent-runs body, wired the way the launcher wires it.
        RunListPane {
            id: launcherPane

            anchors.fill: parent
            compact: true
            visible: window.currentRun.length === 0
            onRunActivated: runId => window.showRun(runId)
        }

        RunDetailPage {
            id: detailPage

            anchors.fill: parent
            runId: window.currentRun
            visible: window.currentRun.length > 0
        }

        // Where `showApproval` builds its page. A real window pushes one onto
        // the stack; this fixture parents it into a visible item instead, for
        // the reason phase 15 gives about Kirigami's own push.
        Item {
            id: approvalHost

            anchors.fill: parent
            visible: window.approvalPage !== null
            // Above the detail page rather than instead of it. `visible` is
            // inherited by children, so hiding the page underneath would drive
            // every binding on it false - including the ones a phase that has
            // already run asserted against.
            z: 1
        }
    }

    Component {
        id: approvalPageComponent

        ApprovalPage {
            anchors.fill: approvalHost
        }
    }

    // A fourth bridge, used only to re-read the store after a surface has been
    // destroyed. It has to be one this fixture still holds: the page's own
    // bridge went away with it, which is the whole point of the check.
    RunsBackend {
        id: verify
    }

    // A bridge of its own, so the overlapping-load check drives it without
    // disturbing the page the earlier phases asserted against.
    RunsBackend {
        id: overlap
    }

    // The Runs view inside the host the shell puts it in. `runsPanel` above is
    // the view on its own; this is its side-panel *contract* - the activity bar
    // reads these members off it, and a view the host cannot resolve is one the
    // bar cannot switch to.
    SidePanel {
        id: hostedPanel

        anchors.right: parent.right
        anchors.top: parent.top
        currentViewId: "runs"
        height: window.height
        visible: false
        width: Kirigami.Units.gridUnit * 30

        RunsPanel {
            id: hostedRuns
        }
    }

    // Stand-ins for the two pages the navigation check needs in a stack. The
    // shell needs a project catalog behind it and the detail page cannot be
    // pushed here at all - see the check itself - so each is reduced to the
    // property Main.qml actually looks for.
    Component {
        id: shellStandIn

        Kirigami.Page {
            property bool isShell: true
        }
    }

    Component {
        id: detailStandIn

        Kirigami.Page {
            property bool isRunDetail: true
            property string runId: ""
        }
    }

    // Main.qml itself. The window's own `showRun` above is a stand-in for this
    // page's benefit; re-implementing the *stack* behaviour is how the
    // regression Main.qml's version fixes went untested in the first place.
    Component {
        id: mainWindow

        // Shown, because an invisible Window has no scene graph and every item
        // pushed into its stack is reported as a graphical object outside the
        // scene - a warning, which this binary makes fatal. Offscreen, so
        // "shown" costs a surface nobody looks at.
        Main {
            height: 720
            visible: true
            width: 960
        }
    }

    // The shell's Runs view, which reaches a page only through `openRun`.
    RunsPanel {
        id: runsPanel

        anchors.left: parent.left
        anchors.top: parent.top
        height: window.height
        // On screen only while it is the subject: the rest of the fixture is
        // about the detail page, which this would otherwise cover.
        visible: window.phase === 0
        width: Kirigami.Units.gridUnit * 30
    }

    Timer {
        id: poll

        interval: 50
        repeat: true
        running: true
        onTriggered: window.advance()
    }

    Timer {
        id: deadline

        interval: 60000
        repeat: false
        running: true
        onTriggered: {
            window.check("timedOutInPhase" + window.phase, false);
            window.finish();
        }
    }

    /// Moves to the next page, resetting the settling counter with it.
    function next(step, runId) {
        settled = 0;
        grabbing = false;
        phase = step;
        if (runId !== undefined)
            showRun(runId);
    }

    function advance() {
        if (phase === 0) {
            if (launcherPane.loading || launcherPane.count === 0
                    || runsPanel.pendingApprovals === 0 || !composed())
                return;
            check("theLauncherPaneListsEverySeededRun", launcherPane.count === 10);
            // The queue is the one thing in this view that is about the reader
            // rather than about a run, so it is the badge the activity bar
            // carries and the section above the history.
            check("theRunsViewBadgesTheQuestionsWaitingForAPerson",
                  runsPanel.viewBadge === 4 && runsPanel.pendingApprovals === 4);
            check("theRunsViewBuildsARowPerWaitingRequest", runsPanel.pendingRows === 4);
            const queued = runsPanel.pendingRow(0);
            check("aQueueRowNamesTheToolItIsAbout",
                  queued !== null && String(queued.request.tool).length > 0);
            check("aQueueRowCarriesTheBreadthsTheRuntimeWouldAccept",
                  queued !== null && queued.request.grantableScopes !== undefined
                  && queued.request.grantableScopes.length >= 1);
            if (queued !== null && window.approvalPage === null) {
                queued.reviewRequested();
                return;
            }
            check("reviewingFromTheQueueOpensThatRequest",
                  window.approvalPage !== null
                  && window.approvalPage.approvalId === String(queued.request.approvalId));
            window.closeApproval();
            check("theLauncherPaneReportsNoFailure", launcherPane.loadErrorKind.length === 0);
            // The side-panel contract, which the activity bar reads off the
            // view rather than being told: a view the host cannot resolve is
            // one no shortcut and no bar entry can reach.
            check("theRunsViewIsResolvableByTheHostThatShowsIt",
                  hostedPanel.view("runs") === hostedRuns);
            check("theRunsViewIsTheOneTheHostHasOnScreen",
                  hostedPanel.currentPanel === hostedRuns && hostedPanel.currentPanelReady);
            check("theRunsViewAppliesToEveryProjectSoTheHostAlwaysHasOne",
                  hostedPanel.hasAvailableView
                  && hostedPanel.firstAvailableViewId() === "runs");
            check("theRunsViewAdvertisesTheShortcutTheShellBinds",
                  String(hostedRuns.viewShortcut) === "Ctrl+Shift+R");
            if (!captured("runs-list", runsPanel))
                return;
            next(1, failedRun);
            return;
        }

        if (phase === 1) {
            if (!detailReady || !composed())
                return;
            check("aFailedRunNamesItsDiscriminant",
                  String(detail.run.errorKind) === "tool_failed");
            check("aFailedRunCarriesTheToolsOwnMessage",
                  String(detail.run.errorMessage).indexOf("<b>alert</b>") !== -1);
            check("aFailedRunShowsNoApprovalBanner", !detail.approvalBannerVisible);
            check("aFinishedRunNamesNothingInFlight", detail.inFlightCall === null);
            check("aRunListsEveryCallItRecorded", detail.calls.length === 2);
            check("aRunListsEveryArtifactItRecorded", detail.artifacts.length === 3);
            check("aRunListsNoApprovalItNeverAsked", detail.approvals.length === 0);
            check("aFailedRunOffersARetry", detail.retryable);
            check("aFinishedRunOffersNoCancellation", !detail.cancellable);
            check("theTimelineHoldsEveryRecordedEvent", detail.timelineView.count === 12);
            check("nothingWasCutFromThisRunsProjection", detail.truncated.length === 0);

            let available = 0;
            let missing = 0;
            let excerptable = 0;
            for (let index = 0; index < detail.artifacts.length; ++index) {
                const artifact = detail.artifacts[index];
                if (String(artifact.availability) === "available")
                    ++available;
                if (String(artifact.availability) === "missing")
                    ++missing;
                if (artifact.excerptable === true) {
                    ++excerptable;
                    window.excerptableArtifact = String(artifact.artifactId);
                }
                check("everyArtifactRowNamesItsMediaType",
                      String(artifact.mediaType).length > 0);
                check("everyArtifactRowNamesWhereItsBytesAre",
                      String(artifact.path).indexOf("artifacts") !== -1);
            }
            check("aDeletedArtifactRendersAsUnavailable", missing === 1);
            check("thePresentArtifactsStillRead", available === 2);
            check("onlySmallTextIsOfferedInline", excerptable === 1);

            // Opening a row is the only way a payload is ever read, so the
            // check is what the reader does: expand the row, ask for its
            // payload, and wait for it to arrive on that row alone.
            const row = detailPage.timelineView.itemAtIndex(0);
            check("theTimelineCreatesDelegatesForTheRowsOnScreen", row !== null);
            if (row !== null && !row.expanded) {
                row.expanded = true;
                detailPage.timelineView.model.loadDetail(row.seq);
                return;
            }
            if (row !== null && row.detail.length === 0)
                return;
            check("openingATimelineRowLoadsThatRowsPayload",
                  row !== null && row.detail.indexOf("state") !== -1);

            // The artifacts section, which no other phase reaches: its rows
            // are the one place this page renders bytes a tool wrote, so the
            // delegates have to be built at least once under QT_FATAL_WARNINGS
            // and the read behind them driven end to end.
            if (detailPage.section !== 1) {
                detailPage.section = 1;
                return;
            }
            check("theArtifactsSectionBuildsARowPerArtifact",
                  detailPage.artifactView.count === 3);
            if (detailPage.openArtifact.length === 0) {
                detailPage.showArtifact(window.excerptableArtifact);
                return;
            }
            if (detailPage.openArtifactText.length === 0)
                return;
            check("showingAnArtifactRendersTheBytesTheRunStored",
                  detailPage.openArtifactText.indexOf("running 1 test") !== -1);
            check("aRenderingInsideTheBudgetIsNotReportedAsCut",
                  !detailPage.openArtifactCut);
            // One identifier, so a second artifact cannot also be open; naming
            // the open one again is how the row's single control hides it.
            detailPage.showArtifact(window.excerptableArtifact);
            check("namingTheOpenArtifactAgainClosesIt",
                  detailPage.openArtifact.length === 0);
            // Back to the section the screenshot is of.
            detailPage.section = 0;

            if (!captured("run-failed", detailPage))
                return;
            next(2, waitingRun);
            return;
        }

        if (phase === 2) {
            if (!detailReady || !composed())
                return;
            check("aParkedRunShowsTheApprovalBanner", detail.approvalBannerVisible);
            check("aParkedRunNamesTheQuestionItIsWaitingOn",
                  detail.pendingApproval !== null
                  && String(detail.pendingApproval.tool).indexOf("process.exec") !== -1);
            check("aParkedRunOffersCancellation", detail.cancellable);
            check("aParkedRunOffersNoRetry", !detail.retryable);
            check("aParkedRunSaysWhyItCannotBeRetried",
                  detail.retryBlocked === "run_still_active");

            // The banner names a question and offers exactly one action. The
            // decision itself is the review surface's, and the check is that
            // this page routes to the request its own run is parked on.
            if (!captured("run-approval", detailPage))
                return;
            if (window.approvalPage === null) {
                detail.reviewApproval();
                return;
            }
            check("reviewingFromARunOpensTheRequestThatRunIsParkedOn",
                  window.approvalPage.approvalId
                      === String(detail.pendingApproval.approvalId));
            window.closeApproval();
            next(3, interruptedRun);
            return;
        }

        if (phase === 3) {
            if (!detailReady || !composed())
                return;
            check("anInterruptedRunNamesTheCallThatWasInFlight",
                  detail.inFlightCall !== null
                  && String(detail.inFlightCall.toolId) === "process.exec");
            check("anInterruptedRunOffersARetry", detail.retryable);
            check("anInterruptedRunShowsNoApprovalBanner", !detail.approvalBannerVisible);
            if (!captured("run-interrupted", detailPage))
                return;
            next(4, runningRun);
            return;
        }

        if (phase === 4) {
            if (!detailReady || !composed())
                return;
            check("aRunningRunOffersNoRetry", !detail.retryable);
            check("aRunningRunSaysWhyItCannotBeRetried",
                  detail.retryBlocked === "run_still_active");
            check("aRunningRunShowsNoApprovalBanner", !detail.approvalBannerVisible);
            check("aRunningRunOffersCancellation", detail.cancellable);
            // The row the reader is watching says what the tool last reported.
            // Only the call still in flight carries one, so a page of finished
            // calls cannot show a line that reads as current.
            let executing = null;
            for (let index = 0; index < detail.calls.length; ++index) {
                if (String(detail.calls[index].state) === "running")
                    executing = detail.calls[index];
            }
            check("aRunningCallIsOnThePage", executing !== null);
            check("aRunningCallShowsTheNewestLineItReported",
                  executing !== null
                  && String(executing.progress).indexOf("Compiling harkness-crate-7") !== -1);
            if (!captured("run-progress", detailPage))
                return;
            next(5, pagedRun);
            return;
        }

        if (phase === 5) {
            if (!detailReady || !composed())
                return;
            // One store page of the twelve hundred events recorded, and the
            // header button is the only thing that reads another.
            check("aLongTimelineOpensOnOneStorePage", detail.timelineView.count === 200);
            const delegates = detail.timelineView.contentItem.children.length;
            check("aLongTimelineMaterializesOnlyItsVisibleRegion",
                  delegates > 0 && delegates * 2 < detail.timelineView.count);

            // The body's two halves ask for twenty-two and eighteen grid units.
            // Below their sum there is no side-by-side arrangement that is not
            // clipping one of them, so the split turns instead.
            if (!narrowed) {
                check("aWidePageSplitsItsBodySideBySide", detail.sideBySide);
                narrowed = true;
                window.width = narrowWidth;
                return;
            }
            check("aNarrowPageStacksItsHalvesRatherThanClippingOne", !detail.sideBySide);
            check("aStackedTimelineSpansTheWholeWidth",
                  detail.timelineView.width > detail.width * 0.8);
            check("aStackedTimelineLeavesTheRecordsBelowItSomeRoom",
                  detail.timelineView.height > 0
                  && detail.timelineView.height < detail.height);
            window.width = wideWidth;

            next(6, progressedRun);
            return;
        }

        if (phase === 6) {
            if (!detailReady || !composed())
                return;
            // One step-started row and one folded progress row: a hundred ticks
            // of one call cost the reader a single row.
            check("aHundredProgressTicksAreOneTimelineRow",
                  detail.timelineView.count === 2);
            check("markupInAToolsTextIsNeutralizedBeforeItReachesRichText",
                  vocabulary.escapedRichText("<b>x</b> & y")
                      === "<span>&lt;b&gt;x&lt;/b&gt; &amp; y</span>");
            check("aStateThisBuildDoesNotDefineKeepsItsSpelling",
                  vocabulary.stateLabel("from_a_later_build") === "from_a_later_build");
            check("anEventKindThisBuildDoesNotDefineKeepsItsSpelling",
                  vocabulary.eventLabel("from_a_later_build") === "from_a_later_build");
            // The shell's Runs view reaches a detail page only through this.
            runsPanel.openRun(failedRun);
            next(7);
            return;
        }

        if (phase === 7) {
            if (!detailReady || detail.runId !== failedRun)
                return;
            openedPage = detail;
            check("theRunsViewOpensTheRunItNamed", detail.runId === failedRun);
            // The launcher's list names the same run; both entry points reach
            // one detail page for one run rather than two.
            launcherPane.runActivated(failedRun);
            check("bothEntryPointsOpenOneDetailPageForOneRun",
                  detail === openedPage && detail.runId === failedRun);
            next(8);
            return;
        }

        if (phase === 8) {
            // Two questions of different kinds, issued back to back on the Qt
            // thread so the run detail is unambiguously the newer of them. A
            // bridge counting staleness once for all three answer properties
            // drops the excerpt's reply here, leaving the row that asked for it
            // expanded, empty, and reporting no failure.
            if (!overlapIssued) {
                overlapIssued = true;
                overlap.loadArtifactExcerpt(window.excerptableArtifact);
                overlap.loadRun(failedRun);
                return;
            }
            if (overlap.busy)
                return;
            check("anOverlappedLoadStillAnswersItsOwnQuestion",
                  overlap.excerpt !== undefined && overlap.excerpt !== null
                  && String(overlap.excerpt.artifactId) === window.excerptableArtifact);
            check("theNewerOfTwoOverlappingLoadsAnswersAsWell",
                  overlap.run !== undefined && overlap.run !== null
                  && String(overlap.run.runId) === failedRun);
            // The shared status describes the operation issued last, which is
            // the one the reader would be watching.
            check("theSharedStatusDescribesTheNewestOperation",
                  String(overlap.status).indexOf(failedRun) !== -1);
            next(9);
            return;
        }

        // --- The review surface ---------------------------------------
        //
        // Every phase from here to 15 answers nothing, and that is deliberate:
        // a decision needs a run this process's coordinator is driving, and
        // reaching a coordinator at all sweeps, which is why phase 16 is last
        // and why these run before it. What they hold to account is everything
        // the surface decides on its own: which breadths it offers, when it
        // withdraws them, what it renders, and that leaving decides nothing.

        if (phase === 9) {
            if (window.approvalPage === null) {
                window.showApproval(widenedApproval, widenedRun);
                return;
            }
            if (!approvalReady || !composed())
                return;
            const approval = window.approvalPage;
            check("aRequestNamesTheToolTheAnswerWouldAuthorize",
                  String(approval.request.tool).indexOf("fs.apply_patch") !== -1);
            check("aRequestNamesItsVersion",
                  String(approval.request.toolVersion) === "1.0.0");
            check("aRequestNamesTheRiskItWasClassifiedAt",
                  String(approval.request.risk) === "workspace_write");
            check("aRequestNamesTheWorkspaceItIsBoundTo",
                  String(approval.request.workspace).length > 0);
            check("aRequestNamesWhatItWouldDoWithoutLoadingAnything",
                  String(approval.request.summary).indexOf("src/lib.rs") !== -1);
            check("aPendingRequestCanBeAnswered",
                  approval.decidable && approval.approveEnabled && approval.denyEnabled);

            // The breadths on offer are the record's own, so the surface
            // cannot express one `ApprovalRequest::decide` would refuse.
            check("aWorkspaceWriteOffersTheBreadthsTheRuntimeWouldAccept",
                  approval.grantableScopes.length === 2
                  && String(approval.grantableScopes[0]) === "exact_call"
                  && String(approval.grantableScopes[1]) === "capability_for_run");
            check("aChoiceIsRenderedBecauseThereIsOneToMake", approval.scopeChoiceAllowed);
            check("theSurfaceStartsOnTheNarrowestBreadth",
                  approval.chosenScope === "exact_call");

            // A page opens on the row's summary. The canonical input the hash
            // binds is a read, and a request nobody expanded pays for none.
            if (!approval.rawExpanded) {
                check("aRequestOpensWithoutReadingTheInputItBinds",
                      approval.input.length === 0);
                approval.toggleRawInput();
                return;
            }
            if (approval.input.length === 0)
                return;
            check("theRawInputIsTheInputTheRecordedCallIsHolding",
                  approval.input.indexOf("src/lib.rs") !== -1);
            check("markupInAToolInputRendersAsTheCharactersTheToolWrote",
                  approval.input.indexOf("<b>alert</b>") !== -1
                  && approval.input.indexOf("&") !== -1);
            if (!captured("approval-review", approval))
                return;
            next(10);
            return;
        }

        if (phase === 10) {
            // Widen the choice, then leave without answering. Neither the
            // widening nor the leaving may reach the store.
            const approval = window.approvalPage;
            if (!scopeWidened) {
                approval.scopeIndex = 1;
                scopeWidened = true;
                return;
            }
            check("choosingTheWiderBreadthIsWhatWouldBeSent",
                  approval.chosenScope === "capability_for_run");
            window.closeApproval();
            check("leavingTheReviewSurfaceDestroysIt", window.approvalPage === null);
            verify.loadRun(widenedRun);
            next(11);
            return;
        }

        if (phase === 11) {
            if (verify.busy || verify.run === undefined || verify.run === null
                    || String(verify.run.runId) !== widenedRun)
                return;
            let request = null;
            for (let index = 0; index < verify.run.approvals.length; ++index) {
                if (String(verify.run.approvals[index].approvalId) === widenedApproval)
                    request = verify.run.approvals[index];
            }
            check("theRequestIsStillThereAfterTheSurfaceWasDestroyed", request !== null);
            check("closingTheApprovalPageLeavesTheRequestPending",
                  request !== null && request.pending === true
                  && String(request.state) === "pending");
            check("closingTheApprovalPageLeavesTheEffectiveBreadthAlone",
                  request !== null && String(request.scope) === "capability_for_run");
            check("closingTheApprovalPageLeavesTheRunWaiting",
                  String(verify.run.state) === "waiting_for_approval");
            next(12);
            return;
        }

        if (phase === 12) {
            if (window.approvalPage === null) {
                window.showApproval(remoteApproval, remoteRun);
                return;
            }
            if (!approvalReady)
                return;
            const approval = window.approvalPage;
            check("aRemoteWriteOffersNoWidenedScopes",
                  approval.grantableScopes.length === 1
                  && String(approval.grantableScopes[0]) === "exact_call");
            check("aRemoteWriteRendersNoScopeControlAtAll", !approval.scopeChoiceAllowed);
            check("aRemoteWriteIsAnsweredForOneCall",
                  approval.chosenScope === "exact_call");
            check("aDowngradedRequestKeepsBothSpellings",
                  String(approval.request.requestedScope) === "capability_for_run"
                  && String(approval.request.scope) === "exact_call"
                  && approval.request.downgraded === true);
            check("aRemoteWriteIsStillAnswerable", approval.decidable);
            window.closeApproval();
            next(13);
            return;
        }

        if (phase === 13) {
            if (window.approvalPage === null) {
                window.showApproval(lapsedApproval, lapsedRun);
                return;
            }
            if (!approvalReady || !composed())
                return;
            const approval = window.approvalPage;
            // A deadline that passed closes nothing on its own: the row is
            // still pending until a sweeper expires it. The surface follows
            // the clock rather than the row, because the runtime refuses a
            // late grant either way.
            check("aLapsedRequestIsStillStoredAsPending",
                  String(approval.requestState) === "pending");
            check("aLapsedRequestReadsAsTooLateToAnswer", approval.lapsed);
            check("aLapsedRequestCannotBeAnswered", !approval.decidable);
            check("aLapsedRequestOffersNoApproval", !approval.approveEnabled);
            check("aLapsedRequestOffersNoDenialEither", !approval.denyEnabled);
            if (!captured("approval-expired", approval))
                return;
            window.closeApproval();
            next(14);
            return;
        }

        if (phase === 14) {
            if (window.approvalPage === null) {
                window.showApproval(refusedApproval, refusedRun);
                return;
            }
            if (!approvalReady || !composed())
                return;
            const approval = window.approvalPage;
            check("aRefusedRequestReadsAsDenied",
                  String(approval.requestState) === "denied");
            check("aRefusalShowsTheReasonThatWasTyped",
                  String(approval.request.reason) === denialReason);
            check("aRefusalNamesTheSurfaceItWasGivenThrough",
                  String(approval.request.decidedVia) === "gui");
            check("aRefusalNamesWhenItWasGiven",
                  String(approval.request.decidedAt).length > 0);
            check("anAnsweredRequestOffersNoSecondAnswer", !approval.decidable);
            check("anAnsweredRequestOffersNoApproval", !approval.approveEnabled);
            if (!captured("approval-denied", approval))
                return;
            window.closeApproval();
            next(15, refusedRun);
            return;
        }

        if (phase === 15) {
            if (!detailReady)
                return;
            // The audit trail is the same thing the reader just saw. The
            // timeline's own row for the decision carries the verdict as its
            // outcome, and its payload carries the scope, the surface and the
            // reason.
            let decided = null;
            for (let index = 0; index < detail.timelineView.count; ++index) {
                const row = detail.timelineView.itemAtIndex(index);
                if (row !== null && String(row.kind) === "approval_decided")
                    decided = row;
            }
            check("theTimelineRecordsTheDecisionThatWasGiven", decided !== null);
            check("theDecisionRowIsMarkedWithItsVerdict",
                  decided !== null && String(decided.outcome) === "denied");
            if (decided !== null && !decided.expanded) {
                decided.expanded = true;
                detail.timelineView.model.loadDetail(decided.seq);
                return;
            }
            if (decided !== null && decided.detail.length === 0)
                return;
            check("theDecisionsPayloadCarriesTheReasonAndTheSurface",
                  decided !== null
                  && decided.detail.indexOf(denialReason) !== -1
                  && decided.detail.indexOf("gui") !== -1);
            check("aRefusedRunStillListsTheApprovalItRecorded",
                  detail.approvals.length === 1
                  && String(detail.approvals[0].state) === "denied");
            check("aRefusedRunShowsNoApprovalBanner", !detail.approvalBannerVisible);
            next(16, waitingRun);
            return;
        }

        if (phase === 16) {
            // Arms the race the next two phases are about. This page reads a
            // request that is genuinely pending, and then the store moves
            // underneath it: phase 17 reaches a coordinator, whose startup
            // sweep supersedes every request no live process can answer. A
            // surface holding the reading it took *before* that is exactly the
            // stale page the runtime has to refuse, and this is the only way to
            // build one on purpose.
            if (window.approvalPage === null) {
                window.showApproval(widenedApproval, widenedRun);
                return;
            }
            if (!approvalReady)
                return;
            check("theRacedRequestReadsAsAnswerableBeforeAnythingMoves",
                  window.approvalPage.decidable
                  && window.approvalPage.approveEnabled);
            check("theRacedRequestHasNothingToReportYet",
                  window.approvalPage.failureKind.length === 0);
            next(17, waitingRun);
            return;
        }

        if (phase === 17) {
            if (!detailReady)
                return;
            // Cancelling attaches this process's coordinator, which sweeps
            // first: every seeded run names no lease, so the sweep can prove
            // the claim is gone and marks the unfinished ones interrupted. The
            // cancellation itself is then refused - this coordinator did not
            // start the run - and both facts are the point. The page has to end
            // up showing what the store now says rather than what it read
            // before the button was pressed, whichever way the request went.
            //
            // Left until last because the sweep changes the runs the earlier
            // phases assert against.
            if (!cancelPressed) {
                check("theParkedRunReadsAsParkedBeforeAnythingIsPressed",
                      detail.runStateValue === "waiting_for_approval");
                check("theCancelControlReadsAsCancelBeforeItIsPressed",
                      detail.cancelLabel === "Cancel" && !detail.cancelling);
                cancelPressed = true;
                // Timed around the call itself. `cancelRun` flips this
                // process's token on the Qt thread and only then spawns the
                // worker that persists anything, so the control the reader is
                // looking at has to have changed by the time the call returns
                // - before any store write, and long before the run itself
                // reaches a terminal state.
                const pressed = Date.now();
                detail.cancel();
                cancelResponse = Date.now() - pressed;
                check("pressingCancelChangesTheControlWithinAQuarterSecond",
                      cancelResponse >= 0 && cancelResponse < 250);
                check("theCancelControlSaysItIsCancelling",
                      detail.cancelling && detail.cancelLabel !== "Cancel");
                check("theCancelControlCannotBePressedTwice", !detail.cancelling
                      || detail.cancelLabel !== "Cancel");
                return;
            }
            // Bounded, and bounded around *both* waits: a page that never
            // clears `mutating` is exactly the failure this checks for, and
            // waiting on it unbounded would report the fixture's deadline
            // instead of the check that did not hold.
            cancelPolls += 1;
            if (cancelPolls < 100
                    && (detail.mutating
                        || detail.runStateValue === "waiting_for_approval"))
                return;
            check("aMutationMakesThePageReadTheRunAgain",
                  !detail.mutating && detail.runStateValue === "interrupted");
            check("theCancelControlStopsSayingItIsCancellingOnceTheRequestSettles",
                  !detail.cancelling);
            next(18);
            return;
        }

        if (phase === 18) {
            // The stale page from phase 16, pressed after the sweep. The
            // request it is holding no longer exists in the shape it read, and
            // this window holds no authority to say otherwise: the runtime
            // refuses, the refusal is displayed rather than swallowed, and
            // nothing was granted.
            const approval = window.approvalPage;
            if (!approvalPressed) {
                approvalPressed = true;
                approval.approve();
                return;
            }
            if (approval.deciding || !approval.runReady)
                return;
            check("aDecisionTheRuntimeRefusesIsReportedRatherThanPretended",
                  approval.failureKind.length > 0
                  && approval.failureMessage.length > 0);
            check("theRefusalKeepsTheDiscriminantTheRuntimePublished",
                  approval.failureKind === "approval_not_active"
                  || approval.failureKind === "approval_refused");
            check("aRefusedDecisionGrantsNothing",
                  String(approval.requestState) !== "granted");
            check("aPageWhoseRequestMovedUnderneathItCatchesUp",
                  String(approval.requestState) === "superseded");
            check("aSupersededRequestOffersNoApproval",
                  !approval.decidable && !approval.approveEnabled);

            // The other verb, through the bridge the page's Deny button calls.
            // A refusal has to reach a caller the same way whichever decision
            // was asked for; the page above covers the path, and this covers
            // the half of the bridge that path does not reach.
            if (!denyIssued) {
                denyIssued = true;
                verify.deny(refusedApproval, "answered twice");
                return;
            }
            if (verify.busy)
                return;
            check("denyingReportsARefusalTheSameWayApprovingDoes",
                  String(verify.kind || "").length > 0
                  && String(verify.status || "").length > 0);
            window.closeApproval();
            next(19);
            return;
        }

        if (phase === 19) {
            // Main.qml's own stack behaviour, which nothing else here reaches:
            // this window's `showRun` is a stand-in for the detail page's
            // benefit, and re-implementing the stack rules rather than
            // exercising them is how the regression below went untested.
            //
            // The pages are built here and pushed as objects. Kirigami's own
            // push creates a page into `pagesLogic`, a QtObject, and reparents
            // it a line later - which Qt reports as a graphical object outside
            // the scene on every push in every Kirigami application, and this
            // binary makes warnings fatal. That is also why `showRun` is driven
            // with a detail page already in the stack rather than being asked
            // to push a real one: what is under test is which page the window
            // finds, not Kirigami's incubation.
            const main = mainWindow.createObject(null);
            check("theWindowLoads", main !== null);
            if (main === null) {
                next(20);
                return;
            }
            const shellPage = shellStandIn.createObject(main.pageStack);
            const detailPageStandIn = detailStandIn.createObject(main.pageStack,
                                                                 { "runId": failedRun });
            check("theNavigationStandInsAreBuilt",
                  shellPage !== null && detailPageStandIn !== null);
            if (shellPage === null || detailPageStandIn === null) {
                main.destroy();
                next(20);
                return;
            }

            main.pageStack.push(shellPage);
            check("theShellIsFoundWhenItIsAlsoTheCurrentPage",
                  main.shellPage() === shellPage);

            main.pageStack.push(detailPageStandIn);
            check("aRunDetailSitsAboveTheShellRatherThanReplacingIt",
                  main.pageStack.depth === 3
                  && main.pageStack.currentItem === detailPageStandIn);
            // The regression this branch fixes. `currentItem` is now the detail
            // page, so a catalog refresh reading it would find no shell and
            // push a second one over what the reader is looking at; walking the
            // stack still finds the one that is there.
            check("theShellIsStillFoundUnderAnOpenRunDetail",
                  main.shellPage() === shellPage);

            // The two requests `showRun` answers by doing nothing, both read
            // off the page that is current.
            main.showRun("");
            check("namingNoRunAtAllChangesNothing",
                  main.pageStack.depth === 3
                  && main.pageStack.currentItem === detailPageStandIn);
            main.showRun(failedRun);
            check("namingTheRunAlreadyOpenChangesNothing",
                  main.pageStack.depth === 3
                  && main.pageStack.currentItem === detailPageStandIn);

            main.destroy();
            next(20);
            return;
        }

        if (phase === 20) {
            // Every grab is asynchronous, so the loop ends when the last of
            // them has written its file rather than when the last check ran.
            if (pendingGrabs > 0)
                return;
            finish();
        }
    }
}
"#;

    /// Loads Main.qml the same way `main` does and asserts the engine
    /// produced a root object, catching broken imports and malformed QML
    /// without a display.
    #[allow(dead_code)]
    pub(crate) fn main_qml_loads() {
        let screenshot_path = std::env::var("HARKNESS_QML_SCREENSHOT_PATH")
            .ok()
            .filter(|path| !path.is_empty());
        let fixture = TempDir::new().unwrap();
        let data_dir = fixture.path().join("data");
        let repository_root = fixture.path().join("token-bridge-repository");
        fs::create_dir_all(&repository_root).unwrap();
        let repository = Repository::init(&repository_root).unwrap();
        // The bridge check commits for real, so the identity has to come from
        // the repository rather than from whatever the developer running the
        // suite happens to have configured globally.
        let mut config = repository.config().unwrap();
        config.set_str("user.name", "Harkness Tests").unwrap();
        config.set_str("user.email", "tests@example.com").unwrap();
        config.set_bool("commit.gpgsign", false).unwrap();
        drop(config);
        let path = Path::new("bridge.txt");
        fs::write(repository_root.join(path), "before\n").unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(path).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repository.find_tree(tree_id).unwrap();
        let signature = Signature::now("Harkness Tests", "tests@example.com").unwrap();
        repository
            .commit(Some("HEAD"), &signature, &signature, "fixture", &tree, &[])
            .unwrap();
        drop(tree);
        drop(index);
        drop(repository);
        fs::write(repository_root.join(path), "after\n").unwrap();
        // A second change the bridge leaves unchecked, so the commit it drives
        // has something to exclude and the exclusion can be proved.
        let excluded_path = Path::new("excluded.txt");
        fs::write(repository_root.join(excluded_path), "not this one\n").unwrap();

        // SAFETY: set before any Qt object is constructed, and tests in this
        // binary run single-threaded with respect to Qt usage.
        unsafe {
            std::env::set_var("QT_QPA_PLATFORM", "offscreen");
            std::env::set_var("QT_FORCE_STDERR_LOGGING", "1");
            std::env::set_var("QT_FATAL_WARNINGS", "1");
            std::env::set_var("HARKNESS_DATA_DIR", &data_dir);
        }
        let real_project = harkness_core::ProjectService::load()
            .unwrap()
            .import_local(&repository_root)
            .unwrap();
        let real_project_id = real_project.id.to_string();
        cxx_qt::init_qml_module!("io.github.fullstacktaiye.harkness");
        let mut app = QGuiApplication::new();
        let mut engine = QQmlApplicationEngine::new();

        static LOADED: AtomicBool = AtomicBool::new(false);
        if let Some(mut engine) = engine.as_mut() {
            let _connection = engine.as_mut().on_object_created(|_engine, object, _url| {
                LOADED.store(!object.is_null(), Ordering::SeqCst);
            });
            engine.as_mut().load(&QUrl::from(
                "qrc:/qt/qml/io/github/fullstacktaiye/harkness/qml/Main.qml",
            ));
        }

        assert!(
            LOADED.load(Ordering::SeqCst),
            "Main.qml failed to load; see QML warnings above"
        );

        // Instantiate the shell directly with a real directory so TreeView
        // creates delegates and validates the filesystem role bindings. The
        // normal application cannot reach this page until an async import or
        // open completes, which the no-event-loop smoke test cannot drive.
        LOADED.store(false, Ordering::SeqCst);
        if let Some(mut engine) = engine.as_mut() {
            let _connection = engine.as_mut().on_object_created(|_engine, object, _url| {
                LOADED.store(!object.is_null(), Ordering::SeqCst);
            });
            engine.as_mut().load_data(
                &QByteArray::from(
                    br#"
import QtQuick
import org.kde.kirigami as Kirigami
import io.github.fullstacktaiye.harkness

Kirigami.ApplicationWindow {
    visible: false
    width: 640
    height: 480

    HarknessBackend { id: backend }
    pageStack.initialPage: ProjectShellPage {
        backend: backend
        project: ({
            "id": "00000000-0000-0000-0000-000000000000",
            "lockScope": "00000000-0000-0000-0000-000000000000",
            "displayName": "QML fixture",
            "root": "/tmp",
            "remote": "",
            "branch": "",
            "managed": false,
            "worktree": false,
            "parentId": "",
            "parentName": "",
            "createdBranch": "",
            "available": true,
            "isGit": false,
            "dirty": false
        })
    }
}
"#,
                ),
                &QUrl::from("qrc:/ProjectShellSmoke.qml"),
            );
        }
        assert!(
            LOADED.load(Ordering::SeqCst),
            "ProjectShellPage failed to load; see QML warnings above"
        );

        // Drive the activity bar and the side panel it switches with stub
        // views. Both are backend-free, so the checks are plain synchronous
        // property reads; what they cover is the view contract itself, which
        // the shell fixture above only ever sees in its unavailable state.
        LOADED.store(false, Ordering::SeqCst);
        static SIDE_PANEL_ROOT: AtomicPtr<QObject> = AtomicPtr::new(ptr::null_mut());
        if let Some(mut engine) = engine.as_mut() {
            let _connection = engine.as_mut().on_object_created(|_engine, object, _url| {
                LOADED.store(!object.is_null(), Ordering::SeqCst);
                SIDE_PANEL_ROOT.store(object, Ordering::SeqCst);
            });
            engine.as_mut().load_data(
                &QByteArray::from(
                    br#"
import QtQuick
import org.kde.kirigami as Kirigami
import io.github.fullstacktaiye.harkness

Kirigami.ApplicationWindow {
    id: window

    visible: false
    width: 640
    height: 480

    component StubPanel: Item {
        property string viewId: ""
        property string viewTitle: ""
        property string viewIcon: "vcs-branch"
        property string viewShortcut: "Ctrl+Shift+G"
        property int viewBadge: 0
        property bool viewAvailable: true
    }

    property string currentViewId: "first"
    property bool secondAvailable: true

    SidePanel {
        id: sidePanel

        anchors.fill: parent
        currentViewId: window.currentViewId

        StubPanel {
            id: firstView

            viewId: "first"
            viewTitle: "First"
            viewBadge: 3
        }

        StubPanel {
            id: secondView

            viewId: "second"
            viewTitle: "Second"
            viewAvailable: window.secondAvailable
        }
    }

    ActivityBar {
        id: activityBar

        anchors.left: parent.left
        anchors.top: parent.top
        anchors.bottom: parent.bottom
        currentViewId: window.currentViewId
        views: sidePanel.views
        visible: sidePanel.hasAvailableView
    }

    Component.onCompleted: {
        const failures = [];
        function check(name, passed) {
            if (!passed)
                failures.push(name);
        }

        check("collectsDeclaredViews", sidePanel.views.length === 2);
        check("keepsDeclarationOrder", sidePanel.firstAvailableViewId() === "first");
        check("looksUpByViewId", sidePanel.view("second") === secondView);
        check("ignoresUnknownViewId", sidePanel.view("third") === null);
        check("showsCurrentView", sidePanel.currentPanel === firstView);
        check("reportsCurrentViewReady", sidePanel.currentPanelReady);
        check("titlesFromCurrentView", sidePanel.currentPanel.viewTitle === "First");
        check("advertisesViewsToTheBar", activityBar.views.length === 2);
        check("showsTheBarWhileAViewApplies", activityBar.visible);

        window.currentViewId = "second";
        check("switchesView", sidePanel.currentPanel === secondView);

        // A view that stops applying must not leave the panel on it; the shell
        // watches `currentPanelReady` to move back to one that does.
        window.secondAvailable = false;
        check("dropsUnavailableView", !sidePanel.currentPanelReady);
        check("fallsBackToAnAvailableView", sidePanel.firstAvailableViewId() === "first");

        firstView.viewAvailable = false;
        check("hidesTheBarWithoutAnyView", !activityBar.visible);
        check("reportsNoAvailableView", sidePanel.firstAvailableViewId() === "");

        window.objectName = failures.length === 0
            ? "SidePanelSmokePassed"
            : "SidePanelSmokeFailed[" + failures.join(",") + "]";
    }
}
"#,
                ),
                &QUrl::from("qrc:/SidePanelSmoke.qml"),
            );
        }
        assert!(
            LOADED.load(Ordering::SeqCst),
            "SidePanel.qml failed to load; see QML warnings above"
        );
        let side_panel_name = unsafe { SIDE_PANEL_ROOT.load(Ordering::SeqCst).as_ref() }
            .map(|object| object.object_name().to_string())
            .unwrap_or_default();
        assert_eq!(
            side_panel_name, "SidePanelSmokePassed",
            "activity bar and side panel view contract check failed"
        );

        // Exercise the Issues surface with representative backend projection
        // rows so labels, metadata, counts, scopes, filters, selection, and
        // sorting all instantiate without making a network request.
        LOADED.store(false, Ordering::SeqCst);
        static ISSUES_ROOT: AtomicPtr<QObject> = AtomicPtr::new(ptr::null_mut());
        if let Some(mut engine) = engine.as_mut() {
            let _connection = engine.as_mut().on_object_created(|_engine, object, _url| {
                LOADED.store(!object.is_null(), Ordering::SeqCst);
                ISSUES_ROOT.store(object, Ordering::SeqCst);
            });
            engine.as_mut().load_data(
                &QByteArray::from(
                    br##"
import QtQuick
import org.kde.kirigami as Kirigami
import io.github.fullstacktaiye.harkness

Kirigami.ApplicationWindow {
    id: window

    visible: true
    width: 1180
    height: 760

    property var projectFixture: ({
        "id": "00000000-0000-0000-0000-000000000000",
        "displayName": "Issues fixture",
        "root": "/tmp",
        "remote": "github.com/example/repository",
        "githubRemote": "github.com/example/repository",
        "available": true,
        "isGit": true
    })

    property var issueFixtureRows: [
            {
                "number": 186,
                "title": "ACP agents, MCP integrations, and GitHub issues",
                "state": "open",
                "author": "octocat",
                "updated": "updated 2d ago",
                "labels": [
                    { "name": "enhancement", "color": "#56d4dd" },
                    { "name": "v0.5", "color": "#3fb950" }
                ],
                "milestone": "v0.5",
                "assignees": ["@octocat", "@maintainer"],
                "commentCount": 41,
                "createdByMe": true,
                "assignedToMe": false
            },
            {
                "number": 185,
                "title": "Performance and output-bound stress benchmarks",
                "state": "open",
                "author": "maintainer",
                "updated": "updated 3d ago",
                "labels": [{ "name": "testing", "color": "#a5d6a7" }],
                "milestone": "v0.5",
                "assignees": ["@me"],
                "commentCount": 3,
                "createdByMe": false,
                "assignedToMe": true
            },
            {
                "number": 120,
                "title": "Persist workspace snapshot identity",
                "state": "closed",
                "author": "octocat",
                "updated": "closed last week",
                "labels": [{ "name": "architecture", "color": "#a371f7" }],
                "milestone": "",
                "assignees": [],
                "commentCount": 8,
                "createdByMe": true,
                "assignedToMe": false
            }
    ]

    QtObject {
        id: fixtureBackend

        property int refreshCallCount: 0

        property var issues: ({
            "projectId": window.projectFixture.id,
            "remote": window.projectFixture.githubRemote,
            "loading": false,
            "viewer": "octocat",
            "rows": window.issueFixtureRows,
            "hasMore": false,
            "limitReached": false,
            "error": ""
        })

        function refreshIssues(projectId, githubRemote) {
            refreshCallCount += 1;
        }
        function loadMoreIssues(projectId, githubRemote) {}
    }

    IssuesPanel {
        id: issues

        anchors.fill: parent
        backend: fixtureBackend
        project: window.projectFixture
    }

    Component.onCompleted: {
        const failures = [];
        function check(name, passed) {
            if (!passed)
                failures.push(name);
        }

        check("availableForGitProject", issues.viewAvailable);
        check("refreshesOnceOnInitialization", fixtureBackend.refreshCallCount === 1);
        // Every local mutation hands the panel a rewritten project map. An
        // issue fetch is a network round trip, so an equal map must not buy
        // one; only a different repository is worth reloading for.
        window.projectFixture = JSON.parse(JSON.stringify(window.projectFixture));
        check("ignoresEqualProjectReplacement", fixtureBackend.refreshCallCount === 1);
        const otherProject = JSON.parse(JSON.stringify(window.projectFixture));
        otherProject.id = "11111111-1111-1111-1111-111111111111";
        window.projectFixture = otherProject;
        check("refreshesForAnotherProject", fixtureBackend.refreshCallCount === 2);
        check("countsOpenRows", issues.openIssueCount === 2);
        check("countsClosedRows", issues.closedIssueCount === 1);
        check("defaultsToOpen", issues.visibleIssueCount === 2);
        check("defaultsToOldest", issues.sortOrder === "oldest"
            && Number(issues.filteredIssues()[0].number) === 185);

        issues.searchText = "performance";
        check("searchesIssueText", issues.filteredIssues().length === 1);
        issues.searchText = "";

        issues.selectScope("assigned");
        check("filtersAssignedToMe", issues.filteredIssues().length === 1);
        check("countsSelectedScope", issues.countByStateAndSelectedScope("open") === 1);
        issues.selectScope("issues");

        issues.setIssueSelected(issues.issueRows[0], true);
        issues.sortOrder = "newest";
        check("selectionUsesIdentity", issues.issueSelected(issues.issueRows[0]));
        check("selectionDoesNotMigrate", !issues.issueSelected(issues.issueRows[1]));
        issues.sortOrder = "oldest";

        issues.labelFilter = "v0.5";
        check("filtersLabels", issues.filteredIssues().length === 1);
        issues.labelFilter = "";

        issues.assigneeFilter = "@octocat";
        check("filtersOneMemberOfAssignmentSet", issues.filteredIssues().length === 1);
        issues.assigneeFilter = "";

        issues.stateFilter = "closed";
        check("switchesState", issues.filteredIssues().length === 1);
        issues.stateFilter = "open";
        check("sortsOldest", Number(issues.filteredIssues()[0].number) === 185);

        window.objectName = failures.length === 0
            ? "IssuesPanelSmokePassed"
            : "IssuesPanelSmokeFailed[" + failures.join(",") + "]";
    }

}
"##,
                ),
                &QUrl::from("qrc:/IssuesPanelSmoke.qml"),
            );
        }
        assert!(
            LOADED.load(Ordering::SeqCst),
            "IssuesPanel.qml failed to load; see QML warnings above"
        );
        let issues_name = unsafe { ISSUES_ROOT.load(Ordering::SeqCst).as_ref() }
            .map(|object| object.object_name().to_string())
            .unwrap_or_default();
        assert_eq!(
            issues_name, "IssuesPanelSmokePassed",
            "IssuesPanel filter and projection check failed"
        );

        // Instantiate the run bridge from QML. cxx-qt does not camel-case
        // names, so a `snake_case` member reaches QML spelled exactly as
        // written and a camel-case call site resolves to `undefined` rather
        // than failing to compile. No Rust test can see that: the spelling only
        // exists once the type is registered and something looks it up. This
        // block is that lookup, for every property and every invokable of all
        // four objects, before #101 and #102 write any QML against them.
        LOADED.store(false, Ordering::SeqCst);
        static RUN_BRIDGE_ROOT: AtomicPtr<QObject> = AtomicPtr::new(ptr::null_mut());
        if let Some(mut engine) = engine.as_mut() {
            let _connection = engine.as_mut().on_object_created(|_engine, object, _url| {
                LOADED.store(!object.is_null(), Ordering::SeqCst);
                RUN_BRIDGE_ROOT.store(object, Ordering::SeqCst);
            });
            engine.as_mut().load_data(
                &QByteArray::from(
                    br#"
import QtQuick
import org.kde.kirigami as Kirigami
import io.github.fullstacktaiye.harkness

Kirigami.ApplicationWindow {
    id: window

    visible: false
    width: 640
    height: 480

    RunsBackend { id: runs }
    RunListModel { id: runList }
    RunTimelineModel { id: timeline }
    ApprovalModel { id: approvals }

    ListView { id: runView; model: runList }
    ListView { id: timelineView; model: timeline }
    ListView { id: approvalView; model: approvals }

    Component.onCompleted: {
        const failures = [];
        function check(name, passed) {
            if (!passed)
                failures.push(name);
        }

        check("runsBackendProperties",
              runs.busy === false && runs.status === "Ready" && runs.kind === "");
        check("runsBackendInvokables",
              typeof runs.cancelRun === "function"
              && typeof runs.retryRun === "function"
              && typeof runs.approve === "function"
              && typeof runs.deny === "function"
              && typeof runs.loadApprovalInput === "function");

        check("runListProperties",
              runList.loading === false && runList.more === false
              && runList.status === "" && runList.kind === "");
        check("runListInvokables",
              typeof runList.refresh === "function"
              && typeof runList.loadMore === "function");
        check("runListStartsEmpty", runView.count === 0);

        check("timelineProperties",
              timeline.run === "" && timeline.loading === false
              && timeline.live === false && timeline.more === false
              && timeline.status === "" && timeline.kind === "");
        check("timelineInvokables",
              typeof timeline.select === "function"
              && typeof timeline.refresh === "function"
              && typeof timeline.loadOlder === "function"
              && typeof timeline.loadDetail === "function");

        check("approvalProperties",
              approvals.count === 0 && approvals.loading === false
              && approvals.status === "" && approvals.kind === "");
        check("approvalInvokables", typeof approvals.refresh === "function");
        check("approvalQueueStartsEmpty", approvalView.count === 0);

        // Selecting nothing is the one bridge path that reads no store, so it
        // is the one a smoke test with no event loop can drive end to end.
        timeline.select("");
        check("selectingNothingClearsTheTimeline",
              timeline.run === "" && timelineView.count === 0
              && timeline.live === false && timeline.loading === false);

        window.objectName = failures.length === 0
            ? "RunBridgeSmokePassed"
            : "RunBridgeSmokeFailed[" + failures.join(",") + "]";
    }
}
"#,
                ),
                &QUrl::from("qrc:/RunBridgeSmoke.qml"),
            );
        }
        assert!(
            LOADED.load(Ordering::SeqCst),
            "the run bridge fixture failed to load; see QML warnings above"
        );
        let run_bridge_name = unsafe { RUN_BRIDGE_ROOT.load(Ordering::SeqCst).as_ref() }
            .map(|object| object.object_name().to_string())
            .unwrap_or_default();
        assert_eq!(
            run_bridge_name, "RunBridgeSmokePassed",
            "run, timeline, approval, and runs-backend QML contract check failed"
        );

        // Exercise every GitPanel delegate with hand-written state. Main.qml
        // cannot populate changed paths or running jobs without driving real
        // asynchronous Git operations, so this fixture is what catches typos
        // in those otherwise-lazy QML branches.
        LOADED.store(false, Ordering::SeqCst);
        static INTERACTION_ROOT: AtomicPtr<QObject> = AtomicPtr::new(ptr::null_mut());
        if let Some(mut engine) = engine.as_mut() {
            let _connection = engine.as_mut().on_object_created(|_engine, object, _url| {
                LOADED.store(!object.is_null(), Ordering::SeqCst);
                INTERACTION_ROOT.store(object, Ordering::SeqCst);
            });
            let interaction_qml = String::from_utf8(
                br#"
import QtQuick
import org.kde.kirigami as Kirigami
import io.github.fullstacktaiye.harkness

Kirigami.ApplicationWindow {
    id: window

    objectName: screenshotSaved
        ? "GitPanelScreenshotSaved"
        : fixtureBackend.workingReviewCalls === 1
        && fixtureBackend.lastReviewStaged === false
        && fixtureBackend.lastReviewPath === "fixture-path"
        && reviewFixture.splitLayout === true
        && selectionDetected === true
        && changesModelDetected === true
        && commitScopeDetected === true
        && amendGatingDetected === true
        && pairedRowsDetected === true
        && editorOpenDetected === true
        && reviewBusyDetected === true
        && mutationBusyDetected === true
        && operationBusyDetected === true
        && fileBoundaryDetected === true
        && filePagingDetected === true
        && navigationLockDetected === true
        && pagingControlsDetected === true
        && crossPageNavigationDetected === true
        && terminalHunkBoundaryDetected === true
        && historyPreviewDetected === true
        && largeModelDetected === true
        && deepScrollDetected === true
        && whitespaceRenderDetected === true
        && whitespaceCopyDetected === true
        && revealScrollDetected === true
        && checkPresentationDetected === true
        && realBridgePassed === true
        ? "GitPanelSmokePassed"
        : "GitPanelSmokeFailed-" + selectionDetected
            + "-" + changesModelDetected
            + "-" + commitScopeDetected
            + "-" + amendGatingDetected
            + "-" + pairedRowsDetected
            + "-" + editorOpenDetected
            + "-" + reviewBusyDetected
            + "-" + mutationBusyDetected
            + "-" + operationBusyDetected
            + "-" + fileBoundaryDetected
            + "-" + filePagingDetected
            + "-" + navigationLockDetected
            + "-" + pagingControlsDetected
            + "-" + crossPageNavigationDetected
            + "-" + terminalHunkBoundaryDetected
            + "-" + historyPreviewDetected
            + "-" + fixtureBackend.nextPageCalls
            + "-" + fixtureBackend.previousPageCalls
            + "-" + reviewFixture.pendingHunkNavigation
            + "-" + largeModelDetected
            + "-" + deepScrollDetected
            + "-" + whitespaceRenderDetected
            + "-" + whitespaceCopyDetected
            + "-" + revealScrollDetected
            + "-" + checkPresentationDetected
            + "-" + reviewFixture.reviewCurrentIndex
            + "-" + realBridgePhase
            + "-" + realBridgePassed
    visible: true
    width: screenshotPath.length > 0 ? 760 : 640
    height: screenshotPath.length > 0 ? 1140 : 720

    property bool reviewBusyDetected: false
    property bool selectionDetected: false
    property bool changesModelDetected: false
    property bool commitScopeDetected: false
    property bool amendGatingDetected: false
    property bool pairedRowsDetected: false
    property bool editorOpenDetected: false
    property bool mutationBusyDetected: false
    property bool operationBusyDetected: false
    property bool fileBoundaryDetected: false
    property bool filePagingDetected: false
    property bool navigationLockDetected: false
    property bool pagingControlsDetected: false
    property bool crossPageNavigationDetected: false
    property bool terminalHunkBoundaryDetected: false
    property bool historyPreviewDetected: false
    property bool largeModelDetected: false
    property bool deepScrollDetected: false
    property bool whitespaceRenderDetected: false
    property bool whitespaceCopyDetected: false
    property bool revealScrollDetected: false
    property bool checkPresentationDetected: false
    property bool screenshotSaved: false
    property bool screenshotReveal: __SCREENSHOT_REVEAL__
    property bool fakePassed: false
    property bool realBridgePassed: false
    property int realBridgePhase: 0
    property string realProjectId: "__REAL_PROJECT_ID__"
    property string screenshotPath: "__SCREENSHOT_PATH__"
    property string realPathId: ""

    // The pair the review fixture shows differs by a word, by an indent that
    // swapped a tab for spaces, by a trailing run and by its terminator: one
    // row that carries every treatment the surface has to render at once.
    readonly property var oldWordSegments: [
        { "text": "\t", "changed": true, "whitespace": "tab", "zone": "leading" },
        { "text": "let old_word = true;", "changed": true, "whitespace": "", "zone": "" },
        { "text": "  ", "changed": false, "whitespace": "space", "zone": "trailing" }
    ]
    readonly property var newWordSegments: [
        { "text": "    ", "changed": true, "whitespace": "space", "zone": "leading" },
        { "text": "let new_word = true;", "changed": true, "whitespace": "", "zone": "" }
    ]

    // Untouched surroundings that already carried a trailing run, which is
    // what a legacy file looks like and what must stay quiet.
    readonly property var trailingContextSegments: [
        { "text": "let kept = true;", "changed": false, "whitespace": "", "zone": "" },
        { "text": "  ", "changed": false, "whitespace": "space", "zone": "trailing" }
    ]

    // Every entity and every drawn character is one column; the markup around
    // them is worth none. Comparing the two reveal states through this is what
    // proves a glyph never moves a column out from under the other side.
    function renderedColumns(html) {
        return String(html)
            .replace(/<[^>]*>/g, "")
            .replace(/&nbsp;/g, "\u0001")
            .length;
    }

    function finishIfReady() {
        if (fakePassed && realBridgePassed) {
            smokeTimeout.stop();
            Qt.quit();
        }
    }

    property var projectFixture: ({
        "id": "00000000-0000-0000-0000-000000000000",
        "lockScope": "fixture-repository-scope",
        "displayName": "Git fixture",
        "root": "/tmp",
        "remote": "github.com/example/repository",
        "branch": "topic",
        "managed": false,
        "worktree": false,
        "parentId": "",
        "parentName": "",
        "createdBranch": "",
        "available": true,
        "isGit": true,
        "dirty": true
    })

    function contextSide(line) {
        return {
            "present": true,
            "line": line,
            "kind": "context",
            "marker": " ",
            "segments": [{ "text": "context " + line, "changed": false }]
        };
    }

    function contextRow(line) {
        return {
            "type": "line",
            "openLine": line,
            "splitHidden": false,
            "unified": {
                "oldLine": line,
                "newLine": line,
                "kind": "context",
                "marker": " ",
                "segments": [{ "text": "context " + line, "changed": false }]
            },
            "old": contextSide(line),
            "new": contextSide(line)
        };
    }

    function hunkRow(hunkId, oldStart, oldLines, newStart, newLines) {
        return {
            "type": "hunk",
            "hunkId": hunkId,
            "header": "@@ -%1,%2 +%3,%4 @@"
                .arg(oldStart).arg(oldLines).arg(newStart).arg(newLines),
            "degradation": ""
        };
    }

    function reviewRowsWithHunkAt(hunkIndex) {
        const rows = [];
        for (let index = 0; index < 12005; ++index)
            rows.push(index === 0
                ? ({ "type": "page", "direction": "previous", "count": 12000 })
                : index === hunkIndex
                    ? hunkRow("review-hunk-fixture", 220, 2, 220, 2)
                    : contextRow(index + 1));
        return rows;
    }

    function crossPageRows(page) {
        if (page === 0) {
            return [
                hunkRow("cross-page-first", 1, 1, 1, 1),
                contextRow(2),
                ({
                    "type": "page",
                    "direction": "next",
                    "count": 24000,
                    "hunkAvailable": true
                })
            ];
        }
        if (page === 1) {
            return [
                ({
                    "type": "page",
                    "direction": "previous",
                    "count": 12000,
                    "hunkAvailable": true
                }),
                contextRow(12001),
                contextRow(12002),
                ({
                    "type": "page",
                    "direction": "next",
                    "count": 12000,
                    "hunkAvailable": true
                })
            ];
        }
        return [
            ({
                "type": "page",
                "direction": "previous",
                "count": 24000,
                "hunkAvailable": true
            }),
            contextRow(24001),
            hunkRow("cross-page-last", 24002, 1, 24002, 1)
        ];
    }

    function replaceReviewRows(rows) {
        const current = fixtureBackend.review;
        fixtureBackend.review = {
            "projectId": current.projectId,
            "title": current.title,
            "detail": current.detail,
            "loading": false,
            "fileLoading": false,
            "error": "",
            "errorKind": "",
            "selectedFileId": current.selectedFileId,
            "commitId": current.commitId || "",
            "checkTargetKind": current.checkTargetKind || "unavailable",
            "checkTargetHead": current.checkTargetHead || "",
            "fileOffset": current.fileOffset || 0,
            "totalFiles": current.totalFiles || current.files.length,
            "files": current.files,
            "file": {
                "fileId": current.file.fileId,
                "path": current.file.path,
                "pathId": current.file.pathId,
                "summary": "",
                "binary": false,
                "hunkCount": 1,
                "totalRows": rows.length,
                "rowOffset": 0,
                "rows": rows
            }
        };
    }

    function replaceReviewFileWindow(files, offset, total, selectedFileId) {
        const current = fixtureBackend.review;
        fixtureBackend.review = {
            "projectId": current.projectId,
            "title": current.title,
            "detail": current.detail,
            "loading": false,
            "fileLoading": false,
            "error": "",
            "errorKind": "",
            "selectedFileId": selectedFileId === undefined
                ? current.selectedFileId
                : selectedFileId,
            "commitId": current.commitId || "",
            "checkTargetKind": current.checkTargetKind || "unavailable",
            "checkTargetHead": current.checkTargetHead || "",
            "fileOffset": offset,
            "totalFiles": total,
            "files": files,
            "file": current.file
        };
    }

    function beginLargeModelSmoke() {
        replaceReviewRows(reviewRowsWithHunkAt(11000));
        largeModelDetected = reviewFixture.reviewRows.length > 12000;
        Qt.callLater(function() {
            reviewFixture.reviewCurrentIndex = -1;
            reviewFixture.navigateHunk(1);
            Qt.callLater(function() {
                deepScrollDetected = reviewFixture.reviewContentY > 1000
                    && reviewFixture.reviewCurrentIndex === 11000;
                // Deep in a long file is exactly where a reader reaches for
                // the reveal control, and exactly where losing their place
                // would cost them the most.
                const heldFile = String(fixtureBackend.review.file.fileId);
                const heldPosition = reviewFixture.reviewContentY;
                const heldIndex = reviewFixture.reviewCurrentIndex;
                reviewFixture.setRevealWhitespace(true);
                Qt.callLater(function() {
                    revealScrollDetected = reviewFixture.revealWhitespace
                        && reviewFixture.reviewCurrentIndex === heldIndex
                        && Math.abs(reviewFixture.reviewContentY - heldPosition) < 1
                        && String(fixtureBackend.review.file.fileId) === heldFile;
                    reviewFixture.setRevealWhitespace(false);
                    // A commit is the one mutation this surface still has to
                    // wait out: navigation must be refused while the index is
                    // moving under the rows it would jump between.
                    fixtureBackend.jobs = [{
                        "id": "job-commit-navigation",
                        "kind": "commit",
                        "projectId": String(projectFixture.id),
                        "lockScope": String(projectFixture.lockScope),
                        "label": "Commit",
                        "progress": "Starting...",
                        "cancellable": true
                    }];
                    navigationLockDetected = !reviewFixture.hunkNavigationEnabled();
                    fixtureBackend.jobs = [];
                    fakePassed = true;
                    finishIfReady();
                });
            });
        });
    }

    function verifyCrossPageNavigation(attempts) {
        const current = reviewFixture.reviewCurrentIndex >= 0
            ? reviewFixture.reviewRows[reviewFixture.reviewCurrentIndex]
            : null;
        if (fixtureBackend.crossPagePhase === 0
                && current && current.hunkId === "cross-page-last") {
            fixtureBackend.crossPagePhase = 1;
            reviewFixture.navigateHunk(-1);
        } else if (fixtureBackend.crossPagePhase === 1
                && current && current.hunkId === "cross-page-first") {
            Qt.callLater(function() {
                crossPageNavigationDetected = fixtureBackend.nextPageCalls === 3
                    && fixtureBackend.previousPageCalls === 3
                    && reviewFixture.pendingHunkNavigation === 0;
                fixtureBackend.crossPageNavigationActive = false;
                replaceReviewRows([
                    hunkRow("terminal-hunk", 1, 24000, 1, 24000),
                    ({
                        "type": "page",
                        "direction": "next",
                        "count": 12000,
                        "hunkAvailable": false
                    })
                ]);
                reviewFixture.reviewCurrentIndex = 0;
                const nextCalls = fixtureBackend.nextPageCalls;
                reviewFixture.navigateHunk(1);
                terminalHunkBoundaryDetected = !reviewFixture.hunkNavigationEnabled(1)
                    && fixtureBackend.nextPageCalls === nextCalls;
                beginLargeModelSmoke();
            });
            return;
        }
        if (attempts <= 0) {
            objectName = "GitPanelCrossPageNavigationTimedOut-"
                + fixtureBackend.crossPagePhase + "-"
                + fixtureBackend.crossPagePage + "-"
                + reviewFixture.reviewCurrentIndex + "-"
                + reviewFixture.pendingHunkNavigation + "-"
                + fixtureBackend.nextPageCalls + "-"
                + fixtureBackend.previousPageCalls;
            smokeTimeout.stop();
            Qt.quit();
            return;
        }
        Qt.callLater(function() {
            verifyCrossPageNavigation(attempts - 1);
        });
    }

    // The review has to have actually rendered a diff before the commit is
    // worth making: it is what proves the real backend produced rows for the
    // working-tree change the commit then records.
    function realReviewHasHunk() {
        const review = realBackend.review;
        if (!review || review.loading === true || review.fileLoading === true
                || !review.file || review.file.rows === undefined)
            return false;
        for (let index = 0; index < review.file.rows.length; ++index) {
            if (review.file.rows[index].type === "hunk")
                return true;
        }
        return false;
    }

    HarknessBackend {
        id: realBackend
    }

    Connections {
        target: realBackend

        function onOpenedChanged() {
            if (realBridgePhase !== 0
                    || String(realBackend.opened.id || "") !== realProjectId)
                return;
            realBridgePhase = 1;
            realBackend.refreshGit(realProjectId);
        }

        function onGitChanged() {
            if (String(realBackend.git.projectId || "") !== realProjectId)
                return;
            const entries = realBackend.git.entries === undefined
                ? []
                : realBackend.git.entries;
            if (realBridgePhase === 1) {
                if (entries.length < 2)
                    return;
                for (let index = 0; index < entries.length; ++index) {
                    if (String(entries[index].path) === "bridge.txt")
                        realPathId = String(entries[index].pathId || "");
                }
                if (realPathId.length === 0)
                    return;
                realBridgePhase = 2;
                realBackend.reviewWorkingChanges(realProjectId, false, realPathId);
            } else if (realBridgePhase === 3) {
                // The commit stages as it goes, so what survives is the whole
                // assertion: the checked path is gone from the list and the
                // unchecked one is still sitting there, untouched.
                if (String(realBackend.git.error || "").length > 0
                        || entries.length !== 1
                        || String(entries[0].path) !== "excluded.txt")
                    return;
                realBridgePhase = 4;
                realBridgePassed = true;
                finishIfReady();
            }
        }

        function onReviewChanged() {
            if (String(realBackend.review.projectId || "") !== realProjectId
                    || realBridgePhase !== 2
                    || !realReviewHasHunk())
                return;
            realBridgePhase = 3;
            realBackend.commit(realProjectId, "bridge commit", false, realPathId);
        }
    }

    QtObject {
        id: fixtureBackend

        property int workingReviewCalls: 0
        property bool lastReviewStaged: true
        property string lastReviewPath: ""
        property int commitCalls: 0
        property bool lastCommitAmend: false
        property string lastCommitPathIds: ""
        property int nextPageCalls: 0
        property int previousPageCalls: 0
        property int loadReviewFileCalls: 0
        property int nextFilePageCalls: 0
        property int previousFilePageCalls: 0
        property int openLineCalls: 0
        property int clipboardCalls: 0
        property string lastClipboardText: ""
        property int lastOpenLine: 0
        property string lastOpenFileId: ""
        property string lastReviewFileId: ""
        property bool crossPageNavigationActive: false
        property int crossPagePage: 0
        property int crossPagePhase: 0

        property var branches: [{
            "name": "topic",
            "current": true,
            "selectable": true,
            "detail": "Checked out here"
        }]
        property var worktrees: [{
            "id": "10000000-0000-0000-0000-000000000000",
            "root": "/tmp/worktree",
            "branch": "agent/topic",
            "owned": true,
            "locked": false,
            "lockReason": "",
            "prunable": false
        }, {
            "id": "20000000-0000-0000-0000-000000000000",
            "root": "/tmp/worktree-locked",
            "branch": "agent/locked",
            "owned": true,
            "locked": true,
            "lockReason": "agent is still working",
            "prunable": false
        }]
        property var jobs: [{
            "id": "job-1",
            "kind": "fetch",
            "projectId": "00000000-0000-0000-0000-000000000000",
            "lockScope": "fixture-repository-scope",
            "label": "Fetch",
            "progress": "Receiving objects",
            "cancellable": true
        }]
        property var git: ({
            "projectId": "00000000-0000-0000-0000-000000000000",
            "branch": "topic",
            "head": "topic",
            "detached": false,
            "unborn": false,
            "upstream": "origin/topic",
            "ahead": 1,
            "behind": 2,
            "pending": "",
            "error": "",
            "errorKind": "",
            "entries": [{
                "pathId": "fixture-path",
                "path": "src/main.rs",
                "staged": "added",
                "unstaged": "modified",
                "renameSource": "",
                "conflicted": false
            }, {
                "pathId": "fixture-path-2",
                "path": "src/lib.rs",
                "staged": "",
                "unstaged": "modified",
                "renameSource": "",
                "conflicted": false
            }]
        })
        property var history: ({
            "projectId": "00000000-0000-0000-0000-000000000000",
            "loading": false,
            "hasMore": true,
            "error": "",
            "errorKind": "",
            "commits": [{
                "id": "0123456789abcdef0123456789abcdef01234567",
                "shortId": "0123456789ab",
                "summary": "Exercise the review surface",
                "message": "Exercise the review surface\n\nFixture body.",
                "author": "QML Fixture",
                "authorEmail": "fixture@example.com",
                "authorTime": "1700000000",
                "parentCount": 1
            }]
        })
        property var checks: ({
            "projectId": "00000000-0000-0000-0000-000000000000",
            "loading": false,
            "error": "",
            "configured": [{
                "id": "cargo.test",
                "label": "Cargo tests",
                "command": ["cargo", "test name", "%2", "<literal>"],
                "cwd": "crate dir",
                "environment": [{
                    "name": "Z_LAST",
                    "value": "last"
                }, {
                    "name": "A_FIRST",
                    "value": "first value"
                }],
                "timeoutSeconds": 45,
                "parser": "cargo_json"
            }],
            "results": [{
                "runId": "check-run-fixture",
                "checkId": "cargo.test",
                "label": "Cargo tests",
                "outcome": "failed",
                "freshness": "stale",
                "freshnessDetail": "src/main.rs changed",
                "stateHead": "0123456789abcdef0123456789abcdef01234567",
                "stateDigest": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
                "evidenceClass": "harkness_observed",
                "definitionCurrent": true,
                "workspaceCleanKnown": true,
                "workspaceClean": true,
                "workspaceMatchesIndexKnown": true,
                "workspaceMatchesIndex": false,
                "createdAt": "2026-08-15T12:00:00.000000000Z",
                "durationMs": 1842,
                "stdoutTail": "stdout tail",
                "stderrTail": "stderr tail",
                "stdoutTruncated": true,
                "stderrTruncated": true,
                "artifactByteLimit": 8388608,
                "stdoutArtifactTruncated": true,
                "stderrArtifactTruncated": false,
                "recordedCommand": ["cargo", "test", "--workspace"],
                "recordedCwd": "",
                "recordedEnvironment": [],
                "recordedTimeoutSeconds": 120,
                "recordedParser": "cargo_json",
                "diagnostics": [{
                    "path": "src/main.rs",
                    "line": 1,
                    "column": 5,
                    "level": "error",
                    "message": "fixture check diagnostic"
                }],
                "diagnosticsOmitted": 3,
                "diagnosticsScanTruncated": true
            }]
        })
        property var review: ({
            "projectId": "00000000-0000-0000-0000-000000000000",
            "title": "Working-tree changes",
            "detail": "Index against the working tree",
            "loading": false,
            "fileLoading": false,
            "error": "",
            "errorKind": "",
            "selectedFileId": "review-file-fixture",
            "commitId": "",
            "checkTargetKind": "commit",
            "checkTargetHead": "0123456789abcdef0123456789abcdef01234567",
            "fileOffset": 0,
            "totalFiles": 1,
            "files": [{
                "fileId": "review-file-fixture",
                "path": "src/main.rs",
                "change": "modified",
                "oldSize": "24",
                "newSize": "24"
            }],
            "file": {
                "fileId": "review-file-fixture",
                "path": "src/main.rs",
                "pathId": "fixture-review-path",
                "summary": "",
                "binary": false,
                "hunkCount": 1,
                "rows": [{
                    "type": "collapsed",
                    "hunkId": "review-hunk-fixture",
                    "direction": "before",
                    "count": 12
                }, {
                    "type": "hunk",
                    "hunkId": "review-hunk-fixture",
                    "header": "@@ -1 +1 @@",
                    "degradation": ""
                }, {
                    "type": "line",
                    "hunkId": "review-hunk-fixture",
                    "openLine": 1,
                    "splitHidden": false,
                    // The pair also loses a trailing run and a carriage
                    // return, so the delegates render the whitespace
                    // treatment rather than only the word emphasis.
                    "lineEndChanged": true,
                    "unified": {
                        "oldLine": 1,
                        "newLine": 0,
                        "kind": "deletion",
                        "marker": "-",
                        "lineEnd": "crlf",
                        "copyText": "\tlet old_word = true;  \r\n",
                        "segments": window.oldWordSegments
                    },
                    "old": {
                        "present": true,
                        "line": 1,
                        "kind": "deletion",
                        "marker": "-",
                        "lineEnd": "crlf",
                        "copyText": "\tlet old_word = true;  \r\n",
                        "segments": window.oldWordSegments
                    },
                    "new": {
                        "present": true,
                        "line": 1,
                        "kind": "addition",
                        "marker": "+",
                        "lineEnd": "lf",
                        "copyText": "    let new_word = true;\n",
                        "segments": window.newWordSegments
                    }
                }, {
                    "type": "line",
                    "hunkId": "review-hunk-fixture",
                    "openLine": 1,
                    "splitHidden": true,
                    "lineEndChanged": true,
                    "unified": {
                        "oldLine": 0,
                        "newLine": 1,
                        "kind": "addition",
                        "marker": "+",
                        "lineEnd": "lf",
                        "copyText": "    let new_word = true;\n",
                        "segments": window.newWordSegments
                    },
                    "old": { "present": false },
                    "new": { "present": false }
                }]
            }
        })

        function refreshGit(projectId) {}
        function refreshHistory(projectId) {}
        function loadMoreHistory(projectId) {}
        function reviewCommit(projectId, revision) {}
        function reviewBranch(projectId, branch, baseBranch) {}
        function reviewWorkingChanges(projectId, staged, pathId) {
            ++workingReviewCalls;
            lastReviewStaged = staged;
            lastReviewPath = String(pathId);
        }
        function openReviewLine(projectId, fileId, line) {
            ++openLineCalls;
            lastOpenFileId = String(fileId);
            lastOpenLine = Number(line);
        }
        function copyToClipboard(text) {
            ++clipboardCalls;
            lastClipboardText = String(text);
        }
        function loadReviewFile(projectId, fileId) {
            ++loadReviewFileCalls;
            lastReviewFileId = String(fileId);
            replaceReviewFileWindow(
                review.files,
                review.fileOffset,
                review.totalFiles,
                String(fileId)
            );
        }
        function expandReviewContext(projectId, hunkId, direction) {}
        function loadMoreReviewFiles(projectId) {
            ++nextFilePageCalls;
            replaceReviewFileWindow([{
                "fileId": "review-file-page-2-first",
                "path": "src/page-2-first.rs",
                "change": "modified",
                "oldSize": "24",
                "newSize": "24"
            }, {
                "fileId": "review-file-page-2-second",
                "path": "src/page-2-second.rs",
                "change": "modified",
                "oldSize": "24",
                "newSize": "24"
            }], 512, 514);
        }
        function loadPreviousReviewFiles(projectId) {
            ++previousFilePageCalls;
            replaceReviewFileWindow([{
                "fileId": "review-file-fixture",
                "path": "src/main.rs",
                "change": "modified",
                "oldSize": "24",
                "newSize": "24"
            }], 0, 514);
        }
        function loadMoreReviewRows(projectId) {
            ++nextPageCalls;
            if (crossPageNavigationActive && crossPagePage < 2) {
                ++crossPagePage;
                replaceReviewRows(crossPageRows(crossPagePage));
            }
        }
        function loadPreviousReviewRows(projectId) {
            ++previousPageCalls;
            if (crossPageNavigationActive && crossPagePage > 0) {
                --crossPagePage;
                replaceReviewRows(crossPageRows(crossPagePage));
            }
        }
        function clearReview() {}
        function refreshBranches(projectId) {}
        function refreshWorktrees(projectId) {}
        function commit(projectId, message, amend, pathIds) {
            ++commitCalls;
            lastCommitAmend = amend;
            lastCommitPathIds = String(pathIds);
        }
        function fetch(projectId) {}
        function pull(projectId) {}
        function push(projectId, allowDefaultBranch) {}
        function cancelJob(jobId) {}
        function checkoutBranch(projectId, branch) {}
        function createBranch(projectId, branch, startPoint) {}
        function createWorktree(projectId, mode, branch, startPoint) {}
        function moveWorktree(projectId, destination) {}
        function lockWorktree(projectId, reason) {}
        function unlockWorktree(projectId) {}
        function reconcileWorktrees(projectId) {}
    }

    GitPanel {
        id: gitPanel

        anchors.fill: parent
        backend: fixtureBackend
        project: projectFixture
        selectedProjectId: String(projectFixture.id)
    }

    GitActivity {
        id: activityFixture

        backend: fixtureBackend
        project: projectFixture
    }

    // Instantiate the history list directly so the smoke test creates its
    // delegates even while the main panel is exercising the review surface.
    HistoryPanel {
        id: historyFixture

        activity: activityFixture
        backend: fixtureBackend
        height: 400
        project: projectFixture
        visible: true
        width: 320
        x: -1000
        y: -1000
    }

    // The tab and the header toolbar are what GitPanel hosts; instantiating
    // them here as well is what makes their functions callable by name below.
    CommitSelection {
        id: selectionFixture

        project: projectFixture
    }

    ChangesPanel {
        id: changesFixture

        activity: activityFixture
        backend: fixtureBackend
        project: projectFixture
        selection: selectionFixture
        visible: false
    }

    RepositoryToolbar {
        id: toolbarFixture

        activity: activityFixture
        backend: fixtureBackend
        project: projectFixture
        visible: false
    }

    ReviewSurface {
        id: reviewFixture

        visible: true
        width: 640
        height: 720
        backend: fixtureBackend
        gitState: fixtureBackend.git
        project: projectFixture
        stateReady: true
    }

    ChecksPanel {
        id: checksFixture

        backend: fixtureBackend
        height: 720
        project: projectFixture
        visible: true
        width: 520
        x: -1000
        y: -1000
    }

    Timer {
        id: smokeTimeout

        interval: 10000
        repeat: false
        running: true
        onTriggered: {
            objectName = "GitPanelSmokeTimedOut";
            Qt.quit();
        }
    }

    Timer {
        id: screenshotTimer

        interval: 250
        repeat: false
        onTriggered: {
            reviewFixture.grabToImage(function(result) {
                screenshotSaved = result.saveToFile(screenshotPath);
                Qt.quit();
            });
        }
    }

    Component.onCompleted: {
        if (screenshotPath.length > 0) {
            fixtureBackend.jobs = [];
            if (screenshotReveal)
                reviewFixture.setRevealWhitespace(true);
            smokeTimeout.stop();
            screenshotTimer.start();
            return;
        }
        realBackend.openProject(realProjectId);

        const configuredCheck = fixtureBackend.checks.configured[0];
        const invocation = checksFixture.invocationPreview(configuredCheck);
        const escapedInvocation = checksFixture.escapedRichMultiline(invocation);
        const marker = reviewFixture.lineDiagnostic("src/main.rs", 1);
        checkPresentationDetected = invocation.indexOf(
                '["cargo", "test name", "%2", "<literal>"]'
            ) !== -1
            && invocation.indexOf("cwd: crate dir") !== -1
            && invocation.indexOf('"A_FIRST": "first value"')
                < invocation.indexOf('"Z_LAST": "last"')
            && invocation.indexOf("timeout: 45 seconds") !== -1
            && invocation.indexOf("parser: cargo_json") !== -1
            && escapedInvocation.indexOf("&lt;literal&gt;") !== -1
            && !checksFixture.invocationMatches(
                configuredCheck,
                fixtureBackend.checks.results[0]
            )
            && checksFixture.recordedInvocation(
                fixtureBackend.checks.results[0]
            ).indexOf('["cargo", "test", "--workspace"]') !== -1
            && marker !== null
            && marker.freshness === "stale"
            && reviewFixture.fileDiagnosticFreshness("src/main.rs") === "stale"
            && reviewFixture.diagnosticIcon(marker.freshness) === "data-warning"
            && reviewFixture.diagnosticStateReference(marker)
                .indexOf("0123456789abcdef") !== -1
            && !reviewFixture.checkCoversReview({
                "definitionCurrent": true,
                "workspaceCleanKnown": true,
                "workspaceClean": true,
                "stateHead": "ffffffffffffffffffffffffffffffffffffffff",
                "freshness": "current"
            })
            && reviewFixture.checkHeadline().indexOf("1 stale") !== -1;

        // Whitespace rendering, both ways round. The property is written
        // directly rather than through `setRevealWhitespace` so that nothing
        // is queued behind this block; what the setter adds
        // on top of it, holding the reader's place, is checked once there is
        // a diff long enough to have a place to hold.
        const revealedBefore = reviewFixture.revealWhitespace;
        const quiet = reviewFixture.highlightedLine(
            oldWordSegments, "src/main.rs", "deletion", "none", false
        );
        const quietUnchangedEnding = reviewFixture.highlightedLine(
            newWordSegments, "src/main.rs", "addition", "lf", false
        );
        const quietChangedEnding = reviewFixture.highlightedLine(
            oldWordSegments, "src/main.rs", "deletion", "crlf", true
        );
        const droppedEnding = reviewFixture.highlightedLine(
            newWordSegments, "src/main.rs", "addition", "none", true
        );
        reviewFixture.revealWhitespace = true;
        const revealed = reviewFixture.highlightedLine(
            oldWordSegments, "src/main.rs", "deletion", "none", false
        );
        const revealedEnding = reviewFixture.highlightedLine(
            newWordSegments, "src/main.rs", "addition", "lf", false
        );
        const revealedOtherEnding = reviewFixture.highlightedLine(
            newWordSegments, "src/main.rs", "addition", "crlf", false
        );
        const revealedContext = reviewFixture.highlightedLine(
            trailingContextSegments, "src/main.rs", "context", "lf", false
        );
        const revealedCopy = reviewFixture.copyTextForRow(
            fixtureBackend.review.file.rows[2], ""
        );
        reviewFixture.revealWhitespace = false;
        const quietContext = reviewFixture.highlightedLine(
            trailingContextSegments, "src/main.rs", "context", "lf", false
        );

        whitespaceRenderDetected =
            // Off by default, and the trailing run is tinted anyway: it is
            // the change nobody thinks to look for.
            revealedBefore === false
            && quiet.indexOf("background-color") !== -1
            && quiet.indexOf("\u00b7") === -1
            && quiet.indexOf("\u00bb") === -1
            // A changed run that is nothing but whitespace carries the
            // emphasis and a tint at once, rather than an empty underline.
            && quiet.indexOf("text-decoration:underline") !== -1
            && quiet.split("background-color").length - 1 >= 2
            // An unchanged line ending says nothing until it is asked to, and
            // trailing whitespace the diff did not touch says nothing either:
            // a legacy file's context must not light up.
            && quietUnchangedEnding.indexOf("\u00b6") === -1
            && quietChangedEnding.indexOf("CRLF") !== -1
            && quietContext.indexOf("background-color") === -1
            // Both halves of a pair that lost its terminator say so; a line
            // that simply has none, and always had none, does not.
            && droppedEnding.indexOf("NO EOL") !== -1
            && quiet.indexOf("NO EOL") === -1
            // Revealed, both whitespace bytes are told apart, every ending is
            // marked and the ones that are not plain LF are named, context
            // whitespace shows, and not one column has moved.
            && revealed.indexOf("\u00b7") !== -1
            && revealed.indexOf("\u00bb") !== -1
            && revealedEnding.indexOf("\u00b6") !== -1
            && revealedOtherEnding.indexOf("CRLF") !== -1
            && revealedContext.indexOf("background-color") !== -1
            && renderedColumns(revealed) === renderedColumns(quiet);

        // What reaches the clipboard is the line as it was read, never the
        // glyphs drawn over it; and a side-by-side row copies the half that
        // was asked for rather than whichever one happens to be present.
        whitespaceCopyDetected = revealedCopy === "\tlet old_word = true;  \r\n"
            && reviewFixture.copyTextForRow(
                fixtureBackend.review.file.rows[2], "old"
            ) === "\tlet old_word = true;  \r\n"
            && reviewFixture.copyTextForRow(
                fixtureBackend.review.file.rows[2], "new"
            ) === "    let new_word = true;\n"
            && reviewFixture.copyTextForRow(
                fixtureBackend.review.file.rows[1], ""
            ) === "";
        // And the whole path to the clipboard, terminator included. The write
        // goes through the backend rather than through a text document for
        // exactly this reason: a document would hand over "...true;  \n".
        reviewFixture.reviewCurrentIndex = 2;
        reviewFixture.copyCurrentReviewLine();
        whitespaceCopyDetected = whitespaceCopyDetected
            && fixtureBackend.clipboardCalls === 1
            && fixtureBackend.lastClipboardText === "\tlet old_word = true;  \r\n";
        changesFixture.selectPath("fixture-path", "added", "modified");
        fixtureBackend.jobs = [];
        const previewProbe = historyFixture.commitSummaryPreview(
            "A commit subject that is deliberately longer than the history row limit "
                + "so the preview must be shortened"
        );
        const unicodePreview = historyFixture.commitSummaryPreview(
            "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\uD83D\uDE00 trailing"
        );
        const emptySummaryPreview = historyFixture.commitSummaryPreview(
            "",
            "The body must not become an empty commit subject's preview"
        );
        historyPreviewDetected = previewProbe.length === historyFixture.commitSummaryLimit
            && previewProbe.endsWith("\u2026");
        historyPreviewDetected = historyPreviewDetected
            && unicodePreview === "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\uD83D\uDE00\u2026"
            && emptySummaryPreview === "";

        // The Changes list is a keyed model rather than the entry array, so
        // this is what catches the roles or the binding drifting apart from
        // the delegate. Dropping one entry must leave the list at the rows the
        // projection still holds.
        const wholeStatus = fixtureBackend.git;
        changesModelDetected = changesFixture.rowCount === 2;
        fixtureBackend.git = Object.assign({}, wholeStatus, {
            "entries": [wholeStatus.entries[1]]
        });
        changesModelDetected = changesModelDetected && changesFixture.rowCount === 1;
        fixtureBackend.git = wholeStatus;
        changesModelDetected = changesModelDetected && changesFixture.rowCount === 2;

        // Everything starts included, and a file is dropped from the commit by
        // unchecking it rather than by moving it across the index.
        const allEntries = fixtureBackend.git.entries;
        selectionDetected = selectionFixture.all(allEntries)
            && selectionFixture.countIncluded(allEntries) === 2
            && selectionFixture.includedPathIds(allEntries)
                === "fixture-path\nfixture-path-2";
        selectionFixture.setIncluded("src/main.rs", false);
        selectionDetected = selectionDetected
            && !selectionFixture.all(allEntries)
            && !selectionFixture.none(allEntries)
            && selectionFixture.countIncluded(allEntries) === 1
            && selectionFixture.includedPathIds(allEntries) === "fixture-path-2";
        // An exclusion for a path that is no longer changed must not survive to
        // silently hold that path back if it changes again.
        selectionFixture.setIncluded("src/gone.rs", false);
        selectionFixture.prune(allEntries);
        selectionDetected = selectionDetected
            && selectionFixture.included("src/gone.rs")
            && !selectionFixture.included("src/main.rs");
        selectionFixture.setAll(allEntries, false);
        selectionDetected = selectionDetected
            && selectionFixture.none(allEntries)
            && selectionFixture.includedPathIds(allEntries) === "";
        selectionFixture.setAll(allEntries, true);
        selectionDetected = selectionDetected && selectionFixture.all(allEntries);

        // The footer commits the panel's own selection, not the whole list.
        gitPanel.selection.setIncluded("src/main.rs", false);
        commitScopeDetected = gitPanel.includedCount === 1;
        const commitsBefore = fixtureBackend.commitCalls;
        gitPanel.backend.commit(
            gitPanel.project.id,
            "fixture commit",
            false,
            gitPanel.selection.includedPathIds(gitPanel.entries)
        );
        commitScopeDetected = commitScopeDetected
            && fixtureBackend.commitCalls === commitsBefore + 1
            && fixtureBackend.lastCommitAmend === false
            && fixtureBackend.lastCommitPathIds === "fixture-path-2";

        // Amending stays available with nothing checked, because rewriting the
        // previous commit's message is a real thing to want; committing does
        // not, because it would record nothing.
        gitPanel.draftSummary = "drafted subject";
        gitPanel.selection.setAll(gitPanel.entries, false);
        amendGatingDetected = gitPanel.includedCount === 0
            && !gitPanel.commitAllowed()
            && gitPanel.amendAllowed();
        gitPanel.selection.setAll(gitPanel.entries, true);
        amendGatingDetected = amendGatingDetected
            && gitPanel.commitAllowed()
            && gitPanel.amendAllowed();
        // There is no previous commit to rewrite on an unborn branch.
        fixtureBackend.git = Object.assign({}, fixtureBackend.git, { "unborn": true });
        amendGatingDetected = amendGatingDetected
            && !gitPanel.amendAllowed()
            && gitPanel.commitAllowed();
        fixtureBackend.git = Object.assign({}, fixtureBackend.git, { "unborn": false });
        gitPanel.draftSummary = "";

        // Side-by-side collapses a replacement's two unified rows into one, so
        // the split view must display exactly one row fewer than the unified
        // view of the same model.
        const unifiedFixtureRowCount = reviewFixture.displayedReviewRowCount();
        const lineOpensBeforeKeyboardActivation = fixtureBackend.openLineCalls;
        reviewFixture.reviewCurrentIndex = 2;
        reviewFixture.openCurrentReviewLine();
        reviewFixture.setSplitLayout(true);
        reviewFixture.reviewCurrentIndex = 2;
        reviewFixture.openCurrentReviewLine();
        editorOpenDetected = fixtureBackend.openLineCalls
                === lineOpensBeforeKeyboardActivation + 2
            && fixtureBackend.lastOpenFileId === "review-file-fixture"
            && fixtureBackend.lastOpenLine === 1;
        pairedRowsDetected = unifiedFixtureRowCount
                === fixtureBackend.review.file.rows.length
            && reviewFixture.displayedReviewRowCount()
                === unifiedFixtureRowCount - 1
            && fixtureBackend.review.file.rows[2].unified.kind === "deletion"
            && fixtureBackend.review.file.rows[3].unified.kind === "addition";
        const fileCallsBeforeBoundaryNavigation = fixtureBackend.loadReviewFileCalls;
        reviewFixture.navigateFile(-1);
        reviewFixture.navigateFile(1);
        fileBoundaryDetected = fixtureBackend.loadReviewFileCalls
            === fileCallsBeforeBoundaryNavigation;
        replaceReviewFileWindow(fixtureBackend.review.files, 0, 514);
        reviewFixture.loadReviewFilePage("next");
        Qt.callLater(function() {
            const nextBoundarySelected = fixtureBackend.lastReviewFileId
                === "review-file-page-2-first";
            reviewFixture.navigateFile(1);
            const adjacentFileSelected = fixtureBackend.lastReviewFileId
                === "review-file-page-2-second";
            reviewFixture.loadReviewFilePage("previous");
            Qt.callLater(function() {
                filePagingDetected = nextBoundarySelected
                    && adjacentFileSelected
                    && fixtureBackend.nextFilePageCalls === 1
                    && fixtureBackend.previousFilePageCalls === 1
                    && fixtureBackend.loadReviewFileCalls
                        === fileCallsBeforeBoundaryNavigation + 3
                    && fixtureBackend.lastReviewFileId
                        === "review-file-fixture";
            });
        });
        fixtureBackend.jobs = [{
            "id": "job-review",
            "kind": "review",
            "projectId": "linked-worktree-job",
            "lockScope": String(projectFixture.lockScope),
            "label": "Load review",
            "progress": "Starting...",
            "cancellable": false
        }];
        reviewBusyDetected = activityFixture.reviewReadRunning()
            && reviewFixture.reviewReadRunning();
        fixtureBackend.jobs = [{
            "id": "job-commit",
            "kind": "commit",
            "projectId": "linked-worktree-job",
            "lockScope": String(projectFixture.lockScope),
            "label": "Commit",
            "progress": "Starting...",
            "cancellable": true
        }];
        mutationBusyDetected = activityFixture.repositoryMutationRunning()
            && reviewFixture.repositoryMutationRunning();
        fixtureBackend.jobs = [{
            "id": "job-status",
            "kind": "status",
            "projectId": "linked-worktree-job",
            "lockScope": String(projectFixture.lockScope),
            "label": "Refresh Git status",
            "progress": "Starting...",
            "cancellable": true
        }];
        operationBusyDetected = activityFixture.repositoryOperationRunning()
            && reviewFixture.repositoryOperationRunning();
        fixtureBackend.jobs = [];
        reviewFixture.loadReviewRowPage("next");
        reviewFixture.loadReviewRowPage("previous");
        pagingControlsDetected = fixtureBackend.nextPageCalls === 1
            && fixtureBackend.previousPageCalls === 1;
        fixtureBackend.crossPageNavigationActive = true;
        fixtureBackend.crossPagePage = 0;
        fixtureBackend.crossPagePhase = 0;
        replaceReviewRows(crossPageRows(0));
        Qt.callLater(function() {
            reviewFixture.reviewCurrentIndex = 0;
            reviewFixture.navigateHunk(1);
            Qt.callLater(function() {
                verifyCrossPageNavigation(40);
            });
        });
    }
}
"#
                .to_vec(),
            )
            .unwrap()
            .replace("__REAL_PROJECT_ID__", &real_project_id)
            // Both reveal states are worth a screenshot, and a screenshot run
            // does not drive the surface through its checks: the state is
            // chosen before the grab rather than toggled during one.
            .replace(
                "__SCREENSHOT_REVEAL__",
                if std::env::var("HARKNESS_QML_SCREENSHOT_REVEAL")
                    .is_ok_and(|value| value == "1" || value == "true")
                {
                    "true"
                } else {
                    "false"
                },
            )
            .replace(
                "__SCREENSHOT_PATH__",
                &screenshot_path
                    .as_deref()
                    .unwrap_or_default()
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('\n', "\\n")
                    .replace('\r', "\\r"),
            );
            engine.as_mut().load_data(
                &QByteArray::from(interaction_qml.as_bytes()),
                &QUrl::from("qrc:/GitPanelSmoke.qml"),
            );
        }
        assert!(
            LOADED.load(Ordering::SeqCst),
            "GitPanel.qml failed to load; see QML warnings above"
        );
        if let Some(app) = app.as_mut() {
            app.exec();
        }
        let interaction_root = INTERACTION_ROOT.load(Ordering::SeqCst);
        let interaction_name = unsafe { interaction_root.as_ref() }
            .map(|object| object.object_name().to_string())
            .unwrap_or_default();
        let expected_interaction_name = if screenshot_path.is_some() {
            "GitPanelScreenshotSaved"
        } else {
            "GitPanelSmokePassed"
        };
        assert_eq!(
            interaction_name, expected_interaction_name,
            "GitPanel and ReviewSurface interaction check failed"
        );
        if screenshot_path.is_none() {
            // The bridge checked one of two changed files and was handed no
            // staged content and no separate staging call. What reached HEAD,
            // and what did not, is the whole proof: the commit staged its own
            // selection and left the rest alone.
            let repository = Repository::open(&repository_root).unwrap();
            let tree = repository
                .head()
                .unwrap()
                .peel_to_commit()
                .unwrap()
                .tree()
                .unwrap();
            let committed = tree
                .get_path(path)
                .expect("the commit bridge must record the checked path")
                .to_object(&repository)
                .unwrap();
            assert_eq!(
                committed.as_blob().unwrap().content(),
                b"after\n",
                "the public commit bridge must record the working-tree bytes exactly"
            );
            assert!(
                tree.get_path(excluded_path).is_err(),
                "the unchecked path must stay out of the commit"
            );
            let statuses = repository.statuses(None).unwrap();
            let left = statuses
                .iter()
                .map(|entry| (entry.path().unwrap_or_default().to_owned(), entry.status()))
                .collect::<Vec<_>>();
            assert_eq!(
                left.len(),
                1,
                "only the unchecked path may survive the commit: {left:?}"
            );
            assert_eq!(left[0].0, "excluded.txt");
            assert!(
                left[0].1.contains(git2::Status::WT_NEW),
                "the unchecked path must be left in the working tree, unstaged: {left:?}"
            );
        }
        // The engine must be released before the application; dropping locals
        // in declaration order would do the opposite.
        drop(engine);
        drop(app);
    }

    /// One-off diagnostic: drives a real `HarknessBackend` push against a
    /// clone whose remote default branch is protected, through the real
    /// `GitPanel.qml`, and reports whether `gitState.errorKind` comes back
    /// as `default_branch_push` the way the override button expects.
    #[allow(dead_code)]
    pub(crate) fn default_branch_push_repro() {
        use std::process::Command;

        let fixture = TempDir::new().unwrap();
        let data_dir = fixture.path().join("data");
        let remote_root = fixture.path().join("remote");
        let clone_root = fixture.path().join("clone");

        let git = |dir: &Path, args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(dir)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed in {dir:?}");
        };

        fs::create_dir_all(&remote_root).unwrap();
        git(&remote_root, &["init", "--bare", "-q"]);
        git(
            fixture.path(),
            &[
                "clone",
                "-q",
                remote_root.to_str().unwrap(),
                clone_root.to_str().unwrap(),
            ],
        );
        git(&clone_root, &["config", "user.email", "tests@example.com"]);
        git(&clone_root, &["config", "user.name", "Harkness Tests"]);
        fs::write(clone_root.join("a.txt"), "one\n").unwrap();
        git(&clone_root, &["add", "a.txt"]);
        git(&clone_root, &["commit", "-q", "-m", "init"]);
        git(&clone_root, &["branch", "-M", "main"]);
        git(&clone_root, &["push", "-q", "-u", "origin", "main"]);
        git(&clone_root, &["remote", "set-head", "origin", "--auto"]);
        fs::write(clone_root.join("a.txt"), "two\n").unwrap();
        git(&clone_root, &["commit", "-q", "-am", "second"]);

        // SAFETY: set before any Qt object is constructed, and this binary
        // runs single-threaded with respect to Qt/env usage.
        unsafe {
            std::env::set_var("QT_QPA_PLATFORM", "offscreen");
            std::env::set_var("QT_FORCE_STDERR_LOGGING", "1");
            std::env::set_var("HARKNESS_DATA_DIR", &data_dir);
        }
        let real_project = harkness_core::ProjectService::load()
            .unwrap()
            .import_local(&clone_root)
            .unwrap();
        let real_project_id = real_project.id.to_string();

        cxx_qt::init_qml_module!("io.github.fullstacktaiye.harkness");
        let mut app = QGuiApplication::new();
        let mut engine = QQmlApplicationEngine::new();

        static RESULT_ROOT: AtomicPtr<QObject> = AtomicPtr::new(ptr::null_mut());
        if let Some(mut engine) = engine.as_mut() {
            let _connection = engine.as_mut().on_object_created(|_engine, object, _url| {
                RESULT_ROOT.store(object, Ordering::SeqCst);
            });
            let qml = String::from_utf8(
                br#"
import QtQuick
import org.kde.kirigami as Kirigami
import io.github.fullstacktaiye.harkness

Kirigami.ApplicationWindow {
    id: window
    visible: false
    width: 640
    height: 480
    objectName: "PushReproPending"

    property int phase: 0

    HarknessBackend { id: backend }

    property var proj: ({
        "id": "__REAL_PROJECT_ID__",
        "lockScope": "__REAL_PROJECT_ID__",
        "displayName": "repro",
        "root": "__REPO_ROOT__",
        "remote": "",
        "branch": "",
        "managed": false,
        "worktree": false,
        "parentId": "",
        "parentName": "",
        "createdBranch": "",
        "available": true,
        "isGit": true,
        "dirty": false
    })

    GitPanel {
        id: gitPanel
        backend: backend
        project: window.proj
    }

    function overrideButtonWouldShow() {
        return gitPanel.stateReady
            && ["default_branch_push", "default_branch_unknown"]
                .indexOf(gitPanel.gitState.errorKind) !== -1;
    }

    Timer {
        interval: 20000
        running: true
        onTriggered: {
            window.objectName = "PushReproTimedOut-phase" + window.phase
                + "-jobs" + backend.jobs.length
                + "-gitProjectId" + String(backend.git.projectId || "<none>")
                + "-jobKinds[" + backend.jobs.map(function(j) { return j.kind; }).join(",") + "]";
            Qt.quit();
        }
    }

    Connections {
        target: backend

        function onOpenedChanged() {
            if (window.phase === 0
                    && String(backend.opened.id || "") === "__REAL_PROJECT_ID__") {
                window.phase = 1;
                backend.refreshGit("__REAL_PROJECT_ID__");
            }
        }

        function onGitChanged() {
            if (window.phase === 1 && backend.jobs.length === 0
                    && String(backend.git.projectId || "") === "__REAL_PROJECT_ID__") {
                window.phase = 2;
                backend.push("__REAL_PROJECT_ID__", false);
            } else if (window.phase === 2 && backend.jobs.length === 0
                    && String(backend.git.projectId || "") === "__REAL_PROJECT_ID__") {
                window.phase = 3;
                Qt.callLater(function() {
                    const passed = gitPanel.stateReady
                        && String(gitPanel.gitState.errorKind || "") === "default_branch_push"
                        && overrideButtonWouldShow();
                    window.objectName = passed
                        ? "PushOverrideConditionPassed"
                        : "PushOverrideConditionFailed:" + JSON.stringify({
                            error: String(gitPanel.gitState.error || ""),
                            errorKind: String(gitPanel.gitState.errorKind || ""),
                            stateReady: gitPanel.stateReady,
                            overrideButtonWouldShow: overrideButtonWouldShow()
                        });
                    Qt.quit();
                });
            }
        }
    }

    Component.onCompleted: backend.openProject("__REAL_PROJECT_ID__")
}
"#
                .to_vec(),
            )
            .unwrap()
            .replace("__REAL_PROJECT_ID__", &real_project_id)
            .replace("__REPO_ROOT__", &clone_root.display().to_string());
            engine.as_mut().load_data(
                &QByteArray::from(qml.as_bytes()),
                &QUrl::from("qrc:/PushOverrideRepro.qml"),
            );
        }

        if let Some(app) = app.as_mut() {
            app.exec();
        }

        let root = RESULT_ROOT.load(Ordering::SeqCst);
        let name = unsafe { root.as_ref() }
            .map(|object| object.object_name().to_string())
            .unwrap_or_default();
        drop(engine);
        drop(app);
        assert_eq!(
            name, "PushOverrideConditionPassed",
            "GitPanel must surface a default_branch_push refusal with the \
             override button's visibility condition true; got: {name}"
        );
    }
}
