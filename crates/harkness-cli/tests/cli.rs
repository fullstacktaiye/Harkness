use std::{
    collections::{BTreeSet, HashMap},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use git2::{ObjectType, Repository, Signature};
use harkness_core::{Project, ProjectService};
use harkness_git::{DiffOptions, DiffTarget, GitService};
use serde_json::{Value, json};
use tempfile::TempDir;

const FIXED_ID: &str = "00000000-0000-4000-8000-000000000001";

#[test]
fn version_output_is_exact_and_remains_text_with_json() {
    let fixture = TempDir::new().unwrap();
    let expected = format!("harkness {}\n", env!("CARGO_PKG_VERSION"));
    for arguments in [&["--version"][..], &["--json", "--version"][..]] {
        let output = harkness(fixture.path(), arguments);
        assert!(output.status.success());
        assert_eq!(output.stdout, expected.as_bytes());
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn no_arguments_is_a_usage_error_with_no_stdout() {
    let fixture = TempDir::new().unwrap();
    let output = harkness::<&str>(fixture.path(), &[]);

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
}

#[test]
fn json_empty_project_list_is_exact() {
    let fixture = TempDir::new().unwrap();
    let output = harkness(fixture.path(), &["--json", "project", "list"]);

    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        b"{\"v\":1,\"type\":\"success\",\"ok\":true,\"data\":{\"projects\":[]}}\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn project_json_wire_shape_and_rfc3339_timestamp_are_exact() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let project_root = fixture.path().join("wire-project");
    initialize_repository(&project_root);
    write_catalog(
        &data_dir,
        vec![json!({
            "id": FIXED_ID,
            "display_name": "wire-project",
            "root": fs::canonicalize(&project_root).unwrap(),
            "source": "local",
            "last_opened": "2026-08-06 18:52:03.000000000 +00:00:00",
        })],
    );
    let repository = Repository::open(&project_root).unwrap();
    let branch = repository.head().unwrap().shorthand().unwrap().to_owned();

    let output = harkness(
        &data_dir,
        &["--json", "project", "show", "--project", FIXED_ID],
    );

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let root =
        serde_json::to_string(&fs::canonicalize(project_root).unwrap().to_string_lossy()).unwrap();
    let branch = serde_json::to_string(&branch).unwrap();
    let expected = format!(
        "{{\"v\":1,\"type\":\"success\",\"ok\":true,\"data\":{{\"project\":{{\"available\":true,\"display_name\":\"wire-project\",\"git\":{{\"branch\":{branch},\"dirty\":false,\"staged\":0,\"unstaged\":0,\"upstream\":null}},\"id\":\"{FIXED_ID}\",\"last_opened\":\"2026-08-06T18:52:03Z\",\"parent\":null,\"path_is_lossy\":false,\"remote\":null,\"root\":{root},\"source\":\"local\",\"status_checked\":true,\"worktree_branch\":null}}}}}}\n"
    );
    assert_eq!(output.stdout, expected.as_bytes());
}

#[test]
fn project_selection_accepts_id_prefix_path_name_and_current_directory() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let parent_root = fixture.path().join("parent");
    let nested_root = parent_root.join("NestedProject");
    let working_directory = nested_root.join("src");
    fs::create_dir_all(&working_directory).unwrap();
    let mut service = ProjectService::load_from_data_dir(&data_dir).unwrap();
    service.import_local(&parent_root).unwrap();
    let nested = service.import_local(&nested_root).unwrap();
    let id = nested.id.to_string();
    let prefix = &id[..8];
    let canonical_root = fs::canonicalize(&nested_root).unwrap();

    for selector in [
        id.as_str(),
        prefix,
        canonical_root.to_str().unwrap(),
        "NestedProject",
        "nestedproject",
    ] {
        let output = harkness(
            &data_dir,
            &["--json", "project", "show", "--project", selector],
        );
        assert_selected(&output, &nested);
    }

    let ambient = harkness_from(
        &data_dir,
        &working_directory,
        &["--json", "project", "show"],
    );
    assert_selected(&ambient, &nested);

    let resolved = harkness(&data_dir, &["--json", "project", "resolve", prefix]);
    assert_selected(&resolved, &nested);
}

#[test]
fn bare_name_resolution_is_independent_of_current_directory() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let first_parent = fixture.path().join("first");
    let second_parent = fixture.path().join("second");
    let first = first_parent.join("alpha");
    let second = second_parent.join("alpha");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();
    let mut service = ProjectService::load_from_data_dir(&data_dir).unwrap();
    service.import_local(first).unwrap();
    service.import_local(second).unwrap();

    for current_dir in [
        fixture.path(),
        first_parent.as_path(),
        second_parent.as_path(),
    ] {
        let output = harkness_from(
            &data_dir,
            current_dir,
            &["--json", "project", "show", "--project", "alpha"],
        );
        assert_eq!(output.status.code(), Some(5));
        assert_eq!(
            json_output(&output)["error"]["kind"],
            "ambiguous_project_selector"
        );
    }
}

#[cfg(unix)]
#[test]
fn editor_configuration_and_cli_open_use_literal_argv() {
    use std::os::unix::fs::PermissionsExt;
    use std::{
        thread,
        time::{Duration, Instant},
    };

    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let project_root = fixture.path().join("project");
    initialize_repository(&project_root);
    fs::create_dir_all(project_root.join("src")).unwrap();
    fs::write(project_root.join("src/main.rs"), "fn main() {}\n").unwrap();
    let project = ProjectService::load_from_data_dir(&data_dir)
        .unwrap()
        .import_local(&project_root)
        .unwrap();
    let editor = fixture.path().join("record-editor");
    fs::write(&editor, "#!/bin/sh\nprintf '%s' \"$2\" > \"$1\"\n").unwrap();
    let mut permissions = fs::metadata(&editor).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&editor, permissions).unwrap();
    let log = fixture.path().join("editor-argv");

    let configured = harkness(
        &data_dir,
        &[
            "editor",
            "set",
            "--",
            editor.to_str().unwrap(),
            log.to_str().unwrap(),
            "{file}:{line}:{column}",
        ],
    );
    assert!(
        configured.status.success(),
        "{}",
        String::from_utf8_lossy(&configured.stderr)
    );
    let opened = harkness(
        &data_dir,
        &[
            "--json",
            "editor",
            "open",
            "--project",
            &project.id.to_string(),
            "src/main.rs",
            "--line",
            "14",
            "--column",
            "6",
        ],
    );
    assert!(
        opened.status.success(),
        "{}",
        String::from_utf8_lossy(&opened.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !log.exists() {
        assert!(Instant::now() < deadline, "editor shim did not record argv");
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        fs::read(&log).unwrap(),
        format!("{}:14:6", project.root.join("src/main.rs").display()).as_bytes()
    );
    let response: Value = serde_json::from_slice(&opened.stdout).unwrap();
    assert_eq!(response["data"]["kind"], "editor_open");
    assert_eq!(response["data"]["line"], 14);

    fs::remove_file(&log).unwrap();
    let dotted = harkness(
        &data_dir,
        &[
            "--json",
            "editor",
            "open",
            "--project",
            &project.id.to_string(),
            "./src/./main.rs",
        ],
    );
    assert!(dotted.status.success());
    let dotted_response = json_output(&dotted);
    assert_eq!(
        dotted_response["data"]["file"],
        project.root.join("src/main.rs").to_string_lossy().as_ref()
    );

    let zero_line = harkness(
        &data_dir,
        &[
            "editor",
            "open",
            "--project",
            &project.id.to_string(),
            "src/main.rs",
            "--line",
            "0",
        ],
    );
    assert_eq!(zero_line.status.code(), Some(2));

    let invalid_template = harkness(
        &data_dir,
        &["--json", "editor", "set", "--", "code", "--wait"],
    );
    assert_eq!(invalid_template.status.code(), Some(3));
    let invalid_template = json_output(&invalid_template);
    assert_eq!(invalid_template["error"]["kind"], "invalid_editor_template");
    assert_eq!(invalid_template["error"]["details"]["command"], "code");

    let missing = "harkness-editor-that-does-not-exist";
    assert!(
        harkness(&data_dir, &["editor", "set", "--", missing, "{file}"])
            .status
            .success()
    );
    let failed = harkness(
        &data_dir,
        &[
            "--json",
            "editor",
            "open",
            "--project",
            &project.id.to_string(),
            "src/main.rs",
        ],
    );
    assert_eq!(failed.status.code(), Some(1));
    let failure: Value = serde_json::from_slice(&failed.stdout).unwrap();
    assert_eq!(failure["error"]["kind"], "editor_launch");
    assert_eq!(failure["error"]["details"]["command"], missing);
    assert!(
        failure["error"]["message"]
            .as_str()
            .unwrap()
            .contains(missing)
    );

    assert!(harkness(&data_dir, &["editor", "clear"]).status.success());
    let fallback_log = fixture.path().join("visual-argv");
    let fallback = Command::new(env!("CARGO_BIN_EXE_harkness"))
        .env("HARKNESS_DATA_DIR", &data_dir)
        .env(
            "VISUAL",
            format!("{} {}", editor.display(), fallback_log.display()),
        )
        .env("EDITOR", "harkness-editor-fallback-should-not-run")
        .args([
            "editor",
            "open",
            "--project",
            &project.id.to_string(),
            "src/main.rs",
        ])
        .output()
        .unwrap();
    assert!(
        fallback.status.success(),
        "{}",
        String::from_utf8_lossy(&fallback.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !fallback_log.exists() {
        assert!(
            Instant::now() < deadline,
            "$VISUAL shim did not record argv"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        fs::read(fallback_log).unwrap(),
        project
            .root
            .join("src/main.rs")
            .as_os_str()
            .as_encoded_bytes()
    );

    let editor_fallback_log = fixture.path().join("editor-fallback-argv");
    let editor_fallback = Command::new(env!("CARGO_BIN_EXE_harkness"))
        .env("HARKNESS_DATA_DIR", &data_dir)
        .env_remove("VISUAL")
        .env(
            "EDITOR",
            format!("{} {}", editor.display(), editor_fallback_log.display()),
        )
        .args([
            "editor",
            "open",
            "--project",
            &project.id.to_string(),
            "src/main.rs",
        ])
        .output()
        .unwrap();
    assert!(editor_fallback.status.success());
    let deadline = Instant::now() + Duration::from_secs(10);
    while !editor_fallback_log.exists() {
        assert!(
            Instant::now() < deadline,
            "$EDITOR shim did not record argv"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        fs::read(editor_fallback_log).unwrap(),
        project
            .root
            .join("src/main.rs")
            .as_os_str()
            .as_encoded_bytes()
    );

    let terminal_editor = fixture.path().join("terminal-editor");
    fs::write(
        &terminal_editor,
        "#!/bin/sh\nIFS= read -r value\nprintf '%s' \"$value\" > \"$1\"\n",
    )
    .unwrap();
    let mut permissions = fs::metadata(&terminal_editor).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&terminal_editor, permissions).unwrap();
    let terminal_log = fixture.path().join("terminal-input");
    assert!(
        harkness(
            &data_dir,
            &[
                "editor",
                "set",
                "--",
                terminal_editor.to_str().unwrap(),
                terminal_log.to_str().unwrap(),
                "{file}",
            ],
        )
        .status
        .success()
    );
    let mut terminal_launch = Command::new(env!("CARGO_BIN_EXE_harkness"))
        .env("HARKNESS_DATA_DIR", &data_dir)
        .args([
            "editor",
            "open",
            "--project",
            &project.id.to_string(),
            "src/main.rs",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    terminal_launch
        .stdin
        .take()
        .unwrap()
        .write_all(b"terminal input\n")
        .unwrap();
    assert!(terminal_launch.wait().unwrap().success());
    let deadline = Instant::now() + Duration::from_secs(10);
    while !terminal_log.exists() {
        assert!(
            Instant::now() < deadline,
            "configured editor did not inherit CLI stdin"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(fs::read(terminal_log).unwrap(), b"terminal input");
}

#[test]
fn ambiguous_project_name_lists_only_honest_identity_candidates() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let first = fixture.path().join("first").join("shared");
    let second = fixture.path().join("second").join("shared");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();
    let mut service = ProjectService::load_from_data_dir(&data_dir).unwrap();
    let first = service.import_local(first).unwrap();
    let second = service.import_local(second).unwrap();

    let output = harkness(
        &data_dir,
        &["--json", "project", "show", "--project", "shared"],
    );

    assert_eq!(output.status.code(), Some(5));
    assert!(output.stderr.is_empty());
    let body = json_output(&output);
    assert_envelope(&body, "error", false);
    assert_eq!(body["error"]["kind"], "ambiguous_project_selector");
    let candidates = body["error"]["details"]["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 2);
    let expected_keys = BTreeSet::from(["display_name", "id", "path_is_lossy", "root", "source"]);
    for candidate in candidates {
        assert_eq!(
            candidate
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            expected_keys
        );
    }
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate["id"] == first.id.to_string())
    );
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate["id"] == second.id.to_string())
    );
}

#[test]
fn deleted_project_root_still_resolves_by_its_written_path() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let project_root = fixture.path().join("gone");
    fs::create_dir_all(&project_root).unwrap();
    let mut service = ProjectService::load_from_data_dir(&data_dir).unwrap();
    let project = service.import_local(&project_root).unwrap();
    fs::remove_dir_all(&project_root).unwrap();

    let output = harkness(
        &data_dir,
        &[
            "--json",
            "project",
            "show",
            "--project",
            project_root.to_str().unwrap(),
        ],
    );

    assert_selected(&output, &project);
    assert_eq!(json_output(&output)["data"]["project"]["available"], false);
}

#[test]
fn missing_explicit_and_ambient_projects_use_exit_four() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    for output in [
        harkness(
            &data_dir,
            &["--json", "project", "show", "--project", "missing"],
        ),
        harkness_from(&data_dir, fixture.path(), &["--json", "project", "show"]),
    ] {
        assert_eq!(output.status.code(), Some(4));
        assert_eq!(
            json_output(&output)["error"]["kind"],
            "project_selector_not_found"
        );
    }
}

#[test]
fn delete_reports_source_guardrails_before_confirmation() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let project_root = fixture.path().join("local-project");
    fs::create_dir_all(&project_root).unwrap();
    let mut service = ProjectService::load_from_data_dir(&data_dir).unwrap();
    let project = service.import_local(project_root).unwrap();
    let id = project.id.to_string();

    for suffix in [&[][..], &["--yes"][..]] {
        let mut arguments = vec!["--json", "project", "delete", "--project", id.as_str()];
        arguments.extend_from_slice(suffix);
        let output = harkness(&data_dir, &arguments);
        assert_eq!(output.status.code(), Some(3));
        assert!(output.stderr.is_empty());
        let body = json_output(&output);
        assert_eq!(body["error"]["kind"], "local_project_requires_forget");
        assert!(body["error"]["details"].as_object().unwrap().is_empty());
    }
}

#[test]
fn data_directory_flag_overrides_the_environment() {
    let fixture = TempDir::new().unwrap();
    let environment_data_dir = fixture.path().join("environment-data");
    let explicit_data_dir = fixture.path().join("explicit-data");
    let output = harkness(
        &environment_data_dir,
        &[
            "--json",
            "--data-dir",
            explicit_data_dir.to_str().unwrap(),
            "project",
            "list",
        ],
    );

    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        b"{\"v\":1,\"type\":\"success\",\"ok\":true,\"data\":{\"projects\":[]}}\n"
    );
    assert!(!environment_data_dir.exists());
}

#[test]
fn project_import_and_forget_use_the_structured_contract() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let project_root = fixture.path().join("imported-project");
    fs::create_dir_all(&project_root).unwrap();

    let imported = harkness(
        &data_dir,
        &[
            "--json",
            "project",
            "import",
            project_root.to_str().unwrap(),
        ],
    );
    assert!(imported.status.success());
    assert!(imported.stderr.is_empty());
    let id = json_output(&imported)["data"]["project"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let forgotten = harkness(
        &data_dir,
        &["--json", "project", "forget", "--project", &id],
    );
    assert!(forgotten.status.success());
    assert!(forgotten.stderr.is_empty());
    let forgotten_body = json_output(&forgotten);
    assert_eq!(forgotten_body["data"]["project"]["id"], id);
    assert_eq!(forgotten_body["data"]["project"]["status_checked"], false);
    assert_eq!(forgotten_body["data"]["project"]["available"], Value::Null);
    assert!(project_root.exists());
    assert!(
        ProjectService::load_from_data_dir(&data_dir)
            .unwrap()
            .list_catalog_only()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn no_status_listing_is_cheap_and_marks_derived_state_unchecked() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let project_root = fixture.path().join("project");
    fs::create_dir_all(&project_root).unwrap();
    ProjectService::load_from_data_dir(&data_dir)
        .unwrap()
        .import_local(project_root)
        .unwrap();

    let output = harkness(&data_dir, &["--json", "project", "list", "--no-status"]);
    let project = &json_output(&output)["data"]["projects"][0];
    assert_eq!(project["status_checked"], false);
    assert_eq!(project["available"], Value::Null);
    assert_eq!(project["git"], Value::Null);

    let human = harkness(&data_dir, &["project", "list", "--no-status"]);
    assert!(
        String::from_utf8(human.stdout)
            .unwrap()
            .contains("\tunchecked\n")
    );
}

#[test]
fn git_and_worktree_commands_round_trip_end_to_end_through_json() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let (remote, source, parent_root) = remote_with_clone(fixture.path());
    let mut service = ProjectService::load_from_data_dir(&data_dir).unwrap();
    let parent = service.import_local(&parent_root).unwrap();
    let parent_id = parent.id.to_string();

    let status = harkness_from(&data_dir, &parent_root, &["--json", "git", "status"]);
    assert_success(&status);
    assert_eq!(
        json_output(&status)["data"]["status"]["head"]["kind"],
        "branch"
    );
    assert!(
        json_output(&status)["data"]["status"]["entries"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let fetched = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "fetch",
            "--project",
            &parent_id,
            "--remote",
            "origin",
            "--prune",
        ],
    );
    assert_success(&fetched);
    assert_eq!(json_output(&fetched)["data"]["remote"], "origin");

    let created_branch = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "branch",
            "create",
            "temporary",
            "--project",
            &parent_id,
            "--checkout",
        ],
    );
    assert_success(&created_branch);
    let checked_out = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "branch",
            "checkout",
            "main",
            "--project",
            &parent_id,
        ],
    );
    assert_success(&checked_out);
    let branches = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "branch",
            "list",
            "--project",
            &parent_id,
            "--all",
        ],
    );
    assert_success(&branches);
    assert!(
        json_output(&branches)["data"]["branches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|branch| branch["name"] == "temporary")
    );
    let deleted_branch = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "branch",
            "delete",
            "temporary",
            "--project",
            &parent_id,
        ],
    );
    assert_success(&deleted_branch);

    fs::write(source.join("remote-change.txt"), "from remote\n").unwrap();
    commit_all(&Repository::open(&source).unwrap(), "remote change");
    run_git(&source, &["push", "origin", "main"]);
    let fetched_update = harkness(
        &data_dir,
        &["--json", "git", "fetch", "--project", &parent_id],
    );
    assert_success(&fetched_update);
    assert_eq!(json_output(&fetched_update)["data"]["updated"], true);
    let pulled = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "pull",
            "--project",
            &parent_id,
            "--ff-only",
        ],
    );
    assert_success(&pulled);
    assert_eq!(
        json_output(&pulled)["data"]["strategy"],
        "fast_forward_only"
    );
    assert_eq!(json_output(&pulled)["data"]["updated"], true);

    let created = harkness(
        &data_dir,
        &[
            "--json",
            "worktree",
            "add",
            "--project",
            &parent_id,
            "--branch",
            "agent/cli",
            "--from",
            "main",
        ],
    );
    assert_success(&created);
    let created_body = json_output(&created);
    let worktree_id = created_body["data"]["project"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let original_checkout =
        PathBuf::from(created_body["data"]["project"]["root"].as_str().unwrap());
    let listed = harkness(
        &data_dir,
        &["--json", "worktree", "list", "--project", &parent_id],
    );
    assert_success(&listed);
    assert!(
        json_output(&listed)["data"]["worktrees"]
            .as_array()
            .unwrap()
            .iter()
            .any(|worktree| worktree["id"] == worktree_id)
    );

    let relative_move = harkness(
        &data_dir,
        &[
            "--json",
            "worktree",
            "move",
            "relative-checkout",
            "--project",
            &worktree_id,
        ],
    );
    assert_eq!(relative_move.status.code(), Some(3));
    assert_eq!(
        json_output(&relative_move)["error"]["kind"],
        "worktree_destination_not_absolute"
    );
    assert_eq!(
        json_output(&relative_move)["error"]["details"]["path"],
        "relative-checkout"
    );

    let moved_parent = fixture.path().join("cli-moved-worktrees");
    fs::create_dir(&moved_parent).unwrap();
    let occupied_checkout = moved_parent.join("occupied");
    fs::create_dir(&occupied_checkout).unwrap();
    let occupied_move = harkness(
        &data_dir,
        &[
            "--json",
            "worktree",
            "move",
            occupied_checkout.to_str().unwrap(),
            "--project",
            &worktree_id,
        ],
    );
    assert_eq!(occupied_move.status.code(), Some(5));
    assert_eq!(
        json_output(&occupied_move)["error"]["kind"],
        "worktree_destination_exists"
    );
    let requested_checkout = moved_parent.join("checkout");
    let moved = harkness(
        &data_dir,
        &[
            "--json",
            "worktree",
            "move",
            requested_checkout.to_str().unwrap(),
            "--project",
            &worktree_id,
        ],
    );
    assert_success(&moved);
    let checkout = PathBuf::from(
        json_output(&moved)["data"]["project"]["root"]
            .as_str()
            .unwrap(),
    );
    assert_eq!(
        checkout.canonicalize().unwrap(),
        requested_checkout.canonicalize().unwrap()
    );
    assert!(!original_checkout.exists());
    assert!(checkout.exists());

    fs::write(checkout.join("agent.txt"), "agent work\n").unwrap();
    let dirty_status = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "status",
            "--paths",
            "--project",
            &worktree_id,
        ],
    );
    assert_success(&dirty_status);
    let entries = json_output(&dirty_status)["data"]["status"]["entries"]
        .as_array()
        .unwrap()
        .clone();
    assert!(entries.iter().any(|entry| {
        entry["path"] == "agent.txt"
            && entry["path_is_lossy"] == false
            && entry["unstaged"] == "untracked"
    }));

    let failed_stage = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "stage",
            "missing.txt",
            "--project",
            &worktree_id,
        ],
    );
    assert_eq!(failed_stage.status.code(), Some(1));
    assert_eq!(
        json_output(&failed_stage)["error"]["kind"],
        "path_operation_failed"
    );
    assert_eq!(
        json_output(&failed_stage)["error"]["details"]["all_succeeded"],
        false
    );

    let staged = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "stage",
            "agent.txt",
            "--project",
            &worktree_id,
        ],
    );
    assert_success(&staged);
    assert_eq!(json_output(&staged)["data"]["all_succeeded"], true);
    let unstaged = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "unstage",
            "agent.txt",
            "--project",
            &worktree_id,
        ],
    );
    assert_success(&unstaged);
    let staged_again = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "stage",
            "agent.txt",
            "--project",
            &worktree_id,
        ],
    );
    assert_success(&staged_again);
    let committed = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "commit",
            "--message",
            "Commit agent work",
            "--project",
            &worktree_id,
        ],
    );
    assert_success(&committed);
    assert!(json_output(&committed)["data"]["commit_id"].is_string());
    let amended = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "commit",
            "--message",
            "Amend agent work",
            "--amend",
            "--allow-empty",
            "--project",
            &worktree_id,
        ],
    );
    assert_success(&amended);
    assert_eq!(json_output(&amended)["data"]["amended"], true);
    let pushed = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "push",
            "--set-upstream",
            "--project",
            &worktree_id,
        ],
    );
    assert_success(&pushed);
    assert_eq!(json_output(&pushed)["data"]["branch"], "agent/cli");
    assert!(
        Repository::open_bare(&remote)
            .unwrap()
            .find_reference("refs/heads/agent/cli")
            .is_ok()
    );

    let protected_push = harkness(
        &data_dir,
        &["--json", "git", "push", "--project", &parent_id],
    );
    assert_eq!(protected_push.status.code(), Some(3));
    assert_eq!(
        json_output(&protected_push)["error"]["kind"],
        "default_branch_push"
    );
    assert_eq!(
        json_output(&protected_push)["error"]["details"]["override_flag"],
        "--allow-default-branch"
    );
    let allowed_push = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "push",
            "--project",
            &parent_id,
            "--allow-default-branch",
            "--force-with-lease",
        ],
    );
    assert_success(&allowed_push);

    fs::write(checkout.join("dirty.txt"), "dirty\n").unwrap();
    let dirty = harkness(
        &data_dir,
        &["--json", "worktree", "remove", "--project", &worktree_id],
    );
    assert_eq!(dirty.status.code(), Some(3));
    assert_eq!(
        json_output(&dirty)["error"]["kind"],
        "dirty_worktree_removal"
    );
    assert_eq!(
        json_output(&dirty)["error"]["details"]["override_flag"],
        "--force"
    );
    let removed = harkness(
        &data_dir,
        &[
            "--json",
            "worktree",
            "remove",
            "--project",
            &worktree_id,
            "--force",
        ],
    );
    assert_success(&removed);
    assert!(!checkout.exists());

    let reused = harkness(
        &data_dir,
        &[
            "--json",
            "worktree",
            "add",
            "--project",
            &parent_id,
            "--branch",
            "agent/cli",
            "--existing",
        ],
    );
    assert_success(&reused);
    let reused_id = json_output(&reused)["data"]["project"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let removed_reused = harkness(
        &data_dir,
        &["--json", "worktree", "remove", "--project", &reused_id],
    );
    assert_success(&removed_reused);

    let unmerged = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "branch",
            "create",
            "unmerged",
            "--checkout",
            "--project",
            &parent_id,
        ],
    );
    assert_success(&unmerged);
    fs::write(parent_root.join("unmerged.txt"), "local only\n").unwrap();
    let staged_all = harkness(
        &data_dir,
        &["--json", "git", "stage", "--all", "--project", &parent_id],
    );
    assert_success(&staged_all);
    let local_commit = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "commit",
            "--message",
            "Unmerged work",
            "--project",
            &parent_id,
        ],
    );
    assert_success(&local_commit);
    let checkout_main = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "branch",
            "checkout",
            "main",
            "--project",
            &parent_id,
        ],
    );
    assert_success(&checkout_main);
    let refused_delete = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "branch",
            "delete",
            "unmerged",
            "--project",
            &parent_id,
        ],
    );
    assert_eq!(refused_delete.status.code(), Some(3));
    assert_eq!(
        json_output(&refused_delete)["error"]["kind"],
        "unmerged_branch_deletion"
    );
    assert_eq!(
        json_output(&refused_delete)["error"]["details"]["override_flag"],
        "--force"
    );
    let forced_delete = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "branch",
            "delete",
            "unmerged",
            "--project",
            &parent_id,
            "--force",
        ],
    );
    assert_success(&forced_delete);

    let detached = harkness(
        &data_dir,
        &[
            "--json",
            "worktree",
            "add",
            "--project",
            &parent_id,
            "--branch",
            "main",
            "--detach",
        ],
    );
    assert_success(&detached);
    assert!(json_output(&detached)["data"]["project"]["worktree_branch"].is_null());
    let detached_id = json_output(&detached)["data"]["project"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let removed_detached = harkness(
        &data_dir,
        &["--json", "worktree", "remove", "--project", &detached_id],
    );
    assert_success(&removed_detached);

    let repair_candidate = harkness(
        &data_dir,
        &[
            "--json",
            "worktree",
            "add",
            "--project",
            &parent_id,
            "--branch",
            "agent/reconciliation-report",
        ],
    );
    assert_success(&repair_candidate);
    let repair_body = json_output(&repair_candidate);
    let repair_id = repair_body["data"]["project"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let repair_source = PathBuf::from(repair_body["data"]["project"]["root"].as_str().unwrap());
    let repair_destination = moved_parent.join("reconciled-checkout");
    run_git(
        &parent_root,
        &[
            "worktree",
            "move",
            "--",
            repair_source.to_str().unwrap(),
            repair_destination.to_str().unwrap(),
        ],
    );
    let repaired = harkness(
        &data_dir,
        &["--json", "worktree", "prune", "--project", &parent_id],
    );
    assert_success(&repaired);
    let repaired_body = json_output(&repaired);
    assert_eq!(repaired_body["data"]["repaired"][0]["id"], repair_id);
    assert_eq!(
        PathBuf::from(
            repaired_body["data"]["repaired"][0]["root"]
                .as_str()
                .unwrap()
        )
        .canonicalize()
        .unwrap(),
        repair_destination.canonicalize().unwrap()
    );
    assert!(
        repaired_body["data"]["removed"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        repaired_body["data"]["skipped"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let removed_repaired = harkness(
        &data_dir,
        &["--json", "worktree", "remove", "--project", &repair_id],
    );
    assert_success(&removed_repaired);

    let pruned = harkness(
        &data_dir,
        &["--json", "worktree", "prune", "--project", &parent_id],
    );
    assert_success(&pruned);
    assert!(
        json_output(&pruned)["data"]["removed"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        json_output(&pruned)["data"]["repaired"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        json_output(&pruned)["data"]["skipped"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn structured_diff_reports_both_sides_binary_renames_and_lossless_lines() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let root = fixture.path().join("diff-project");
    initialize_repository(&root);
    let repository = Repository::open(&root).unwrap();
    fs::write(root.join("staged.txt"), b"staged before\n").unwrap();
    fs::write(root.join("unstaged.txt"), b"unstaged before\n").unwrap();
    fs::write(root.join("binary.bin"), b"before\0bytes").unwrap();
    fs::write(root.join("old-name.txt"), b"rename me\n").unwrap();
    fs::write(root.join("encoded.txt"), b"before\xff\n").unwrap();
    commit_all(&repository, "prepare diff fixture");
    let project = ProjectService::load_from_data_dir(&data_dir)
        .unwrap()
        .import_local(&root)
        .unwrap();

    fs::write(root.join("staged.txt"), b"staged after\n").unwrap();
    run_git(&root, &["add", "--", "staged.txt"]);
    fs::write(root.join("unstaged.txt"), b"unstaged after\n").unwrap();
    fs::write(root.join("binary.bin"), b"after\0bytes").unwrap();
    fs::rename(root.join("old-name.txt"), root.join("new-name.txt")).unwrap();
    run_git(&root, &["add", "--", "old-name.txt", "new-name.txt"]);
    fs::write(root.join("encoded.txt"), b"after\xfe\n").unwrap();

    let output = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "diff",
            "--project",
            &project.id.to_string(),
        ],
    );

    assert_success(&output);
    assert!(output.stderr.is_empty());
    let body = json_output(&output);
    assert_envelope(&body, "success", true);
    let files = body["data"]["files"].as_array().unwrap();
    assert!(files.iter().any(|file| {
        file["target"] == "staged" && file["new_path"] == "staged.txt" && file["binary"] == false
    }));
    assert!(files.iter().any(|file| {
        file["target"] == "unstaged"
            && file["new_path"] == "unstaged.txt"
            && file["binary"] == false
    }));
    let binary = files
        .iter()
        .find(|file| file["new_path"] == "binary.bin")
        .expect("binary diff is present");
    assert_eq!(binary["binary"], true);
    assert!(binary["hunks"].as_array().unwrap().is_empty());
    assert!(files.iter().any(|file| {
        file["change"] == "renamed"
            && file["old_path"] == "old-name.txt"
            && file["new_path"] == "new-name.txt"
    }));

    let encoded = files
        .iter()
        .find(|file| file["new_path"] == "encoded.txt")
        .expect("non-UTF-8 diff is present");
    let encoded_lines = encoded["hunks"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|hunk| hunk["lines"].as_array().unwrap())
        .filter(|line| line["content_encoding"] == "base64")
        .map(|line| BASE64.decode(line["content"].as_str().unwrap()).unwrap())
        .collect::<Vec<_>>();
    assert!(encoded_lines.contains(&b"before\xff\n".to_vec()));
    assert!(encoded_lines.contains(&b"after\xfe\n".to_vec()));
}

#[test]
fn diff_hunk_flags_stage_unstage_and_refuse_a_stale_selection() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let root = fixture.path().join("hunk-project");
    initialize_repository(&root);
    let repository = Repository::open(&root).unwrap();
    let original = b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\neleven\ntwelve\nthirteen\nfourteen\nfifteen\n";
    let changed = b"one\nTWO\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\neleven\ntwelve\nthirteen\nFOURTEEN\nfifteen\n";
    fs::write(root.join("tracked.txt"), original).unwrap();
    commit_all(&repository, "prepare hunk fixture");
    let project = ProjectService::load_from_data_dir(&data_dir)
        .unwrap()
        .import_local(&root)
        .unwrap();
    let project_id = project.id.to_string();
    fs::write(root.join("tracked.txt"), changed).unwrap();

    let unstaged = diff_file(&data_dir, &project_id, "--unstaged", "tracked.txt");
    assert_eq!(unstaged["hunks"].as_array().unwrap().len(), 2);
    let stage_arguments = hunk_arguments("stage", &project_id, &unstaged, 0);
    let staged = harkness(&data_dir, &stage_arguments);
    assert_success(&staged);

    let status = harkness(
        &data_dir,
        &["--json", "git", "status", "--project", &project_id],
    );
    let tracked = json_output(&status)["data"]["status"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"] == "tracked.txt")
        .cloned()
        .unwrap();
    assert_eq!(tracked["staged"], "modified");
    assert_eq!(tracked["unstaged"], "modified");

    let staged_file = diff_file(&data_dir, &project_id, "--staged", "tracked.txt");
    let unstage_arguments = hunk_arguments("unstage", &project_id, &staged_file, 0);
    let unstaged_again = harkness(&data_dir, &unstage_arguments);
    assert_success(&unstaged_again);
    let staged_after_unstage = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "diff",
            "--staged",
            "--project",
            &project_id,
        ],
    );
    assert!(
        json_output(&staged_after_unstage)["data"]["files"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    assert_success(&harkness(&data_dir, &stage_arguments));
    fs::write(
        root.join("tracked.txt"),
        b"one\nTWO AGAIN\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\neleven\ntwelve\nthirteen\nFOURTEEN\nfifteen\n",
    )
    .unwrap();
    let stale = harkness(&data_dir, &stage_arguments);
    assert_eq!(stale.status.code(), Some(3));
    let stale_body = json_output(&stale);
    assert_eq!(stale_body["error"]["kind"], "stale_hunk_selection");
    assert_eq!(stale_body["error"]["details"]["path"], "tracked.txt");

    // Unstaging revalidates the same way. The selection is captured, the index
    // is then moved underneath it, and the refusal must arrive before any
    // write rather than after a patch is applied to the wrong content.
    let staged_now = diff_file(&data_dir, &project_id, "--staged", "tracked.txt");
    let unstage_stale = hunk_arguments("unstage", &project_id, &staged_now, 0);
    assert_success(&harkness(
        &data_dir,
        &["--json", "git", "stage", "--all", "--project", &project_id],
    ));
    let refused = harkness(&data_dir, &unstage_stale);
    assert_eq!(refused.status.code(), Some(3));
    assert_eq!(
        json_output(&refused)["error"]["kind"],
        "stale_hunk_selection"
    );
}

#[test]
fn discard_requires_confirmation_and_keeps_tracked_boundaries_explicit() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let root = fixture.path().join("discard-project");
    initialize_repository(&root);
    let repository = Repository::open(&root).unwrap();
    fs::write(root.join("tracked.txt"), b"committed\n").unwrap();
    commit_all(&repository, "prepare discard fixture");
    let project = ProjectService::load_from_data_dir(&data_dir)
        .unwrap()
        .import_local(&root)
        .unwrap();
    let project_id = project.id.to_string();

    fs::write(root.join("tracked.txt"), b"staged\n").unwrap();
    run_git(&root, &["add", "--", "tracked.txt"]);
    fs::write(root.join("tracked.txt"), b"working tree\n").unwrap();

    let confirmation = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "discard",
            "--from",
            "index",
            "--project",
            &project_id,
            "tracked.txt",
        ],
    );
    assert_eq!(confirmation.status.code(), Some(3));
    let body = json_output(&confirmation);
    assert_eq!(body["error"]["kind"], "confirmation_required");
    assert_eq!(body["error"]["details"]["override_flag"], "--yes");
    assert_eq!(
        body["error"]["details"]["discard"]["operation"],
        "restore_tracked"
    );
    assert_eq!(body["error"]["details"]["discard"]["source"], "index");
    assert_eq!(
        body["error"]["details"]["discard"]["recoverability"],
        "git_recorded_baseline"
    );
    assert_eq!(
        fs::read(root.join("tracked.txt")).unwrap(),
        b"working tree\n",
        "declining confirmation must not touch the file"
    );

    let restored = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "discard",
            "--from",
            "index",
            "--yes",
            "--project",
            &project_id,
            "tracked.txt",
        ],
    );
    assert_success(&restored);
    assert_eq!(worktree_text(&root.join("tracked.txt")), "staged\n");
    let mut index = repository.index().unwrap();
    index.read(true).unwrap();
    let entry = index.get_path(Path::new("tracked.txt"), 0).unwrap();
    assert_eq!(
        repository.find_blob(entry.id).unwrap().content(),
        b"staged\n"
    );

    let reset_to_head = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "discard",
            "--from",
            "head",
            "--yes",
            "--project",
            &project_id,
            "tracked.txt",
        ],
    );
    assert_success(&reset_to_head);
    assert_eq!(worktree_text(&root.join("tracked.txt")), "committed\n");
    let mut index = repository.index().unwrap();
    index.read(true).unwrap();
    let entry = index.get_path(Path::new("tracked.txt"), 0).unwrap();
    assert_eq!(
        repository.find_blob(entry.id).unwrap().content(),
        b"committed\n"
    );
}

