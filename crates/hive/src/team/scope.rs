//! Which team a verb acts on, and the workspace and hived that come with
//! it: explicit `-t`, else the caller's binding; the current-context file;
//! the team's window identity for the hived; the `hive team` payload.

use std::path::Path;

use anyhow::{anyhow, bail, Result};
use serde_json::{Map, Value};

use super::{created_at_key, Team};
use crate::hived::{same_created, same_workspace};
use crate::identity;
use crate::json_fields::map_str;
use crate::paths::getcwd;
use crate::tmux;

pub(crate) fn load_team(team: &str, prefer_pane: &str) -> Result<Team> {
    Team::load(team, prefer_pane).map_err(|_| anyhow!("team '{team}' not found"))
}

/// Addressing order: explicit team -> binding discovery (pane tags, then the
/// engine's own session row). An explicit team is the caller's intent — it
/// loads from the registry wherever the caller happens to be.
pub(crate) fn resolve_scoped_team(
    team: Option<&str>,
    required: bool,
) -> Result<(Option<String>, Option<Team>)> {
    if let Some(team) = team.filter(|t| !t.is_empty()) {
        let loaded = load_team(team, "")?;
        return Ok((Some(team.to_string()), Some(loaded)));
    }
    if let Some(discovered) = identity::default_team() {
        let prefer_pane = identity::current_pane_id().unwrap_or_default();
        let loaded = load_team(&discovered, &prefer_pane)?;
        return Ok((Some(discovered), Some(loaded)));
    }
    if required {
        bail!(
            "no Hive team in scope — pass -t <team> (see `hive ls`), or run \
             from a bound pane (`hive create` binds one)"
        );
    }
    Ok((None, None))
}

pub(crate) fn resolve_workspace(team: Option<&Team>, required: bool) -> Result<String> {
    if let Some(t) = team {
        if !t.workspace.is_empty() {
            return Ok(t.workspace.clone());
        }
    }
    let ctx = crate::context::load_current_context();
    if let Some(ws) = ctx.get("workspace").filter(|w| !w.is_empty()) {
        return Ok(ws.clone());
    }
    if required {
        bail!("workspace not found (create a team with --workspace, or run `hive create`)");
    }
    Ok(String::new())
}

pub(crate) fn ensure_pane_in_scope(t: &Team, pane_id: &str) -> Result<()> {
    if pane_id.is_empty() {
        return Ok(());
    }
    let pane_window = tmux::get_pane_window_target(pane_id).unwrap_or_default();
    let team_window = t.tmux_window.clone();
    if !team_window.is_empty() && !pane_window.is_empty() && pane_window != team_window {
        bail!(
            "pane '{pane_id}' is in tmux window '{pane_window}', not team '{}' window '{team_window}'",
            t.name
        );
    }
    if let Some(pane_team) = tmux::get_pane_option(pane_id, "hive-team") {
        if !pane_team.is_empty() && pane_team != t.name {
            bail!("pane '{pane_id}' already belongs to team '{pane_team}'");
        }
    }
    Ok(())
}

pub(crate) fn add_runtime_location_fields(payload: &mut Map<String, Value>) {
    if !payload.contains_key("runtimeWorkspace") && payload.contains_key("workspace") {
        if let Some(ws) = payload.shift_remove("workspace") {
            payload.insert("runtimeWorkspace".to_string(), ws);
        }
    }
    payload.insert("cwd".to_string(), Value::String(getcwd()));
}

pub(crate) fn remember_context(team: &str, workspace: &str, agent: &str) {
    let current = crate::context::load_current_context();
    let get = |key: &str| current.get(key).cloned().unwrap_or_default();
    let team = if team.is_empty() {
        get("team")
    } else {
        team.to_string()
    };
    let workspace = if workspace.is_empty() {
        get("workspace")
    } else {
        workspace.to_string()
    };
    let agent = if agent.is_empty() {
        get("agent")
    } else {
        agent.to_string()
    };
    let _ = crate::context::save_current_context(&team, &workspace, &agent);
}

