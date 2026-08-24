//! The diagnostic log's lifecycle, and the span fields a log is filtered by.
//!
//! Its own binary because [`observe::init`] installs a process-global
//! subscriber: whichever test calls it first decides where every later line in
//! the process goes, so exactly one test here may call it.

use std::collections::BTreeMap;
use std::fs;
use std::sync::{Arc, Mutex};

use harkness_runtime::approval::ApprovalId;
use harkness_runtime::domain::{RunId, StepId, ToolCallId};
use harkness_runtime::observe::{self, LOG_FILE_NAME, log_directory, log_file};
use harkness_runtime::tool::ToolIdentity;
use tempfile::TempDir;
use tracing::field::{Field, Visit};
use tracing::span::Attributes;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};

/// One span's name and the fields it was opened with.
type Opened = (String, BTreeMap<String, String>);

/// Every span opened while this is installed, by name and fields.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<Opened>>>);

impl Captured {
    fn named(&self, name: &str) -> BTreeMap<String, String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .find(|(recorded, _)| recorded == name)
            .map(|(_, fields)| fields.clone())
            .unwrap_or_else(|| panic!("no span named {name} was opened"))
    }
}

struct Fields<'a>(&'a mut BTreeMap<String, String>);

impl Visit for Fields<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }
}

impl<S: tracing::Subscriber> Layer<S> for Captured {
    fn on_new_span(
        &self,
        attributes: &Attributes<'_>,
        _id: &tracing::span::Id,
        _ctx: Context<'_, S>,
    ) {
        let mut fields = BTreeMap::new();
        attributes.record(&mut Fields(&mut fields));
        self.0
            .lock()
            .unwrap()
            .push((attributes.metadata().name().to_owned(), fields));
    }
}

#[test]
fn every_span_names_its_run_and_the_fields_the_module_documents() {
    let run = RunId::new();
    let step = StepId::new();
    let call = ToolCallId::new();
    let approval = ApprovalId::new();
    let tool = ToolIdentity::parse("fixture.observe", "1.2.3").unwrap();

    let captured = Captured::default();
    let subscriber = tracing_subscriber::registry().with(captured.clone());
    tracing::subscriber::with_default(subscriber, || {
        let _run = observe::run_span(run).entered();
        let _step = observe::step_span(run, step).entered();
        let _call = observe::tool_call_span(run, step, call, &tool).entered();
        let _approval = observe::approval_span(run, call, approval).entered();
    });

    for name in ["run", "step", "tool_call", "approval"] {
        assert_eq!(
            captured.named(name).get("run_id").map(String::as_str),
            Some(run.to_string().as_str()),
            "{name} must name its run itself: work crosses threads, and a span \
             that inherited the field would lose it"
        );
    }

    let step_fields = captured.named("step");
    assert_eq!(
        step_fields.get("step_id").map(String::as_str),
        Some(step.to_string().as_str())
    );

    let call_fields = captured.named("tool_call");
    assert_eq!(
        call_fields.get("step_id").map(String::as_str),
        Some(step.to_string().as_str())
    );
    assert_eq!(
        call_fields.get("tool_call_id").map(String::as_str),
        Some(call.to_string().as_str())
    );
    assert_eq!(
        call_fields.get("tool_id").map(String::as_str),
        Some("fixture.observe")
    );
    assert_eq!(
        call_fields.get("tool_version").map(String::as_str),
        Some("1.2.3"),
        "the resolved version, so a line can be compared with tool_calls.tool_version"
    );

    let approval_fields = captured.named("approval");
    assert_eq!(
        approval_fields.get("approval_id").map(String::as_str),
        Some(approval.to_string().as_str())
    );
    assert_eq!(
        approval_fields.get("tool_call_id").map(String::as_str),
        Some(call.to_string().as_str())
    );
}

#[test]
fn the_log_is_created_by_the_first_line_written_and_initialized_exactly_once() {
    let data_dir = TempDir::new().unwrap();

    let outcome = observe::init(
        Some(data_dir.path()),
        observe::Options::default().with_default_filter("info"),
    );
    let observe::InitOutcome::Logging { path, mirrored } = outcome else {
        panic!("a resolvable data directory should have been arranged: {outcome:?}");
    };
    assert_eq!(path, log_file(data_dir.path()));
    assert!(!mirrored, "the mirror is opt-in");
    assert!(
        !log_directory(data_dir.path()).exists(),
        "installing a subscriber must not write to a data directory nobody asked to change"
    );

    // A field carrying a forged newline and a quote, which is the shape a tool
    // would use to write a log line of its own. It must survive as *one* JSON
    // string rather than becoming two records.
    tracing::info!(
        forged = "first\n{\"level\":\"INFO\",\"fields\":{\"message\":\"forged\"}}",
        leaked = "cloning https://user:hunter2@example.com/repo.git",
        "diagnostics smoke line"
    );

    assert!(
        log_file(data_dir.path()).exists(),
        "the first line creates the file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(log_directory(data_dir.path()))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(log_file(data_dir.path()))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    let written = fs::read_to_string(log_file(data_dir.path())).unwrap();
    let lines: Vec<&str> = written
        .lines()
        .filter(|line| line.contains("diagnostics smoke line"))
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "a forged newline must stay inside a JSON string: {written}"
    );
    let parsed: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(parsed["fields"]["message"], "diagnostics smoke line");
    assert!(
        !written.contains("hunter2"),
        "the log writer applies the same rules the store does: {written}"
    );
    assert!(written.contains("«redacted:url_userinfo»"));

    // The whole point of the guard: a second front end, or a second call from
    // the same one, changes nothing that is already installed.
    let again = observe::init(Some(data_dir.path()), observe::Options::default());
    assert_eq!(again, observe::InitOutcome::AlreadyInitialized);
    assert!(
        again.describe().contains("already initialized"),
        "a front end has to be able to say what happened"
    );

    // The directory holds one file and no archives: nothing here approached the
    // rotation bound, and rotation must not happen on reopen.
    let names: Vec<String> = fs::read_dir(log_directory(data_dir.path()))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec![LOG_FILE_NAME.to_owned()]);
}
