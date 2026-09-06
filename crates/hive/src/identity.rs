//! Who this process is, and which pane it is on.
//!
//! The identity ladder, strongest rung first: the current pane's own tags,
//! the roster row keyed by the id the engine minted for itself and exports
//! to its own tool subprocesses (`CODEX_THREAD_ID`, `GROK_SESSION_ID`, a
//! Claude messaging socket), then the saved context file. This is the only
//! module that reads those engine markers; `tmux/` is display, takes explicit
//! targets, and reads neither markers nor the registry. Display is resolved
//! on top of identity here: a member engine's tools carry no usable
//! `TMUX_PANE`, so `current_pane_id` walks from the engine's own marker to
//! its pane before falling back to the env var.

use serde_json::{Map, Value};

use crate::json_fields::map_str;
use crate::team::LEAD_AGENT_NAME;
use crate::tmux;

/// Env an engine mints for its own subprocesses. A process carrying one of
/// these is an engine context: it is some member, or it is nobody — it is
/// never the human at the orch's keyboard.
const ENGINE_MARKER_ENV: [&str; 3] = [
    "CODEX_THREAD_ID",
    "GROK_SESSION_ID",
    "CLAUDE_CODE_MESSAGING_SOCKET",
];

pub(crate) fn env_string(key: &str) -> String {
    std::env::var(key).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Current pane and window
// ---------------------------------------------------------------------------

/// True inside a tmux client — or inside a member engine's tool subprocess.
///
/// A claude bg engine runs on the supervisor's pty, not in any tmux client,
/// so its tools see no reliable $TMUX; but the member's pane identity is
/// resolvable from the engine's own env markers, and the tmux server on the
/// default socket answers targeted commands without $TMUX. Gating on $TMUX
/// alone would lock every member out of hive.
pub(crate) fn is_inside_tmux() -> bool {
    if !env_string("TMUX").is_empty() {
        return true;
    }
    member_env_pane().is_some()
}

/// Pane resolved from a member engine's per-tool env markers, or None.
///
/// - codex injects the thread's `CODEX_THREAD_ID` into tool subprocesses;
///   hive records which pane each thread is bound to.
/// - a claude bg engine's tools carry `CLAUDE_CODE_MESSAGING_SOCKET`
///   (`/tmp/cc-socks/<enginePid>.sock`); the engine's registry entry names
///   its jobId, and hive records which pane each job is bound to. An
///   interactive claude session's tools carry the socket too, but have no
///   bg registry entry (and no job record), so they fall through.
/// - a grok member's leader exports `GROK_SESSION_ID` into its tools; that
///   id keys the member's grok roster row, and the member's pane is the one
///   tagged with that team and name on the default server. The leader
///   carries no `TMUX_PANE` (it is minted by identity before any pane
///   exists), so display is resolved from identity here, as for the other
///   two.
///
/// A member whose pane is gone (window closed, server restarted) resolves
/// nothing here; its identity is the registry row keyed by its sessionId —
/// the ladder's session rung (`session_member_binding`).
fn member_env_pane() -> Option<String> {
    let thread_id = env_string("CODEX_THREAD_ID").trim().to_string();
    if !thread_id.is_empty() {
        if let Some(pane) = crate::adapters::codex_app_server::pane_for_thread(&thread_id) {
            if !pane.is_empty() {
                return Some(pane);
            }
        }
    }
    let sock = env_string("CLAUDE_CODE_MESSAGING_SOCKET")
        .trim()
        .to_string();
    if !sock.is_empty() {
        let base = sock.rsplit('/').next().unwrap_or("");
        let stem = match base.rfind('.') {
            Some(i) => &base[..i],
            None => base,
        };
        if !stem.is_empty() && stem.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(pid) = stem.parse::<u32>() {
                if let Some(engine) = crate::adapters::claude_bg::engine_session_for_pid(pid) {
                    if let Some(pane) = crate::adapters::claude_bg::pane_for_job(&engine.job_id) {
                        if !pane.is_empty() {
                            return Some(pane);
                        }
                    }
                }
            }
        }
    }
    let grok_session = env_string("GROK_SESSION_ID").trim().to_string();
    if !grok_session.is_empty() {
        if let Some((team, member)) =
            crate::registry::member_for_session(&grok_session, Some("grok"))
        {
            if let Some(pane) = tmux::list_panes_all()
                .into_iter()
                .find(|p| p.team == team && p.agent == member)
            {
                return Some(pane.pane_id);
            }
        }
    }
    // A pane-keyed grok leader (a raw `hive grok` outside any team) pins
    // its pane's TMUX_PANE into the env it spawns tools with, but carries
    // no $TMUX; a member's identity-keyed leader pins nothing (the rung
    // above is its display). Trust a pinned pane only when it is real on
    // the default server.
    let pinned = env_string("TMUX_PANE").trim().to_string();
    if !pinned.is_empty() && env_string("TMUX").is_empty() {
        if let Ok(r) = tmux::run(
            &["display-message", "-t", &pinned, "-p", "#{pane_id}"],
            false,
            5,
        ) {
            if r.stdout.trim() == pinned {
                return Some(pinned);
            }
        }
    }
    None
}

