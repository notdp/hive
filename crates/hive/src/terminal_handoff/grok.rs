//! The grok side of a terminal handoff: a launch leader (`l-<id>`) keeps
//! serving the session; binding aliases the member to it
//! (`grok_leader::handoff`), and the team pane's TUI reaches the leader
//! through the pane's member tags the way any grok member pane does.

use anyhow::{anyhow, Result};
use serde_json::Value;

use crate::adapters::grok_leader;

use super::session::Session;
use super::Target;

/// The launch key a grok session rides on, from its `data`.
pub(crate) fn launch_key(session: &Session) -> Result<String> {
    session
        .data
        .get("launchKey")
        .and_then(Value::as_str)
        .filter(|key| grok_leader::is_launch_key(key))
        .map(str::to_string)
        .ok_or_else(|| anyhow!("grok handoff session names no launch key"))
}

/// The local TUI on the launch leader, resumed on the session hive minted.
pub(crate) fn resume_args(session: &Session) -> Vec<String> {
    let key = launch_key(session).unwrap_or_default();
    vec![
        "--leader".into(),
        "--leader-socket".into(),
        grok_leader::socket_path_for_key(&key)
            .to_string_lossy()
            .into_owned(),
        "--resume".into(),
        session.id.clone(),
    ]
}

pub(crate) fn bind(session: &Session, target: &Target) -> Result<()> {
    grok_leader::bind_launch(
        &launch_key(session)?,
        &session.id,
        &session.cwd,
        &target.team,
        &target.member,
        &target.pane,
    )
}

pub(crate) fn binding_matches(session: &Session, target: &Target) -> bool {
    launch_key(session).is_ok_and(|key| {
        grok_leader::launch_is_bound(
            &key,
            &session.id,
            &target.team,
            &target.member,
            &target.pane,
        )
    })
}

pub(crate) fn clear_binding(session: &Session, target: &Target) {
    if let Ok(key) = launch_key(session) {
        let _ = grok_leader::rollback_launch(
            &key,
            &session.id,
            &target.team,
            &target.member,
            &target.pane,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn session(data: Value) -> Session {
        Session {
            cli: "grok",
            id: "sid-1".into(),
            cwd: "/w".into(),
            data,
        }
    }

    #[test]
    fn test_grok_session_carries_its_launch_key_in_data() {
        let _env = crate::testenv::EnvGuard::new();
        let s = Session::grok("l-ab12", "sid-1", "/w");
        assert_eq!(launch_key(&s).unwrap(), "l-ab12");
        assert!(launch_key(&session(json!({}))).is_err());
        assert!(launch_key(&session(json!({"launchKey": "p7"}))).is_err());
        let round = Session::parse(&s.json()).unwrap();
        assert_eq!((round.cli, round.id.as_str()), ("grok", "sid-1"));
        assert_eq!(launch_key(&round).unwrap(), "l-ab12");
    }

    #[test]
    fn test_grok_resume_args_target_the_launch_leader_and_team_viewer_resumes_by_id() {
        let mut env = crate::testenv::EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        env.set("GROK_HOME", tmp.path());
        let s = Session::grok("l-ab12", "sid-1", "/w");
        let sock = tmp.path().join("hive").join("l-ab12.sock");
        assert_eq!(
            s.resume_args(),
            vec![
                "--leader",
                "--leader-socket",
                sock.to_str().unwrap(),
                "--resume",
                "sid-1"
            ]
        );
        assert_eq!(s.team_viewer(), "grok --resume sid-1");
    }
}