/// The window the team's hived is told about: one that carries this
/// instance's full tags — team, workspace and `createdAt` — and nothing
/// else. The window the team loaded with is checked first, then the
/// caller's own window (`hive create` inside tmux has just tagged it); a
/// caller's window that is another team's, or a same-name window of
/// another instance, is not this team's display, and a team with no
/// display passes none rather than borrow one. Whatever matches fills the
/// team's empty window fields.
fn team_window_identity(t: &mut Team) -> (String, String) {
    let created = created_at_key(t.created_at);
    let owned = |tags: &tmux::WindowInstanceTags| {
        tags.team == t.name
            && same_workspace(&tags.workspace, &t.workspace)
            && same_created(&tags.created, &created)
    };
    // The loaded window came with its tags: no second question to tmux.
    if !t.tmux_window.is_empty() && !t.tmux_window_id.is_empty() {
        let loaded = tmux::WindowInstanceTags {
            window_id: t.tmux_window_id.clone(),
            team: t.name.clone(),
            workspace: t.window_workspace.clone(),
            created: t.window_created.clone(),
        };
        if owned(&loaded) {
            return (t.tmux_window.clone(), t.tmux_window_id.clone());
        }
    }
    let current = identity::current_window_target().unwrap_or_default();
    if current.is_empty() {
        return (String::new(), String::new());
    }
    let Some(tags) = tmux::window_instance_tags(&current).filter(owned) else {
        return (String::new(), String::new());
    };
    if t.tmux_window.is_empty() {
        t.tmux_window = current.clone();
    }
    if t.tmux_window_id.is_empty() {
        t.tmux_window_id = tags.window_id.clone();
    }
    (current, tags.window_id)
}

/// Start (or find) the team's hived, filling the team's window identity.
/// The error is a hived this hive must not touch (`hived::ensure_hived`).
pub(crate) fn start_team_hived(t: &mut Team, workspace: &str) -> Result<Option<i32>> {
    let (window_target, window_id) = team_window_identity(t);
    crate::hived::ensure_hived(workspace, &t.name, &window_target, &window_id)
}

/// `start_team_hived` for a verb that goes on without the hived: the
/// refusal is reported on stderr, and the request it was needed for
/// fails on its own.
pub(crate) fn start_team_hived_or_warn(t: &mut Team, workspace: &str) {
    if let Err(err) = start_team_hived(t, workspace) {
        eprintln!("warning: {err}");
    }
}

/// Seam used by send.rs and workflow.rs (team not mutated).
pub(crate) fn ensure_team_hived(t: &Team, workspace: &Path) -> Result<()> {
    let mut clone = t.clone();
    start_team_hived(&mut clone, &workspace.to_string_lossy()).map(|_| ())
}

fn augment_team_payload_with_runtime(
    t: &mut Team,
    mut payload: Map<String, Value>,
) -> Map<String, Value> {
    let ws = resolve_workspace(Some(&*t), false).unwrap_or_default();
    if ws.is_empty() {
        return payload;
    }
    start_team_hived_or_warn(t, &ws);
    let Some(runtime) = super::usable_runtime(crate::hived::request_team_runtime(&ws, &t.name))
    else {
        return payload;
    };
    let members_runtime = match runtime.get("members").and_then(Value::as_object) {
        Some(m) => m.clone(),
        None => return payload,
    };
    if let Some(Value::Array(members)) = payload.get_mut("members") {
        for member in members.iter_mut() {
            let member = match member.as_object_mut() {
                Some(m) => m,
                None => continue,
            };
            let name = map_str(member, "name");
            let runtime_fields = match members_runtime.get(&name).and_then(Value::as_object) {
                Some(f) => f,
                None => continue,
            };
            for key in [
                "alive",
                "cliAlive",
                "busy",
                "model",
                "sessionId",
                "inputState",
                "inputReason",
                "busySource",
                "hookEvent",
            ] {
                match runtime_fields.get(key) {
                    None | Some(Value::Null) => continue,
                    Some(Value::String(s)) if s.is_empty() => continue,
                    Some(value) => {
                        member.insert(key.to_string(), value.clone());
                    }
                }
            }
        }
    }
    if let Some(Value::Array(needs_answer)) = runtime.get("needsAnswer") {
        if !needs_answer.is_empty() {
            payload.insert(
                "needsAnswer".to_string(),
                Value::Array(needs_answer.clone()),
            );
        }
    }
    payload
}