#[test]
fn discard_separates_untracked_deletion_and_can_reverse_one_hunk() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let root = fixture.path().join("granular-discard-project");
    initialize_repository(&root);
    let repository = Repository::open(&root).unwrap();
    let original = (1..=30)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    fs::write(root.join("tracked.txt"), original.as_bytes()).unwrap();
    commit_all(&repository, "prepare granular discard fixture");
    let project = ProjectService::load_from_data_dir(&data_dir)
        .unwrap()
        .import_local(&root)
        .unwrap();
    let project_id = project.id.to_string();
    let changed = original
        .replace("line 2\n", "FIRST\n")
        .replace("line 28\n", "SECOND\n");
    fs::write(root.join("tracked.txt"), changed.as_bytes()).unwrap();
    fs::write(root.join("untracked.txt"), b"unrecoverable\n").unwrap();

    let refused = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "discard",
            "--from",
            "index",
            "--yes",
            "--project",
            &project_id,
            "untracked.txt",
        ],
    );
    assert_eq!(refused.status.code(), Some(3));
    assert_eq!(
        json_output(&refused)["error"]["kind"],
        "untracked_discard_requires_delete"
    );
    assert!(root.join("untracked.txt").exists());

    let delete_confirmation = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "discard",
            "--delete-untracked",
            "--project",
            &project_id,
            "untracked.txt",
        ],
    );
    assert_eq!(delete_confirmation.status.code(), Some(3));
    let body = json_output(&delete_confirmation);
    assert_eq!(body["error"]["kind"], "confirmation_required");
    assert_eq!(
        body["error"]["details"]["discard"]["recoverability"],
        "unrecoverable"
    );
    assert!(root.join("untracked.txt").exists());

    let deleted = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "discard",
            "--delete-untracked",
            "--yes",
            "--project",
            &project_id,
            "untracked.txt",
        ],
    );
    assert_success(&deleted);
    assert!(!root.join("untracked.txt").exists());

    let file = diff_file(&data_dir, &project_id, "--unstaged", "tracked.txt");
    assert_eq!(file["hunks"].as_array().unwrap().len(), 2);
    let mut arguments = hunk_arguments("discard", &project_id, &file, 0);
    arguments.extend(["--from".to_owned(), "index".to_owned(), "--yes".to_owned()]);
    assert_success(&harkness(&data_dir, &arguments));
    let remaining = diff_file(&data_dir, &project_id, "--unstaged", "tracked.txt");
    assert_eq!(remaining["hunks"].as_array().unwrap().len(), 1);
    assert!(
        fs::read_to_string(root.join("tracked.txt"))
            .unwrap()
            .contains("SECOND")
    );
}

