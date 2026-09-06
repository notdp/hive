use std::collections::HashSet;
use std::path::Path;

use anyhow::{anyhow, bail, Result};
use serde_json::{Map, Value};

use super::*;
use crate::team::Team;
use crate::tmux;

pub(crate) const TMUX_REQUIRED_MESSAGE: &str =
    "Hive requires tmux. Start or attach to a tmux session first.";

/// Refusal for an engine whose own session id names no roster row. Told
/// apart from `TMUX_REQUIRED_MESSAGE` because the caller has no terminal to
/// go find: it is an engine subprocess, and its identity is the broken part.
pub(crate) const UNROSTERED_ENGINE_MESSAGE: &str =
    "this engine's session names nobody on any team's roster \
     (the member was killed, or the team deleted)";

// Verbs that never need a tmux context — plus the team verbs, which read the
// registry (the truth layer) and address the team's window by id, so a
// caller outside tmux or in another session reaches it the same way. `flow`
// rides the same doctrine, and `flow node --team` exists for callers without
// a pane identity (a workflow proxy subagent, a desktop session).
pub(crate) const TMUX_OPTIONAL_ROOT_COMMANDS: &[&str] = &[
    "plugin",
    "config",
    "shell-init",
    "codex",
    "claude",
    "grok",
    "resume-hint",
    "worktree",
    "ls",
    "ccd",
    "create",
    "join",
    "spawn",
    "team",
    "kill",
    "delete",
    "attach",
    "view",
    "flow",
];

pub(crate) const CODEX_NATIVE_REQUIRED_BYPASS_COMMANDS: &[&str] = &[
    "claude",
    "codex",
    "config",
    "doctor",
    "grok",
    "inject",
    "plugin",
    "resume-hint",
    "shell-init",
];

pub const TEAM_NAME_POOL: [&str; 10] = [
    "honey", "comb", "wasp", "bumble", "hornet", "nectar", "pollen", "amber", "clover", "sage",
];

pub(crate) const RANDOM_AGENT_NAMES: [&str; 10] = [
    "yoyo", "lulu", "nini", "bobo", "kiki", "dodo", "pipi", "toto", "momo", "coco",
];

// ---------------------------------------------------------------------------
// Small shared utilities
// ---------------------------------------------------------------------------

/// Print `Error: msg` to stderr, exit 1.
pub(crate) fn fail(msg: &str) -> ! {
    eprintln!("Error: {msg}");
    std::process::exit(1);
}

/// Bridge for anyhow-returning helpers used from CLI handlers: any Err takes
/// the `_fail` exit lane.
pub(crate) fn ok_or_fail<T>(result: Result<T>) -> T {
    match result {
        Ok(value) => value,
        Err(err) => fail(&err.to_string()),
    }
}

pub(crate) fn getcwd() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub(crate) use crate::identity::env_string;
pub(crate) use crate::json_fields::{is_set, map_str};
pub(crate) use crate::paths::expanduser;
pub(crate) use crate::shell::shlex_quote;
pub(crate) use crate::team::created_at_key;

/// Replace this process with *program*; print the error and exit 1 when
/// the exec fails.
pub(crate) fn execvp(program: &str, args: &[String]) -> ! {
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(program).args(args).exec();
    eprintln!("Error: {err}");
    std::process::exit(1);
}

/// No control characters or line separators: safe to echo into a terminal.
// ponytail: the control-char gate covers the documented threats (ESC/OSC/BEL/
// newline); the full Unicode C*/Z* table is overkill.
pub(crate) fn is_printable(s: &str) -> bool {
    s.chars()
        .all(|c| !c.is_control() && c != '\u{2028}' && c != '\u{2029}')
}

pub(crate) fn stdout_isatty() -> bool {
    unsafe { libc::isatty(1) == 1 }
}

/// A settings value as an environment-variable string: strings bare, null
/// empty, anything else its JSON text.
pub(crate) fn value_as_env_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// `json.dumps(payload, indent=2, ensure_ascii=False)`.
pub(crate) fn json_pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// os.urandom-grade bytes for name picks and artifact filenames.
pub(super) fn os_random_bytes(n: usize) -> Vec<u8> {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(&mut buf).is_ok() {
            return buf;
        }
    }
    // ponytail: nanos fallback only fires when /dev/urandom is unreadable —
    // uniqueness is what matters here, not cryptographic strength.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    for (i, b) in buf.iter_mut().enumerate() {
        *b = ((nanos >> ((i % 16) * 8)) & 0xff) as u8;
    }
    buf
}

/// `secrets.token_urlsafe(4)` — 4 random bytes, base64url, no padding.
pub(crate) fn token_urlsafe4() -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let data = os_random_bytes(4);
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6 & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 63) as usize] as char);
        }
    }
    out
}

