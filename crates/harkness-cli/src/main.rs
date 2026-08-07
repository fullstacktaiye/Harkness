use harkness_core::{Cancellation, ProjectId, ProjectService, WorktreeBase};

const USAGE: &str = "Usage:
  harkness project list
  harkness worktree list <parent-id>
  harkness worktree create <parent-id> --new <branch> [--start <revision>]
  harkness worktree create <parent-id> --existing <branch>
  harkness worktree create <parent-id> --detached <revision>
  harkness worktree remove <worktree-id> [--force]
  harkness worktree reconcile <parent-id>";

fn main() {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments.is_empty() {
        println!("{}", harkness_core::greeting());
        return;
    }

    match run(&arguments) {
        Ok(output) if !output.is_empty() => println!("{output}"),
        Ok(_) => {}
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}

fn run(arguments: &[String]) -> Result<String, String> {
    match arguments {
        [command] if command == "--help" || command == "-h" => Ok(USAGE.to_owned()),
        [group, command, rest @ ..] if group == "project" => run_project(command, rest),
        [group, command, rest @ ..] if group == "worktree" => run_worktree(command, rest),
        _ => Err(USAGE.to_owned()),
    }
}

fn run_project(command: &str, arguments: &[String]) -> Result<String, String> {
    if command != "list" || !arguments.is_empty() {
        return Err(USAGE.to_owned());
    }
    let service = ProjectService::load().map_err(|error| error.to_string())?;
    let projects = service.list().map_err(|error| error.to_string())?;
    Ok(projects
        .into_iter()
        .map(|project| {
            let source = match project.source {
                harkness_core::ProjectSource::Local => "local",
                harkness_core::ProjectSource::ManagedRepository { .. } => "managed",
                harkness_core::ProjectSource::Worktree { .. } => "worktree",
            };
            let availability = if project.available {
                "available"
            } else {
                "missing"
            };
            format!(
                "{}\t{}\t{}\t{source}\t{availability}",
                project.id,
                project.display_name,
                project.root.display()
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

fn run_worktree(command: &str, arguments: &[String]) -> Result<String, String> {
    let cancellation = Cancellation::default();
    let mut service = ProjectService::load().map_err(|error| error.to_string())?;
    match (command, arguments) {
        ("list", [parent]) => {
            let worktrees = service
                .worktrees(project_id(parent, "parent")?, &cancellation)
                .map_err(|error| error.to_string())?;
            Ok(worktrees
                .into_iter()
                .map(|worktree| {
                    let id = worktree
                        .project
                        .as_ref()
                        .map_or_else(|| "-".to_owned(), |project| project.id.to_string());
                    let branch = worktree
                        .branch
                        .unwrap_or_else(|| "detached HEAD".to_owned());
                    let owner = if worktree.project.is_some() {
                        "harkness"
                    } else {
                        "external"
                    };
                    let state = if worktree.locked {
                        "locked"
                    } else if worktree.prunable {
                        "prunable"
                    } else {
                        "active"
                    };
                    format!(
                        "{id}\t{branch}\t{}\t{owner}\t{state}",
                        worktree.root.display()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        ("create", [parent, flag, value]) if flag == "--new" => create_worktree(
            &mut service,
            parent,
            WorktreeBase::NewBranch {
                name: value.clone(),
                start_point: None,
            },
            &cancellation,
        ),
        ("create", [parent, flag, value, start_flag, start])
            if flag == "--new" && start_flag == "--start" =>
        {
            create_worktree(
                &mut service,
                parent,
                WorktreeBase::NewBranch {
                    name: value.clone(),
                    start_point: Some(start.clone()),
                },
                &cancellation,
            )
        }
        ("create", [parent, flag, branch]) if flag == "--existing" => create_worktree(
            &mut service,
            parent,
            WorktreeBase::ExistingBranch {
                name: branch.clone(),
            },
            &cancellation,
        ),
        ("create", [parent, flag, commit]) if flag == "--detached" => create_worktree(
            &mut service,
            parent,
            WorktreeBase::Detached {
                commit: commit.clone(),
            },
            &cancellation,
        ),
        ("remove", [worktree]) => remove_worktree(
            &mut service,
            project_id(worktree, "worktree")?,
            false,
            &cancellation,
        ),
        ("remove", [worktree, force]) if force == "--force" => remove_worktree(
            &mut service,
            project_id(worktree, "worktree")?,
            true,
            &cancellation,
        ),
        ("reconcile", [parent]) => {
            let removed = service
                .reconcile_worktrees(project_id(parent, "parent")?, &cancellation)
                .map_err(|error| error.to_string())?;
            Ok(format!(
                "reconciled {} stale worktree entries",
                removed.len()
            ))
        }
        _ => Err(USAGE.to_owned()),
    }
}

fn create_worktree(
    service: &mut ProjectService,
    parent: &str,
    base: WorktreeBase,
    cancellation: &Cancellation,
) -> Result<String, String> {
    let project = service
        .create_worktree(project_id(parent, "parent")?, &base, cancellation)
        .map_err(|error| error.to_string())?;
    Ok(format!(
        "created {}\t{}\t{}",
        project.id,
        project.display_name,
        project.root.display()
    ))
}

fn remove_worktree(
    service: &mut ProjectService,
    id: ProjectId,
    force: bool,
    cancellation: &Cancellation,
) -> Result<String, String> {
    let project = service
        .remove_worktree(id, force, cancellation)
        .map_err(|error| error.to_string())?;
    Ok(format!("removed {}", project.display_name))
}

fn project_id(value: &str, kind: &str) -> Result<ProjectId, String> {
    value
        .parse()
        .map_err(|_| format!("invalid {kind} project identifier '{value}'"))
}

#[cfg(test)]
mod tests {
    use super::{USAGE, run};

    #[test]
    fn invalid_worktree_syntax_is_actionable() {
        assert_eq!(run(&["worktree".to_owned()]).unwrap_err(), USAGE);
        assert_eq!(run(&["--help".to_owned()]).unwrap(), USAGE,);
    }
}