#[test]
fn selector_help_is_exposed_only_where_it_is_accepted() {
    let fixture = TempDir::new().unwrap();
    let list_help = harkness(fixture.path(), &["project", "list", "--help"]);
    assert!(list_help.status.success());
    assert!(
        !String::from_utf8(list_help.stdout)
            .unwrap()
            .contains("--project")
    );

    let show_help = harkness(fixture.path(), &["project", "show", "--help"]);
    assert!(show_help.status.success());
    assert!(
        String::from_utf8(show_help.stdout)
            .unwrap()
            .contains("--project")
    );

    let root_selector = harkness(
        fixture.path(),
        &["--json", "--project", "anything", "project", "list"],
    );
    assert_eq!(root_selector.status.code(), Some(2));
    assert_eq!(json_output(&root_selector)["error"]["kind"], "usage_error");
}

/// Help is checked for the flags it must offer, not for the prose around them.
/// Both mutating commands flatten the same hunk arguments, so both are checked.
#[test]
fn new_command_help_offers_every_diff_and_hunk_flag() {
    let fixture = TempDir::new().unwrap();
    let log_help =
        String::from_utf8(harkness(fixture.path(), &["git", "log", "--help"]).stdout).unwrap();
    for text in ["--limit", "--cursor", "OLD..NEW", "BASE...BRANCH"] {
        assert!(log_help.contains(text), "git log --help lacks {text}");
    }

    let diff_help =
        String::from_utf8(harkness(fixture.path(), &["git", "diff", "--help"]).stdout).unwrap();
    for flag in [
        "--staged",
        "--unstaged",
        "--commit",
        "--parent",
        "--revisions",
        "--worktree",
        "--branch",
        "--context-lines",
        "--expand-context",
        "--full-file-context",
        "--context-from",
        "--intra-line",
        "--max-file-size",
        "--max-total-bytes",
        "--max-files",
    ] {
        assert!(diff_help.contains(flag), "git diff --help lacks {flag}");
    }

    for command in ["stage", "unstage", "discard"] {
        let help = String::from_utf8(harkness(fixture.path(), &["git", command, "--help"]).stdout)
            .unwrap();
        for flag in [
            "--hunk",
            "--hunk-selection",
            "--old-path",
            "--old-path-base64",
            "--new-path-base64",
            "--old-blob-id",
            "--new-lines",
        ] {
            assert!(help.contains(flag), "git {command} --help lacks {flag}");
        }
    }
    let discard_help =
        String::from_utf8(harkness(fixture.path(), &["git", "discard", "--help"]).stdout).unwrap();
    for flag in ["--from", "--delete-untracked", "--yes"] {
        assert!(
            discard_help.contains(flag),
            "git discard --help lacks {flag}"
        );
    }
}

