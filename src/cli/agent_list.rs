use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::api::schema::{EmptyParams, Method, Request};

const USAGE: &str = "usage: herdr agent list [--flat] [--project [PATH] | --workspace [PATH]]";

#[derive(Debug, Default, PartialEq, Eq)]
struct ListArgs {
    flat: bool,
    project: Option<ProjectScope>,
}

#[derive(Debug, PartialEq, Eq)]
enum ProjectScope {
    Caller,
    Path(PathBuf),
}

fn parse_args(args: &[String]) -> Result<ListArgs, String> {
    let mut parsed = ListArgs::default();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--flat" => {
                parsed.flat = true;
                index += 1;
            }
            "--project" | "--workspace" => {
                if parsed.project.is_some() {
                    return Err("give --project or --workspace once".into());
                }
                match args.get(index + 1).filter(|value| !value.starts_with("--")) {
                    Some(path) => {
                        parsed.project = Some(ProjectScope::Path(PathBuf::from(path)));
                        index += 2;
                    }
                    None => {
                        parsed.project = Some(ProjectScope::Caller);
                        index += 1;
                    }
                }
            }
            other => return Err(format!("unknown option: {other}\n{USAGE}")),
        }
    }
    Ok(parsed)
}

pub(super) fn run(args: &[String]) -> std::io::Result<i32> {
    let parsed = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };
    let scope_root = match &parsed.project {
        None => None,
        Some(scope) => {
            let current_dir = std::env::current_dir()?;
            let directory = match scope {
                ProjectScope::Caller => current_dir.clone(),
                ProjectScope::Path(path) => current_dir.join(path),
            };
            match project_root(&directory) {
                Some(root) => Some(root),
                None => {
                    eprintln!("no project for {}", directory.display());
                    return Ok(2);
                }
            }
        }
    };

    let mut response = super::send_request(&Request {
        id: "cli:agent:list".into(),
        method: Method::AgentList(EmptyParams::default()),
    })?;
    if response.get("error").is_some() {
        return super::print_response(&response);
    }
    if let Some(root) = scope_root.as_deref() {
        if let Some(agents) = response["result"]["agents"].as_array_mut() {
            agents.retain(|agent| in_project(agent, root, project_root));
        }
    }
    if parsed.flat {
        return super::print_response(&response);
    }

    let workspaces = super::send_request(&Request {
        id: "cli:agent:list:workspaces".into(),
        method: Method::WorkspaceList(EmptyParams::default()),
    })?;
    if workspaces.get("error").is_some() {
        return super::print_response(&workspaces);
    }
    let empty = Vec::new();
    let groups = group_by_workspace(
        response["result"]["agents"].as_array().unwrap_or(&empty),
        workspaces["result"]["workspaces"]
            .as_array()
            .unwrap_or(&empty),
    );
    super::print_response(&json!({
        "id": response["id"],
        "result": {"type": "agent_list_by_workspace", "workspaces": groups},
    }))
}

