//! A reconciling list model for the approvals waiting to be answered.
//!
//! Every pending approval is a run parked mid-execution waiting for a person, so
//! this is the one model whose rows are a queue rather than a history. The
//! listing is unpaged on purpose — the runtime's pending set is bounded by
//! construction, because a request exists only while a call is parked on it and
//! the scheduler caps how many calls are in flight.
//!
//! # Rows are reconciled, not replaced
//!
//! Answering one question must not disturb the dialog above another. A refresh
//! therefore plans the smallest set of row notifications that turns what the
//! model holds into what the runtime reports: an answered request is one
//! removal, a newly parked call is one insertion, and everything else keeps its
//! delegate. Only a reordering or an ambiguous key falls back to a reset, which
//! is the same rule `ChangesModel` follows for the working tree.
//!
//! # The Qt-thread mutation invariant
//!
//! Every `ApprovalModelRust` field is read and written on the Qt thread. Listing
//! the queue opens SQLite, so it happens on a `std::thread::spawn` worker and
//! comes back through `qt_thread().queue(...)`; the worker owns plain `String`s
//! and never a `QString`, a `QVariant`, or a pinned reference.
//!
//! # The staleness counter
//!
//! `next_request` is `HarknessBackend::next_review_request`'s mechanism: a
//! listing whose number is no longer the newest is dropped rather than applied,
//! so a slow refresh cannot restore a row that a later refresh — taken after the
//! answer — already removed.
//!
//! # Rows are summaries
//!
//! A row carries the binding fields a person needs in order to answer: which
//! tool at which version, its declared risk and capabilities, the breadth the
//! answer would authorize, and the request's own bounded input *summary*. The
//! validated input itself is loaded on demand through
//! `RunsBackend::loadApprovalInput`, because it is the tool call's and may be as
//! large as the store's inline bound allows.
//!
//! The one role that is not a plain field is `grantableScopes`: every breadth
//! the *runtime* would accept a decision on this request at. It is carried
//! rather than re-derived in QML, because a decision surface that computed it
//! itself would be a second copy of `ApprovalRequest::decide`'s rule, free to
//! drift into offering a button the runtime refuses.

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

        include!("listmodelbase.h");
        type ApprovalModelBase;

        #[rust_name = "begin_insert"]
        fn beginInsert(self: Pin<&mut ApprovalModelBase>, first: i32, last: i32);

        #[rust_name = "end_insert"]
        fn endInsert(self: Pin<&mut ApprovalModelBase>);

        #[rust_name = "begin_remove"]
        fn beginRemove(self: Pin<&mut ApprovalModelBase>, first: i32, last: i32);

        #[rust_name = "end_remove"]
        fn endRemove(self: Pin<&mut ApprovalModelBase>);

        #[rust_name = "begin_reset"]
        fn beginReset(self: Pin<&mut ApprovalModelBase>);

        #[rust_name = "end_reset"]
        fn endReset(self: Pin<&mut ApprovalModelBase>);

        #[rust_name = "emit_changed"]
        fn changed(self: Pin<&mut ApprovalModelBase>, first: i32, last: i32);
    }

    extern "RustQt" {
        /// Every unanswered approval, oldest first.
        ///
        /// cxx-qt does not convert names to camel case, so property names are
        /// kept to a single word and every multi-word member names its Qt
        /// spelling explicitly. `count` is the queue length, which is what a
        /// badge binds to; `loading` is true while a listing is in flight and
        /// `status` carries the last failure's message.
        #[qobject]
        #[qml_element]
        #[base = ApprovalModelBase]
        #[qproperty(i32, count)]
        #[qproperty(bool, loading)]
        #[qproperty(QString, status)]
        #[qproperty(QString, kind)]
        type ApprovalModel = super::ApprovalModelRust;

        #[cxx_override]
        fn data(self: &ApprovalModel, index: &QModelIndex, role: i32) -> QVariant;

        #[cxx_override]
        #[cxx_name = "rowCount"]
        fn row_count(self: &ApprovalModel, parent: &QModelIndex) -> i32;

        #[cxx_override]
        #[cxx_name = "roleNames"]
        fn role_names(self: &ApprovalModel) -> QHash_i32_QByteArray;

        /// Re-reads the pending queue and reconciles the rows against it.
        #[qinvokable]
        fn refresh(self: Pin<&mut ApprovalModel>);
    }

    impl cxx_qt::Threading for ApprovalModel {}
}