/// Every flag combination the argument grammar forbids has to be a usage error
/// rather than a surprising success, and every one of them exits 2.
#[test]
fn rejected_flag_combinations_are_usage_errors() {
    let fixture = TempDir::new().unwrap();
    let cases: [&[&str]; 15] = [
        &["--json", "git", "diff", "--staged", "--unstaged"],
        &["--json", "git", "diff", "--staged", "--commit", "HEAD"],
        &["--json", "git", "diff", "--parent", "HEAD"],
        &[
            "--json",
            "git",
            "diff",
            "--expand-context",
            "2",
            "--full-file-context",
        ],
        &[
            "--json",
            "git",
            "diff",
            "--staged",
            "--expand-context",
            "2",
            "--context-from",
            "-",
        ],
        &[
            "--json",
            "git",
            "diff",
            "--context-lines",
            "4",
            "--full-file-context",
            "--context-from",
            "-",
        ],
        &[
            "--json",
            "git",
            "diff",
            "--max-files",
            "1",
            "--full-file-context",
            "--context-from",
            "-",
        ],
        &["--json", "git", "diff", "--context-lines", "100001"],
        &["--json", "git", "diff", "--revisions", "main...feature"],
        &["--json", "git", "log", "--limit", "0"],
        &["--json", "git", "log", "main....feature"],
        &["--json", "git", "stage", "--hunk"],
        &["--json", "git", "stage", "--hunk", "--all"],
        &["--json", "git", "stage", "--hunk", "some-path.txt"],
        &["--json", "git", "unstage", "--hunk-selection", "-", "path"],
    ];
    for arguments in cases {
        let output = harkness(fixture.path(), arguments);
        assert_eq!(output.status.code(), Some(2), "for {arguments:?}");
        assert_eq!(
            json_output(&output)["error"]["kind"],
            "usage_error",
            "for {arguments:?}"
        );
    }

    // The first line of a clap diagnostic names no argument, so the list that
    // follows it has to survive as data or the message is unactionable.
    let missing = harkness(fixture.path(), &["--json", "git", "stage", "--hunk"]);
    let listed = json_output(&missing)["error"]["details"]["missing"]
        .as_array()
        .expect("the missing arguments are reported as data")
        .iter()
        .map(|value| value.as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert!(
        listed.iter().any(|line| line.contains("--old-blob-id")),
        "missing list is unhelpful: {listed:?}"
    );
}

#[test]
fn git_log_pages_statelessly_and_preserves_commit_bytes() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let root = fixture.path().join("history-project");
    initialize_repository(&root);
    let repository = Repository::open(&root).unwrap();
    fs::write(root.join("tracked.txt"), b"second\n").unwrap();
    commit_all(&repository, "second");
    fs::write(root.join("tracked.txt"), b"third\n").unwrap();
    commit_all(&repository, "third");
    let raw_message = b"raw summary \xff\nraw body \xfe\n";
    let raw_id = raw_commit(&repository, raw_message);
    let project = ProjectService::load_from_data_dir(&data_dir)
        .unwrap()
        .import_local(&root)
        .unwrap();
    let project_id = project.id.to_string();

    let full = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "log",
            "HEAD",
            "--limit",
            "10",
            "--project",
            &project_id,
        ],
    );
    assert_success(&full);
    let full_body = json_output(&full);
    assert_envelope(&full_body, "success", true);
    assert_eq!(full_body["data"]["kind"], "git_log");
    assert_eq!(full_body["data"]["range"]["kind"], "revision");
    assert_eq!(full_body["data"]["range"]["revision"], "HEAD");
    assert_eq!(full_body["data"]["limit"], 10);
    assert!(full_body["data"]["next_cursor"].is_null());
    let full_ids = full_body["data"]["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|commit| commit["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    let git_order = Command::new("git")
        .args(["rev-list", "HEAD"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(git_order.status.success());
    assert_eq!(
        full_ids,
        String::from_utf8(git_order.stdout)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    );

    let first = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "log",
            "HEAD",
            "--limit",
            "2",
            "--project",
            &project_id,
        ],
    );
    assert_success(&first);
    let first_body = json_output(&first);
    let cursor = first_body["data"]["next_cursor"]
        .as_str()
        .expect("the first page has a continuation")
        .to_owned();
    let raw = first_body["data"]["commits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|commit| commit["id"] == raw_id.to_string())
        .expect("the raw commit is the history tip");
    assert_eq!(raw["kind"], "commit");
    assert_eq!(raw["summary_encoding"], "base64");
    assert_eq!(raw["message_encoding"], "base64");
    assert_eq!(
        BASE64.decode(raw["message"].as_str().unwrap()).unwrap(),
        raw_message
    );
    assert_eq!(raw["author"]["name_encoding"], "base64");
    assert_eq!(
        BASE64
            .decode(raw["author"]["name"].as_str().unwrap())
            .unwrap(),
        b"Auth\xffor"
    );
    assert_eq!(raw["author"]["time"]["offset_minutes"], -90);
    assert_eq!(raw["author"]["time"]["sign"], "-");

    // A new tip must not move a page addressed by the cursor.
    commit_all(&repository, "new tip after first page");
    let second = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "log",
            "HEAD",
            "--limit",
            "2",
            "--cursor",
            &cursor,
            "--project",
            &project_id,
        ],
    );
    assert_success(&second);
    let second_body = json_output(&second);
    let joined = first_body["data"]["commits"]
        .as_array()
        .unwrap()
        .iter()
        .chain(second_body["data"]["commits"].as_array().unwrap())
        .map(|commit| commit["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(joined, full_ids);
    assert!(second_body["data"]["next_cursor"].is_null());
}

#[test]
fn log_ranges_and_every_revision_diff_target_use_the_shared_git_model() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let root = fixture.path().join("revision-project");
    initialize_repository(&root);
    let repository = Repository::open(&root).unwrap();
    let common = repository.head().unwrap().target().unwrap().to_string();
    run_git(&root, &["branch", "feature"]);

    fs::write(root.join("main-only.txt"), b"main\n").unwrap();
    commit_all(&repository, "main advanced");
    run_git(&root, &["checkout", "feature"]);
    fs::write(root.join("feature-only.txt"), b"feature\n").unwrap();
    commit_all(&repository, "feature advanced");
    let feature = repository.head().unwrap().target().unwrap().to_string();

    let project = ProjectService::load_from_data_dir(&data_dir)
        .unwrap()
        .import_local(&root)
        .unwrap();
    let project_id = project.id.to_string();

    for range in ["main..feature", "main...feature"] {
        let output = harkness(
            &data_dir,
            &["--json", "git", "log", range, "--project", &project_id],
        );
        assert_success(&output);
        let commits = json_output(&output)["data"]["commits"]
            .as_array()
            .cloned()
            .unwrap();
        assert_eq!(commits.len(), 1, "wrong log range for {range}");
        assert_eq!(commits[0]["id"], feature);
    }

    let diff = |target: &[&str]| {
        let mut arguments = vec!["--json", "git", "diff"];
        arguments.extend_from_slice(target);
        arguments.extend(["--project", &project_id]);
        let output = harkness(&data_dir, &arguments);
        assert_success(&output);
        let body = json_output(&output);
        assert_envelope(&body, "success", true);
        assert_eq!(body["data"]["kind"], "git_diff");
        assert_eq!(body["data"]["targets"].as_array().unwrap().len(), 1);
        body["data"]["files"].as_array().cloned().unwrap()
    };

    let commit = diff(&["--commit", "feature"]);
    assert_eq!(commit.len(), 1);
    assert_eq!(commit[0]["target"], "commit");
    assert_eq!(commit[0]["target_details"]["revision"], "feature");
    assert_eq!(commit[0]["new_path"], "feature-only.txt");
    let git_commit = GitService::new(&root, &data_dir)
        .diff(
            DiffTarget::Commit {
                revision: "feature".to_owned(),
                parent: None,
            },
            &DiffOptions::default(),
        )
        .unwrap();
    assert_eq!(git_commit.len(), commit.len());
    assert_eq!(commit[0]["old_blob_id"], git_commit[0].old_blob_id);
    assert_eq!(commit[0]["new_blob_id"], git_commit[0].new_blob_id);
    assert_eq!(
        commit[0]["hunks"].as_array().unwrap().len(),
        git_commit[0].hunks.len()
    );
    assert_eq!(
        commit[0]["hunks"][0]["new_start"],
        git_commit[0].hunks[0].new_start
    );

    let pair = diff(&["--revisions", &format!("{common}..main")]);
    assert_eq!(pair.len(), 1);
    assert_eq!(pair[0]["target"], "revisions");
    assert_eq!(pair[0]["new_path"], "main-only.txt");

    let branch = diff(&["--branch", "main...feature"]);
    assert_eq!(branch.len(), 1);
    assert_eq!(branch[0]["target"], "branch_against_base");
    assert_eq!(branch[0]["new_path"], "feature-only.txt");
    assert!(
        branch
            .iter()
            .all(|file| file["new_path"] != "main-only.txt"),
        "base-only changes leaked into the branch review"
    );

    fs::write(root.join("worktree-only.txt"), b"working\n").unwrap();
    let worktree = diff(&["--worktree", "feature"]);
    assert_eq!(worktree.len(), 1);
    assert_eq!(worktree[0]["target"], "revision_against_worktree");
    assert_eq!(worktree[0]["new_path"], "worktree-only.txt");

    let bounded = diff(&["--commit", "feature", "--max-file-size", "1"]);
    assert_eq!(bounded[0]["omission"]["kind"], "file_too_large");
}

#[test]
fn diff_context_and_intra_line_metadata_are_opt_in_and_bounded() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let root = fixture.path().join("review-project");
    initialize_repository(&root);
    let repository = Repository::open(&root).unwrap();
    let original = (1..=25)
        .map(|line| format!("line {line} alpha\n"))
        .collect::<String>();
    fs::write(root.join("review.txt"), &original).unwrap();
    let long_old = format!("{} old\n", "a".repeat(5_000));
    fs::write(root.join("long.txt"), &long_old).unwrap();
    fs::write(root.join("bytes.txt"), b"old \xff\n").unwrap();
    commit_all(&repository, "add review fixtures");
    fs::write(
        root.join("review.txt"),
        original.replace("line 13 alpha", "line 13 beta"),
    )
    .unwrap();
    fs::write(
        root.join("long.txt"),
        format!("{} new\n", "a".repeat(5_000)),
    )
    .unwrap();
    fs::write(root.join("bytes.txt"), b"new \xfe\n").unwrap();
    let project = ProjectService::load_from_data_dir(&data_dir)
        .unwrap()
        .import_local(&root)
        .unwrap();
    let project_id = project.id.to_string();

    let review = |extra: &[&str], path: &str| {
        let mut arguments = vec!["--json", "git", "diff", "--unstaged"];
        arguments.extend_from_slice(extra);
        arguments.extend(["--project", &project_id, "--", path]);
        let output = harkness(&data_dir, &arguments);
        assert_success(&output);
        json_output(&output)["data"]["files"][0].clone()
    };

    let plain = review(&[], "review.txt");
    let plain_hunk = &plain["hunks"][0];
    assert!(plain_hunk.get("intra_line_degradation").is_none());
    assert!(
        plain_hunk["lines"]
            .as_array()
            .unwrap()
            .iter()
            .all(|line| line.get("paired_line_index").is_none())
    );

    let ranged = review(&["--intra-line", "--expand-context", "2"], "review.txt");
    let hunk = &ranged["hunks"][0];
    assert!(hunk["intra_line_degradation"].is_null());
    assert_eq!(hunk["context"]["kind"], "hunk_context");
    assert_eq!(hunk["context"]["old"]["range"]["kind"], "hunk");
    assert_eq!(hunk["context"]["old"]["range"]["lines_before"], 2);
    let changed = hunk["lines"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|line| matches!(line["kind"].as_str(), Some("deletion" | "addition")))
        .collect::<Vec<_>>();
    assert_eq!(changed.len(), 2);
    for line in changed {
        assert!(line["paired_line_index"].is_number());
        assert!(
            !line["intra_line_ranges"].as_array().unwrap().is_empty(),
            "the word-level byte range is missing: {line:#?}"
        );
    }

    let full = review(&["--full-file-context"], "review.txt");
    assert_eq!(full["context"]["kind"], "full_file_context");
    assert_eq!(full["context"]["new"]["range"]["kind"], "full_file");
    assert_eq!(full["context"]["new"]["total_lines"], 25);
    assert_eq!(
        full["context"]["new"]["lines"].as_array().unwrap().len(),
        25
    );

    let bytes = review(&["--full-file-context"], "bytes.txt");
    let encoded = &bytes["context"]["new"]["lines"][0];
    assert_eq!(encoded["content_encoding"], "base64");
    assert_eq!(
        BASE64.decode(encoded["content"].as_str().unwrap()).unwrap(),
        b"new \xfe\n"
    );

    let degraded = review(&["--intra-line"], "long.txt");
    assert_eq!(
        degraded["hunks"][0]["intra_line_degradation"]["kind"],
        "line_too_long"
    );
}

