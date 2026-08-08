use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use git2::{Repository, Signature};
use harkness_core::{Project, ProjectService};
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
    let output = harkness(fixture.path(), &[]);

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
        "{{\"v\":1,\"type\":\"success\",\"ok\":true,\"data\":{{\"project\":{{\"available\":true,\"display_name\":\"wire-project\",\"git\":{{\"branch\":{branch},\"dirty\":false,\"staged\":0,\"unstaged\":0,\"upstream\":null}},\"id\":\"{FIXED_ID}\",\"last_opened\":\"2026-08-06T18:52:03Z\",\"parent\":null,\"remote\":null,\"root\":{root},\"source\":\"local\",\"status_checked\":true,\"worktree_branch\":null}}}}}}\n"
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
    let expected_keys = BTreeSet::from(["display_name", "id", "root", "source"]);
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
fn worktree_create_rejects_start_with_existing_or_detached() {
    let fixture = TempDir::new().unwrap();
    for base in [["--existing", "feature"], ["--detached", "HEAD"]] {
        let output = harkness(
            fixture.path(),
            &[
                "--json", "worktree", "create", FIXED_ID, base[0], base[1], "--start", "HEAD",
            ],
        );
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stderr.is_empty());
        let body = json_output(&output);
        assert_eq!(body["error"]["kind"], "usage_error");
        assert!(
            !body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Usage:")
        );
        assert!(
            !body["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("error: ")
        );
    }
}

#[test]
fn worktree_commands_cover_modes_guardrails_reconciliation_and_exit_codes() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let parent_root = fixture.path().join("parent");
    initialize_repository(&parent_root);
    let mut service = ProjectService::load_from_data_dir(&data_dir).unwrap();
    let parent = service.import_local(&parent_root).unwrap();
    let parent_id = parent.id.to_string();

    let missing_branch = harkness(
        &data_dir,
        &[
            "--json",
            "worktree",
            "create",
            &parent_id,
            "--existing",
            "missing",
        ],
    );
    assert_eq!(missing_branch.status.code(), Some(4));
    assert_eq!(
        json_output(&missing_branch)["error"]["kind"],
        "no_such_branch"
    );

    let invalid_start = harkness(
        &data_dir,
        &[
            "--json",
            "worktree",
            "create",
            &parent_id,
            "--new",
            "invalid-start",
            "--start",
            "does-not-exist",
        ],
    );
    assert_eq!(invalid_start.status.code(), Some(1));
    assert_eq!(
        json_output(&invalid_start)["error"]["kind"],
        "invalid_start_point"
    );

    let created = harkness(
        &data_dir,
        &[
            "worktree",
            "create",
            &parent_id,
            "--new",
            "agent/cli",
            "--start",
            "HEAD",
        ],
    );
    assert_success(&created);
    let worktree_id = created_id(&created);

    let duplicate_branch = harkness(
        &data_dir,
        &[
            "--json",
            "worktree",
            "create",
            &parent_id,
            "--new",
            "agent/cli",
        ],
    );
    assert_eq!(duplicate_branch.status.code(), Some(5));
    assert_eq!(
        json_output(&duplicate_branch)["error"]["kind"],
        "branch_already_exists"
    );

    let listed = harkness(&data_dir, &["worktree", "list", &parent_id]);
    assert_success(&listed);
    let listed_text = String::from_utf8(listed.stdout).unwrap();
    assert!(listed_text.contains(&format!("{worktree_id}\tagent/cli\t")));
    assert!(listed_text.contains("\tharkness\tactive"));

    let delete_worktree = harkness(
        &data_dir,
        &["--json", "project", "delete", "--project", &worktree_id],
    );
    assert_eq!(delete_worktree.status.code(), Some(3));
    assert_eq!(
        json_output(&delete_worktree)["error"]["kind"],
        "worktree_requires_remove"
    );

    let checkout = ProjectService::load_from_data_dir(&data_dir)
        .unwrap()
        .resolve(&harkness_core::ProjectSelector::from(worktree_id.as_str()))
        .unwrap();
    fs::write(checkout.root.join("dirty.txt"), "dirty\n").unwrap();
    let dirty = harkness(&data_dir, &["--json", "worktree", "remove", &worktree_id]);
    assert_eq!(dirty.status.code(), Some(3));
    assert_eq!(
        json_output(&dirty)["error"]["kind"],
        "dirty_worktree_removal"
    );
    assert_eq!(
        json_output(&dirty)["error"]["details"]["override_flags"],
        json!(["--force", "--yes"])
    );

    let unconfirmed = harkness(
        &data_dir,
        &["--json", "worktree", "remove", &worktree_id, "--force"],
    );
    assert_eq!(unconfirmed.status.code(), Some(3));
    assert_eq!(
        json_output(&unconfirmed)["error"]["kind"],
        "confirmation_required"
    );

    let removed = harkness(
        &data_dir,
        &["worktree", "remove", &worktree_id, "--force", "--yes"],
    );
    assert_success(&removed);

    let recreated = harkness(
        &data_dir,
        &["worktree", "create", &parent_id, "--existing", "agent/cli"],
    );
    assert_success(&recreated);
    let recreated_id = created_id(&recreated);
    let reloaded = ProjectService::load_from_data_dir(&data_dir).unwrap();
    let checkout = reloaded
        .list()
        .unwrap()
        .into_iter()
        .find(|project| project.id.to_string() == recreated_id)
        .unwrap();
    fs::remove_dir_all(checkout.root).unwrap();

    let reconciled = harkness(&data_dir, &["worktree", "reconcile", &parent_id]);
    assert_success(&reconciled);
    assert_eq!(reconciled.stdout, b"reconciled 1 stale worktree entries\n");

    let non_repository_root = fixture.path().join("plain");
    fs::create_dir_all(&non_repository_root).unwrap();
    let plain = service.import_local(non_repository_root).unwrap();
    let not_repository = harkness(
        &data_dir,
        &["--json", "worktree", "list", &plain.id.to_string()],
    );
    assert_eq!(not_repository.status.code(), Some(4));
    assert_eq!(
        json_output(&not_repository)["error"]["kind"],
        "not_a_repository"
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

fn initialize_repository(root: &Path) {
    fs::create_dir_all(root).unwrap();
    let repository = Repository::init(root).unwrap();
    fs::write(root.join("README.md"), "fixture\n").unwrap();
    let mut index = repository.index().unwrap();
    index.add_path(Path::new("README.md")).unwrap();
    let tree_id = index.write_tree().unwrap();
    index.write().unwrap();
    let tree = repository.find_tree(tree_id).unwrap();
    let signature = Signature::now("Harkness Tests", "tests@example.com").unwrap();
    repository
        .commit(Some("HEAD"), &signature, &signature, "fixture", &tree, &[])
        .unwrap();
}

fn write_catalog(data_dir: &Path, projects: Vec<Value>) {
    fs::create_dir_all(data_dir).unwrap();
    fs::write(
        data_dir.join("projects.json"),
        serde_json::to_vec_pretty(&json!({ "version": 1, "projects": projects })).unwrap(),
    )
    .unwrap();
}

fn harkness(data_dir: &Path, arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_harkness"))
        .env("HARKNESS_DATA_DIR", data_dir)
        .args(arguments)
        .output()
        .expect("harkness command should start")
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

fn created_id(output: &std::process::Output) -> String {
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .strip_prefix("created ")
        .unwrap()
        .split('\t')
        .next()
        .unwrap()
        .to_owned()
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
