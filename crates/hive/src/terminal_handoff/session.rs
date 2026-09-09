//! Native engine identity and bindings behind the terminal handoff protocol.
use std::path::PathBuf;
use std::process::Command;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use crate::adapters::{claude_bg, claude_sessions, codex_app_server, grok_leader};
use crate::shell::shlex_quote;

use super::Target;

#[derive(Clone, Debug)]
pub(crate) struct Session {
    pub cli: &'static str,
    pub id: String,
    pub cwd: String,
    pub data: Value,
}

impl Session {
    pub(crate) fn claude(engine: claude_bg::EngineSession) -> Self {
        Self {
            cli: "claude",
            id: engine.job_id,
            cwd: engine.cwd,
            data: json!({"sessionId":engine.session_id}),
        }
    }

    /// A grok launch: the leader on *launch_key* serving *session_id*
    /// (`grok_leader::handoff`). The roster id is the session id, the
    /// launch key rides `data`.
    pub(crate) fn grok(launch_key: &str, session_id: &str, cwd: &str) -> Self {
        Self {
            cli: "grok",
            id: session_id.to_string(),
            cwd: cwd.to_string(),
            data: json!({"launchKey": launch_key}),
        }
    }

    pub(crate) fn control_dir(cli: &str) -> Result<PathBuf> {
        match cli {
            "claude" => Ok(claude_sessions::config_dir().join("hive-control")),
            "codex" => Ok(codex_app_server::codex_home().join("hive-control")),
            "grok" => Ok(grok_leader::grok_home().join("hive-control")),
            _ => bail!("unsupported terminal handoff engine: {cli}"),
        }
    }

    pub(crate) fn record_path(cli: &str, id: &str) -> Result<PathBuf> {
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            bail!("invalid {cli} handoff session id");
        }
        Ok(Self::control_dir(cli)?.join(format!("launch-{id}.json")))
    }

    pub(crate) fn json(&self) -> Value {
        json!({"cli":self.cli,"id":self.id,"cwd":self.cwd,"data":self.data})
    }

    pub(crate) fn parse(value: &Value) -> Result<Self> {
        let cli = match super::field(value, "cli")? {
            "claude" => "claude",
            "codex" => "codex",
            "grok" => "grok",
            other => bail!("unsupported terminal handoff engine: {other}"),
        };
        let id = super::field(value, "id")?.to_string();
        Self::record_path(cli, &id)?;
        Ok(Self {
            cli,
            id,
            cwd: super::field(value, "cwd")?.to_string(),
            data: value.get("data").cloned().unwrap_or(Value::Null),
        })
    }

    pub(crate) fn resume_args(&self) -> Vec<String> {
        match self.cli {
            "claude" => vec!["attach".into(), self.id.clone()],
            "codex" => vec![
                "--remote".into(),
                format!(
                    "unix://{}",
                    codex_app_server::shared_socket_path().display()
                ),
                "--cd".into(),
                self.cwd.clone(),
                "resume".into(),
                self.id.clone(),
            ],
            "grok" => super::grok::resume_args(self),
            _ => unreachable!("validated engine"),
        }
    }

    pub(crate) fn command(&self, args: &[String]) -> Command {
        let mut command = Command::new(self.cli);
        command.args(args).current_dir(&self.cwd);
        if self.cli == "claude" {
            command.env_clear().envs(claude_bg::bg_env(None));
        } else {
            command
                .env_clear()
                .envs(crate::adapters::base::washed_spawner_env(&["TMUX_PANE"]));
        }
        command
    }

    pub(crate) fn bind(&self, target: &Target) -> Result<()> {
        match self.cli {
            "claude" => Ok(claude_bg::write_pane_job(
                &target.pane,
                &self.id,
                self.data
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                &self.cwd,
            )?),
            "codex" => codex_app_server::write_pane_thread(
                &target.pane,
                &self.id,
                &self.cwd,
                crate::tmux::own_socket_path().as_deref(),
            ),
            "grok" => super::grok::bind(self, target),
            _ => Err(anyhow!("unsupported engine")),
        }
    }

    pub(crate) fn binding_matches(&self, target: &Target) -> bool {
        match self.cli {
            "claude" => claude_bg::job_id_for_pane(&target.pane).as_deref() == Some(&self.id),
            "codex" => {
                codex_app_server::thread_id_for_pane(&target.pane).as_deref() == Some(&self.id)
            }
            "grok" => super::grok::binding_matches(self, target),
            _ => false,
        }
    }

    pub(crate) fn clear_binding(&self, target: &Target) {
        if !self.binding_matches(target) {
            return;
        }
        match self.cli {
            "claude" => claude_bg::clear_pane_job(&target.pane),
            "codex" => {
                let _ = codex_app_server::clear_pane_thread(&target.pane);
            }
            "grok" => super::grok::clear_binding(self, target),
            _ => {}
        }
    }

    pub(crate) fn team_viewer(&self) -> String {
        let args = match self.cli {
            "claude" => vec!["claude", "--resume", &self.id],
            "codex" => vec!["codex", "resume", &self.id],
            "grok" => vec!["grok", "--resume", &self.id],
            _ => unreachable!("validated engine"),
        };
        args.iter()
            .map(|s| shlex_quote(s))
            .collect::<Vec<_>>()
            .join(" ")
    }
}
