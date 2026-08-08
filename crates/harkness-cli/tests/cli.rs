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
    let checkout = moved_parent.join("checkout");
    let moved = harkness(
        &data_dir,
        &[
            "--json",
            "worktree",
            "move",
            checkout.to_str().unwrap(),
            "--project",
            &worktree_id,
        ],
    );
    assert_success(&moved);
    assert_eq!(
        json_output(&moved)["data"]["project"]["root"],
        checkout.to_str().unwrap()
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
        repaired_body["data"]["repaired"][0]["root"],
        repair_destination.to_str().unwrap()
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

fn initialize_repository(root: &Path) {
    fs::create_dir_all(root).unwrap();
    let repository = Repository::init(root).unwrap();
    repository.set_head("refs/heads/main").unwrap();
    configure_identity(&repository);
    fs::write(root.join("README.md"), "fixture\n").unwrap();
    commit_all(&repository, "fixture");
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
