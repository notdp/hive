// pending port (wave 3)
pub fn main() {
    eprintln!("hive rs: cli not ported yet");
    std::process::exit(2);
}

// ---- wave-3 seams flow.rs already links against (stub signatures) ----

use std::collections::HashSet;
use std::path::Path;

pub fn resolve_scoped_team(
    _team: Option<&str>,
    _required: bool,
) -> anyhow::Result<(Option<String>, Option<crate::team::Team>)> {
    anyhow::bail!("cli not ported yet")
}

pub fn resolve_workspace(
    _team: Option<&crate::team::Team>,
    _required: bool,
) -> anyhow::Result<String> {
    anyhow::bail!("cli not ported yet")
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_team_agent<'a>(
    _team: &'a mut crate::team::Team,
    _team_name: &str,
    _agent_name: &str,
    _model: &str,
    _prompt: &str,
    _cwd: &str,
    _skill: &str,
    _extra_env: &[(String, String)],
    _cli_name: Option<&str>,
) -> anyhow::Result<&'a crate::agent::Agent> {
    anyhow::bail!("cli not ported yet")
}

pub fn ensure_team_hived(_team: &crate::team::Team, _workspace: &Path) {}

pub fn wait_for_peer_ready(
    _workspace: &str,
    _team_name: &str,
    _agents: &HashSet<String>,
    _timeout: f64,
    _interval: f64,
) -> HashSet<String> {
    HashSet::new()
}

#[allow(clippy::too_many_arguments)]
pub fn request_send_payload(
    _workspace: &str,
    _team: &crate::team::Team,
    _sender: &str,
    _target: &str,
    _body: &str,
    _artifact: &str,
    _reply_to: &str,
    _command_name: &str,
    _warn_on_long_body: bool,
) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    anyhow::bail!("cli not ported yet")
}