#[test]
fn context_expansion_handles_gitlinks_and_rejects_unknown_replay_targets() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let root = fixture.path().join("gitlink-context-project");
    initialize_repository(&root);
    let repository = Repository::open(&root).unwrap();
    let gitlink = repository.head().unwrap().target().unwrap().to_string();
    let cache_entry = format!("160000,{gitlink},README.md");
    run_git(
        &root,
        &["update-index", "--cacheinfo", cache_entry.as_str()],
    );
    let project = ProjectService::load_from_data_dir(&data_dir)
        .unwrap()
        .import_local(&root)
        .unwrap();
    let project_id = project.id.to_string();

    let full = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "diff",
            "--staged",
            "--full-file-context",
            "--project",
            &project_id,
        ],
    );
    assert_success(&full);
    let full_file = &json_output(&full)["data"]["files"][0];
    assert_eq!(full_file["change"], "type_changed");
    assert_eq!(
        full_file["context"]["old"]["lines"][0]["content"],
        "fixture\n"
    );
    assert!(full_file["context"]["new"].is_null());

    commit_index(&repository, "record gitlink");
    let next_gitlink = repository.head().unwrap().target().unwrap().to_string();
    let next_cache_entry = format!("160000,{next_gitlink},README.md");
    run_git(
        &root,
        &["update-index", "--cacheinfo", next_cache_entry.as_str()],
    );

    let plain = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "diff",
            "--staged",
            "--project",
            &project_id,
        ],
    );
    assert_success(&plain);

    let expanded = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "diff",
            "--staged",
            "--expand-context",
            "2",
            "--project",
            &project_id,
        ],
    );
    assert_success(&expanded);
    let expanded_hunk = &json_output(&expanded)["data"]["files"][0]["hunks"][0];
    assert!(expanded_hunk["context"]["old"].is_null());
    assert!(expanded_hunk["context"]["new"].is_null());

    let replayed = harkness_with_stdin(
        &data_dir,
        &[
            "--json",
            "git",
            "diff",
            "--expand-context",
            "2",
            "--context-from",
            "-",
            "--project",
            &project_id,
        ],
        &String::from_utf8(plain.stdout.clone()).unwrap(),
    );
    assert_success(&replayed);
    let replayed_hunk = &json_output(&replayed)["data"]["files"][0]["hunks"][0];
    assert!(replayed_hunk["context"]["old"].is_null());
    assert!(replayed_hunk["context"]["new"].is_null());

    let mut unsupported = json_output(&plain);
    unsupported["data"]["files"][0]["target"] = json!("future_target");
    let refused = harkness_with_stdin(
        &data_dir,
        &[
            "--json",
            "git",
            "diff",
            "--expand-context",
            "2",
            "--context-from",
            "-",
            "--project",
            &project_id,
        ],
        &unsupported.to_string(),
    );
    assert_eq!(refused.status.code(), Some(2));
    assert_eq!(json_output(&refused)["error"]["kind"], "usage_error");
}

#[test]
fn context_from_a_prior_diff_keeps_blob_content_stable_and_refuses_stale_worktree_bytes() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let root = fixture.path().join("stable-context-project");
    initialize_repository(&root);
    let repository = Repository::open(&root).unwrap();
    fs::write(root.join("tracked.txt"), b"one\ntwo\nbase\nfour\nfive\n").unwrap();
    commit_all(&repository, "add tracked context fixture");
    let project = ProjectService::load_from_data_dir(&data_dir)
        .unwrap()
        .import_local(&root)
        .unwrap();
    let project_id = project.id.to_string();

    fs::write(root.join("tracked.txt"), b"one\ntwo\nstaged\nfour\nfive\n").unwrap();
    run_git(&root, &["add", "tracked.txt"]);
    let staged = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "diff",
            "--staged",
            "--project",
            &project_id,
        ],
    );
    assert_success(&staged);
    fs::write(
        root.join("tracked.txt"),
        b"one\ntwo\nlater worktree\nfour\nfive\n",
    )
    .unwrap();

    let expanded_staged = harkness_with_stdin(
        &data_dir,
        &[
            "--json",
            "git",
            "diff",
            "--expand-context",
            "2",
            "--context-from",
            "-",
            "--project",
            &project_id,
        ],
        &String::from_utf8(staged.stdout).unwrap(),
    );
    assert_success(&expanded_staged);
    let staged_body = json_output(&expanded_staged);
    assert_envelope(&staged_body, "success", true);
    assert_eq!(staged_body["data"]["kind"], "git_diff_context");
    let staged_lines = staged_body["data"]["files"][0]["hunks"][0]["context"]["new"]["lines"]
        .as_array()
        .unwrap();
    assert!(
        staged_lines
            .iter()
            .any(|line| line["content"] == "staged\n")
    );
    assert!(
        staged_lines
            .iter()
            .all(|line| line["content"] != "later worktree\n")
    );

    let unstaged = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "diff",
            "--unstaged",
            "--project",
            &project_id,
        ],
    );
    assert_success(&unstaged);
    fs::write(
        root.join("tracked.txt"),
        b"one\ntwo\nchanged again\nfour\nfive\n",
    )
    .unwrap();
    let stale = harkness_with_stdin(
        &data_dir,
        &[
            "--json",
            "git",
            "diff",
            "--expand-context",
            "2",
            "--context-from",
            "-",
            "--project",
            &project_id,
        ],
        &String::from_utf8(unstaged.stdout).unwrap(),
    );
    assert_eq!(stale.status.code(), Some(3));
    assert_eq!(json_output(&stale)["error"]["kind"], "stale_hunk_selection");
}

#[test]
fn missing_and_ambiguous_revisions_are_distinct_not_found_errors() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let root = fixture.path().join("revision-errors");
    initialize_repository(&root);
    let repository = Repository::open(&root).unwrap();
    let ambiguous = ambiguous_object_prefix(&repository);
    let project = ProjectService::load_from_data_dir(&data_dir)
        .unwrap()
        .import_local(&root)
        .unwrap();
    let project_id = project.id.to_string();

    for (revision, kind) in [
        ("definitely-missing".to_owned(), "revision_not_found"),
        (ambiguous, "ambiguous_revision"),
    ] {
        let output = harkness(
            &data_dir,
            &["--json", "git", "log", &revision, "--project", &project_id],
        );
        assert_eq!(output.status.code(), Some(4), "for {revision}");
        let body = json_output(&output);
        assert_envelope(&body, "error", false);
        assert_eq!(body["error"]["kind"], kind);
        assert_eq!(body["error"]["details"]["revision"], revision);
    }
}

/// An unresolved merge is where an agent most needs a diff, and the index has
/// no resolved blob for the conflicted path. The command must still succeed,
/// name the conflict, and return every other file's content.
#[test]
fn a_merge_conflict_is_named_without_failing_the_whole_diff() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let root = fixture.path().join("conflict-project");
    initialize_repository(&root);
    let repository = Repository::open(&root).unwrap();
    fs::write(root.join("conflict.txt"), b"base\n").unwrap();
    fs::write(root.join("clean.txt"), b"base\n").unwrap();
    commit_all(&repository, "base");
    run_git(&root, &["checkout", "-b", "side"]);
    fs::write(root.join("conflict.txt"), b"side\n").unwrap();
    commit_all(&repository, "side");
    run_git(&root, &["checkout", "main"]);
    fs::write(root.join("conflict.txt"), b"main\n").unwrap();
    commit_all(&repository, "main");
    Command::new("git")
        .args(["merge", "side"])
        .current_dir(&root)
        .output()
        .expect("git merge should run");
    fs::write(root.join("clean.txt"), b"edited\n").unwrap();
    let project = ProjectService::load_from_data_dir(&data_dir)
        .unwrap()
        .import_local(&root)
        .unwrap();
    let project_id = project.id.to_string();

    let output = harkness(
        &data_dir,
        &["--json", "git", "diff", "--project", &project_id],
    );

    assert_success(&output);
    let files = json_output(&output)["data"]["files"]
        .as_array()
        .cloned()
        .unwrap();
    let unmerged = files
        .iter()
        .filter(|file| file["change"] == "unmerged")
        .collect::<Vec<_>>();
    assert!(!unmerged.is_empty(), "the conflict is missing: {files:#?}");
    for file in unmerged {
        assert_eq!(file["omission"]["kind"], "unmerged");
        assert!(file["hunks"].as_array().unwrap().is_empty());
    }
    let clean = files
        .iter()
        .find(|file| file["new_path"] == "clean.txt")
        .expect("an unrelated file survived the conflict");
    assert!(clean["omission"].is_null());
    assert!(!clean["hunks"].as_array().unwrap().is_empty());
}

/// Each diff bound has to reach the Git service and be reported when it bites, or the
/// flags are decoration and a wiring mistake ships green.
#[test]
fn diff_bounds_narrow_the_response_and_name_what_they_withheld() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let root = fixture.path().join("bounded-project");
    initialize_repository(&root);
    let repository = Repository::open(&root).unwrap();
    let body = (1..=30)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    for name in ["first.txt", "second.txt", "third.txt"] {
        fs::write(root.join(name), body.as_bytes()).unwrap();
    }
    commit_all(&repository, "add bounded files");
    for name in ["first.txt", "second.txt", "third.txt"] {
        fs::write(root.join(name), body.replace("line 15\n", "CHANGED\n")).unwrap();
    }
    let project = ProjectService::load_from_data_dir(&data_dir)
        .unwrap()
        .import_local(&root)
        .unwrap();
    let project_id = project.id.to_string();
    let files = |arguments: &[&str]| {
        let mut all = vec!["--json", "git", "diff", "--unstaged"];
        all.extend_from_slice(arguments);
        all.extend(["--project", &project_id]);
        let output = harkness(&data_dir, &all);
        assert_success(&output);
        json_output(&output)["data"]["files"]
            .as_array()
            .cloned()
            .unwrap()
    };

    assert_eq!(files(&[]).len(), 3);
    let narrowed = files(&["second.txt"]);
    assert_eq!(narrowed.len(), 1, "the path argument did not narrow");
    assert_eq!(narrowed[0]["new_path"], "second.txt");

    let tight = &files(&["--context-lines", "1", "second.txt"])[0]["hunks"][0];
    let wide = &files(&["--context-lines", "5", "second.txt"])[0]["hunks"][0];
    assert_eq!(tight["old_lines"], 3);
    assert_eq!(wide["old_lines"], 11);

    let sized = files(&["--max-file-size", "10"]);
    assert_eq!(sized.len(), 3, "a bounded file must still be listed");
    for file in &sized {
        assert_eq!(file["omission"]["kind"], "file_too_large");
        assert_eq!(file["omission"]["limit"], 10);
        assert!(file["hunks"].as_array().unwrap().is_empty());
    }

    let counted = files(&["--max-files", "1"]);
    assert_eq!(counted.len(), 3);
    assert_eq!(
        counted
            .iter()
            .filter(|file| file["omission"]["kind"] == "file_budget_exhausted")
            .count(),
        2
    );

    let budgeted = files(&["--max-total-bytes", "60"]);
    assert_eq!(budgeted.len(), 3);
    assert!(
        budgeted
            .iter()
            .any(|file| file["omission"]["kind"] == "content_budget_exhausted"),
        "the content budget never reported itself: {budgeted:#?}"
    );
}

/// Staging one hunk rewrites the index and shifts every other selection taken
/// from the same diff, so a batch has to be expressible in one atomic call.
/// The document is `git diff` output with the unwanted hunks removed.
#[test]
fn a_selection_document_stages_several_hunks_atomically() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let root = fixture.path().join("batch-project");
    initialize_repository(&root);
    let repository = Repository::open(&root).unwrap();
    let original = (1..=30)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    fs::write(root.join("tracked.txt"), original.as_bytes()).unwrap();
    commit_all(&repository, "add tracked");
    let project = ProjectService::load_from_data_dir(&data_dir)
        .unwrap()
        .import_local(&root)
        .unwrap();
    let project_id = project.id.to_string();
    let changed = original
        .replace("line 2\n", "FIRST\n")
        .replace("line 15\n", "SECOND\n")
        .replace("line 28\n", "THIRD\n");
    fs::write(root.join("tracked.txt"), changed.as_bytes()).unwrap();

    let mut file = diff_file(&data_dir, &project_id, "--unstaged", "tracked.txt");
    assert_eq!(file["hunks"].as_array().unwrap().len(), 3);
    let hunks = file["hunks"].as_array().unwrap().clone();
    file["hunks"] = json!([hunks[0], hunks[2]]);
    let document = json!({ "files": [file] }).to_string();
    let document_path = fixture.path().join("selection.json");
    fs::write(&document_path, &document).unwrap();

    let staged = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "stage",
            "--hunk-selection",
            document_path.to_str().unwrap(),
            "--project",
            &project_id,
        ],
    );

    assert_success(&staged);
    assert_eq!(json_output(&staged)["data"]["hunks"], 2);
    let staged_after = diff_file(&data_dir, &project_id, "--staged", "tracked.txt");
    assert_eq!(staged_after["hunks"].as_array().unwrap().len(), 2);
    let unstaged_after = diff_file(&data_dir, &project_id, "--unstaged", "tracked.txt");
    assert_eq!(
        unstaged_after["hunks"].as_array().unwrap().len(),
        1,
        "the unselected hunk must remain unstaged"
    );

    // The whole staged response is fed back verbatim through standard input.
    let response = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "diff",
            "--staged",
            "--project",
            &project_id,
        ],
    );
    let unstaged_again = harkness_with_stdin(
        &data_dir,
        &[
            "--json",
            "git",
            "unstage",
            "--hunk-selection",
            "-",
            "--project",
            &project_id,
        ],
        &String::from_utf8(response.stdout).unwrap(),
    );

    assert_success(&unstaged_again);
    assert_eq!(json_output(&unstaged_again)["data"]["hunks"], 2);
    let staged_files = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "diff",
            "--staged",
            "--project",
            &project_id,
        ],
    );
    assert!(
        json_output(&staged_files)["data"]["files"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_line_selection_document_stages_and_unstages_only_retained_changed_lines() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let root = fixture.path().join("line-selection-project");
    initialize_repository(&root);
    let repository = Repository::open(&root).unwrap();
    fs::write(root.join("tracked.txt"), b"one\ntwo\nthree\nfour\n").unwrap();
    commit_all(&repository, "prepare line selection");
    let project = ProjectService::load_from_data_dir(&data_dir)
        .unwrap()
        .import_local(&root)
        .unwrap();
    let project_id = project.id.to_string();
    fs::write(
        root.join("tracked.txt"),
        b"one\nselected\ntwo\nthree\nnot selected\nfour\n",
    )
    .unwrap();

    let mut file = diff_file(&data_dir, &project_id, "--unstaged", "tracked.txt");
    assert_eq!(file["hunks"].as_array().unwrap().len(), 1);
    file["hunks"][0]["lines"]
        .as_array_mut()
        .unwrap()
        .retain(|line| line["kind"] != "addition" || line["content"] == "selected\n");
    let document = json!({ "files": [file] }).to_string();
    let staged = harkness_with_stdin(
        &data_dir,
        &[
            "--json",
            "git",
            "stage",
            "--line-selection",
            "-",
            "--project",
            &project_id,
        ],
        &document,
    );

    assert_success(&staged);
    assert_eq!(json_output(&staged)["data"]["lines"], 1);
    assert_eq!(json_output(&staged)["data"]["hunks"], 1);
    let repository = Repository::open(&root).unwrap();
    let index = repository.index().unwrap();
    let entry = index.get_path(Path::new("tracked.txt"), 0).unwrap();
    assert_eq!(
        repository.find_blob(entry.id).unwrap().content(),
        b"one\nselected\ntwo\nthree\nfour\n"
    );
    assert_eq!(
        fs::read(root.join("tracked.txt")).unwrap(),
        b"one\nselected\ntwo\nthree\nnot selected\nfour\n"
    );

    let response = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "diff",
            "--staged",
            "--project",
            &project_id,
        ],
    );
    let unstaged = harkness_with_stdin(
        &data_dir,
        &[
            "--json",
            "git",
            "unstage",
            "--line-selection",
            "-",
            "--project",
            &project_id,
        ],
        &String::from_utf8(response.stdout).unwrap(),
    );
    assert_success(&unstaged);
    assert_eq!(json_output(&unstaged)["data"]["lines"], 1);
    let repository = Repository::open(&root).unwrap();
    let index = repository.index().unwrap();
    let entry = index.get_path(Path::new("tracked.txt"), 0).unwrap();
    assert_eq!(
        repository.find_blob(entry.id).unwrap().content(),
        b"one\ntwo\nthree\nfour\n"
    );
}