/// Get the pane id of the calling process.
///
/// Inside a member engine's tool subprocess the env's TMUX_PANE is
/// unreliable — the codex shared daemon's env is frozen at spawn time (and
/// hive strips TMUX_PANE from it), and a claude bg engine has none at all —
/// so the per-CLI identity markers win over the env var (see
/// `member_env_pane`); everywhere else the per-pane TMUX_PANE env var
/// is the answer.
pub(crate) fn current_pane_id() -> Option<String> {
    if let Some(pane) = member_env_pane() {
        if !pane.is_empty() {
            return Some(pane);
        }
    }
    std::env::var("TMUX_PANE").ok()
}

fn current_pane_display(fmt: &str) -> Option<String> {
    let pane_id = current_pane_id()?;
    if pane_id.is_empty() {
        return None;
    }
    let r = tmux::run(&["display-message", "-t", &pane_id, "-p", fmt], false, 5).ok()?;
    let out = r.stdout.trim().to_string();
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Get the window target that contains the calling pane.
pub(crate) fn current_window_target() -> Option<String> {
    current_pane_display("#{session_name}:#{window_index}")
}

/// Get the tmux session name for the calling pane.
pub(crate) fn current_session_name() -> Option<String> {
    current_pane_display("#{session_name}")
}

/// Get the stable tmux window id for the calling pane.
pub(crate) fn current_window_id() -> Option<String> {
    let pane_id = current_pane_id()?;
    if pane_id.is_empty() {
        return None;
    }
    tmux::display_value(&pane_id, "#{window_id}")
}

// ---------------------------------------------------------------------------
// Codex-native gate
// ---------------------------------------------------------------------------

pub(crate) fn is_codex_tool_env() -> bool {
    !env_string("CODEX_THREAD_ID").trim().is_empty()
}

/// Hive-managed identity for a codex tool thread: a pane record (display
/// bound), or a codex roster row whose sessionId is this thread — the
/// registry, not the pane record, is the truth layer.
pub(crate) fn codex_thread_is_hive_managed(thread_id: &str) -> bool {
    let thread_id = thread_id.trim();
    if thread_id.is_empty() {
        return false;
    }
    if crate::adapters::codex_app_server::pane_for_thread(thread_id)
        .filter(|p| !p.is_empty())
        .is_some()
    {
        return true;
    }
    codex_thread_member(thread_id).is_some()
}

/// `codex_thread_is_hive_managed` for this process's own tool thread.
pub(crate) fn current_codex_thread_is_hive_managed() -> bool {
    codex_thread_is_hive_managed(&env_string("CODEX_THREAD_ID"))
}

/// (team, member) of the codex roster row whose sessionId is *thread_id*.
///
/// The self-identity rung for a codex tool: the row match *is* the identity
/// — a claude row carrying the same id is a stranger. The registry records
/// no liveness for a thread, and no cheaper authority exists (the pane
/// record is display binding, not a heartbeat), so liveness is enforced at
/// delivery, where the daemon answers or does not.
fn codex_thread_member(thread_id: &str) -> Option<(String, String)> {
    crate::registry::member_for_session(thread_id.trim(), Some("codex"))
}

/// The codex member this process's own tool thread belongs to, or None.
fn codex_thread_member_env() -> Option<(String, String)> {
    codex_thread_member(&env_string("CODEX_THREAD_ID"))
}

/// The grok member this process's own leader session belongs to, or None.
///
/// A grok leader exports `GROK_SESSION_ID` into every tool subprocess it
/// runs, and that id is the one hive minted for the member and recorded in
/// its roster row — the same shape as the codex rung, and narrowed to grok
/// rows for the same reason: another cli's row carrying the same id is a
/// stranger.
fn grok_session_member_env() -> Option<(String, String)> {
    crate::registry::member_for_session(env_string("GROK_SESSION_ID").trim(), Some("grok"))
}

// ---------------------------------------------------------------------------
// Binding discovery
// ---------------------------------------------------------------------------

/// Roster identity of the engine session this process runs inside.
///
/// The session rung of the scope ladder: pane tags cover a caller sitting in
/// a member pane, but an engine's tool subprocess carries no pane of its own
/// — its scope is the registry row keyed by its sessionId.
/// Three engines key that row, each by an id it mints for itself and hands
/// to its own tool subprocesses: a codex tool by its `CODEX_THREAD_ID`
/// (restricted to codex rows), a grok tool by its `GROK_SESSION_ID`
/// (restricted to grok rows), a Claude session by its messaging socket.
/// Inherited env is never identity, which is why nothing below this rung
/// reads env at all. This is the same authority `hive send` resolves guest
/// senders and the codex-native gate by.
pub(crate) fn session_member_binding() -> Map<String, Value> {
    let Some((team, agent)) = codex_thread_member_env()
        .or_else(grok_session_member_env)
        .or_else(claude_session_member)
    else {
        return Map::new();
    };
    let workspace = crate::registry::load(&team)
        .map(|entry| map_str(&entry, "workspace"))
        .unwrap_or_default();
    let mut payload = Map::new();
    payload.insert("team".to_string(), Value::String(team));
    payload.insert("workspace".to_string(), Value::String(workspace));
    payload.insert("agent".to_string(), Value::String(agent));
    payload.insert("role".to_string(), Value::String("agent".to_string()));
    payload.insert("pane".to_string(), Value::String(String::new()));
    payload.insert("tmuxSession".to_string(), Value::String(String::new()));
    payload.insert("tmuxWindow".to_string(), Value::String(String::new()));
    payload
}

fn claude_session_member() -> Option<(String, String)> {
    let session = crate::adapters::claude_sessions::self_session()?;
    crate::registry::member_for_session(&session.session_id, None)
}

/// The pane's own tags, or — with no pane identity — the session row.
pub(crate) fn discover_tmux_binding() -> Map<String, Value> {
    let pane = discover_pane_binding();
    if pane.is_empty() {
        session_member_binding()
    } else {
        pane
    }
}

/// The current pane's own tags, and nothing else: empty outside tmux, on
/// an untagged pane, or on a tagged pane that names no agent or role.
fn discover_pane_binding() -> Map<String, Value> {
    if !is_inside_tmux() {
        return Map::new();
    }
    let current_pane = match current_pane_id() {
        Some(p) if !p.is_empty() => p,
        _ => return Map::new(),
    };
    let team_name = match tmux::get_pane_option(&current_pane, "hive-team") {
        Some(t) if !t.is_empty() => t,
        _ => return Map::new(),
    };
    let agent_name = tmux::get_pane_option(&current_pane, "hive-agent").unwrap_or_default();
    let role = tmux::get_pane_option(&current_pane, "hive-role").unwrap_or_default();
    if agent_name.is_empty() && role.is_empty() {
        return Map::new();
    }
    let window_target = current_window_target().unwrap_or_default();
    let session_name = current_session_name().unwrap_or_default();
    let workspace = if !window_target.is_empty() {
        tmux::get_window_option(&window_target, "hive-workspace").unwrap_or_default()
    } else {
        String::new()
    };
    let group = tmux::get_pane_option(&current_pane, "hive-group").unwrap_or_default();
    let mut payload = Map::new();
    payload.insert("team".to_string(), Value::String(team_name));
    payload.insert("workspace".to_string(), Value::String(workspace));
    payload.insert("agent".to_string(), Value::String(agent_name));
    payload.insert("role".to_string(), Value::String(role));
    payload.insert("pane".to_string(), Value::String(current_pane));
    payload.insert("tmuxSession".to_string(), Value::String(session_name));
    payload.insert("tmuxWindow".to_string(), Value::String(window_target));
    if !group.is_empty() {
        payload.insert("group".to_string(), Value::String(group));
    }
    payload
}

/// The binding ladder, one walk: pane tags, then the engine's own session
/// row. Each lane is read at most once, and the session row only when the
/// pane's tags did not settle *pick*.
fn first_binding<T>(pick: impl Fn(&Map<String, Value>) -> Option<T>) -> Option<T> {
    [
        discover_pane_binding as fn() -> Map<String, Value>,
        session_member_binding,
    ]
    .into_iter()
    .find_map(|lane| pick(&lane()))
}

/// The first non-empty *field* of the binding ladder.
fn default_binding_field(field: &str) -> Option<String> {
    first_binding(|binding| Some(map_str(binding, field)).filter(|value| !value.is_empty()))
}

pub(crate) fn default_team() -> Option<String> {
    default_binding_field("team")
}

pub(crate) fn default_agent() -> Option<String> {
    default_binding_field("agent")
}

/// The sender an explicit name, the identity ladder, or the plain-shell
/// fallback names; None when this process is nobody.
pub(crate) fn resolve_sender(agent_name: Option<&str>) -> Option<String> {
    agent_name
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .or_else(default_agent)
        .or_else(unresolved_sender_fallback)
}

/// Sender when the identity ladder resolves nothing.
///
/// Only a human in a plain tmux shell speaks as orch: the shell carries no
/// engine marker and sits in a real tmux client. A process carrying an
/// engine's marker (codex thread, grok session, Claude messaging socket) or
/// running with no tmux client at all is a member context, and an unresolved
/// member must not sign as orch.
fn unresolved_sender_fallback() -> Option<String> {
    if engine_marker_env() || env_string("TMUX").is_empty() {
        return None;
    }
    Some(LEAD_AGENT_NAME.to_string())
}

/// True when this process carries an engine's own identity marker.
pub(crate) fn engine_marker_env() -> bool {
    ENGINE_MARKER_ENV
        .iter()
        .any(|key| !env_string(key).trim().is_empty())
}

/// Which member of *team* this process is, or "" when it is none of them.
///
/// The scope ladder, strongest evidence first: the pane's own tags, the
/// roster row keyed by this engine's own session id, and only then the
/// saved context file. The session rung is what answers outside tmux,
/// where a member's tool has no pane: the context file there was written
/// by whoever spawned it and would answer with the orch — see
/// [`session_member_binding`].
pub(crate) fn self_member_for_team(team: &str) -> String {
    match self_binding() {
        Some((bound_team, member)) if bound_team == team => member,
        _ => String::new(),
    }
}

/// (team, member) this process is, by the strongest rung that answers.
///
/// The first rung to resolve settles it, even when it names another team:
/// this engine is that member, so a weaker rung claiming the team being
/// asked about is a leftover, not a second identity.
fn self_binding() -> Option<(String, String)> {
    let bound = first_binding(|binding| {
        let team = map_str(binding, "team");
        let agent = map_str(binding, "agent");
        (!team.is_empty() && !agent.is_empty()).then_some((team, agent))
    });
    if bound.is_some() {
        return bound;
    }
    let ctx = crate::context::load_current_context();
    let team = ctx.get("team").cloned().unwrap_or_default();
    let agent = ctx.get("agent").cloned().unwrap_or_default();
    (!team.is_empty() && !agent.is_empty()).then_some((team, agent))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testenv::{iso, EnvGuard};
    use crate::tmux::{ok_run, set_run_override, v};
    use std::cell::RefCell;
    use std::rc::Rc;

    // --- current pane and window ---

    #[test]
    fn test_context_helpers_use_environment_and_display_message() {
        let mut env = EnvGuard::new();
        env.set("TMUX", "/tmp/tmux-1");
        env.set("TMUX_PANE", "%7");
        env.remove("CODEX_THREAD_ID");
        env.remove("GROK_SESSION_ID");
        env.remove("CLAUDE_CODE_MESSAGING_SOCKET");
        set_run_override(|args, _check, _timeout| {
            let stdout = if args.iter().any(|a| a == "#{session_name}:#{window_index}") {
                "dev:2\n"
            } else if args.iter().any(|a| a == "#{session_name}") {
                "dev\n"
            } else if args.iter().any(|a| a == "#{window_id}") {
                "@42\n"
            } else {
                "2\n"
            };
            Ok(ok_run(0, stdout, ""))
        });

        assert!(is_inside_tmux());
        assert_eq!(current_pane_id().as_deref(), Some("%7"));
        assert_eq!(current_window_target().as_deref(), Some("dev:2"));
        assert_eq!(current_session_name().as_deref(), Some("dev"));
        assert_eq!(current_window_id().as_deref(), Some("@42"));
    }

    #[test]
    fn test_current_window_helpers_return_none_without_tmux_pane() {
        let mut env = EnvGuard::new();
        env.remove("TMUX_PANE");
        env.remove("TMUX");
        env.remove("CODEX_THREAD_ID");
        env.remove("GROK_SESSION_ID");
        env.remove("CLAUDE_CODE_MESSAGING_SOCKET");

        assert_eq!(current_window_target(), None);
        assert_eq!(current_session_name(), None);
        assert_eq!(current_window_id(), None);
    }

    #[test]
    fn test_grok_session_resolves_the_members_tagged_pane() {
        // A grok member's tools carry GROK_SESSION_ID and nothing else — no
        // $TMUX, no TMUX_PANE (the leader is minted by identity before any
        // pane exists). The id keys the member's roster row, and the pane
        // tagged with that team and name is its display.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut env = EnvGuard::cleared(&crate::testenv::IDENTITY_VARS);
        env.set("HIVE_HOME", tmp.path().join(".hive"));
        let mut member = serde_json::Map::new();
        member.insert(
            "name".to_string(),
            serde_json::Value::String("rex".to_string()),
        );
        member.insert(
            "cli".to_string(),
            serde_json::Value::String("grok".to_string()),
        );
        member.insert(
            "sessionId".to_string(),
            serde_json::Value::String("s-rex".to_string()),
        );
        assert_eq!(
            crate::registry::record_team("honey", "/tmp/ws-h", "1.0", &[member], "").unwrap(),
            "written"
        );
        env.set("GROK_SESSION_ID", "s-rex");
        let calls = Rc::new(RefCell::new(Vec::new()));
        let recorded = Rc::clone(&calls);
        set_run_override(move |args, _check, _timeout| {
            recorded.borrow_mut().push(args.to_vec());
            let stdout = if args.first().map(String::as_str) == Some("list-panes") {
                concat!(
                    "%3\t[orch]\tclaude\tagent\torch\thoney\tclaude\t\n",
                    "%5\t[rex]\tgrok\tagent\trex\thoney\tgrok\t\n",
                    "%8\t[rex]\tgrok\tagent\trex\twasp\tgrok\t\n"
                )
            } else if args.iter().any(|a| a == "#{window_id}") {
                "@4\n"
            } else {
                ""
            };
            Ok(ok_run(0, stdout, ""))
        });

        assert!(is_inside_tmux());
        assert_eq!(current_pane_id().as_deref(), Some("%5"));
        assert_eq!(current_window_id().as_deref(), Some("@4"));
        assert!(calls.borrow().iter().all(
            |args| args[0] == "list-panes" || args[..3] == v(&["display-message", "-t", "%5"])
        ));

        // the same id on a claude row is a stranger, and no pane is anyone's
        let mut stranger = serde_json::Map::new();
        stranger.insert(
            "name".to_string(),
            serde_json::Value::String("rex".to_string()),
        );
        stranger.insert(
            "cli".to_string(),
            serde_json::Value::String("claude".to_string()),
        );
        stranger.insert(
            "sessionId".to_string(),
            serde_json::Value::String("s-rex".to_string()),
        );
        crate::registry::record_team("honey", "/tmp/ws-h", "1.0", &[stranger], "").unwrap();
        assert!(!is_inside_tmux());
        assert_eq!(current_pane_id(), None);
    }

    #[test]
    fn test_grok_member_without_a_pane_keeps_its_identity_but_no_display() {
        // The member's window is gone: the roster still names it (the session
        // rung answers hive send), but there is no pane to act on.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut env = EnvGuard::cleared(&crate::testenv::IDENTITY_VARS);
        env.set("HIVE_HOME", tmp.path().join(".hive"));
        let mut member = serde_json::Map::new();
        member.insert(
            "name".to_string(),
            serde_json::Value::String("rex".to_string()),
        );
        member.insert(
            "cli".to_string(),
            serde_json::Value::String("grok".to_string()),
        );
        member.insert(
            "sessionId".to_string(),
            serde_json::Value::String("s-rex".to_string()),
        );
        crate::registry::record_team("honey", "/tmp/ws-h", "1.0", &[member], "").unwrap();
        env.set("GROK_SESSION_ID", "s-rex");
        set_run_override(|_args, _check, _timeout| Ok(ok_run(0, "", "")));

        assert!(!is_inside_tmux());
        assert_eq!(current_pane_id(), None);
        assert_eq!(
            crate::registry::member_for_session("s-rex", Some("grok")),
            Some(("honey".to_string(), "rex".to_string()))
        );
    }

    // --- session rung: env-lane cases ---

    /// Isolated HIVE_HOME and CLAUDE_CONFIG_DIR under *tmp*, with no engine
    /// identity inherited from the shell, for the test's lifetime.
    fn isolated(tmp: &std::path::Path) -> EnvGuard {
        let mut env = EnvGuard::cleared(&crate::testenv::IDENTITY_VARS);
        env.set("HIVE_HOME", tmp.join(".hive"));
        env.set("CLAUDE_CONFIG_DIR", tmp.join(".claude"));
        env
    }

    /// A grok member row on *team*, keyed by *session_id*.
    fn record_grok_member(team: &str, workspace: &str, name: &str, session_id: &str) {
        let mut member = Map::new();
        member.insert("name".to_string(), Value::String(name.to_string()));
        member.insert("cli".to_string(), Value::String("grok".to_string()));
        member.insert(
            "sessionId".to_string(),
            Value::String(session_id.to_string()),
        );
        assert_eq!(
            crate::registry::record_team(team, workspace, "1.0", &[member], "").unwrap(),
            "written"
        );
    }

    #[test]
    fn test_grok_session_id_resolves_identity_and_workspace() {
        // A grok member's tool has no pane, no thread and no Claude
        // socket: its leader exports GROK_SESSION_ID into every tool
        // subprocess, and that id keys its grok roster row.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut env = isolated(tmp.path());
        record_grok_member("honey", "/tmp/ws-h", "rex", "s-rex");
        env.set("GROK_SESSION_ID", "s-rex");

        let binding = session_member_binding();
        assert_eq!(map_str(&binding, "team"), "honey");
        assert_eq!(map_str(&binding, "agent"), "rex");
        assert_eq!(map_str(&binding, "workspace"), "/tmp/ws-h");
        assert_eq!(map_str(&binding, "pane"), "");
        assert_eq!(discover_tmux_binding(), binding);
        assert_eq!(default_team().as_deref(), Some("honey"));
        assert_eq!(default_agent().as_deref(), Some("rex"));
        assert_eq!(resolve_sender(None).as_deref(), Some("rex"));
        assert_eq!(self_member_for_team("honey"), "rex");
        // another team's status payload is not this member's identity
        assert_eq!(self_member_for_team("wasp"), "");

        // the member was killed: the leader's env survives it, the roster
        // does not, and nothing signs as it
        record_grok_member("honey", "/tmp/ws-h", "ant", "s-ant");
        assert!(session_member_binding().is_empty());
        assert_eq!(default_team(), None);
        assert_eq!(default_agent(), None);
        assert_eq!(self_member_for_team("honey"), "");
    }

    #[test]
    fn test_grok_session_ignores_a_row_of_another_cli() {
        // The row match is the identity: a claude row carrying the same id
        // is a stranger, exactly as it is for a codex thread.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut env = isolated(tmp.path());
        let mut member = Map::new();
        member.insert("name".to_string(), Value::String("orch".to_string()));
        member.insert("cli".to_string(), Value::String("claude".to_string()));
        member.insert("sessionId".to_string(), Value::String("s-both".to_string()));
        crate::registry::record_team("wasp", "/tmp/ws-w", "1.0", &[member], "").unwrap();
        env.set("GROK_SESSION_ID", "s-both");

        assert!(session_member_binding().is_empty());
        assert_eq!(default_team(), None);
        assert_eq!(default_agent(), None);
        assert_eq!(self_member_for_team("wasp"), "");
    }

    #[test]
    fn test_grok_session_outranks_the_saved_context_file() {
        // The saved context file was written by the spawner and names the
        // orch; the member's own session row must outrank it.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut env = isolated(tmp.path());
        record_grok_member("hornet", "/tmp/ws-hn", "bee", "s-bee");
        crate::context::save_context_for_pane("", "hornet", "/tmp/ws-hn", LEAD_AGENT_NAME).unwrap();

        // context file alone: the orch answers
        assert_eq!(self_member_for_team("hornet"), LEAD_AGENT_NAME);

        env.set("GROK_SESSION_ID", "s-bee");
        assert_eq!(self_member_for_team("hornet"), "bee");
    }

    #[test]
    fn test_default_team_falls_back_to_session_membership() {
        // A session that created or joined a team from outside tmux has no
        // pane tags; its scope lives in the registry row keyed by its
        // sessionId — the same authority `hive send` resolves guests by.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut env = isolated(tmp.path());
        let mut member = Map::new();
        member.insert("name".to_string(), Value::String("orch".to_string()));
        member.insert("cli".to_string(), Value::String("claude".to_string()));
        member.insert("sessionId".to_string(), Value::String("s-wasp".to_string()));
        assert_eq!(
            crate::registry::record_team("wasp", "/tmp/ws-w", "1.0", &[member], "").unwrap(),
            "written"
        );
        let sessions = tmp.path().join(".claude").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("me.json"),
            serde_json::json!({
                "name": "me",
                "pid": std::process::id(),
                "messagingSocketPath": "/tmp/me.sock",
                "sessionId": "s-wasp",
            })
            .to_string(),
        )
        .unwrap();
        env.set("CLAUDE_CODE_MESSAGING_SOCKET", "/tmp/me.sock");

        assert_eq!(default_team().as_deref(), Some("wasp"));
        assert_eq!(default_agent().as_deref(), Some("orch"));
        let binding = session_member_binding();
        assert_eq!(map_str(&binding, "workspace"), "/tmp/ws-w");

        // A session on no roster resolves nothing.
        env.set("CLAUDE_CODE_MESSAGING_SOCKET", "/tmp/ghost.sock");
        assert_eq!(default_team(), None);
    }

    #[test]
    fn test_default_team_resolves_a_codex_member_by_its_thread() {
        // A codex member's tool with no pane record and no Claude socket:
        // its CODEX_THREAD_ID keys a codex roster row.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut env = isolated(tmp.path());
        env.set("CODEX_HOME", tmp.path().join(".codex"));
        let mut member = Map::new();
        member.insert("name".to_string(), Value::String("review".to_string()));
        member.insert("cli".to_string(), Value::String("codex".to_string()));
        member.insert(
            "sessionId".to_string(),
            Value::String("01aa-headless".to_string()),
        );
        assert_eq!(
            crate::registry::record_team("rr", "/tmp/ws-rr", "1.0", &[member], "").unwrap(),
            "written"
        );
        env.set("CODEX_THREAD_ID", "01aa-headless");

        assert_eq!(default_team().as_deref(), Some("rr"));
        assert_eq!(default_agent().as_deref(), Some("review"));
        assert_eq!(resolve_sender(None).as_deref(), Some("review"));
        let binding = session_member_binding();
        assert_eq!(map_str(&binding, "workspace"), "/tmp/ws-rr");
        assert_eq!(map_str(&binding, "pane"), "");

        // A thread on no roster resolves nothing.
        env.set("CODEX_THREAD_ID", "01aa-ghost");
        assert_eq!(default_team(), None);
        assert_eq!(default_agent(), None);
    }

    #[test]
    fn test_codex_thread_ignores_claude_row_with_same_session_id() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut env = isolated(tmp.path());
        env.set("CODEX_HOME", tmp.path().join(".codex"));
        let mut member = Map::new();
        member.insert("name".to_string(), Value::String("orch".to_string()));
        member.insert("cli".to_string(), Value::String("claude".to_string()));
        member.insert(
            "sessionId".to_string(),
            Value::String("01aa-shared".to_string()),
        );
        crate::registry::record_team("wasp", "/tmp/ws-w", "1.0", &[member], "").unwrap();
        env.set("CODEX_THREAD_ID", "01aa-shared");

        assert_eq!(default_team(), None);
        assert_eq!(default_agent(), None);
        assert!(session_member_binding().is_empty());
    }

    #[test]
    fn test_unresolved_sender_defaults_to_orch_only_for_tmux_shell() {
        let mut env = EnvGuard::cleared(&crate::testenv::IDENTITY_VARS);
        // no tmux client, no engine marker: nothing to sign as
        assert_eq!(unresolved_sender_fallback(), None);
        // a human shell inside a tmux client speaks as orch
        env.set("TMUX", "/tmp/tmux-0/default,1,0");
        assert_eq!(unresolved_sender_fallback().as_deref(), Some("orch"));
        // an engine marker makes it a member context even inside tmux
        for key in ENGINE_MARKER_ENV {
            env.set(key, "x");
            assert_eq!(unresolved_sender_fallback(), None, "{key}");
            env.remove(key);
        }
    }

    // --- codex-native gate: pane-less members are hive-managed via the registry ---

    #[test]
    fn test_codex_thread_unknown_everywhere_is_unmanaged() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _env = iso(tmp.path());
        assert!(!codex_thread_is_hive_managed("01aa-unknown"));
        assert!(!codex_thread_is_hive_managed(""));
    }

    #[test]
    fn test_codex_thread_with_pane_record_is_managed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _env = iso(tmp.path());
        crate::adapters::codex_app_server::write_pane_thread("%7", "01aa-pane", "/tmp").unwrap();
        assert!(codex_thread_is_hive_managed("01aa-pane"));
    }

    #[test]
    fn test_codex_thread_matching_registry_member_is_managed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _env = iso(tmp.path());
        let member: Map<String, Value> = [
            ("name", "review"),
            ("cli", "codex"),
            ("sessionId", "01aa-headless"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
        .collect();
        crate::registry::record_team("rr", "", "1.0", &[member], "").unwrap();
        // no pane record: a pane-less member's identity is the registry row
        assert!(codex_thread_is_hive_managed(" 01aa-headless "));
        assert!(!codex_thread_is_hive_managed("01aa-other"));
        assert_eq!(
            codex_thread_member(" 01aa-headless "),
            Some(("rr".to_string(), "review".to_string()))
        );
    }

    #[test]
    fn test_codex_thread_matching_claude_row_is_not_managed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut env = iso(tmp.path());
        let member: Map<String, Value> = [
            ("name", "orch"),
            ("cli", "claude"),
            ("sessionId", "01aa-claude"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
        .collect();
        crate::registry::record_team("rr", "", "1.0", &[member], "").unwrap();
        // A claude session id colliding with the thread id is a stranger to
        // the codex gate; the generic session lookup still sees the row.
        assert_eq!(codex_thread_member("01aa-claude"), None);
        assert!(!codex_thread_is_hive_managed("01aa-claude"));
        assert!(crate::registry::member_for_session("01aa-claude", None).is_some());
        env.set("CODEX_THREAD_ID", "01aa-claude");
        assert_eq!(codex_thread_member_env(), None);
    }
}