fn group_by_workspace(agents: &[Value], workspaces: &[Value]) -> Vec<Value> {
    let mut groups: Vec<Value> = workspaces
        .iter()
        .map(|workspace| {
            json!({
                "workspace_id": workspace["workspace_id"],
                "label": workspace["label"],
                "agents": agents
                    .iter()
                    .filter(|agent| agent["workspace_id"] == workspace["workspace_id"])
                    .cloned()
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    let known: Vec<&Value> = workspaces
        .iter()
        .map(|workspace| &workspace["workspace_id"])
        .collect();
    for agent in agents {
        if known.contains(&&agent["workspace_id"]) {
            continue;
        }
        match groups
            .iter_mut()
            .find(|group| group["workspace_id"] == agent["workspace_id"])
        {
            Some(group) => {
                if let Some(members) = group["agents"].as_array_mut() {
                    members.push(agent.clone());
                }
            }
            None => groups.push(json!({
                "workspace_id": agent["workspace_id"],
                "label": Value::Null,
                "agents": [agent],
            })),
        }
    }
    groups.retain(|group| group["agents"].as_array().is_some_and(|a| !a.is_empty()));
    groups
}

/// Where the agent is working: its foreground process's directory, else the pane's.
fn agent_directory(agent: &Value) -> Option<&str> {
    ["foreground_cwd", "cwd"]
        .into_iter()
        .filter_map(|key| agent[key].as_str())
        .find(|directory| !directory.is_empty())
}

fn in_project(agent: &Value, root: &Path, resolve: impl Fn(&Path) -> Option<PathBuf>) -> bool {
    agent_directory(agent)
        .and_then(|directory| resolve(Path::new(directory)))
        .is_some_and(|agent_root| same_directory(root, &agent_root))
}

/// The project a directory belongs to: its repository's main checkout, so a linked worktree
/// counts as the checkout it was made from, or the directory itself outside a repository.
/// A directory that cannot be read belongs to no project.
fn project_root(directory: &Path) -> Option<PathBuf> {
    if !directory.is_dir() {
        return None;
    }
    project_root_with(directory, |directory| {
        crate::worktree::git_common_dir(directory, false)
            .map(|common_dir| crate::worktree::canonical_or_original(&common_dir))
            .and_then(|common_dir| common_dir.parent().map(Path::to_path_buf))
    })
}

fn project_root_with(
    directory: &Path,
    repository_root: impl Fn(&Path) -> Option<PathBuf>,
) -> Option<PathBuf> {
    Some(repository_root(directory).unwrap_or_else(|| directory.to_path_buf()))
}

fn same_directory(a: &Path, b: &Path) -> bool {
    let a = a.components().collect::<PathBuf>();
    let b = b.components().collect::<PathBuf>();
    a == b
        || crate::worktree::canonical_or_original(&a) == crate::worktree::canonical_or_original(&b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn stub_repository_root(directory: &Path) -> Option<PathBuf> {
        directory
            .starts_with("/repo")
            .then(|| PathBuf::from("/repo"))
    }

    fn stub_project_root(directory: &Path) -> Option<PathBuf> {
        project_root_with(directory, stub_repository_root)
    }

    #[test]
    fn project_scope_takes_the_worktrees_along() {
        assert_eq!(stub_project_root(Path::new("/repo")), Some("/repo".into()));
        assert_eq!(
            stub_project_root(Path::new("/repo/.worktrees/side")),
            Some("/repo".into())
        );
        assert_eq!(
            stub_project_root(Path::new("/elsewhere")),
            Some("/elsewhere".into())
        );

        let root = stub_project_root(Path::new("/repo/")).unwrap();
        let in_repo = |agent: Value| in_project(&agent, &root, stub_project_root);
        assert!(in_repo(json!({"cwd": "/repo"})));
        assert!(in_repo(json!({"cwd": "/repo/.worktrees/side"})));
        assert!(in_repo(
            json!({"cwd": "/repo", "foreground_cwd": "/repo/.worktrees/x"})
        ));
        assert!(!in_repo(json!({"cwd": "/elsewhere"})));
        assert!(!in_repo(json!({"pane_id": "w1:p1"})));
    }

    #[test]
    fn foreground_directory_decides_the_project() {
        let root = PathBuf::from("/repo");
        assert!(!in_project(
            &json!({"cwd": "/repo", "foreground_cwd": "/elsewhere"}),
            &root,
            stub_project_root
        ));
        assert!(in_project(
            &json!({"cwd": "/repo", "foreground_cwd": ""}),
            &root,
            stub_project_root
        ));
    }

    #[test]
    fn unreadable_directory_belongs_to_no_project() {
        let missing =
            std::env::temp_dir().join(format!("herdr-agent-list-missing-{}", std::process::id()));
        assert_eq!(project_root(&missing), None);
    }

    #[test]
    fn linked_worktree_resolves_to_its_main_checkout() {
        let base = std::env::temp_dir().join(format!(
            "herdr-agent-list-project-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        let repo = base.join("repo");
        let elsewhere = base.join("elsewhere");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .unwrap();
            assert!(status.status.success(), "{:?}", status);
        };
        git(&repo, &["init", "-q"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let side = repo.join(".worktrees").join("side");
        git(&repo, &["worktree", "add", "-q", side.to_str().unwrap()]);

        let root = project_root(&repo).unwrap();
        assert!(same_directory(&root, &repo));
        assert!(same_directory(&project_root(&side).unwrap(), &repo));
        assert!(same_directory(
            &project_root(&elsewhere).unwrap(),
            &elsewhere
        ));
        assert!(in_project(
            &json!({"cwd": side.to_str().unwrap()}),
            &root,
            project_root
        ));
        assert!(!in_project(
            &json!({"cwd": elsewhere.to_str().unwrap()}),
            &root,
            project_root
        ));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn groups_follow_workspace_order_and_skip_empty_workspaces() {
        let agents = [
            json!({"pane_id": "w2:p1", "workspace_id": "w2"}),
            json!({"pane_id": "w1:p1", "workspace_id": "w1"}),
            json!({"pane_id": "w1:p2", "workspace_id": "w1"}),
        ];
        let workspaces = [
            json!({"workspace_id": "w1", "label": "tropos"}),
            json!({"workspace_id": "w3", "label": "empty"}),
            json!({"workspace_id": "w2", "label": "henia"}),
        ];
        let groups = group_by_workspace(&agents, &workspaces);
        let summary: Vec<(String, usize)> = groups
            .iter()
            .map(|group| {
                (
                    group["label"].as_str().unwrap().to_string(),
                    group["agents"].as_array().unwrap().len(),
                )
            })
            .collect();
        assert_eq!(summary, [("tropos".into(), 2), ("henia".into(), 1)]);
    }

    #[test]
    fn parses_flat_and_optional_project_path() {
        assert_eq!(parse_args(&args(&[])).unwrap(), ListArgs::default());
        assert_eq!(
            parse_args(&args(&["--project"])).unwrap().project,
            Some(ProjectScope::Caller)
        );
        assert_eq!(
            parse_args(&args(&["--workspace", "../repo", "--flat"])).unwrap(),
            ListArgs {
                flat: true,
                project: Some(ProjectScope::Path("../repo".into())),
            }
        );
        assert_eq!(
            parse_args(&args(&["--project", "--flat"])).unwrap(),
            ListArgs {
                flat: true,
                project: Some(ProjectScope::Caller),
            }
        );
        assert!(parse_args(&args(&["--project", "--workspace"])).is_err());
        assert!(parse_args(&args(&["--json"])).is_err());
    }
}
