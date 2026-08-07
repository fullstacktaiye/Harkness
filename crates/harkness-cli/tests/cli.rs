use std::{fs, path::Path, process::Command};

use git2::{Repository, Signature};
use harkness_core::{Project, ProjectService};
use serde_json::Value;
use tempfile::TempDir;

#[test]
fn version_output_is_exact() {
    let fixture = TempDir::new().unwrap();
    let output = harkness(fixture.path(), &["--version"]);

    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        format!("harkness {}\n", env!("CARGO_PKG_VERSION")).as_bytes()
    );
    assert!(output.stderr.is_empty());
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
    assert_eq!(output.stdout, b"{\"ok\":true,\"data\":{\"projects\":[]}}\n");
    assert!(output.stderr.is_empty());
}

fn initialize_repository(root: &Path) {
    fs::create_dir_all(root).unwrap();
    let repository = Repository::init(root).unwrap();
    fs::write(root.join("README.md"), "fixture\n").unwrap();
    let mut index = repository.index().unwrap();
    index.add_path(Path::new("README.md")).unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repository.find_tree(tree_id).unwrap();
    let signature = Signature::now("Harkness Tests", "tests@example.com").unwrap();
    repository
        .commit(Some("HEAD"), &signature, &signature, "fixture", &tree, &[])
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

fn json_output(output: &std::process::Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout was not JSON ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn assert_selected(output: &std::process::Output, project: &Project) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    assert_eq!(
        json_output(output)["data"]["project"]["id"],
        project.id.to_string()
    );
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
            &["--json", "--project", selector, "project", "show"],
        );
        assert_selected(&output, &nested);
    }

    let ambient = harkness_from(
        &data_dir,
        &working_directory,
        &["--json", "project", "show"],
    );
    assert_selected(&ambient, &nested);
}

#[test]
fn ambiguous_project_name_lists_candidates() {
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
        &["--json", "--project", "shared", "project", "show"],
    );

    assert_eq!(output.status.code(), Some(5));
    assert!(output.stderr.is_empty());
    let body = json_output(&output);
    assert_eq!(body["ok"], false);
    assert_eq!(body["error"]["kind"], "ambiguous_project_selector");
    let candidates = body["error"]["details"]["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 2);
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
fn delete_confirmation_is_a_structured_guardrail_refusal() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let project_root = fixture.path().join("local-project");
    fs::create_dir_all(&project_root).unwrap();
    let mut service = ProjectService::load_from_data_dir(&data_dir).unwrap();
    let project = service.import_local(project_root).unwrap();

    let output = harkness(
        &data_dir,
        &[
            "--json",
            "--project",
            &project.id.to_string(),
            "project",
            "delete",
        ],
    );

    assert_eq!(output.status.code(), Some(3));
    assert!(output.stderr.is_empty());
    let body = json_output(&output);
    assert_eq!(body["error"]["kind"], "confirmation_required");
    assert_eq!(body["error"]["details"]["override_flag"], "--yes");
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
    assert_eq!(output.stdout, b"{\"ok\":true,\"data\":{\"projects\":[]}}\n");
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
        &["--json", "--project", &id, "project", "forget"],
    );
    assert!(forgotten.status.success());
    assert!(forgotten.stderr.is_empty());
    assert_eq!(json_output(&forgotten)["data"]["project"]["id"], id);
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
fn worktree_commands_create_list_remove_and_reconcile() {
    let fixture = TempDir::new().unwrap();
    let data_dir = fixture.path().join("data");
    let parent_root = fixture.path().join("parent");
    initialize_repository(&parent_root);
    let mut service = ProjectService::load_from_data_dir(&data_dir).unwrap();
    let parent = service.import_local(&parent_root).unwrap();

    let projects = harkness(&data_dir, &["project", "list"]);
    assert!(projects.status.success());
    assert!(
        String::from_utf8(projects.stdout)
            .unwrap()
            .starts_with(&parent.id.to_string())
    );

    let created = harkness(
        &data_dir,
        &[
            "worktree",
            "create",
            &parent.id.to_string(),
            "--new",
            "agent/cli",
        ],
    );
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let created_text = String::from_utf8(created.stdout).unwrap();
    let worktree_id = created_text
        .strip_prefix("created ")
        .unwrap()
        .split('\t')
        .next()
        .unwrap()
        .to_owned();

    let listed = harkness(&data_dir, &["worktree", "list", &parent.id.to_string()]);
    assert!(listed.status.success());
    let listed_text = String::from_utf8(listed.stdout).unwrap();
    assert!(listed_text.contains(&format!("{worktree_id}\tagent/cli\t")));
    assert!(listed_text.contains("\tharkness\tactive"));

    let listed_json = harkness(
        &data_dir,
        &["--json", "worktree", "list", &parent.id.to_string()],
    );
    assert!(listed_json.status.success());
    assert!(listed_json.stderr.is_empty());
    assert_eq!(
        json_output(&listed_json)["data"]["worktrees"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let removed = harkness(&data_dir, &["worktree", "remove", worktree_id.as_str()]);
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );

    let recreated = harkness(
        &data_dir,
        &[
            "worktree",
            "create",
            &parent.id.to_string(),
            "--existing",
            "agent/cli",
        ],
    );
    assert!(
        recreated.status.success(),
        "{}",
        String::from_utf8_lossy(&recreated.stderr)
    );
    let recreated_id = String::from_utf8(recreated.stdout)
        .unwrap()
        .strip_prefix("created ")
        .unwrap()
        .split('\t')
        .next()
        .unwrap()
        .to_owned();
    let reloaded = ProjectService::load_from_data_dir(&data_dir).unwrap();
    let checkout = reloaded
        .list()
        .unwrap()
        .into_iter()
        .find(|project| project.id.to_string() == recreated_id)
        .unwrap();
    fs::remove_dir_all(checkout.root).unwrap();

    let reconciled = harkness(
        &data_dir,
        &["worktree", "reconcile", &parent.id.to_string()],
    );
    assert!(reconciled.status.success());
    assert_eq!(reconciled.stdout, b"reconciled 1 stale worktree entries\n");
}