fn should_show_description(desc: Option<&Value>) -> bool {
    match desc {
        Some(Value::String(s)) if !s.is_empty() => !s.starts_with("auto-init from "),
        _ => false,
    }
}

pub(crate) fn team_status_payload(t: &mut Team) -> Map<String, Value> {
    let status = t.status();
    let mut payload = augment_team_payload_with_runtime(t, status);
    if !should_show_description(payload.get("description")) {
        payload.shift_remove("description");
    }
    let me = identity::self_member_for_team(&t.name);
    if !me.is_empty() {
        payload.insert("self".to_string(), Value::String(me));
    }
    add_runtime_location_fields(&mut payload);
    payload
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::hived::testhook;
    use crate::testkit::display_env;

    /// A tmux whose caller pane `%0` sits in `other:1`, answering the
    /// instance tags of any window from *tags*: `target → (window_id,
    /// team, workspace, created)`.
    type Tags = Rc<RefCell<HashMap<String, (String, String, String, String)>>>;

    fn tmux_with_windows() -> Tags {
        let tags: Tags = Rc::new(RefCell::new(HashMap::new()));
        let table = Rc::clone(&tags);
        crate::tmux::set_run_override(move |args, _check, _timeout| {
            assert_eq!(args[0], "display-message", "{args:?}");
            let target = args[2].as_str();
            let fmt = args[4].as_str();
            let stdout = if fmt == "#{session_name}:#{window_index}" {
                assert_eq!(target, "%0");
                "other:1\n".to_string()
            } else {
                assert!(fmt.starts_with("#{window_id}\t"), "{fmt}");
                match table.borrow().get(target) {
                    Some((id, team, ws, created)) => {
                        format!("{id}\t{team}\t{ws}\t{created}\n")
                    }
                    None => "\t\t\t\n".to_string(),
                }
            };
            Ok(crate::tmux::Run {
                returncode: 0,
                stdout,
                stderr: String::new(),
            })
        });
        tags
    }

    fn probe_team(workspace: &str) -> Team {
        Team {
            name: "probe".to_string(),
            workspace: workspace.to_string(),
            created_at: 123.0,
            ..Default::default()
        }
    }

    #[test]
    fn test_windowless_team_does_not_borrow_callers_window() {
        let env = display_env();
        let ws = env._tmp.path().join("ws").to_string_lossy().into_owned();
        let tags = tmux_with_windows();
        let tag = |target: &str, id: &str, team: &str, ws: &str, created: &str| {
            tags.borrow_mut().insert(
                target.to_string(),
                (
                    id.to_string(),
                    team.to_string(),
                    ws.to_string(),
                    created.to_string(),
                ),
            );
        };

        // The caller sits in another team's window: the target team has no
        // display and is told of none.
        tag("other:1", "@5", "fern", "/ws/fern", "100");
        let mut t = probe_team(&ws);
        assert_eq!(team_window_identity(&mut t), (String::new(), String::new()));
        assert_eq!(t.tmux_window, "");
        assert_eq!(t.tmux_window_id, "");

        // The same name alone is not this instance: the workspace or the
        // createdAt of another instance, or no createdAt at all.
        for (other_ws, created) in [
            ("/ws/elsewhere", "123"),
            (ws.as_str(), "124"),
            (ws.as_str(), ""),
            ("", "123"),
        ] {
            tag("other:1", "@5", "probe", other_ws, created);
            let mut t = probe_team(&ws);
            assert_eq!(
                team_window_identity(&mut t),
                (String::new(), String::new()),
                "workspace {other_ws:?} created {created:?}"
            );
            assert_eq!(t.tmux_window, "");
            assert_eq!(t.tmux_window_id, "");
        }

        // The caller's window carrying this instance's full tags (create
        // has just written them) is the display, and fills the team in.
        tag("other:1", "@5", "probe", &ws, "123");
        let mut t = probe_team(&ws);
        assert_eq!(
            team_window_identity(&mut t),
            ("other:1".to_string(), "@5".to_string())
        );
        assert_eq!(t.tmux_window, "other:1");
        assert_eq!(t.tmux_window_id, "@5");

        // A same-name window the team loaded with (its tags read with
        // it), of an earlier instance, yields to the caller's window that
        // is this instance's — and is not overwritten by it.
        let mut t = probe_team(&ws);
        t.tmux_window = "probe:1".to_string();
        t.tmux_window_id = "@2".to_string();
        t.window_workspace = ws.clone();
        t.window_created = "99".to_string();
        assert_eq!(
            team_window_identity(&mut t),
            ("other:1".to_string(), "@5".to_string())
        );
        assert_eq!(t.tmux_window, "probe:1");
        assert_eq!(t.tmux_window_id, "@2");

        // The loaded window that is this instance's wins over the caller's,
        // without asking tmux again.
        let mut t = probe_team(&ws);
        t.tmux_window = "probe:1".to_string();
        t.tmux_window_id = "@2".to_string();
        t.window_workspace = ws.clone();
        t.window_created = "123.0".to_string();
        crate::tmux::set_run_override(|args, _, _| panic!("tmux asked: {args:?}"));
        assert_eq!(
            team_window_identity(&mut t),
            ("probe:1".to_string(), "@2".to_string())
        );
    }

    #[test]
    fn test_windowless_team_starts_its_hived_with_no_window() {
        let env = display_env();
        let ws = env._tmp.path().join("ws").to_string_lossy().into_owned();
        std::fs::create_dir_all(&ws).unwrap();
        let tags = tmux_with_windows();
        tags.borrow_mut().insert(
            "other:1".to_string(),
            (
                "@5".to_string(),
                "fern".to_string(),
                "/ws/fern".to_string(),
                "100".to_string(),
            ),
        );
        let spawned: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let spawns = Arc::clone(&spawned);
        let started = Arc::new(AtomicUsize::new(0));
        let ping_started = Arc::clone(&started);
        let identity = {
            let mut m = Map::new();
            m.insert("ok".to_string(), Value::Bool(true));
            m.insert(
                "apiVersion".to_string(),
                Value::from(crate::hived::HIVED_API_VERSION),
            );
            m.insert(
                "buildHash".to_string(),
                Value::from(crate::hived::hived_build_hash()),
            );
            m.insert("team".to_string(), Value::from("probe"));
            m
        };
        let _hook = testhook::install(testhook::Hook {
            request_ping: Some(Arc::new(move |_ws, _timeout| {
                (ping_started.load(Ordering::SeqCst) > 0).then(|| identity.clone())
            })),
            cleanup_socket: Some(Arc::new(|_ws| {})),
            popen: Some(Arc::new(move |argv, _stderr| {
                spawns.lock().unwrap().push(argv.to_vec());
                started.fetch_add(1, Ordering::SeqCst);
                4242
            })),
            ..Default::default()
        });

        let mut t = probe_team(&ws);
        assert_eq!(start_team_hived(&mut t, &ws).unwrap(), Some(4242));
        let spawned = spawned.lock().unwrap();
        assert_eq!(spawned.len(), 1);
        assert_eq!(
            spawned[0][1..],
            [
                "--hived".to_string(),
                ws.clone(),
                "probe".to_string(),
                String::new(),
                String::new()
            ]
        );
        assert_eq!(t.tmux_window, "");
        assert_eq!(t.tmux_window_id, "");
    }

    #[test]
    fn test_should_show_description_filters_auto_init() {
        assert!(!should_show_description(None));
        assert!(!should_show_description(Some(
            &Value::String(String::new())
        )));
        assert!(!should_show_description(Some(&Value::String(
            "auto-init from tmux dev (dev:1)".to_string()
        ))));
        assert!(should_show_description(Some(&Value::String(
            "real description".to_string()
        ))));
    }
}