/// The two refusals a line-selection document can earn that hunk selection
/// cannot: a changed line the fresh diff does not have, and a selection whose
/// retained side would leave an unterminated line somewhere other than last.
/// Both exit 3 with the index untouched, like every other selection refusal.
#[test]
fn line_selection_documents_are_refused_line_by_line_without_touching_the_index() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let root = fixture.path().join("line-refusal-project");
    initialize_repository(&root);
    let repository = Repository::open(&root).unwrap();
    fs::write(root.join("tracked.txt"), b"one\ntwo\nthree\n").unwrap();
    fs::write(root.join("tail.txt"), b"one\ntwo\nlast").unwrap();
    commit_all(&repository, "prepare line refusals");
    let project = ProjectService::load_from_data_dir(&data_dir)
        .unwrap()
        .import_local(&root)
        .unwrap();
    let project_id = project.id.to_string();
    fs::write(root.join("tracked.txt"), b"one\nTWO\nthree\n").unwrap();
    fs::write(root.join("tail.txt"), b"one\ntwo\nLAST").unwrap();
    let index_before = fs::read(repository.path().join("index")).unwrap();

    let stage = |document: &str| {
        harkness_with_stdin(
            &data_dir,
            &[
                "--json",
                "git",
                "stage",
                "--line-selection",
                "-",
                "--project",
                &project_id,
            ],
            document,
        )
    };

    // A well-formed record whose coordinates name no changed line in the hunk
    // the rest of the record still resolves to.
    let mut file = diff_file(&data_dir, &project_id, "--unstaged", "tracked.txt");
    file["hunks"][0]["lines"]
        .as_array_mut()
        .unwrap()
        .retain(|line| line["kind"] == "addition");
    file["hunks"][0]["lines"][0]["new_line_number"] = json!(4096);
    let missing = stage(&json!({ "files": [file] }).to_string());
    assert_eq!(missing.status.code(), Some(3));
    let body = json_output(&missing);
    assert_eq!(body["error"]["kind"], "line_not_found");
    assert_eq!(body["error"]["details"]["path"], "tracked.txt");
    assert_eq!(body["error"]["details"]["new_line_number"], 4096);
    assert!(body["error"]["details"]["old_line_number"].is_null());

    // Keeping only the addition would put the retained unterminated line ahead
    // of it, which no patch can express.
    let mut tail = diff_file(&data_dir, &project_id, "--unstaged", "tail.txt");
    tail["hunks"][0]["lines"]
        .as_array_mut()
        .unwrap()
        .retain(|line| line["kind"] != "deletion");
    let stranded = stage(&json!({ "files": [tail] }).to_string());
    assert_eq!(stranded.status.code(), Some(3));
    let body = json_output(&stranded);
    assert_eq!(body["error"]["kind"], "unrepresentable_line_selection");
    assert_eq!(body["error"]["details"]["path"], "tail.txt");

    assert_eq!(
        fs::read(repository.path().join("index")).unwrap(),
        index_before,
        "a refused line selection still wrote to the index"
    );

    // The same file stages cleanly once both sides of the change are named.
    let tail = diff_file(&data_dir, &project_id, "--unstaged", "tail.txt");
    assert_success(&stage(&json!({ "files": [tail] }).to_string()));
    let repository = Repository::open(&root).unwrap();
    let index = repository.index().unwrap();
    let entry = index.get_path(Path::new("tail.txt"), 0).unwrap();
    assert_eq!(
        repository.find_blob(entry.id).unwrap().content(),
        b"one\ntwo\nLAST"
    );
}

/// A batch can name the same hunk twice, or two hunks that cover the same
/// lines. The first is deduplicated and must be counted as what landed rather
/// than as what was asked for; the second cannot be expressed as one patch and
/// is refused with the path in hand. Neither is reachable one hunk at a time.
#[test]
fn a_batch_deduplicates_repeats_and_refuses_overlaps() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let root = fixture.path().join("batch-edges");
    initialize_repository(&root);
    let repository = Repository::open(&root).unwrap();
    let original = (1..=30)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    fs::write(root.join("tracked.txt"), original.as_bytes()).unwrap();
    commit_all(&repository, "add tracked");
    let project = ProjectService::load_from_data_dir(&data_dir)
        .unwrap()
        .import_local(&root)
        .unwrap();
    let project_id = project.id.to_string();
    fs::write(
        root.join("tracked.txt"),
        original
            .replace("line 2\n", "FIRST\n")
            .replace("line 15\n", "SECOND\n")
            .as_bytes(),
    )
    .unwrap();
    let apply = |document: &Value| {
        harkness_with_stdin(
            &data_dir,
            &[
                "--json",
                "git",
                "stage",
                "--hunk-selection",
                "-",
                "--project",
                &project_id,
            ],
            &document.to_string(),
        )
    };

    let mut file = diff_file(&data_dir, &project_id, "--unstaged", "tracked.txt");
    let first = file["hunks"][0].clone();
    file["hunks"] = json!([first, first]);

    let repeated = apply(&json!({ "files": [file] }));

    assert_success(&repeated);
    assert_eq!(
        json_output(&repeated)["data"]["hunks"],
        1,
        "a repeated hunk must be counted once, not once per selection"
    );

    // A wider context setting produces a hunk that spans the narrow one.
    let wide = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "diff",
            "--unstaged",
            "--context-lines",
            "8",
            "--project",
            &project_id,
        ],
    );
    let wide = json_output(&wide)["data"]["files"][0].clone();
    let narrow = diff_file(&data_dir, &project_id, "--unstaged", "tracked.txt");
    let selection = |file: &Value, hunk: &Value| {
        json!({
            "old_path": file["old_path"],
            "new_path": file["new_path"],
            "old_blob_id": file["old_blob_id"],
            "new_blob_id": file["new_blob_id"],
            "context_lines": file["context_lines"],
            "old_start": hunk["old_start"],
            "old_lines": hunk["old_lines"],
            "new_start": hunk["new_start"],
            "new_lines": hunk["new_lines"],
        })
    };
    let overlapping = apply(&json!({
        "selections": [
            selection(&narrow, &narrow["hunks"][0]),
            selection(&wide, &wide["hunks"][0]),
        ],
    }));

    assert_eq!(overlapping.status.code(), Some(3));
    let body = json_output(&overlapping);
    assert_eq!(body["error"]["kind"], "overlapping_hunk_selection");
    assert_eq!(body["error"]["details"]["path"], "tracked.txt");
}

/// Piping a whole combined diff into one side's command is the obvious
/// mistake. Revalidation would refuse it as stale, which is true of the blob
/// IDs and misleading as advice, so the wrong side is named as the wrong side.
#[test]
fn a_document_from_the_wrong_side_of_the_index_is_refused_with_advice() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let root = fixture.path().join("wrong-side");
    initialize_repository(&root);
    let repository = Repository::open(&root).unwrap();
    let original = (1..=30)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    fs::write(root.join("tracked.txt"), original.as_bytes()).unwrap();
    commit_all(&repository, "add tracked");
    let project = ProjectService::load_from_data_dir(&data_dir)
        .unwrap()
        .import_local(&root)
        .unwrap();
    let project_id = project.id.to_string();
    fs::write(
        root.join("tracked.txt"),
        original.replace("line 2\n", "FIRST\n").as_bytes(),
    )
    .unwrap();
    assert_success(&harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "stage",
            "tracked.txt",
            "--project",
            &project_id,
        ],
    ));
    fs::write(
        root.join("tracked.txt"),
        original
            .replace("line 2\n", "FIRST\n")
            .replace("line 25\n", "SECOND\n")
            .as_bytes(),
    )
    .unwrap();

    let combined = harkness(
        &data_dir,
        &["--json", "git", "diff", "--project", &project_id],
    );
    assert_eq!(
        json_output(&combined)["data"]["files"]
            .as_array()
            .unwrap()
            .len(),
        2,
        "the fixture needs both a staged and an unstaged record"
    );

    let refused = harkness_with_stdin(
        &data_dir,
        &[
            "--json",
            "git",
            "stage",
            "--hunk-selection",
            "-",
            "--project",
            &project_id,
        ],
        &String::from_utf8(combined.stdout).unwrap(),
    );

    assert_eq!(refused.status.code(), Some(2));
    let body = json_output(&refused);
    assert_eq!(body["error"]["kind"], "usage_error");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("--unstaged"), "{message}");
    assert!(
        !message.contains("stale"),
        "the wrong side must not be reported as staleness: {message}"
    );
}

/// A path the diff can print is a path the mutation must accept. A leading
/// hyphen has to survive ordinary argv, and a name that is not UTF-8 has to be
/// nameable through its exact bytes rather than through a lossy stand-in.
#[test]
fn unusual_paths_survive_the_diff_to_stage_round_trip() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let root = fixture.path().join("unusual-project");
    initialize_repository(&root);
    let repository = Repository::open(&root).unwrap();
    fs::write(root.join("-leading.txt"), b"one\ntwo\n").unwrap();
    #[cfg(target_os = "linux")]
    let lossy_name = {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
        PathBuf::from(OsStr::from_bytes(b"lossy-\xff.txt"))
    };
    #[cfg(target_os = "linux")]
    fs::write(root.join(&lossy_name), b"one\ntwo\n").unwrap();
    commit_all(&repository, "add unusual paths");
    let project = ProjectService::load_from_data_dir(&data_dir)
        .unwrap()
        .import_local(&root)
        .unwrap();
    let project_id = project.id.to_string();
    fs::write(root.join("-leading.txt"), b"ONE\ntwo\n").unwrap();
    #[cfg(target_os = "linux")]
    fs::write(root.join(&lossy_name), b"ONE\ntwo\n").unwrap();

    let hyphenated = diff_file(&data_dir, &project_id, "--unstaged", "-leading.txt");
    assert_eq!(hyphenated["new_path"], "-leading.txt");
    let staged = harkness(
        &data_dir,
        &hunk_arguments("stage", &project_id, &hyphenated, 0),
    );
    assert_success(&staged);

    #[cfg(target_os = "linux")]
    {
        let output = harkness(
            &data_dir,
            &[
                "--json",
                "git",
                "diff",
                "--unstaged",
                "--project",
                &project_id,
            ],
        );
        assert_success(&output);
        let file = json_output(&output)["data"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|file| file["new_path_is_lossy"] == true)
            .cloned()
            .expect("the non-UTF-8 path is listed");
        assert_eq!(
            BASE64
                .decode(file["new_path_base64"].as_str().unwrap())
                .unwrap(),
            b"lossy-\xff.txt".to_vec(),
            "the exact path bytes must reach the wire"
        );

        // Replaying only the lossy spelling names a different file, so it is
        // refused with the field that fixes it rather than reported as stale.
        let mut without_bytes = file.clone();
        without_bytes["old_path_base64"] = Value::Null;
        without_bytes["new_path_base64"] = Value::Null;
        let refused = harkness_with_stdin(
            &data_dir,
            &[
                "--json",
                "git",
                "stage",
                "--hunk-selection",
                "-",
                "--project",
                &project_id,
            ],
            &json!({ "files": [without_bytes] }).to_string(),
        );
        assert_eq!(refused.status.code(), Some(2));
        let message = json_output(&refused)["error"]["message"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            message.contains("path_base64"),
            "the refusal must name the field that fixes it: {message}"
        );

        let accepted = harkness_with_stdin(
            &data_dir,
            &[
                "--json",
                "git",
                "stage",
                "--hunk-selection",
                "-",
                "--project",
                &project_id,
            ],
            &json!({ "files": [file] }).to_string(),
        );
        assert_success(&accepted);
    }
}

