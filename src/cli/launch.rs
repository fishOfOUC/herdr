//! Resolve an explicit launch directory in the caller and open it through the JSON API.

use std::io;
use std::path::{Path, PathBuf};

use crate::api::client::{ApiClient, ApiClientError};
use crate::api::schema::{
    EmptyParams, Method, Request, ResponseResult, SessionSnapshot, TabCreateParams,
    WorkspaceCreateParams, WorkspaceInfo,
};

pub(crate) fn directory_argument(args: &[String]) -> Result<Option<PathBuf>, String> {
    let args = if args.first().is_some_and(|arg| arg == "--") {
        &args[1..]
    } else if args.first().is_none_or(|arg| arg.starts_with('-')) {
        return Ok(None);
    } else {
        args
    };
    let [directory] = args else {
        return Err("usage: herdr [--session <name>] [directory]".into());
    };
    if directory.is_empty() {
        return Err("directory must not be empty".into());
    }
    let path = std::path::absolute(directory)
        .map_err(|err| format!("cannot resolve directory {directory:?}: {err}"))?;
    let metadata = path
        .metadata()
        .map_err(|err| format!("cannot open directory {directory:?}: {err}"))?;
    if !metadata.is_dir() {
        return Err(format!("not a directory: {directory:?}"));
    }
    Ok(Some(crate::platform::normalize_cwd_for_launch(&path)))
}

pub(crate) fn open_directory(directory: &Path) -> io::Result<()> {
    let client = ApiClient::local();
    open_directory_with(directory, |request| {
        client.request(request).map(|response| response.result)
    })
    .map_err(io::Error::other)
}

fn open_directory_with(
    directory: &Path,
    mut send: impl FnMut(Request) -> Result<ResponseResult, ApiClientError>,
) -> Result<(), ApiClientError> {
    let response = send(Request {
        id: "cli:launch:snapshot".into(),
        method: Method::SessionSnapshot(EmptyParams::default()),
    })?;
    let ResponseResult::SessionSnapshot { snapshot } = response else {
        return Err(ApiClientError::UnexpectedResult(
            "expected a session snapshot before opening a directory".into(),
        ));
    };
    let cwd = directory.to_string_lossy().into_owned();
    let method = if let Some(workspace_id) = matching_workspace(&snapshot, directory) {
        Method::TabCreate(TabCreateParams {
            workspace_id: Some(workspace_id),
            cwd: Some(cwd.clone()),
            focus: true,
            label: None,
            env: Default::default(),
        })
    } else {
        Method::WorkspaceCreate(WorkspaceCreateParams {
            source_workspace_id: None,
            cwd: Some(cwd),
            focus: true,
            label: None,
            env: Default::default(),
        })
    };
    // Send exactly one mutation. A timeout does not prove that creation failed.
    send(Request {
        id: "cli:launch:open".into(),
        method,
    })
    .map(|_| ())
}

fn matching_workspace(snapshot: &SessionSnapshot, directory: &Path) -> Option<String> {
    let directory = directory.canonicalize().ok()?;
    snapshot
        .workspaces
        .iter()
        .filter(|workspace| workspace_matches(snapshot, workspace, &directory))
        .min_by_key(|workspace| (!workspace.focused, workspace.number))
        .map(|workspace| workspace.workspace_id.clone())
}