use std::pin::Pin;

use cxx_qt::{CxxQtType, Threading, casting::Upcast};
use cxx_qt_lib::{
    QByteArray, QHash, QHashPair_i32_QByteArray, QMap, QMapPair_QString_QVariant, QModelIndex,
    QString, QVariant,
};

use harkness_runtime::approval::ApprovalRequest;

use super::reconcile::{Edit, Keyed, plan};

use super::runs_backend::{
    RunsFailure, data_dir, note_qt_thread, optional_rfc3339, read_store, rfc3339, strings,
};

/// `Qt::DisplayRole`, so a row reads as its tool in accessibility tooling.
const DISPLAY_ROLE: i32 = 0;
/// `Qt::UserRole + 1` and up: the roles QML delegates bind to.
const APPROVAL_ID_ROLE: i32 = 257;
const RUN_ID_ROLE: i32 = 258;
const TOOL_CALL_ID_ROLE: i32 = 259;
const TOOL_ROLE: i32 = 260;
const TOOL_ID_ROLE: i32 = 261;
const TOOL_VERSION_ROLE: i32 = 262;
const RISK_ROLE: i32 = 263;
const SCOPE_ROLE: i32 = 264;
const REQUESTED_SCOPE_ROLE: i32 = 265;
const DOWNGRADED_ROLE: i32 = 266;
const CAPABILITIES_ROLE: i32 = 267;
const SUMMARY_ROLE: i32 = 268;
const REQUESTED_ROLE: i32 = 269;
const EXPIRES_ROLE: i32 = 270;
const WORKSPACE_ROLE: i32 = 271;
const PROJECT_ROLE: i32 = 272;
const GRANTABLE_ROLE: i32 = 273;

fn model_roles() -> QHash<QHashPair_i32_QByteArray> {
    let mut roles = QHash::<QHashPair_i32_QByteArray>::default();
    roles.insert(DISPLAY_ROLE, QByteArray::from("display"));
    roles.insert(APPROVAL_ID_ROLE, QByteArray::from("approvalId"));
    roles.insert(RUN_ID_ROLE, QByteArray::from("runId"));
    roles.insert(TOOL_CALL_ID_ROLE, QByteArray::from("toolCallId"));
    roles.insert(TOOL_ROLE, QByteArray::from("tool"));
    roles.insert(TOOL_ID_ROLE, QByteArray::from("toolId"));
    roles.insert(TOOL_VERSION_ROLE, QByteArray::from("toolVersion"));
    roles.insert(RISK_ROLE, QByteArray::from("risk"));
    roles.insert(SCOPE_ROLE, QByteArray::from("scope"));
    roles.insert(REQUESTED_SCOPE_ROLE, QByteArray::from("requestedScope"));
    roles.insert(DOWNGRADED_ROLE, QByteArray::from("downgraded"));
    roles.insert(CAPABILITIES_ROLE, QByteArray::from("capabilities"));
    roles.insert(SUMMARY_ROLE, QByteArray::from("summary"));
    roles.insert(REQUESTED_ROLE, QByteArray::from("requested"));
    roles.insert(EXPIRES_ROLE, QByteArray::from("expires"));
    roles.insert(WORKSPACE_ROLE, QByteArray::from("workspace"));
    roles.insert(PROJECT_ROLE, QByteArray::from("projectId"));
    roles.insert(GRANTABLE_ROLE, QByteArray::from("grantableScopes"));
    roles
}

/// One pending approval as a dialog draws it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ApprovalRow {
    approval_id: String,
    run_id: String,
    tool_call_id: String,
    tool: String,
    tool_id: String,
    tool_version: String,
    risk: String,
    scope: String,
    requested_scope: String,
    downgraded: bool,
    capabilities: String,
    summary: String,
    requested: String,
    expires: String,
    workspace: String,
    project_id: String,
    /// Stored spellings of every breadth a decision on this request may be
    /// given at, narrowest first — the runtime's own
    /// `ApprovalRequest::grantable_scopes`, carried rather than re-derived.
    grantable: Vec<String>,
}

