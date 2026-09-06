//! `hive node run`: one node as one blocking command — the seam the
//! `hive-node` plugin agent runs in the background from a Claude Code
//! Workflow script.

use super::util::fail;

/// The task is stdin (no shell quoting to get wrong), progress goes to
/// stderr, the single JSON result to stdout.
pub(crate) fn node_run_cmd(name: &str, cli: Option<&str>, model: &str, team: Option<&str>) {
    let mut task = String::new();
    if std::io::Read::read_to_string(&mut std::io::stdin(), &mut task).is_err()
        || task.trim().is_empty()
    {
        fail("node run reads the task from stdin — pipe or heredoc the task text");
    }
    let env = crate::flow::RealEnv::for_team(team.map(str::to_string));
    let spec = crate::flow::NodeSpec {
        name: name.to_string(),
        cli: cli.map(str::to_string),
        model: model.to_string(),
        task: task.trim_end().to_string(),
    };
    match crate::flow::run_node(&env, &spec) {
        Ok(result) => println!("{}", serde_json::Value::Object(result)),
        Err(e) => fail(&e.0),
    }
}