/// Every hunk refusal names an alternative in its message; the path that
/// alternative applies to has to arrive as data, not only as prose.
#[test]
fn hunk_refusals_carry_structured_details() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let root = fixture.path().join("refusal-project");
    initialize_repository(&root);
    let repository = Repository::open(&root).unwrap();
    fs::write(root.join("mode.sh"), b"#!/bin/sh\n").unwrap();
    fs::write(root.join("filtered.bin"), b"payload\n").unwrap();
    commit_all(&repository, "add refusal fixtures");
    let project = ProjectService::load_from_data_dir(&data_dir)
        .unwrap()
        .import_local(&root)
        .unwrap();
    let project_id = project.id.to_string();
    // Blob IDs are taken from the live record where one exists, because
    // revalidation compares identities before it classifies the change.
    let coordinates = |path: &str, old_blob: &str, new_blob: &str| {
        vec![
            "--json".to_owned(),
            "git".to_owned(),
            "stage".to_owned(),
            "--hunk".to_owned(),
            "--old-path".to_owned(),
            path.to_owned(),
            "--new-path".to_owned(),
            path.to_owned(),
            "--old-blob-id".to_owned(),
            old_blob.to_owned(),
            "--new-blob-id".to_owned(),
            new_blob.to_owned(),
            "--context-lines".to_owned(),
            "3".to_owned(),
            "--old-start".to_owned(),
            "1".to_owned(),
            "--old-lines".to_owned(),
            "1".to_owned(),
            "--new-start".to_owned(),
            "1".to_owned(),
            "--new-lines".to_owned(),
            "1".to_owned(),
            "--project".to_owned(),
            project_id.clone(),
        ]
    };

    fs::write(root.join(".gitattributes"), b"*.bin filter=lfs\n").unwrap();
    fs::write(root.join("filtered.bin"), b"changed\n").unwrap();
    let zero = "0".repeat(40);
    let filtered = harkness(&data_dir, &coordinates("filtered.bin", &zero, &zero));
    assert_eq!(filtered.status.code(), Some(3));
    let body = json_output(&filtered);
    assert_eq!(body["error"]["kind"], "filtered_hunk_selection");
    assert_eq!(body["error"]["details"]["path"], "filtered.bin");
    assert_eq!(body["error"]["details"]["driver"], "lfs");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(root.join("mode.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        let record = diff_file(&data_dir, &project_id, "--unstaged", "mode.sh");
        assert!(
            record["hunks"].as_array().unwrap().is_empty(),
            "a mode change carries no content to select"
        );
        let metadata_only = harkness(
            &data_dir,
            &coordinates(
                "mode.sh",
                record["old_blob_id"].as_str().unwrap(),
                record["new_blob_id"].as_str().unwrap(),
            ),
        );
        assert_eq!(metadata_only.status.code(), Some(3));
        let body = json_output(&metadata_only);
        assert_eq!(body["error"]["kind"], "metadata_only_hunk_selection");
        assert_eq!(body["error"]["details"]["path"], "mode.sh");
        assert_eq!(body["error"]["details"]["old_mode"], 0o100_644);
        assert_eq!(body["error"]["details"]["new_mode"], 0o100_755);
    }

    // A parent-relative escape is refused on every platform, unlike an
    // absolute Unix path, which on Windows is merely a rooted relative one.
    let outside = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "diff",
            "--project",
            &project_id,
            "--",
            "../outside.txt",
        ],
    );
    assert_eq!(outside.status.code(), Some(3));
    let body = json_output(&outside);
    assert_eq!(body["error"]["kind"], "path_outside_repository");
    assert_eq!(body["error"]["details"]["path"], "../outside.txt");
    assert!(body["error"]["details"]["repository"].is_string());
}

/// A consumer must be able to learn which exit code an error kind reports
/// instead of hardcoding it, or a deliberate reclassification is
/// indistinguishable from an unannounced break.
#[test]
fn the_contract_publishes_an_exit_code_for_every_error_kind() {
    let fixture = TempDir::new().unwrap();
    let output = harkness(fixture.path(), &["--json", "contract"]);

    assert_success(&output);
    let body = json_output(&output);
    let data = &body["data"];
    let map = &data["exit_code_by_kind"];
    for namespace in ["cli", "project", "git", "editor"] {
        let kinds = data["error_kinds"][namespace].as_array().unwrap();
        let mapped = map[namespace].as_object().unwrap();
        assert_eq!(
            kinds.len(),
            mapped.len(),
            "{namespace} kinds and exit codes disagree"
        );
        for kind in kinds {
            let kind = kind.as_str().unwrap();
            assert!(
                mapped.contains_key(kind),
                "{namespace} kind {kind} has no published exit code"
            );
        }
    }
    assert_eq!(map["git"]["worktree_locked"], 3);
    assert_eq!(map["git"]["worktree_already_locked"], 5);
    assert_eq!(map["project"]["worktree_destination_exists"], 5);
    assert_eq!(map["editor"]["editor_path_outside_project"], 3);
    assert_eq!(map["editor"]["editor_file_unavailable"], 4);
    assert_eq!(map["editor"]["editor_launch"], 1);
    assert_eq!(map["cli"]["usage_error"], 2);
}

/// The plain-text path is secondary but still has to answer which files
/// changed and which came back without content.
#[test]
fn human_diff_output_names_every_file_and_its_omission() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let root = fixture.path().join("human-project");
    initialize_repository(&root);
    let repository = Repository::open(&root).unwrap();
    fs::write(root.join("text.txt"), b"one\n").unwrap();
    fs::write(root.join("blob.bin"), b"old\0bytes").unwrap();
    commit_all(&repository, "add human fixtures");
    let project = ProjectService::load_from_data_dir(&data_dir)
        .unwrap()
        .import_local(&root)
        .unwrap();
    let project_id = project.id.to_string();
    fs::write(root.join("text.txt"), b"two\n").unwrap();
    fs::write(root.join("blob.bin"), b"new\0bytes\0here").unwrap();

    let output = harkness(&data_dir, &["git", "diff", "--project", &project_id]);

    assert_success(&output);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.starts_with("0 staged, 2 unstaged\n"), "{text}");
    assert!(text.contains("text.txt\t1 hunks"), "{text}");
    assert!(text.contains("blob.bin\tno content (binary)"), "{text}");
}

#[test]
fn json_detection_ignores_a_literal_after_the_argument_terminator() {
    let fixture = TempDir::new().unwrap();
    let output = harkness(
        fixture.path(),
        &["worktree", "list", "--", "--json", "extra"],
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
}

/// The lock reason has to reach the wire and come back, and the state
/// refusals have to carry their deliberate refusal or conflict exit codes.
#[test]
fn worktree_lock_and_unlock_round_trip_through_the_json_contract() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let parent_root = fixture.path().join("lock-parent");
    fs::create_dir_all(&parent_root).unwrap();
    initialize_repository(&parent_root);
    let mut service = ProjectService::load_from_data_dir(&data_dir).unwrap();
    let parent_id = service.import_local(&parent_root).unwrap().id.to_string();

    let created = harkness(
        &data_dir,
        &[
            "--json",
            "worktree",
            "add",
            "--project",
            &parent_id,
            "--branch",
            "agent/cli-lock",
        ],
    );
    assert_success(&created);
    let worktree_id = json_output(&created)["data"]["project"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let missing_reason = harkness(
        &data_dir,
        &["--json", "worktree", "lock", "--project", &worktree_id],
    );
    assert_eq!(missing_reason.status.code(), Some(2));
    assert_eq!(json_output(&missing_reason)["error"]["kind"], "usage_error");

    let blank = harkness(
        &data_dir,
        &[
            "--json",
            "worktree",
            "lock",
            "--project",
            &worktree_id,
            "--reason",
            "   ",
        ],
    );
    assert_eq!(blank.status.code(), Some(3));
    assert_eq!(
        json_output(&blank)["error"]["kind"],
        "empty_worktree_lock_reason"
    );

    // Padding is trimmed by Git, so the envelope must report what was stored.
    let locked = harkness(
        &data_dir,
        &[
            "--json",
            "worktree",
            "lock",
            "--project",
            &worktree_id,
            "--reason",
            "  agent is still working  ",
        ],
    );
    assert_success(&locked);
    assert_eq!(
        json_output(&locked)["data"]["lock_reason"],
        "agent is still working"
    );

    let listed = harkness(
        &data_dir,
        &["--json", "worktree", "list", "--project", &parent_id],
    );
    assert_success(&listed);
    let row = json_output(&listed)["data"]["worktrees"]
        .as_array()
        .unwrap()
        .iter()
        .find(|worktree| worktree["id"] == worktree_id.as_str())
        .cloned()
        .expect("the locked worktree is listed");
    assert_eq!(row["locked"], true);
    assert_eq!(row["lock_reason"], "agent is still working");

    let relocked = harkness(
        &data_dir,
        &[
            "--json",
            "worktree",
            "lock",
            "--project",
            &worktree_id,
            "--reason",
            "again",
        ],
    );
    assert_eq!(relocked.status.code(), Some(5));
    assert_eq!(
        json_output(&relocked)["error"]["kind"],
        "worktree_already_locked"
    );

    // A lock refuses removal even with --force, so an explicit unlock is the
    // only way through.
    let forced = harkness(
        &data_dir,
        &[
            "--json",
            "worktree",
            "remove",
            "--project",
            &worktree_id,
            "--force",
        ],
    );
    assert_eq!(forced.status.code(), Some(3));
    assert_eq!(json_output(&forced)["error"]["kind"], "worktree_locked");
    assert_eq!(
        json_output(&forced)["error"]["details"]["reason"],
        "agent is still working"
    );

    let replaced = harkness(
        &data_dir,
        &[
            "--json",
            "worktree",
            "lock",
            "--project",
            &worktree_id,
            "--reason",
            "agent is deploying",
            "--replace",
        ],
    );
    assert_success(&replaced);
    assert_eq!(
        json_output(&replaced)["data"]["lock_reason"],
        "agent is deploying"
    );

    let unlocked = harkness(
        &data_dir,
        &["--json", "worktree", "unlock", "--project", &worktree_id],
    );
    assert_success(&unlocked);

    let again = harkness(
        &data_dir,
        &["--json", "worktree", "unlock", "--project", &worktree_id],
    );
    assert_eq!(again.status.code(), Some(5));
    assert_eq!(json_output(&again)["error"]["kind"], "worktree_not_locked");

    let removed = harkness(
        &data_dir,
        &["--json", "worktree", "remove", "--project", &worktree_id],
    );
    assert_success(&removed);
}

#[test]
fn contract_manifest_is_versioned_and_enumerates_exit_codes_and_kinds() {
    let fixture = TempDir::new().unwrap();
    let output = harkness(fixture.path(), &["--json", "contract"]);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let body = json_output(&output);
    assert_envelope(&body, "success", true);
    assert_eq!(body["data"]["envelope_version"], 1);
    assert_eq!(body["data"]["exit_codes"]["cancelled"], 130);
    assert!(
        body["data"]["error_kinds"]["cli"]
            .as_array()
            .unwrap()
            .iter()
            .any(|kind| kind == "confirmation_required")
    );
    assert!(
        body["data"]["error_kinds"]["project"]
            .as_array()
            .unwrap()
            .iter()
            .any(|kind| kind == "ambiguous_project_selector")
    );
    assert!(
        body["data"]["error_kinds"]["git"]
            .as_array()
            .unwrap()
            .iter()
            .any(|kind| kind == "not_a_repository")
    );
}

#[test]
fn project_reconcile_removes_only_uuid_named_orphan_storage() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let orphan = data_dir.join("repositories").join(FIXED_ID);
    let unrelated = data_dir.join("repositories").join("notes");
    fs::create_dir_all(orphan.join("checkout")).unwrap();
    fs::create_dir_all(&unrelated).unwrap();

    let output = harkness(&data_dir, &["--json", "project", "reconcile"]);
    assert!(output.status.success());
    let body = json_output(&output);
    assert_eq!(body["data"]["removed"].as_array().unwrap().len(), 1);
    assert!(!orphan.exists());
    assert!(unrelated.exists());
}

#[test]
fn a_closed_stdout_pipe_is_a_clean_exit_without_a_panic() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let projects = (1..=1_000)
        .map(|index| {
            json!({
                "id": format!("00000000-0000-4000-8000-{index:012x}"),
                "display_name": format!("project-{index:04}"),
                "root": fixture.path().join(format!("a-very-long-project-root-{index:04}")),
                "source": "local",
                "last_opened": "2026-08-06 18:52:03.000000000 +00:00:00",
            })
        })
        .collect();
    write_catalog(&data_dir, projects);

    let mut child = Command::new(env!("CARGO_BIN_EXE_harkness"))
        .env("HARKNESS_DATA_DIR", &data_dir)
        .args(["--json", "project", "list", "--no-status"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut prefix = [0; 5];
    stdout.read_exact(&mut prefix).unwrap();
    assert_eq!(&prefix, b"{\"v\":");
    drop(stdout);
    let status = child.wait().unwrap();
    let mut stderr = Vec::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_end(&mut stderr)
        .unwrap();

    assert!(status.success());
    assert!(stderr.is_empty(), "{}", String::from_utf8_lossy(&stderr));
}

#[cfg(unix)]
#[test]
fn clone_progress_failure_success_delete_and_plain_text_compatibility_are_covered() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let remote = fixture.path().join("remote");
    initialize_repository(&remote);
    let bin = fixture.path().join("bin");
    let fake_git = bin.join("git");
    fs::create_dir_all(&bin).unwrap();
    make_executable(
        &fake_git,
        "#!/bin/sh\necho 'fixture clone failed' >&2\nexit 23\n",
    );
    let path = path_with_prefix(&bin);

    let failed = harkness_with_path(
        &data_dir,
        &path,
        &[
            "--json",
            "project",
            "clone",
            "https://github.com/example/failure.git",
        ],
    );
    assert_eq!(failed.status.code(), Some(1));
    assert_eq!(json_output(&failed)["error"]["kind"], "clone_failed");
    assert!(repositories_are_empty(&data_dir));

    let real_git = find_git_executable();
    make_executable(
        &fake_git,
        &format!(
            "#!/bin/sh\necho 'fixture progress' >&2\nexec {} clone --progress -- {} checkout\n",
            shell_quote(&real_git),
            shell_quote(&remote)
        ),
    );
    let cloned = harkness_with_path(
        &data_dir,
        &path,
        &[
            "--json",
            "project",
            "clone",
            "https://github.com/example/success.git",
        ],
    );
    assert!(
        cloned.status.success(),
        "{}",
        String::from_utf8_lossy(&cloned.stderr)
    );
    let cloned_body = json_output(&cloned);
    let project = &cloned_body["data"]["project"];
    let id = project["id"].as_str().unwrap();
    let checkout = PathBuf::from(project["root"].as_str().unwrap());
    assert!(checkout.exists());
    assert_eq!(project["source"], "managed_repository");
    for line in cloned
        .stderr
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let progress: Value = serde_json::from_slice(line).unwrap();
        assert_envelope(
            &progress,
            "progress",
            progress.get("ok").and_then(Value::as_bool).unwrap_or(true),
        );
        assert_eq!(progress["v"], 1);
        assert!(progress["message"].is_string());
    }
    assert!(String::from_utf8_lossy(&cloned.stderr).contains("fixture progress"));

    let plain = harkness(&data_dir, &["project", "list"]);
    assert!(
        String::from_utf8(plain.stdout)
            .unwrap()
            .contains("\tmanaged\tavailable\n")
    );

    let forgotten = harkness(&data_dir, &["--json", "project", "forget", "--project", id]);
    assert_eq!(forgotten.status.code(), Some(3));
    assert_eq!(
        json_output(&forgotten)["error"]["kind"],
        "managed_project_requires_delete"
    );

    let confirmation = harkness(&data_dir, &["--json", "project", "delete", "--project", id]);
    assert_eq!(confirmation.status.code(), Some(3));
    assert_eq!(
        json_output(&confirmation)["error"]["kind"],
        "confirmation_required"
    );

    let deleted = harkness(
        &data_dir,
        &["--json", "project", "delete", "--project", id, "--yes"],
    );
    assert!(deleted.status.success());
    assert!(!checkout.exists());
    assert!(
        ProjectService::load_from_data_dir(&data_dir)
            .unwrap()
            .list_catalog_only()
            .unwrap()
            .is_empty()
    );
}