impl Keyed for ApprovalRow {
    /// The approval's own identity, which is stable for as long as the question
    /// is unanswered — and an answered one leaves the queue rather than
    /// changing key.
    fn key(&self) -> &str {
        &self.approval_id
    }
}

/// Projects one durable request into a row.
///
/// `scope` is the *effective* scope — what an answer may actually authorize —
/// and `requestedScope` is what was asked for before the risk ceiling; the two
/// are separate roles for the same reason the record stores both, so a surface
/// can show a downgrade instead of hiding it.
pub(crate) fn approval_row(request: &ApprovalRequest) -> ApprovalRow {
    ApprovalRow {
        approval_id: request.id().to_string(),
        run_id: request.run_id().to_string(),
        tool_call_id: request.tool_call_id().to_string(),
        tool: request.tool().to_string(),
        tool_id: request.tool().id.as_str().to_owned(),
        tool_version: request.tool().version.to_string(),
        risk: request.risk().as_str().to_owned(),
        scope: request.effective_scope().as_str().to_owned(),
        requested_scope: request.requested_scope().as_str().to_owned(),
        downgraded: request.was_downgraded(),
        capabilities: request
            .capabilities()
            .iter()
            .map(|capability| capability.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        summary: request.input_summary().to_owned(),
        requested: rfc3339(request.created_at()),
        expires: optional_rfc3339(request.expires_at()),
        workspace: request.workspace().canonical_root().display().to_string(),
        project_id: request
            .workspace()
            .project_id()
            .map(|id| id.to_string())
            .unwrap_or_default(),
        // Read off the record rather than derived here. A surface offering
        // these and nothing else cannot ask for a breadth `decide` would
        // refuse, which is what stops the window and the runtime disagreeing
        // about what a button would authorize.
        grantable: request
            .grantable_scopes()
            .iter()
            .map(|scope| scope.as_str().to_owned())
            .collect(),
    }
}

/// One row as a map keyed by the model's own role names.
///
/// A run's detail page names the approval it is parked on with the same words
/// the queue uses, so it reads this projection rather than a second one beside
/// it. Every key is a role name from [`model_roles`], which the test below
/// asserts.
pub(crate) fn row_map(row: &ApprovalRow) -> QMap<QMapPair_QString_QVariant> {
    let mut map = QMap::<QMapPair_QString_QVariant>::default();
    let mut text = |key: &str, value: &str| {
        map.insert(QString::from(key), QVariant::from(&QString::from(value)));
    };
    text("approvalId", &row.approval_id);
    text("runId", &row.run_id);
    text("toolCallId", &row.tool_call_id);
    text("tool", &row.tool);
    text("toolId", &row.tool_id);
    text("toolVersion", &row.tool_version);
    text("risk", &row.risk);
    text("scope", &row.scope);
    text("requestedScope", &row.requested_scope);
    text("capabilities", &row.capabilities);
    text("summary", &row.summary);
    text("requested", &row.requested);
    text("expires", &row.expires);
    text("workspace", &row.workspace);
    text("projectId", &row.project_id);
    map.insert(QString::from("downgraded"), QVariant::from(&row.downgraded));
    map.insert(QString::from("grantableScopes"), strings(&row.grantable));
    map
}

/// Reads the pending queue off the Qt thread.
///
/// A data directory that has recorded nothing answers with an empty queue rather
/// than creating a run store: opening the approvals surface is a read.
fn load_pending() -> Result<Vec<ApprovalRow>, RunsFailure> {
    load_pending_in(&data_dir()?)
}

/// Reads the pending queue from a named data directory.
///
/// Split from [`load_pending`] so a test can seed a temporary store and read it
/// back without touching `HARKNESS_DATA_DIR`, which is process-wide.
fn load_pending_in(data_dir: &std::path::Path) -> Result<Vec<ApprovalRow>, RunsFailure> {
    let Some(store) = read_store(data_dir)? else {
        return Ok(Vec::new());
    };
    Ok(store
        .pending_approvals()?
        .iter()
        .map(approval_row)
        .collect())
}

#[derive(Default)]
pub struct ApprovalModelRust {
    rows: Vec<ApprovalRow>,
    count: i32,
    loading: bool,
    status: QString,
    kind: QString,
    next_request: u64,
}

impl ffi::ApprovalModel {
    fn data(&self, index: &QModelIndex, role: i32) -> QVariant {
        if !index.is_valid() {
            return QVariant::default();
        }
        let Ok(row) = usize::try_from(index.row()) else {
            return QVariant::default();
        };
        let Some(entry) = self.rust().rows.get(row) else {
            return QVariant::default();
        };
        let text = |value: &str| QVariant::from(&QString::from(value));
        match role {
            DISPLAY_ROLE | TOOL_ROLE => text(&entry.tool),
            APPROVAL_ID_ROLE => text(&entry.approval_id),
            RUN_ID_ROLE => text(&entry.run_id),
            TOOL_CALL_ID_ROLE => text(&entry.tool_call_id),
            TOOL_ID_ROLE => text(&entry.tool_id),
            TOOL_VERSION_ROLE => text(&entry.tool_version),
            RISK_ROLE => text(&entry.risk),
            SCOPE_ROLE => text(&entry.scope),
            REQUESTED_SCOPE_ROLE => text(&entry.requested_scope),
            DOWNGRADED_ROLE => QVariant::from(&entry.downgraded),
            CAPABILITIES_ROLE => text(&entry.capabilities),
            SUMMARY_ROLE => text(&entry.summary),
            REQUESTED_ROLE => text(&entry.requested),
            EXPIRES_ROLE => text(&entry.expires),
            WORKSPACE_ROLE => text(&entry.workspace),
            PROJECT_ROLE => text(&entry.project_id),
            GRANTABLE_ROLE => strings(&entry.grantable),
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

    fn refresh(mut self: Pin<&mut Self>) {
        note_qt_thread();
        let request = {
            let rust = self.as_mut().rust_mut().get_mut();
            rust.next_request += 1;
            rust.next_request
        };
        self.as_mut().set_loading(true);
        let qt_thread = self.qt_thread();
        std::thread::spawn(move || {
            let outcome = load_pending();
            let _ = qt_thread.queue(move |model| apply_queue(model, request, outcome));
        });
    }

    /// Applies one planned edit, wrapping the row mutation in the notification
    /// Qt requires for it.
    fn edit(mut self: Pin<&mut Self>, edit: Edit<ApprovalRow>) {
        match edit {
            Edit::Remove { first, last } => {
                {
                    let base: Pin<&mut ffi::ApprovalModelBase> = self.as_mut().upcast_pin();
                    base.begin_remove(first as i32, last as i32);
                }
                self.as_mut().rust_mut().get_mut().rows.drain(first..=last);
                let base: Pin<&mut ffi::ApprovalModelBase> = self.as_mut().upcast_pin();
                base.end_remove();
            }
            Edit::Insert { first, rows } => {
                let last = first + rows.len() - 1;
                {
                    let base: Pin<&mut ffi::ApprovalModelBase> = self.as_mut().upcast_pin();
                    base.begin_insert(first as i32, last as i32);
                }
                self.as_mut()
                    .rust_mut()
                    .get_mut()
                    .rows
                    .splice(first..first, rows);
                let base: Pin<&mut ffi::ApprovalModelBase> = self.as_mut().upcast_pin();
                base.end_insert();
            }
            Edit::Update { first, rows } => {
                let last = first + rows.len() - 1;
                self.as_mut()
                    .rust_mut()
                    .get_mut()
                    .rows
                    .splice(first..=last, rows);
                let base: Pin<&mut ffi::ApprovalModelBase> = self.as_mut().upcast_pin();
                base.emit_changed(first as i32, last as i32);
            }
        }
    }
}

fn apply_queue(
    mut model: Pin<&mut ffi::ApprovalModel>,
    request: u64,
    outcome: Result<Vec<ApprovalRow>, RunsFailure>,
) {
    // A listing a later refresh superseded would restore rows that were answered
    // between the two reads.
    if model.as_ref().rust().next_request != request {
        return;
    }
    model.as_mut().set_loading(false);
    let incoming = match outcome {
        Ok(incoming) => incoming,
        Err(failure) => {
            model
                .as_mut()
                .set_status(QString::from(failure.message.as_str()));
            // The discriminant travels beside the message, so a surface can
            // tell a directory that has recorded nothing from one it could not
            // read.
            model
                .as_mut()
                .set_kind(QString::from(failure.kind.as_str()));
            return;
        }
    };
    model.as_mut().set_status(QString::default());
    model.as_mut().set_kind(QString::default());
    // Bound before the loop: a temporary in a `for` head lives for the whole
    // loop, and this one borrows the object the body mutates.
    let planned = plan(&model.as_ref().rust().rows, &incoming);
    match planned {
        Some(edits) => {
            for edit in edits {
                model.as_mut().edit(edit);
            }
        }
        None => {
            {
                let base: Pin<&mut ffi::ApprovalModelBase> = model.as_mut().upcast_pin();
                base.begin_reset();
            }
            model.as_mut().rust_mut().get_mut().rows = incoming;
            let base: Pin<&mut ffi::ApprovalModelBase> = model.as_mut().upcast_pin();
            base.end_reset();
        }
    }
    let count = i32::try_from(model.as_ref().rust().rows.len()).unwrap_or(i32::MAX);
    model.as_mut().set_count(count);
}

#[cfg(test)]
mod tests {
    use cxx_qt_lib::QByteArray;
    use serde_json::json;
    use time::OffsetDateTime;

    use harkness_core::ProjectId;
    use harkness_runtime::approval::{
        ApprovalRequest, ApprovalScope, PendingApproval, WorkspaceBinding, canonical_input_hash,
    };
    use harkness_runtime::domain::{
        ExecutionState, Run, RunId, Step, Task, TaskId, ToolCall, ToolCallId,
    };
    use harkness_runtime::store::Store;
    use harkness_runtime::tool::{Capability, RiskLevel, ToolIdentity};
    use tempfile::TempDir;

    use super::super::reconcile::{Edit, plan};
    use super::{
        APPROVAL_ID_ROLE, CAPABILITIES_ROLE, DISPLAY_ROLE, DOWNGRADED_ROLE, EXPIRES_ROLE,
        GRANTABLE_ROLE, PROJECT_ROLE, REQUESTED_ROLE, REQUESTED_SCOPE_ROLE, RISK_ROLE, RUN_ID_ROLE,
        SCOPE_ROLE, SUMMARY_ROLE, TOOL_CALL_ID_ROLE, TOOL_ID_ROLE, TOOL_ROLE, TOOL_VERSION_ROLE,
        WORKSPACE_ROLE, approval_row, load_pending_in, model_roles, row_map,
    };

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_755_000_000 + seconds).unwrap()
    }

    fn pending(tool: &str, risk: RiskLevel) -> PendingApproval {
        PendingApproval::new(
            RunId::new(),
            ToolCallId::new(),
            ToolIdentity::parse(tool, "1.0.0").unwrap(),
            canonical_input_hash(&json!({"command": ["cargo", "test"]})).unwrap(),
            WorkspaceBinding::new(Some(ProjectId::new()), "/workspace/harkness"),
            risk,
            at(0),
        )
    }

    fn request(tool: &str) -> ApprovalRequest {
        ApprovalRequest::open(
            pending(tool, RiskLevel::Execute)
                .summarized_as("cargo test")
                .with_capabilities([Capability::new("process.spawn").unwrap()]),
        )
        .unwrap()
    }

    #[test]
    fn qml_roles_have_stable_names() {
        let roles = model_roles();

        for (role, name) in [
            (DISPLAY_ROLE, "display"),
            (APPROVAL_ID_ROLE, "approvalId"),
            (RUN_ID_ROLE, "runId"),
            (TOOL_CALL_ID_ROLE, "toolCallId"),
            (TOOL_ROLE, "tool"),
            (TOOL_ID_ROLE, "toolId"),
            (TOOL_VERSION_ROLE, "toolVersion"),
            (RISK_ROLE, "risk"),
            (SCOPE_ROLE, "scope"),
            (REQUESTED_SCOPE_ROLE, "requestedScope"),
            (DOWNGRADED_ROLE, "downgraded"),
            (CAPABILITIES_ROLE, "capabilities"),
            (SUMMARY_ROLE, "summary"),
            (REQUESTED_ROLE, "requested"),
            (EXPIRES_ROLE, "expires"),
            (WORKSPACE_ROLE, "workspace"),
            (PROJECT_ROLE, "projectId"),
            (GRANTABLE_ROLE, "grantableScopes"),
        ] {
            assert_eq!(roles.get(&role), Some(QByteArray::from(name)));
        }
    }

    #[test]
    fn the_banner_projection_uses_exactly_the_role_names_a_delegate_binds_to() {
        let map = row_map(&approval_row(&request("process.exec")));

        let mut expected: Vec<String> = model_roles()
            .iter()
            .map(|(_, name)| name.to_string())
            // `display` is Qt's own role for accessibility tooling and names no
            // field of its own; every other role is a field a banner shows.
            .filter(|name| name != "display")
            .collect();
        expected.sort();
        let mut published: Vec<String> = map.iter().map(|(key, _)| key.to_string()).collect();
        published.sort();
        assert_eq!(published, expected);
    }

    #[test]
    fn a_row_carries_the_binding_fields_an_answer_is_given_against() {
        let request = request("process.exec");

        let row = approval_row(&request);

        assert_eq!(row.approval_id, request.id().to_string());
        assert_eq!(row.tool, "process.exec@1.0.0");
        assert_eq!(row.tool_id, "process.exec");
        assert_eq!(row.tool_version, "1.0.0");
        assert_eq!(row.risk, "execute");
        assert_eq!(row.capabilities, "process.spawn");
        assert_eq!(row.summary, "cargo test");
        assert_eq!(row.requested, "2025-08-12T12:00:00Z");
        assert_eq!(row.expires, "", "v0.3 requests wait for a person");
        assert_eq!(row.workspace, "/workspace/harkness");
    }

    #[test]
    fn a_row_carries_the_input_summary_and_never_the_input() {
        let row = approval_row(&request("process.exec"));

        assert_eq!(row.summary, "cargo test");
        assert!(
            !row.summary.contains("command"),
            "the validated input is loaded on demand, not carried here"
        );
    }

    #[test]
    fn a_downgraded_request_shows_both_the_asked_for_and_the_allowed_breadth() {
        let request = ApprovalRequest::open(
            pending("git.push", RiskLevel::RemoteWrite).requesting(ApprovalScope::ToolForRun),
        )
        .unwrap();

        let row = approval_row(&request);

        assert_eq!(row.requested_scope, "tool_for_run");
        assert_eq!(row.scope, "exact_call", "the risk ceiling narrowed it");
        assert!(row.downgraded);
    }

    #[test]
    fn a_request_carries_the_breadths_the_runtime_would_accept_an_answer_at() {
        let request = ApprovalRequest::open(
            pending("fs.apply_patch", RiskLevel::WorkspaceWrite)
                .requesting(ApprovalScope::CapabilityForRun)
                .with_capabilities([Capability::new("fs.write").unwrap()]),
        )
        .unwrap();

        let row = approval_row(&request);

        assert_eq!(row.grantable, vec!["exact_call", "capability_for_run"]);
        assert_eq!(
            row.grantable,
            request
                .grantable_scopes()
                .iter()
                .map(|scope| scope.as_str().to_owned())
                .collect::<Vec<_>>(),
            "the row must carry the record's own answer rather than a second derivation"
        );
    }

    #[test]
    fn a_one_call_only_request_offers_the_surface_no_breadth_to_choose_from() {
        let request = ApprovalRequest::open(
            pending("git.push", RiskLevel::RemoteWrite).requesting(ApprovalScope::ToolForRun),
        )
        .unwrap();

        let row = approval_row(&request);

        assert_eq!(
            row.grantable,
            vec!["exact_call"],
            "a downgraded request has one answer, so a surface renders no choice"
        );
    }

    #[test]
    fn an_unchanged_queue_plans_no_edits() {
        let rows = vec![
            approval_row(&request("process.exec")),
            approval_row(&request("fs.apply_patch")),
        ];

        assert_eq!(plan(&rows, &rows.clone()), Some(Vec::new()));
    }

    #[test]
    fn approval_rows_disappear_when_the_coordinator_resolves_them() {
        let current = vec![
            approval_row(&request("process.exec")),
            approval_row(&request("fs.apply_patch")),
            approval_row(&request("test.run")),
        ];
        let incoming = vec![current[0].clone(), current[2].clone()];

        assert_eq!(
            plan(&current, &incoming),
            Some(vec![Edit::Remove { first: 1, last: 1 }])
        );
    }

    #[test]
    fn a_newly_parked_call_is_inserted_at_the_end_of_the_queue() {
        let current = vec![approval_row(&request("process.exec"))];
        let added = approval_row(&request("test.run"));
        let incoming = vec![current[0].clone(), added.clone()];

        assert_eq!(
            plan(&current, &incoming),
            Some(vec![Edit::Insert {
                first: 1,
                rows: vec![added],
            }])
        );
    }

    #[test]
    fn answering_the_whole_queue_removes_every_row_in_one_run() {
        let current = vec![
            approval_row(&request("process.exec")),
            approval_row(&request("test.run")),
        ];

        assert_eq!(
            plan(&current, &[]),
            Some(vec![Edit::Remove { first: 0, last: 1 }])
        );
    }

    #[test]
    fn a_reordered_queue_falls_back_to_a_reset() {
        let current = vec![
            approval_row(&request("process.exec")),
            approval_row(&request("test.run")),
        ];
        let incoming = vec![current[1].clone(), current[0].clone()];

        assert_eq!(plan(&current, &incoming), None);
    }

    #[test]
    fn ambiguous_keys_fall_back_to_a_reset() {
        let row = approval_row(&request("process.exec"));

        assert_eq!(plan(&[], &[row.clone(), row]), None);
    }

    #[test]
    fn a_data_directory_that_recorded_nothing_reads_as_an_empty_queue() {
        let fixture = TempDir::new().unwrap();

        assert!(
            load_pending_in(&fixture.path().join("never-used"))
                .unwrap()
                .is_empty()
        );
        assert!(
            !fixture.path().join("never-used").exists(),
            "a read must not be what creates the run store"
        );
    }

    #[test]
    fn a_seeded_store_lists_the_question_its_run_is_holding() {
        let fixture = TempDir::new().unwrap();
        let data_dir = fixture.path().join("data");
        let store = Store::open(&data_dir).unwrap();
        let task = Task::with_id(
            TaskId::new(),
            "Check: cargo test",
            "/workspace/harkness",
            None,
            at(0),
        );
        store.insert_task(&task).unwrap();
        let run = Run::with_id(RunId::new(), task.id(), at(1));
        store.insert_run(&run).unwrap();
        let step = Step::new(run.id(), 0, "run the check", at(1));
        store.insert_step(&step).unwrap();
        let input = json!({"command": ["cargo", "test"]});
        let call = ToolCall::new(&step, "process.exec", "1.0.0", input.clone(), at(1));
        store.insert_tool_call(&call).unwrap();
        let opened = ApprovalRequest::open(
            PendingApproval::new(
                run.id(),
                call.id(),
                ToolIdentity::parse("process.exec", "1.0.0").unwrap(),
                canonical_input_hash(&input).unwrap(),
                WorkspaceBinding::new(None, "/workspace/harkness"),
                RiskLevel::Execute,
                at(2),
            )
            .summarized_as("cargo test"),
        )
        .unwrap();
        store.open_approval(opened.clone()).unwrap();
        store
            .transition_run(run.id(), ExecutionState::Running, at(3))
            .unwrap();
        store
            .transition_run(run.id(), ExecutionState::Succeeded, at(4))
            .unwrap();
        drop(store);

        let rows = load_pending_in(&data_dir).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].approval_id, opened.id().to_string());
        assert_eq!(rows[0].run_id, run.id().to_string());
        assert_eq!(rows[0].tool_call_id, call.id().to_string());
        assert_eq!(rows[0].tool, "process.exec@1.0.0");
        assert_eq!(rows[0].summary, "cargo test");
        assert_eq!(rows[0].scope, "exact_call");
    }
}
