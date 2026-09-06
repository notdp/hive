//! `hive workflow run` and `hive workflow done`: the two ends of one
//! workflow node. `run` is the seam the `hive-node` plugin agent runs in
//! the background from a Claude Code Workflow script; `done` is the
//! member's return statement.

use super::util::{fail, ok_or_fail, resolve_artifact_path};
use crate::identity;
use crate::team::{load_team, resolve_workspace};

/// The task is stdin (no shell quoting to get wrong), progress goes to
/// stderr, the single JSON result to stdout. Exit 1 with the error on
/// stderr only when the run never reached a dispatch (bad team, spawn or
/// ready failure, the dispatch refused); every end past that is a verdict
/// in the JSON line and exit 0.
pub(crate) fn run_cmd(name: &str, cli: Option<&str>, model: &str, team: Option<&str>) {
    let mut task = String::new();
    if std::io::Read::read_to_string(&mut std::io::stdin(), &mut task).is_err()
        || task.trim().is_empty()
    {
        fail("workflow run reads the task from stdin — pipe or heredoc the task text");
    }
    let env = crate::workflow::RealEnv::for_team(team.map(str::to_string));
    let spec = crate::workflow::WorkflowSpec {
        name: name.to_string(),
        cli: cli.map(str::to_string),
        model: model.to_string(),
        task: task.trim_end().to_string(),
    };
    match crate::workflow::run_workflow(&env, &spec) {
        Ok(result) => println!("{}", serde_json::Value::Object(result)),
        Err(e) => fail(&e.0),
    }
}

/// The calling process is the member: its identity comes from the same
/// ladder `hive send` signs with (pane tags, then the engine's own session
/// row), never from an argument. The summary is a return value, not a
/// message: it may be as long as the task needs, and `--artifact -` reads
/// the rest from stdin like a send.
pub(crate) fn done_cmd(summary: &str, artifact: &str) {
    if summary.trim().is_empty() {
        fail("workflow done takes the summary as its argument: hive workflow done \"<summary>\"");
    }
    let (Some(team_name), Some(name)) = (identity::default_team(), identity::default_agent())
    else {
        fail(
            "cannot resolve own member identity: `hive workflow done` is a team \
             member's return, run from the member's own pane or engine",
        );
    };
    let prefer_pane = identity::current_pane_id().unwrap_or_default();
    let team = ok_or_fail(load_team(&team_name, &prefer_pane));
    let ws = ok_or_fail(resolve_workspace(Some(&team), true));
    let artifact = resolve_artifact_path(artifact, &ws);
    if let Err(e) = crate::workflow::record_done(&ws, &name, summary, &artifact) {
        fail(&e.0);
    }
    // A return prints nothing, like a send.
}