/// `secrets.choice(seq)`.
pub(super) fn random_choice<'a>(options: &[&'a str]) -> &'a str {
    let idx = os_random_bytes(1)[0] as usize % options.len();
    options[idx]
}

pub(crate) fn stdin_isatty() -> bool {
    unsafe { libc::isatty(0) == 1 }
}

pub(crate) fn resolve_sender(agent_name: Option<&str>) -> String {
    identity::resolve_sender(agent_name).unwrap_or_else(|| {
        fail(
            "cannot resolve own member identity: this engine is on no roster \
             (a codex thread, grok session or Claude session not recorded by \
             any team) — join a team first, or run from a bound pane",
        )
    })
}

// ---------------------------------------------------------------------------
// Team / workspace resolution
// ---------------------------------------------------------------------------

pub(crate) fn load_team(team: &str, prefer_pane: &str) -> Result<Team> {
    Team::load(team, prefer_pane).map_err(|_| anyhow!("team '{team}' not found"))
}

/// Addressing order: explicit team -> binding discovery (pane tags, then the
/// engine's own session row). An explicit team is the caller's intent — it
/// loads from the registry wherever the caller happens to be.
pub fn resolve_scoped_team(
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

pub fn resolve_workspace(team: Option<&Team>, required: bool) -> Result<String> {
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

// ---------------------------------------------------------------------------
// Pane target resolution (compact / fork)
// ---------------------------------------------------------------------------

/// Identity of an agent pane resolved straight from its tmux options.
///
/// Deliberately team-agnostic: `team_name` is empty for panes not bound to
/// any Hive team, and `member_label` falls back to the literal pane id.
#[derive(Debug, Clone)]
pub(crate) struct PaneTarget {
    pub pane_id: String,
    pub team_name: String,
    pub is_team_bound: bool,
    pub cli: String,
    pub member_label: String,
}

/// Resolve a pane's identity from tmux pane options *only* (never re-resolve
/// an agent by name through Team state — the cross-window same-name bug PR #8
/// fixed for `compact --pane`).
pub(crate) fn resolve_pane_target(pane_id: &str) -> PaneTarget {
    let pane = if !pane_id.is_empty() {
        pane_id.to_string()
    } else {
        identity::current_pane_id().unwrap_or_default()
    };
    if pane.is_empty() {
        fail("cannot determine current pane (pass --pane explicitly)");
    }
    let team_name = tmux::get_pane_option(&pane, "hive-team").unwrap_or_default();
    let option_cli = crate::agent_cli::normalize_command(
        &tmux::get_pane_option(&pane, "hive-cli").unwrap_or_default(),
    );
    let cli_name = if crate::agent_cli::AGENT_CLI_NAMES.contains(&option_cli.as_str()) {
        option_cli
    } else {
        crate::agent_cli::detect_profile_for_pane(&pane)
            .map(|profile| profile.name.to_string())
            .unwrap_or_default()
    };
    if cli_name.is_empty() {
        fail(&format!("unsupported agent pane '{pane}'"));
    }
    let member_label = match tmux::get_pane_option(&pane, "hive-agent") {
        Some(agent) if !agent.is_empty() => agent,
        _ => pane.clone(),
    };
    PaneTarget {
        pane_id: pane,
        is_team_bound: !team_name.is_empty(),
        team_name,
        cli: cli_name,
        member_label,
    }
}

pub(crate) fn ensure_pane_in_scope(t: &Team, pane_id: &str) {
    if pane_id.is_empty() {
        return;
    }
    let pane_window = tmux::get_pane_window_target(pane_id).unwrap_or_default();
    let team_window = t.tmux_window.clone();
    if !team_window.is_empty() && !pane_window.is_empty() && pane_window != team_window {
        fail(&format!(
            "pane '{pane_id}' is in tmux window '{pane_window}', not team '{}' window '{team_window}'",
            t.name
        ));
    }
    if let Some(pane_team) = tmux::get_pane_option(pane_id, "hive-team") {
        if !pane_team.is_empty() && pane_team != t.name {
            fail(&format!(
                "pane '{pane_id}' already belongs to team '{pane_team}'"
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// Send protocol helpers
// ---------------------------------------------------------------------------

pub(crate) fn maybe_warn_long_body(body: &str, command: &str) {
    if let Some(hint) = crate::message::body_warning_hint(body) {
        eprintln!("{}", crate::message::format_body_warning(command, &hint));
    }
}

/// Pure core of `validate_root_send_protocol`: Some(error) when the body
/// violates the root-thread protocol.
fn root_send_protocol_error(body: &str) -> Option<String> {
    let summary = body.trim();
    if summary.is_empty() {
        return Some("new root send requires a short body summary".to_string());
    }
    // artifact is not mandatory — short confirmations like "ack" legitimately
    // don't need one. The length/structure gate below already forces bulky or
    // structured content into --artifact.
    if crate::message::body_warning_hint(summary).is_some() {
        return Some(
            "new root send body must stay short and unstructured; move details into --artifact \
             (prefer `--artifact -` unless you already have a file)"
                .to_string(),
        );
    }
    None
}

pub(crate) fn validate_root_send_protocol(body: &str) {
    if let Some(err) = root_send_protocol_error(body) {
        fail(&err);
    }
}

// ---------------------------------------------------------------------------
// Payload helpers
// ---------------------------------------------------------------------------

pub(crate) fn add_runtime_location_fields(payload: &mut Map<String, Value>) {
    if !payload.contains_key("runtimeWorkspace") && payload.contains_key("workspace") {
        if let Some(ws) = payload.shift_remove("workspace") {
            payload.insert("runtimeWorkspace".to_string(), ws);
        }
    }
    payload.insert("cwd".to_string(), Value::String(getcwd()));
}

/// Stable per-window slug. Uses the tmux window id (`@42` → `w42`); falls
/// back to the mutable window index only when no id is available.
fn window_id_slug(window_id: &str, fallback_index: &str) -> String {
    let raw = window_id.trim_start_matches('@');
    let raw = if raw.is_empty() {
        if fallback_index.is_empty() {
            "0"
        } else {
            fallback_index
        }
    } else {
        raw
    };
    format!("w{raw}")
}

/// Window-id-derived team name — the overflow scheme behind the pool.
fn default_team_name_for_window(session_name: &str, window_id: &str, window_index: &str) -> String {
    format!("{session_name}-{}", window_id_slug(window_id, window_index))
}

/// Group tags and qualified `@hive-agent` prefixes claimed by live panes.
fn claimed_group_namespaces() -> HashSet<String> {
    let mut claimed = HashSet::new();
    for pane in tmux::list_panes_all() {
        let group = pane.group.trim();
        if !group.is_empty() {
            claimed.insert(group.to_string());
        }
        let agent = pane.agent.trim();
        if let Some((prefix, _)) = agent.split_once('.') {
            if !prefix.is_empty() {
                claimed.insert(prefix.to_string());
            }
        }
    }
    claimed
}

/// Short memorable name for a new team; window-id scheme as overflow.
pub(crate) fn pick_team_name(session_name: &str, window_id: &str, window_index: &str) -> String {
    let mut used: HashSet<String> = tmux::list_panes_all()
        .into_iter()
        .filter(|p| !p.team.is_empty())
        .map(|p| p.team)
        .collect();
    used.extend(claimed_group_namespaces());
    // The registry is the name authority: a team whose window is gone owns
    // its name until `hive delete` — a pool pick must never clobber it.
    for entry in crate::registry::list_entries() {
        let team = map_str(&entry, "team");
        if !team.is_empty() {
            used.insert(team);
        }
    }
    for candidate in TEAM_NAME_POOL {
        if !used.contains(candidate) {
            return candidate.to_string();
        }
    }
    default_team_name_for_window(session_name, window_id, window_index)
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

pub(crate) fn parse_entries(entries: &[String]) -> Map<String, Value> {
    match crate::bus::parse_key_value(entries) {
        Ok(map) => map,
        Err(err) => fail(&err.to_string()),
    }
}

fn team_window_identity(t: &mut Team) -> (String, String) {
    let window_target = if !t.tmux_window.is_empty() {
        t.tmux_window.clone()
    } else {
        identity::current_window_target().unwrap_or_default()
    };
    let mut window_id = t.tmux_window_id.clone();
    if window_id.is_empty() && !window_target.is_empty() {
        window_id = tmux::get_window_id(&window_target).unwrap_or_default();
    }
    if window_id.is_empty() {
        window_id = identity::current_window_id().unwrap_or_default();
    }
    if !window_target.is_empty() && t.tmux_window.is_empty() {
        t.tmux_window = window_target.clone();
    }
    if !window_id.is_empty() && t.tmux_window_id.is_empty() {
        t.tmux_window_id = window_id.clone();
    }
    (window_target, window_id)
}

/// Start (or find) the team's hived, filling the team's window identity.
pub(crate) fn start_team_hived(t: &mut Team, workspace: &str) -> Option<i32> {
    let (window_target, window_id) = team_window_identity(t);
    crate::hived::ensure_hived(workspace, &t.name, &window_target, &window_id)
}

/// Seam used by flow.rs (return ignored there; team not mutated).
pub fn ensure_team_hived(t: &Team, workspace: &Path) {
    let mut clone = t.clone();
    let _ = start_team_hived(&mut clone, &workspace.to_string_lossy());
}

fn augment_team_payload_with_runtime(
    t: &mut Team,
    mut payload: Map<String, Value>,
) -> Map<String, Value> {
    let ws = resolve_workspace(Some(&*t), false).unwrap_or_default();
    if ws.is_empty() {
        return payload;
    }
    let _ = start_team_hived(t, &ws);
    let Some(runtime) =
        crate::team::usable_runtime(crate::hived::request_team_runtime(&ws, &t.name))
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
                "turnPhase",
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
    // The flow runner's mailbox is a reserved address, not a member — list
    // it beside the roster so "hive team can't find flow" never reads as
    // "my report was lost".
    payload.insert(
        "mailboxes".to_string(),
        serde_json::json!([{"addr": "flow.run", "kind": "flow", "delivery": "bus"}]),
    );
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

pub(crate) fn resolve_target_pane() -> String {
    match identity::current_pane_id() {
        Some(current) if !current.is_empty() => current,
        _ => fail("cannot determine target pane (run inside tmux)"),
    }
}

pub(crate) fn resolve_artifact_path(artifact: &str, workspace: &str) -> String {
    if artifact.is_empty() {
        return String::new();
    }
    if artifact == "-" {
        // Read from stdin, save to workspace artifacts
        if workspace.is_empty() {
            fail("--artifact - requires a workspace (run inside a team)");
        }
        let heredoc_recipe = "  hive <cmd> <args> --artifact - <<'EOF'\n  # details\n  EOF";
        if stdin_isatty() {
            fail(&format!(
                "--artifact - expects piped stdin but a terminal is attached; \
                 use a heredoc instead:\n{heredoc_recipe}"
            ));
        }
        let mut content = String::new();
        use std::io::Read;
        let _ = std::io::stdin().read_to_string(&mut content);
        if content.is_empty() {
            fail(&format!(
                "--artifact - received empty stdin; pipe content in or use a heredoc:\n{heredoc_recipe}"
            ));
        }
        let ws_artifacts = Path::new(workspace).join("artifacts");
        let _ = std::fs::create_dir_all(&ws_artifacts);
        // Short random id — file name is never parsed by downstream code.
        let filename = format!("{}.md", token_urlsafe4());
        let path = ws_artifacts.join(filename);
        let _ = std::fs::write(&path, &content);
        return path.to_string_lossy().into_owned();
    }
    let resolved_artifact = expanduser(artifact);
    if !Path::new(&resolved_artifact).exists() {
        fail(&format!("artifact not found: {resolved_artifact}"));
    }
    resolved_artifact
}

pub(crate) fn resolve_spawn_cli_name(cli_name: Option<&str>) -> String {
    if let Some(cli) = cli_name {
        if crate::agent_cli::AGENT_CLI_NAMES.contains(&cli) {
            return cli.to_string();
        }
    }
    let current_pane = identity::current_pane_id().unwrap_or_default();
    if !current_pane.is_empty() {
        let option_cli = crate::agent_cli::normalize_command(
            &tmux::get_pane_option(&current_pane, "hive-cli").unwrap_or_default(),
        );
        if crate::agent_cli::AGENT_CLI_NAMES.contains(&option_cli.as_str()) {
            return option_cli;
        }
        if let Some(profile) = crate::agent_cli::detect_profile_for_pane(&current_pane) {
            return profile.name.to_string();
        }
    }
    "claude".to_string()
}

// ---------------------------------------------------------------------------
// Tests (ported from tests/unit — logic-level only)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_window_id_slug_prefers_window_id() {
        assert_eq!(window_id_slug("@42", "3"), "w42");
        assert_eq!(window_id_slug("", "3"), "w3");
        assert_eq!(window_id_slug("", ""), "w0");
    }

    #[test]
    fn test_default_team_name_for_window_uses_slug() {
        assert_eq!(default_team_name_for_window("dev", "@7", "1"), "dev-w7");
        assert_eq!(default_team_name_for_window("dev", "", "5"), "dev-w5");
    }

    #[test]
    fn test_root_send_protocol_rejects_empty_and_structured_bodies() {
        assert_eq!(
            root_send_protocol_error("  "),
            Some("new root send requires a short body summary".to_string())
        );
        assert!(root_send_protocol_error("ack").is_none());
        let long_body = "x".repeat(501);
        assert!(root_send_protocol_error(&long_body).is_some());
    }

    #[test]
    fn test_token_urlsafe4_shape() {
        let token = token_urlsafe4();
        assert_eq!(token.len(), 6);
        assert!(token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
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
