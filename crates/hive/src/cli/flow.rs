//! Flow verbs: `flow run` (the embedded JS engine), `flow node run` (one
//! node as a blocking command), and the `--task` workspace preflight a
//! spawn dispatch needs.

use anyhow::Result;

use super::util::fail;
use crate::team::{resolve_workspace, Team};

/// Workspace the `--task` dispatch will ride, or None without `--task`.
///
/// Split out so the requirement is checked before the spawn: the dispatch
/// needs a workspace, and discovering that after the member is registered
/// and its engine minted leaves a half-born member on the roster.
pub(crate) fn task_dispatch_workspace(
    t: &Team,
    task_artifact: Option<&str>,
) -> Result<Option<String>> {
    match task_artifact {
        Some(_) => resolve_workspace(Some(t), true).map(Some),
        None => Ok(None),
    }
}

/// Flow scripts are trusted JavaScript (you or your orch wrote them),
/// evaluated by the embedded engine in `crate::flow_script` — no external
/// interpreter, no materialized client.
pub(crate) fn flow_run_cmd(script: &str, resume: Option<&str>) {
    let script_path = std::fs::canonicalize(script)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| script.to_string());
    std::process::exit(crate::flow_script::run_cmd(&script_path, resume));
}

/// `hive flow node run`: the task is stdin (no shell quoting to get wrong),
/// progress goes to stderr, the single JSON result to stdout.
pub(crate) fn flow_node_run_cmd(
    name: &str,
    cli: Option<&str>,
    model: &str,
    phase: &str,
    team: Option<&str>,
) {
    let mut task = String::new();
    if std::io::Read::read_to_string(&mut std::io::stdin(), &mut task).is_err()
        || task.trim().is_empty()
    {
        fail("flow node run reads the task from stdin — pipe or heredoc the task text");
    }
    let env = crate::flow::RealEnv::for_team(team.map(str::to_string));
    let spec = crate::flow::NodeSpec {
        name: name.to_string(),
        cli: cli.map(str::to_string),
        model: model.to_string(),
        phase: phase.to_string(),
        task: task.trim_end().to_string(),
    };
    match crate::flow::run_node(&env, &spec) {
        Ok(result) => println!("{}", serde_json::Value::Object(result)),
        Err(e) => fail(&e.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testenv::EnvGuard;

    #[test]
    fn test_task_dispatch_workspace_fails_before_the_spawn_when_none_resolves() {
        let mut env = EnvGuard::cleared(&crate::testenv::IDENTITY_VARS);
        let tmp = tempfile::TempDir::new().unwrap();
        env.set("HIVE_HOME", tmp.path().join(".hive"));
        let workspaceless = crate::team::Team {
            name: "hornet".to_string(),
            ..Default::default()
        };

        // no --task: nothing is required, nothing is resolved
        assert_eq!(task_dispatch_workspace(&workspaceless, None).unwrap(), None);

        let err = task_dispatch_workspace(&workspaceless, Some("/tmp/task.md"))
            .expect_err("a task dispatch with no workspace must refuse");
        assert!(err.to_string().contains("workspace not found"), "{err}");

        let with_workspace = crate::team::Team {
            name: "hornet".to_string(),
            workspace: "/tmp/ws-hn".to_string(),
            ..Default::default()
        };
        assert_eq!(
            task_dispatch_workspace(&with_workspace, Some("/tmp/task.md")).unwrap(),
            Some("/tmp/ws-hn".to_string())
        );
    }
}