#[cfg(unix)]
#[test]
fn ctrl_c_cancels_clone_with_exit_130_and_cleans_partial_storage() {
    use std::{thread, time::Duration};

    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let bin = fixture.path().join("bin");
    let fake_git = bin.join("git");
    let ready = fixture.path().join("ready");
    fs::create_dir_all(&bin).unwrap();
    make_executable(
        &fake_git,
        "#!/bin/sh\ntouch \"$HARKNESS_TEST_READY\"\necho ready >&2\nwhile true; do sleep 1; done\n",
    );
    let path = path_with_prefix(&bin);
    let child = Command::new(env!("CARGO_BIN_EXE_harkness"))
        .env("HARKNESS_DATA_DIR", &data_dir)
        .env("HARKNESS_TEST_READY", &ready)
        .env("PATH", path)
        .args([
            "--json",
            "project",
            "clone",
            "https://github.com/example/slow.git",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    for _ in 0..200 {
        if ready.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(ready.exists(), "fake Git process never started");
    // SAFETY: the child PID is live and belongs to this test process.
    assert_eq!(
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) },
        0
    );
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(130));
    assert_eq!(json_output(&output)["error"]["kind"], "clone_cancelled");
    assert!(repositories_are_empty(&data_dir));
}

// Darwin filesystems reject this deliberately malformed UTF-8 filename with
// EILSEQ before Harkness can inspect it. Keep the end-to-end fixture on a
// platform where arbitrary-byte pathnames are supported; the parser's Unix
// byte-preservation behavior remains covered by harkness-core unit tests.
#[cfg(target_os = "linux")]
#[test]
fn non_utf8_status_paths_are_lossy_and_explicitly_flagged() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let project_root = fixture.path().join("non-utf8-status");
    initialize_repository(&project_root);
    let mut service = ProjectService::load_from_data_dir(&data_dir).unwrap();
    let project = service.import_local(&project_root).unwrap();
    let path = OsString::from_vec(b"invalid-\xff.txt".to_vec());
    fs::write(project_root.join(path), "not UTF-8\n").unwrap();

    let output = harkness(
        &data_dir,
        &[
            "--json",
            "git",
            "status",
            "--project",
            &project.id.to_string(),
        ],
    );

    assert_success(&output);
    let body = json_output(&output);
    let entry = body["data"]["status"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path_is_lossy"] == true)
        .expect("the non-UTF-8 path should be flagged");
    assert!(entry["path"].as_str().unwrap().contains('\u{fffd}'));
}

#[cfg(unix)]
#[test]
fn ctrl_c_cancels_fetch_with_exit_130_and_kills_its_process_group() {
    use std::{thread, time::Duration};

    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let project_root = fixture.path().join("fetch-project");
    initialize_repository(&project_root);
    Repository::open(&project_root)
        .unwrap()
        .remote("origin", "file:///does-not-need-to-exist")
        .unwrap();
    let mut service = ProjectService::load_from_data_dir(&data_dir).unwrap();
    let project = service.import_local(&project_root).unwrap();

    let bin = fixture.path().join("bin");
    let fake_git = bin.join("git");
    let ready = fixture.path().join("fetch-ready");
    let activity = fixture.path().join("fetch-helper-activity");
    fs::create_dir_all(&bin).unwrap();
    make_executable(
        &fake_git,
        "#!/bin/sh\n(while true; do printf x >> \"$HARKNESS_TEST_ACTIVITY\"; sleep 0.01; done) 2>/dev/null &\ntouch \"$HARKNESS_TEST_READY\"\necho ready >&2\nwait\n",
    );
    let path = path_with_prefix(&bin);
    let child = Command::new(env!("CARGO_BIN_EXE_harkness"))
        .env("HARKNESS_DATA_DIR", &data_dir)
        .env("HARKNESS_TEST_READY", &ready)
        .env("HARKNESS_TEST_ACTIVITY", &activity)
        .env("PATH", path)
        .args([
            "--json",
            "git",
            "fetch",
            "--project",
            &project.id.to_string(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    for _ in 0..500 {
        if ready.exists() && activity.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(ready.exists(), "fake Git process never started");
    assert!(activity.exists(), "fake Git helper never started");
    // SAFETY: the child PID is live and belongs to this test process.
    assert_eq!(
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) },
        0
    );
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(130));
    assert_eq!(json_output(&output)["error"]["kind"], "cancelled");
    let activity_after_cancel = fs::read(&activity).unwrap();
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        fs::read(&activity).unwrap(),
        activity_after_cancel,
        "a helper survived fetch cancellation"
    );
}

/// The single record for `path` on `target`.
///
/// The length and target are asserted rather than assumed: taking the first
/// element would keep passing if path narrowing or target narrowing regressed,
/// which is exactly what a helper positioned here exists to notice.
fn diff_file(data_dir: &Path, project_id: &str, target: &str, path: &str) -> Value {
    // The path follows `--` so a leading hyphen is a path rather than a flag,
    // which is the same separator `git` itself requires.
    let output = harkness(
        data_dir,
        &[
            "--json",
            "git",
            "diff",
            target,
            "--project",
            project_id,
            "--",
            path,
        ],
    );
    assert_success(&output);
    let files = json_output(&output)["data"]["files"]
        .as_array()
        .cloned()
        .unwrap();
    assert_eq!(files.len(), 1, "expected exactly one record: {files:#?}");
    assert_eq!(files[0]["target"], target.trim_start_matches("--"));
    files.into_iter().next().unwrap()
}

fn hunk_arguments(command: &str, project_id: &str, file: &Value, hunk_index: usize) -> Vec<String> {
    let hunk = &file["hunks"][hunk_index];
    let mut arguments = vec![
        "--json".to_owned(),
        "git".to_owned(),
        command.to_owned(),
        "--hunk".to_owned(),
        "--project".to_owned(),
        project_id.to_owned(),
    ];
    for (flag, value) in [
        ("--old-path", &file["old_path"]),
        ("--new-path", &file["new_path"]),
    ] {
        if let Some(value) = value.as_str() {
            arguments.extend([flag.to_owned(), value.to_owned()]);
        }
    }
    for (flag, value) in [
        ("--old-blob-id", &file["old_blob_id"]),
        ("--new-blob-id", &file["new_blob_id"]),
        ("--context-lines", &file["context_lines"]),
        ("--old-start", &hunk["old_start"]),
        ("--old-lines", &hunk["old_lines"]),
        ("--new-start", &hunk["new_start"]),
        ("--new-lines", &hunk["new_lines"]),
    ] {
        let value = value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| value.as_u64().unwrap().to_string());
        arguments.extend([flag.to_owned(), value]);
    }
    arguments
}

fn initialize_repository(root: &Path) {
    fs::create_dir_all(root).unwrap();
    let repository = Repository::init(root).unwrap();
    repository.set_head("refs/heads/main").unwrap();
    configure_identity(&repository);
    fs::write(root.join("README.md"), "fixture\n").unwrap();
    commit_all(&repository, "fixture");
}

fn worktree_text(path: &Path) -> String {
    fs::read_to_string(path).unwrap().replace("\r\n", "\n")
}

fn configure_identity(repository: &Repository) {
    let mut config = repository.config().unwrap();
    config.set_str("user.name", "Harkness Tests").unwrap();
    config
        .set_str("user.email", "tests@harkness.invalid")
        .unwrap();
    config.set_bool("commit.gpgsign", false).unwrap();
}

fn commit_all(repository: &Repository, message: &str) {
    let mut index = repository.index().unwrap();
    index
        .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    commit_from_index(repository, message, &mut index);
}

fn commit_index(repository: &Repository, message: &str) {
    let mut index = repository.index().unwrap();
    commit_from_index(repository, message, &mut index);
}

fn commit_from_index(repository: &Repository, message: &str, index: &mut git2::Index) {
    let tree_id = index.write_tree().unwrap();
    index.write().unwrap();
    let tree = repository.find_tree(tree_id).unwrap();
    let signature = Signature::now("Harkness Tests", "tests@example.com").unwrap();
    let parents = repository
        .head()
        .ok()
        .and_then(|head| head.target())
        .map(|id| repository.find_commit(id).unwrap())
        .into_iter()
        .collect::<Vec<_>>();
    repository
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &parents.iter().collect::<Vec<_>>(),
        )
        .unwrap();
}

fn raw_commit(repository: &Repository, message: &[u8]) -> git2::Oid {
    let parent = repository.head().unwrap().target().unwrap();
    let tree = repository.find_commit(parent).unwrap().tree_id();
    let mut raw = Vec::new();
    raw.extend_from_slice(format!("tree {tree}\nparent {parent}\nauthor ").as_bytes());
    raw.extend_from_slice(b"Auth\xffor <a\xfe@example.invalid> 1700000010 -0130\n");
    raw.extend_from_slice(b"committer Comm\xfdtter <c\xfc@example.invalid> 1700000020 +0200\n\n");
    raw.extend_from_slice(message);
    let id = repository
        .odb()
        .unwrap()
        .write(ObjectType::Commit, &raw)
        .unwrap();
    let reference = repository.head().unwrap().name().unwrap().to_owned();
    repository
        .reference(&reference, id, true, "raw byte CLI fixture")
        .unwrap();
    id
}

fn ambiguous_object_prefix(repository: &Repository) -> String {
    let database = repository.odb().unwrap();
    let mut seen = HashMap::new();
    for index in 0..20_000 {
        let bytes = format!("ambiguous object {index}");
        let id = database.write(ObjectType::Blob, bytes.as_bytes()).unwrap();
        let prefix = id.to_string()[..4].to_owned();
        if seen.insert(prefix.clone(), id).is_some() {
            return prefix;
        }
    }
    panic!("failed to construct an ambiguous four-hex object prefix")
}

fn remote_with_clone(fixture: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let remote = fixture.join("remote.git");
    let bare = Repository::init_bare(&remote).unwrap();
    bare.set_head("refs/heads/main").unwrap();

    let source = fixture.join("source");
    initialize_repository(&source);
    let remote_url = format!("file://{}", remote.display());
    run_git(&source, &["remote", "add", "origin", &remote_url]);
    run_git(&source, &["push", "--set-upstream", "origin", "main"]);

    let clone = fixture.join("parent");
    run_git(
        fixture,
        &["clone", "--", &remote_url, clone.to_str().unwrap()],
    );
    configure_identity(&Repository::open(&clone).unwrap());
    (remote, source, clone)
}

fn run_git(current_dir: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .current_dir(current_dir)
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {} failed:\nstdout: {}\nstderr: {}",
        arguments.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_catalog(data_dir: &Path, projects: Vec<Value>) {
    fs::create_dir_all(data_dir).unwrap();
    fs::write(
        data_dir.join("projects.json"),
        serde_json::to_vec_pretty(&json!({ "version": 1, "projects": projects })).unwrap(),
    )
    .unwrap();
}

fn harkness<S: AsRef<std::ffi::OsStr>>(data_dir: &Path, arguments: &[S]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_harkness"))
        .env("HARKNESS_DATA_DIR", data_dir)
        .args(arguments)
        .output()
        .expect("harkness command should start")
}

/// Runs the CLI with a document on standard input.
fn harkness_with_stdin<S: AsRef<std::ffi::OsStr>>(
    data_dir: &Path,
    arguments: &[S],
    stdin: &str,
) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_harkness"))
        .env("HARKNESS_DATA_DIR", data_dir)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("harkness command should start");
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(stdin.as_bytes())
        .expect("the document should be written");
    child.wait_with_output().expect("harkness should finish")
}

fn harkness_from(data_dir: &Path, current_dir: &Path, arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_harkness"))
        .env("HARKNESS_DATA_DIR", data_dir)
        .current_dir(current_dir)
        .args(arguments)
        .output()
        .expect("harkness command should start")
}

#[cfg(unix)]
fn harkness_with_path(
    data_dir: &Path,
    path: &std::ffi::OsStr,
    arguments: &[&str],
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_harkness"))
        .env("HARKNESS_DATA_DIR", data_dir)
        .env("PATH", path)
        .args(arguments)
        .output()
        .expect("harkness command should start")
}

fn json_output(output: &std::process::Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout was not JSON ({error}): {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn assert_envelope(body: &Value, kind: &str, ok: bool) {
    assert_eq!(body["v"], 1);
    assert_eq!(body["type"], kind);
    if body.get("ok").is_some() {
        assert_eq!(body["ok"], ok);
    }
}

fn assert_selected(output: &std::process::Output, project: &Project) {
    assert_success(output);
    assert!(output.stderr.is_empty());
    assert_eq!(
        json_output(output)["data"]["project"]["id"],
        project.id.to_string()
    );
}

fn assert_success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn repositories_are_empty(data_dir: &Path) -> bool {
    let repositories = data_dir.join("repositories");
    !repositories.exists() || fs::read_dir(repositories).unwrap().next().is_none()
}

#[cfg(unix)]
fn make_executable(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;

    fs::write(path, contents).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

#[cfg(unix)]
fn path_with_prefix(prefix: &Path) -> std::ffi::OsString {
    let mut paths = vec![prefix.to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::env::join_paths(paths).unwrap()
}

#[cfg(unix)]
fn find_git_executable() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join("git"))
        .find(|candidate| candidate.is_file())
        .expect("system Git should be on PATH")
}

#[cfg(unix)]
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}
