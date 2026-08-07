use std::{fs, path::Path, process::Command};

use git2::{Repository, Signature};
use harkness_core::ProjectService;
use tempfile::TempDir;

#[test]
fn prints_exact_greeting() {
    let output = Command::new(env!("CARGO_BIN_EXE_harkness"))
        .output()
        .expect("harkness should start");

    assert!(output.status.success());
    assert_eq!(output.stdout, b"Hello World\n");
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