fn workspace_matches(
    snapshot: &SessionSnapshot,
    workspace: &WorkspaceInfo,
    directory: &Path,
) -> bool {
    let same_directory = |candidate: &str| {
        Path::new(candidate)
            .canonicalize()
            .is_ok_and(|candidate| candidate == directory)
    };
    if workspace
        .worktree
        .as_ref()
        .is_some_and(|worktree| same_directory(&worktree.checkout_path))
    {
        return true;
    }
    // The first tab anchors Herdr's workspace identity. Agent tabs and foreground
    // process directories must not make an unrelated project match this workspace.
    let Some(first_tab) = snapshot
        .tabs
        .iter()
        .filter(|tab| tab.workspace_id == workspace.workspace_id)
        .min_by_key(|tab| tab.number)
    else {
        return false;
    };
    snapshot
        .panes
        .iter()
        .find(|pane| pane.workspace_id == workspace.workspace_id && pane.tab_id == first_tab.tab_id)
        .and_then(|pane| pane.cwd.as_deref())
        .is_some_and(same_directory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{ErrorBody, ErrorResponse};

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn launch_directory_resolves_current_parent_and_absolute_paths_in_caller() {
        for value in [".".into(), "..".into(), std::env::current_dir().unwrap()] {
            let resolved = directory_argument(&[value.to_string_lossy().into_owned()])
                .unwrap()
                .unwrap();
            assert!(resolved.is_absolute());
            assert_eq!(
                resolved.canonicalize().unwrap(),
                value.canonicalize().unwrap()
            );
        }
    }

    #[test]
    fn launch_directory_preserves_plain_launch_and_unknown_options() {
        assert_eq!(directory_argument(&[]).unwrap(), None);
        assert_eq!(directory_argument(&args(&["--unknown"])).unwrap(), None);
        assert!(directory_argument(&args(&["--", "."])).unwrap().is_some());
    }

    #[test]
    fn launch_directory_help_metadata_preserves_session_options_and_subcommands() {
        for values in [
            vec!["herdr", "."],
            vec!["herdr", "--session", "work", ".."],
            vec!["herdr", "--", "status"],
        ] {
            let matches = super::super::spec::command()
                .try_get_matches_from(values)
                .unwrap();
            assert!(matches.get_one::<String>("directory").is_some());
            assert_eq!(matches.subcommand_name(), None);
        }
        let matches = super::super::spec::command()
            .try_get_matches_from(["herdr", "status", "server"])
            .unwrap();
        assert_eq!(matches.subcommand_name(), Some("status"));
    }

    #[test]
    fn launch_directory_rejects_files_missing_paths_and_extra_arguments() {
        for values in [
            vec!["Cargo.toml"],
            vec![".herdr-nonexistent-launch-directory"],
            vec![""],
            vec![".", ".."],
            vec![".", "--unknown"],
            vec!["--"],
        ] {
            assert!(directory_argument(&args(&values)).is_err(), "{values:?}");
        }
    }

    #[test]
    fn launch_directory_accepts_spaces_unicode_and_the_option_terminator() {
        let directory =
            std::env::temp_dir().join(format!("herdr launch 中文 {}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let parsed = directory_argument(&args(&["--", directory.to_str().unwrap()]));
        std::fs::remove_dir(&directory).unwrap();
        assert_eq!(
            parsed.unwrap(),
            Some(crate::platform::normalize_cwd_for_launch(&directory))
        );
    }

    fn snapshot(projects: &[(&str, &Path, bool)]) -> SessionSnapshot {
        let mut workspaces = Vec::new();
        let mut tabs = Vec::new();
        let mut panes = Vec::new();
        for (index, (id, cwd, focused)) in projects.iter().enumerate() {
            let tab_id = format!("{id}:tab");
            workspaces.push(serde_json::json!({
                "workspace_id": id, "number": index + 1, "label": "same project name",
                "focused": focused, "pane_count": 1, "tab_count": 1,
                "active_tab_id": tab_id, "agent_status": "unknown"
            }));
            tabs.push(serde_json::json!({
                "tab_id": tab_id, "workspace_id": id, "number": 1, "label": "agent",
                "focused": focused, "pane_count": 1, "agent_status": "unknown"
            }));
            panes.push(serde_json::json!({
                "pane_id": format!("{id}:pane"), "terminal_id": format!("{id}:terminal"),
                "workspace_id": id, "tab_id": tab_id, "focused": focused,
                "cwd": cwd, "agent_status": "unknown", "revision": 0
            }));
        }
        serde_json::from_value(serde_json::json!({
            "version": "test", "protocol": 0, "workspaces": workspaces,
            "tabs": tabs, "panes": panes, "layouts": [], "agents": []
        }))
        .unwrap()
    }

    fn open_with_snapshot(
        directory: &Path,
        snapshot: SessionSnapshot,
        mut mutate: impl FnMut(Method) -> Result<ResponseResult, ApiClientError>,
    ) -> Result<(), ApiClientError> {
        open_directory_with(directory, |request| match request.method {
            Method::SessionSnapshot(_) => Ok(ResponseResult::SessionSnapshot {
                snapshot: Box::new(snapshot.clone()),
            }),
            method => mutate(method),
        })
    }

    fn success() -> ResponseResult {
        ResponseResult::WorkspaceList { workspaces: vec![] }
    }

    #[test]
    fn launch_directory_creates_a_workspace_for_a_new_project() {
        let directory = std::env::current_dir().unwrap();
        for projects in [
            snapshot(&[]),
            snapshot(&[("unrelated", directory.parent().unwrap(), true)]),
        ] {
            let mut calls = 0;
            open_with_snapshot(&directory, projects, |method| {
                calls += 1;
                let Method::WorkspaceCreate(params) = method else {
                    panic!("a new project needs its own workspace");
                };
                assert_eq!(params.cwd.as_deref(), directory.to_str());
                assert!(params.focus);
                Ok(success())
            })
            .unwrap();
            assert_eq!(calls, 1);
        }
    }

    #[test]
    fn launch_directory_opens_a_tab_in_the_matching_inactive_workspace() {
        let directory = std::env::current_dir().unwrap();
        let projects = snapshot(&[
            ("active-other-project", directory.parent().unwrap(), true),
            ("target-project", &directory, false),
        ]);
        open_with_snapshot(&directory, projects, |method| {
            let Method::TabCreate(params) = method else {
                panic!("expected tab.create")
            };
            assert_eq!(params.workspace_id.as_deref(), Some("target-project"));
            assert_eq!(params.cwd.as_deref(), directory.to_str());
            assert!(params.focus);
            Ok(success())
        })
        .unwrap();
    }

    #[test]
    fn launch_directory_matches_equivalent_paths_and_prefers_a_focused_duplicate() {
        let directory = std::env::current_dir().unwrap();
        let projects = snapshot(&[("first", &directory, false), ("focused", &directory, true)]);
        assert_eq!(
            matching_workspace(&projects, &directory.join("src/..")),
            Some("focused".into())
        );
    }

    #[test]
    fn launch_directory_uses_checkout_identity_even_when_the_first_shell_moved() {
        let directory = std::env::current_dir().unwrap();
        let mut projects = snapshot(&[("project", directory.parent().unwrap(), false)]);
        projects.workspaces[0].worktree = Some(crate::api::schema::WorkspaceWorktreeInfo {
            repo_key: "repo".into(),
            repo_name: "project".into(),
            repo_root: directory.to_string_lossy().into_owned(),
            checkout_path: directory.to_string_lossy().into_owned(),
            is_linked_worktree: false,
        });
        assert_eq!(
            matching_workspace(&projects, &directory),
            Some("project".into())
        );
    }

    #[test]
    fn launch_directory_does_not_match_an_agent_tab_from_another_project() {
        let directory = std::env::current_dir().unwrap();
        let mut projects = snapshot(&[("other", directory.parent().unwrap(), false)]);
        let extra = snapshot(&[("agent", &directory, false)]);
        let mut tab = extra.tabs[0].clone();
        tab.workspace_id = "other".into();
        tab.number = 2;
        let mut pane = extra.panes[0].clone();
        pane.workspace_id = "other".into();
        projects.tabs.push(tab);
        projects.panes.push(pane);
        assert_eq!(matching_workspace(&projects, &directory), None);
    }

    fn rejection(code: &str) -> ApiClientError {
        ApiClientError::ErrorResponse(ErrorResponse {
            id: "cli:launch:open".into(),
            error: ErrorBody {
                code: code.into(),
                message: "rejected".into(),
            },
        })
    }

    #[test]
    fn launch_directory_does_not_retry_rejections_or_unknown_transport_outcomes() {
        let directory = std::env::current_dir().unwrap();
        for error in [
            rejection("unknown_method"),
            rejection("workspace_not_found"),
            rejection("permission_denied"),
            ApiClientError::Io(io::ErrorKind::TimedOut.into()),
        ] {
            let mut error = Some(error);
            let mut calls = 0;
            assert!(open_with_snapshot(
                &directory,
                snapshot(&[("project", &directory, true)]),
                |_| {
                    calls += 1;
                    Err(error.take().expect("must not retry"))
                }
            )
            .is_err());
            assert_eq!(calls, 1);
        }
    }

    #[test]
    fn launch_directory_does_not_mutate_without_a_valid_snapshot() {
        for response in [Ok(success()), Err(rejection("unknown_method"))] {
            let mut response = Some(response);
            let mut calls = 0;
            assert!(open_directory_with(Path::new("/project"), |_| {
                calls += 1;
                response
                    .take()
                    .expect("must not mutate after snapshot failure")
            })
            .is_err());
            assert_eq!(calls, 1);
        }
    }
}
