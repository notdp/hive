//! The remaining command handlers — port of the non-core half of
//! `src/hive/cli.py`: fork, spawn, config, inject, compact, layout, pr, flow,
//! attach, thread, capture, cvim/vim/vfork/hfork, notify, plugin, the
//! claude/codex/grok launchers, ccd, resume-hint, shell-init, and worktree.

use std::collections::{HashMap, HashSet};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{bail, Result};
use serde_json::{json, Map, Value};

use super::*;
use crate::tmux;

// ---------------------------------------------------------------------------
// Python-compatible output helpers
// ---------------------------------------------------------------------------

/// `json.dumps(value, ...)` — Python's separators (", ", ": "), optional
/// `indent`, `sort_keys`, and `ensure_ascii` \uXXXX escaping.
pub(crate) fn py_dumps(
    value: &Value,
    ensure_ascii: bool,
    indent: Option<usize>,
    sort_keys: bool,
) -> String {
    let mut out = String::new();
    write_py_value(&mut out, value, ensure_ascii, indent, sort_keys, 0);
    out
}

fn write_py_string(out: &mut String, s: &str, ensure_ascii: bool) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if ensure_ascii && (c as u32) > 0x7f => {
                let cp = c as u32;
                if cp > 0xffff {
                    let v = cp - 0x10000;
                    out.push_str(&format!(
                        "\\u{:04x}\\u{:04x}",
                        0xd800 + (v >> 10),
                        0xdc00 + (v & 0x3ff)
                    ));
                } else {
                    out.push_str(&format!("\\u{cp:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn write_py_value(
    out: &mut String,
    value: &Value,
    ensure_ascii: bool,
    indent: Option<usize>,
    sort_keys: bool,
    level: usize,
) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => write_py_string(out, s, ensure_ascii),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            match indent {
                None => {
                    out.push('[');
                    for (i, item) in items.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        write_py_value(out, item, ensure_ascii, indent, sort_keys, level);
                    }
                    out.push(']');
                }
                Some(width) => {
                    out.push('[');
                    let pad = " ".repeat(width * (level + 1));
                    for (i, item) in items.iter().enumerate() {
                        out.push_str(if i > 0 { ",\n" } else { "\n" });
                        out.push_str(&pad);
                        write_py_value(out, item, ensure_ascii, indent, sort_keys, level + 1);
                    }
                    out.push('\n');
                    out.push_str(&" ".repeat(width * level));
                    out.push(']');
                }
            }
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            let mut keys: Vec<&String> = map.keys().collect();
            if sort_keys {
                keys.sort();
            }
            match indent {
                None => {
                    out.push('{');
                    for (i, key) in keys.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        write_py_string(out, key, ensure_ascii);
                        out.push_str(": ");
                        write_py_value(out, &map[*key], ensure_ascii, indent, sort_keys, level);
                    }
                    out.push('}');
                }
                Some(width) => {
                    out.push('{');
                    let pad = " ".repeat(width * (level + 1));
                    for (i, key) in keys.iter().enumerate() {
                        out.push_str(if i > 0 { ",\n" } else { "\n" });
                        out.push_str(&pad);
                        write_py_string(out, key, ensure_ascii);
                        out.push_str(": ");
                        write_py_value(out, &map[*key], ensure_ascii, indent, sort_keys, level + 1);
                    }
                    out.push('\n');
                    out.push_str(&" ".repeat(width * level));
                    out.push('}');
                }
            }
        }
    }
}

/// Python `shlex.quote`.
pub(crate) fn shlex_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    let safe = value.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '@' | '%' | '+' | '=' | ':' | ',' | '.' | '/' | '_' | '-')
    });
    if safe {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// `str(uuid.uuid4())`.
fn uuid4() -> String {
    let b = os_random_bytes(16);
    let mut b: Vec<u8> = b;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13],
        b[14], b[15]
    )
}

/// `os.execvp` — replace this process; print the error and exit 1 on failure.
fn execvp(program: &str, args: &[String]) -> ! {
    let err = std::process::Command::new(program).args(args).exec();
    eprintln!("Error: {err}");
    std::process::exit(1);
}

fn py_isprintable(s: &str) -> bool {
    // ponytail: control-char gate covers the documented threats (ESC/OSC/BEL/
    // newline); the full Unicode C*/Z* table of str.isprintable is overkill.
    s.chars()
        .all(|c| !c.is_control() && c != '\u{2028}' && c != '\u{2029}')
}

fn stdout_isatty() -> bool {
    unsafe { libc::isatty(1) == 1 }
}

fn value_as_env_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// fork
// ---------------------------------------------------------------------------

const _FORK_MIN_COLS: i64 = 80;
const _FORK_MIN_ROWS: i64 = 20;

/// True for horizontal (left/right) split, false for vertical (top/bottom).
///
/// Accounts for the 1-cell tmux separator consumed by the split.
pub(crate) fn _choose_fork_split(width: i64, height: i64) -> bool {
    let h_half = (width - 1) / 2;
    let v_half = (height - 1) / 2;
    let can_h = h_half >= _FORK_MIN_COLS;
    let can_v = v_half >= _FORK_MIN_ROWS;
    if can_h && can_v {
        return width as f64 >= height as f64 * 2.5;
    }
    if can_h {
        return true;
    }
    if can_v {
        return false;
    }
    let h_score = f64::min(
        h_half as f64 / _FORK_MIN_COLS as f64,
        height as f64 / _FORK_MIN_ROWS as f64,
    );
    let v_score = f64::min(
        width as f64 / _FORK_MIN_COLS as f64,
        v_half as f64 / _FORK_MIN_ROWS as f64,
    );
    h_score >= v_score
}

pub fn fork_cmd(pane_id: &str, split: &str, join_as: &str, prompt: &str) {
    let target = _resolve_pane_target(pane_id);
    if !target.is_team_bound {
        // Non-team pane: clone it bare — no member registration, no @hive-* tags.
        // The clone is an independent agent that belongs to no Hive team.
        if !join_as.is_empty() {
            fail("--join-as requires a team-bound pane");
        }
        let new_pane = _fork_orphan_clone(&target.pane_id, split, prompt);
        let mut payload = Map::new();
        payload.insert("pane".to_string(), Value::String(new_pane));
        payload.insert("registered".to_string(), Value::Null);
        payload.insert("team".to_string(), Value::Null);
        println!("{}", json_pretty(&Value::Object(payload)));
        return;
    }

    // Team-bound fork: register the clone as a new team member.
    let mut target_team = if !pane_id.is_empty() {
        ok_or_fail(_load_team(&target.team_name, ""))
    } else {
        ok_or_fail(resolve_scoped_team(None, true))
            .1
            .expect("required resolve returned no team")
    };

    let join_as = if join_as.is_empty() {
        let window_target = if !target_team.tmux_window.is_empty() {
            target_team.tmux_window.clone()
        } else {
            tmux::get_current_window_target().unwrap_or_default()
        };
        let panes = if window_target.is_empty() {
            Vec::new()
        } else {
            tmux::list_panes_full(&window_target)
        };
        let mut seen_names = _window_seen_names(&target_team, &panes);
        _derive_agent_name(&mut seen_names)
    } else {
        join_as.to_string()
    };

    let (_registered_agent, new_pane) =
        _fork_registered_agent(&mut target_team, pane_id, split, &join_as, prompt);
    let mut payload = Map::new();
    payload.insert("pane".to_string(), Value::String(new_pane));
    payload.insert("registered".to_string(), Value::String(join_as));
    payload.insert("team".to_string(), Value::String(target_team.name.clone()));
    println!("{}", json_pretty(&Value::Object(payload)));
}

/// Resolve the fork source pane: (pane, profile, session_id, horizontal, cwd).
fn _fork_source_details(
    pane_id: &str,
    split: &str,
    workspace: &str,
) -> (
    String,
    &'static crate::agent_cli::CLIProfile,
    String,
    bool,
    String,
) {
    if !tmux::is_inside_tmux() {
        fail("hive fork requires tmux");
    }
    let current_pane = if !pane_id.is_empty() {
        pane_id.to_string()
    } else {
        tmux::get_current_pane_id().unwrap_or_default()
    };
    if current_pane.is_empty() {
        fail("cannot determine current pane (pass --pane explicitly)");
    }
    let profile = match crate::agent_cli::detect_profile_for_pane(&current_pane) {
        Some(profile) => profile,
        None => fail(&format!("unsupported agent pane '{current_pane}'")),
    };

    let horizontal = if split == "auto" {
        let width = tmux::display_value(&current_pane, "#{pane_width}")
            .filter(|s| !s.is_empty())
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(80);
        let height = tmux::display_value(&current_pane, "#{pane_height}")
            .filter(|s| !s.is_empty())
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(24);
        _choose_fork_split(width, height)
    } else {
        split == "h"
    };

    let mut session_id = String::new();
    if !workspace.is_empty() {
        let payload =
            crate::hived::request_runtime_snapshot(workspace, &current_pane).unwrap_or_default();
        if let Some(snapshot) = payload.get("snapshot").and_then(Value::as_object) {
            let fresh = match snapshot.get("_sessionIdFresh") {
                None => true,
                some => truthy(some),
            };
            if fresh {
                let sid = map_str(snapshot, "sessionId");
                if !sid.is_empty() && sid != "unresolved" {
                    session_id = sid;
                }
            }
        }
    }
    if session_id.is_empty() {
        session_id = crate::agent_cli::resolve_session_id_for_pane(&current_pane, Some(profile))
            .unwrap_or_default();
    }
    if session_id.is_empty() {
        fail(&format!(
            "cannot determine session id for pane '{current_pane}'"
        ));
    }

    let source_cwd = tmux::display_value(&current_pane, "#{pane_current_path}").unwrap_or_default();
    (current_pane, profile, session_id, horizontal, source_cwd)
}

pub(crate) const _FORK_NEW_TASK_MARKER: &str = "NEW TASK FOR THIS FORK:";
pub(crate) const _FORK_BOUNDARY_TEXT: &str =
    "FORK BOUNDARY: you are a freshly forked agent. Run `hive team` to find your \
own identity (the `self` field).\n\n\
Everything before this boundary is read-only inherited context for the \
original agent. This includes the user's most recent instruction, any \
unfinished request, and any pending tool/bash/action from the prior \
transcript. Treat all of it as already owned by the original agent. Do NOT \
continue, retry, or re-execute any task from before this boundary.\n\n\
After `hive team`, act only on instructions explicitly provided after the \
marker `NEW TASK FOR THIS FORK:` in this message, or on future messages \
that arrive after this boundary. If no `NEW TASK FOR THIS FORK:` section \
is present, stop after identifying yourself and wait for new input.";
// Orphan variant: a non-team fork has no team and no `self`, so it must NOT be
// told to run `hive team` to find an identity. The anti-re-execution core is
// preserved verbatim — that is what stops the clone from re-running the
// parent's in-flight work regardless of team membership.
pub(crate) const _FORK_ORPHAN_BOUNDARY_TEXT: &str =
    "FORK BOUNDARY: you are a freshly forked, independent clone. You are NOT \
bound to any Hive team — running `hive team` only confirms you have no team \
binding, and there is no `self` identity to look up.\n\n\
Everything before this boundary is read-only inherited context for the \
original agent. This includes the user's most recent instruction, any \
unfinished request, and any pending tool/bash/action from the prior \
transcript. Treat all of it as already owned by the original agent. Do NOT \
continue, retry, or re-execute any task from before this boundary.\n\n\
Act only on instructions explicitly provided after the marker \
`NEW TASK FOR THIS FORK:` in this message, or on future messages that \
arrive after this boundary. If no `NEW TASK FOR THIS FORK:` section is \
present, stop and wait for new human input.";

/// The boundary message every fork receives as its first user input.
fn _fork_boundary_prompt(team_bound: bool) -> &'static str {
    if team_bound {
        _FORK_BOUNDARY_TEXT
    } else {
        _FORK_ORPHAN_BOUNDARY_TEXT
    }
}

/// Cached static boundary file under `$HIVE_HOME`; rewritten on drift.
fn _fork_boundary_file(team_bound: bool) -> PathBuf {
    let text = _fork_boundary_prompt(team_bound);
    let filename = if team_bound {
        "fork-boundary.txt"
    } else {
        "fork-boundary-orphan.txt"
    };
    let path = crate::team::hive_home().join(filename);
    let stale = match std::fs::read_to_string(&path) {
        Ok(existing) => existing != text,
        Err(_) => true,
    };
    if stale {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&path, text);
    }
    path
}

fn _fork_registered_agent(
    t: &mut Team,
    pane_id: &str,
    split: &str,
    join_as: &str,
    prompt: &str,
) -> (Agent, String) {
    _ensure_pane_in_scope(t, pane_id);
    let window_target = if !t.tmux_window.is_empty() {
        t.tmux_window.clone()
    } else {
        tmux::get_current_window_target().unwrap_or_default()
    };
    let panes = if window_target.is_empty() {
        Vec::new()
    } else {
        tmux::list_panes_full(&window_target)
    };
    let mut seen_names = _window_seen_names(t, &panes);
    _claim_member_name(join_as, &mut seen_names);

    let workspace = t.workspace.clone();
    let (current_pane, profile, session_id, horizontal, source_cwd) =
        _fork_source_details(pane_id, split, &workspace);

    // Both clones launch through hive's managed launcher; boundary text is
    // static, so cache it under $HIVE_HOME and expand via shell command
    // substitution when there is no prompt. With --prompt we inline boundary +
    // marker + prompt together so the fork sees both in one user message.
    let cmd_base = profile.fork_cmd_for(&session_id);
    let launch_cmd = if !prompt.is_empty() {
        let composed = format!(
            "{}\n\n{}\n{}",
            _fork_boundary_prompt(true),
            _FORK_NEW_TASK_MARKER,
            prompt
        );
        format!("{cmd_base} {}", shlex_quote(&composed))
    } else {
        format!(
            "{cmd_base} \"$(cat {})\"",
            shlex_quote(&_fork_boundary_file(true).to_string_lossy())
        )
    };
    let new_pane = ok_or_fail(tmux::split_window(
        &current_pane,
        horizontal,
        None,
        false,
        if source_cwd.is_empty() {
            None
        } else {
            Some(&source_cwd)
        },
    ));
    ok_or_fail(tmux::send_keys(&new_pane, &launch_cmd, true));
    let group = if join_as.contains('.') {
        join_as.split('.').next().unwrap_or("").to_string()
    } else {
        String::new()
    };
    let team_name = t.name.clone();
    let cwd = if source_cwd.is_empty() {
        getcwd()
    } else {
        source_cwd.clone()
    };
    let registered_agent = _register_agent_member(
        t,
        &new_pane,
        &team_name,
        join_as,
        profile.name,
        &cwd,
        false,
        &group,
    );
    (registered_agent, new_pane)
}

/// Fork a non-team agent pane into a bare, independent clone.
fn _fork_orphan_clone(pane_id: &str, split: &str, prompt: &str) -> String {
    let (current_pane, profile, session_id, horizontal, source_cwd) =
        _fork_source_details(pane_id, split, "");
    let cmd_base = profile.fork_cmd_for(&session_id);
    let launch_cmd = if !prompt.is_empty() {
        let composed = format!(
            "{}\n\n{}\n{}",
            _fork_boundary_prompt(false),
            _FORK_NEW_TASK_MARKER,
            prompt
        );
        format!("{cmd_base} {}", shlex_quote(&composed))
    } else {
        format!(
            "{cmd_base} \"$(cat {})\"",
            shlex_quote(&_fork_boundary_file(false).to_string_lossy())
        )
    };
    let new_pane = ok_or_fail(tmux::split_window(
        &current_pane,
        horizontal,
        None,
        false,
        if source_cwd.is_empty() {
            None
        } else {
            Some(&source_cwd)
        },
    ));
    ok_or_fail(tmux::send_keys(&new_pane, &launch_cmd, true));
    new_pane
}

// ---------------------------------------------------------------------------
// spawn
// ---------------------------------------------------------------------------

/// Spawn a member with no pane: engine first, registry as its existence.
#[allow(clippy::too_many_arguments)]
fn _spawn_headless_member(
    t: &mut Team,
    team_name: &str,
    agent_name: &str,
    model: &str,
    prompt: &str,
    cwd: &str,
    skill: &str,
    env_entries: &[String],
    cli_name: Option<&str>,
) -> Result<Agent> {
    let resolved_cli = match cli_name {
        Some(cli) if crate::agent_cli::AGENT_CLI_NAMES.contains(&cli) => cli.to_string(),
        _ => "claude".to_string(),
    };
    if let Some(model_error) = crate::agent_cli::validate_spawn_model(&resolved_cli, model) {
        bail!("{model_error}");
    }
    if agent_name == "flow" || agent_name.starts_with("flow.") {
        bail!(
            "'{agent_name}' collides with the flow runner's mailbox address kind (flow.run), not a member name"
        );
    }
    if t.agent_named(agent_name).is_some() {
        bail!("Agent '{agent_name}' already exists in team '{}'", t.name);
    }
    let resolved_cwd = if cwd.is_empty() {
        getcwd()
    } else {
        expanduser(cwd)
    };
    let extra_env: Map<String, Value> = if env_entries.is_empty() {
        Map::new()
    } else {
        _parse_entries(env_entries)
    };

    let profile = crate::agent_cli::get_profile(&resolved_cli);
    let mut initial_prompt = String::new();
    if !skill.is_empty() && skill != "none" {
        let skill_ref = if resolved_cli == "claude" {
            skill.to_string()
        } else {
            skill
                .rsplit_once(':')
                .map(|(_, rest)| rest.to_string())
                .unwrap_or_else(|| skill.to_string())
        };
        initial_prompt = match profile {
            Some(profile) => profile.skill_cmd_for(&skill_ref),
            None => format!("/{skill_ref}"),
        };
        // The skill takes the team as its argument — one entry form for
        // spawn bootstrap and manual joins alike.
        initial_prompt = format!("{initial_prompt} {team_name}");
    }
    if !prompt.is_empty() {
        initial_prompt = if initial_prompt.is_empty() {
            prompt.to_string()
        } else {
            format!("{initial_prompt}\n\n{prompt}")
        };
    }

    let mut session_id = String::new();
    if resolved_cli == "claude" {
        use crate::adapters::claude_bg;
        let mut extra_args: Vec<String> = Vec::new();
        if !model.is_empty() {
            extra_args.push("--model".to_string());
            extra_args.push(model.to_string());
        }
        let mut env: HashMap<String, String> = HashMap::new();
        env.insert("HIVE_TEAM".to_string(), team_name.to_string());
        env.insert("HIVE_MEMBER".to_string(), agent_name.to_string());
        for (key, value) in &extra_env {
            env.insert(key.clone(), value_as_env_string(value));
        }
        let job_id = claude_bg::spawn_job(
            &resolved_cwd,
            &format!("{team_name}.{agent_name}"),
            &initial_prompt,
            &extra_args,
            Some(&env),
            "claude",
        );
        let job_id = match job_id {
            Some(job_id) if !job_id.is_empty() => job_id,
            _ => bail!(
                "`claude --bg` returned no usable job id for '{agent_name}'; \
                 refusing to register a member without a job identity"
            ),
        };
        if claude_bg::wait_engine_entry(&job_id, crate::agent::AGENT_STARTUP_TIMEOUT).is_none() {
            claude_bg::stop_job(&job_id, "claude");
            bail!(
                "claude job '{job_id}' started but its engine never \
                 registered an inbox; refusing an undeliverable member"
            );
        }
        session_id = job_id;
    } else if resolved_cli == "codex" {
        use crate::adapters::codex_app_server;
        if !codex_app_server::spawn_daemon() {
            bail!("codex shared app-server daemon failed to start");
        }
        let _ = codex_app_server::ensure_dir_trusted(&resolved_cwd);
        let thread_id = codex_app_server::start_member_thread(
            &resolved_cwd,
            &format!("{team_name}.{agent_name}"),
            model,
        );
        let thread_id = match thread_id {
            Some(thread_id) if !thread_id.is_empty() => thread_id,
            _ => bail!("codex app-server refused to mint a thread for '{agent_name}'"),
        };
        if !initial_prompt.is_empty()
            && codex_app_server::send_to_thread(&thread_id, &initial_prompt).is_none()
        {
            bail!("codex thread '{thread_id}' refused the bootstrap turn");
        }
        session_id = thread_id;
    } else if resolved_cli == "grok" {
        if !model.is_empty() {
            bail!(
                "headless grok spawn cannot pick a model yet (the TUI flag \
                 has no verified ACP equivalent); omit --model"
            );
        }
        use crate::adapters::grok_leader;
        session_id = uuid4();
        if !grok_leader::create_member_session(team_name, agent_name, &session_id, &resolved_cwd) {
            bail!("grok leader for '{agent_name}' did not materialize the session");
        }
        if !initial_prompt.is_empty()
            && grok_leader::send_to_key(
                &grok_leader::member_key(team_name, agent_name),
                &initial_prompt,
            )
            .is_none()
        {
            grok_leader::kill_daemon_key(&grok_leader::member_key(team_name, agent_name));
            bail!("grok member '{agent_name}' refused the bootstrap prompt");
        }
    }

    let agent = Agent {
        name: agent_name.to_string(),
        team_name: team_name.to_string(),
        pane_id: String::new(),
        model: model.to_string(),
        prompt: String::new(),
        cwd: resolved_cwd,
        session_id: if session_id.is_empty() {
            None
        } else {
            Some(session_id)
        },
        spawned_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0),
        cli: resolved_cli,
    };
    t.upsert_agent(agent.clone());
    let ws = resolve_workspace(Some(&*t), false).unwrap_or_default();
    _remember_context(team_name, &ws, LEAD_AGENT_NAME);
    _registry_record_member(t, &agent);
    Ok(agent)
}

#[allow(clippy::too_many_arguments)]
pub fn spawn(
    agent_name: &str,
    model: &str,
    prompt: &str,
    cwd: &str,
    skill: &str,
    env: &[String],
    cli_name: Option<&str>,
    task_artifact: Option<&str>,
    team_arg: &str,
) {
    if task_artifact.is_some() && !prompt.is_empty() {
        fail("--task and --prompt are mutually exclusive (the task rides the message, not the birth prompt)");
    }
    let (team_name, t) = ok_or_fail(resolve_scoped_team(Some(team_arg), true));
    let team_name = team_name.expect("required resolve returned no team");
    let mut t = t.expect("required resolve returned no team");
    // A live display and a tmux-resident caller get a pane; anything else —
    // a ccd orch outside tmux, a team with no window — spawns engine-only.
    let headless = !(!t.tmux_window.is_empty() && tmux::is_inside_tmux());
    let use_prompt = if task_artifact.is_some() { "" } else { prompt };
    let use_skill = if task_artifact.is_some() {
        "hive:hive"
    } else {
        skill
    };
    let spawned: Result<Agent> = if headless {
        _spawn_headless_member(
            &mut t, &team_name, agent_name, model, use_prompt, cwd, use_skill, env, cli_name,
        )
    } else {
        let entries: Map<String, Value> = if env.is_empty() {
            Map::new()
        } else {
            _parse_entries(env)
        };
        let pairs: Vec<(String, String)> = entries
            .iter()
            .map(|(key, value)| (key.clone(), value_as_env_string(value)))
            .collect();
        spawn_team_agent(
            &mut t, &team_name, agent_name, model, use_prompt, cwd, use_skill, &pairs, cli_name,
        )
        .map(|agent| agent.clone())
    };
    let agent = match spawned {
        Ok(agent) => agent,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    let Some(task_artifact) = task_artifact else {
        if !agent.pane_id.is_empty() {
            println!("Agent '{agent_name}' spawned in pane {}", agent.pane_id);
        } else {
            println!(
                "Agent '{agent_name}' spawned headless (engine only — `hive attach {team_name}` renders it)"
            );
        }
        return;
    };

    let workspace = ok_or_fail(resolve_workspace(Some(&t), true));
    let _ = _ensure_team_hived(&mut t, &workspace);
    if agent.cli != "claude" {
        // A claude member's inbox is a queue: the task can land while the
        // bootstrap turn is still running and waits its turn. Only CLIs
        // whose delivery injects into a live TUI need the ready gate.
        let agents: HashSet<String> = [agent_name.to_string()].into_iter().collect();
        let not_ready = wait_for_peer_ready(&workspace, &team_name, &agents, 30.0, 0.5);
        if !not_ready.is_empty() {
            println!(
                "{}",
                py_dumps(
                    &json!({
                        "status": "spawn_ready_timeout",
                        "agent": agent_name,
                        "pane": agent.pane_id,
                        "hint": "pane spawned but did not reach ready within 30s; dispatch manually via `hive send`",
                    }),
                    true,
                    Some(2),
                    false
                )
            );
            std::process::exit(1);
        }
    }

    let task_path = std::fs::canonicalize(task_artifact)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| task_artifact.to_string());
    let task_name = Path::new(&task_path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let sender = _resolve_sender(None);
    let dispatch = request_send_payload(
        &workspace,
        &t,
        &sender,
        agent_name,
        &format!("task dispatch: {task_name}"),
        &task_path,
        "",
        "spawn-dispatch",
        false,
    );
    if let Err(exc) = dispatch {
        println!(
            "{}",
            py_dumps(
                &json!({
                    "status": "dispatch_failed",
                    "agent": agent_name,
                    "pane": agent.pane_id,
                    "error": exc.to_string(),
                    "hint": format!("member is ready but dispatch failed; retry: hive send {agent_name} ... --artifact {task_path}"),
                }),
                true,
                Some(2),
                false
            )
        );
        std::process::exit(1);
    }
    println!(
        "{}",
        py_dumps(
            &json!({
                "agent": agent_name,
                "pane": agent.pane_id,
                "task": task_path,
                "dispatched": true,
            }),
            true,
            Some(2),
            false
        )
    );
}

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

pub(crate) fn _parse_config_value(raw: &str) -> Value {
    let lowered = raw.trim().to_lowercase();
    if lowered == "true" {
        return Value::Bool(true);
    }
    if lowered == "false" {
        return Value::Bool(false);
    }
    if let Ok(int_value) = raw.trim().parse::<i64>() {
        return Value::Number(int_value.into());
    }
    if let Ok(float_value) = raw.trim().parse::<f64>() {
        if let Some(number) = serde_json::Number::from_f64(float_value) {
            return Value::Number(number);
        }
    }
    Value::String(raw.to_string())
}

pub fn config_get(key: &str) {
    let value = match crate::settings::get_setting(key) {
        Some(value) => value,
        None => std::process::exit(1),
    };
    match &value {
        Value::Object(_) | Value::Array(_) => {
            println!("{}", py_dumps(&value, true, Some(2), true));
        }
        _ => println!("{}", py_dumps(&value, true, None, false)),
    }
}

pub fn config_set(key: &str, value: &str) {
    let parsed = _parse_config_value(value);
    ok_or_fail(crate::settings::set_setting(key, parsed.clone()));
    println!("{}", py_dumps(&parsed, true, None, false));
}

pub fn config_unset(key: &str) {
    if !ok_or_fail(crate::settings::unset_setting(key)) {
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// inject / compact
// ---------------------------------------------------------------------------

pub fn inject_cmd(agent_name: &str, text: &str) {
    let (_, t) = ok_or_fail(resolve_scoped_team(None, true));
    let t = t.expect("required resolve returned no team");
    let agent = match t.get(agent_name) {
        Ok(agent) => agent,
        Err(_) => fail(&format!(
            "member '{agent_name}' not found in team '{}'",
            t.name
        )),
    };
    // Documented low-level bypass: raw composer keystrokes for every CLI, so
    // delivery paths (channel/RPC) can be debugged from outside themselves.
    if let Err(exc) = crate::agent::_submit_interactive_text(&agent.pane_id, text, &agent.cli) {
        fail(&exc.to_string());
    }
    let mut result = Map::new();
    result.insert("member".to_string(), Value::String(agent_name.to_string()));
    result.insert("action".to_string(), Value::String("inject".to_string()));
    result.insert("pane".to_string(), Value::String(agent.pane_id.clone()));
    result.insert("success".to_string(), Value::Bool(true));
    println!("{}", json_pretty(&Value::Object(result)));
}

/// Run `/compact` on the literal pane. Returns the compaction status.
fn _compact_target(target: &PaneTarget) -> String {
    if target.cli == "codex" || target.cli == "grok" {
        // Daemon-backed CLIs: an idle agent compacts via the dedicated RPC;
        // when busy we keystroke `/compact` into the CLI's own TUI so it can
        // refuse visibly instead of a silent background compaction.
        let status = if target.cli == "codex" {
            crate::adapters::codex_app_server::compact_pane(&target.pane_id)
        } else {
            crate::adapters::grok_leader::compact_pane(&target.pane_id)
        };
        if status != "compacted" {
            ok_or_fail(crate::agent::_submit_interactive_text(
                &target.pane_id,
                "/compact",
                &target.cli,
            ));
        }
        return status.to_string();
    }
    // claude (and embedded codex without a daemon): `/compact` is a TUI
    // slash command, so it must go through the composer.
    if let Err(exc) =
        crate::agent::_submit_interactive_text(&target.pane_id, "/compact", &target.cli)
    {
        fail(&exc.to_string());
    }
    "compacted".to_string()
}

pub fn compact_cmd(pane_id: &str) {
    // Resolve the pane straight from its tmux options — never re-resolve
    // through Team state (the cross-window same-name bug PR #8 fixed).
    let target = _resolve_pane_target(pane_id);
    let status = _compact_target(&target);
    let mut result = Map::new();
    result.insert(
        "member".to_string(),
        Value::String(target.member_label.clone()),
    );
    result.insert("action".to_string(), Value::String("compact".to_string()));
    result.insert("pane".to_string(), Value::String(target.pane_id.clone()));
    result.insert("status".to_string(), Value::String(status.clone()));
    result.insert("success".to_string(), Value::Bool(status == "compacted"));
    if !target.is_team_bound {
        // Pane-only compact has no team identity; `member` is the pane id.
        result.insert("team".to_string(), Value::Null);
    }
    println!("{}", json_pretty(&Value::Object(result)));
}

// ---------------------------------------------------------------------------
// layout
// ---------------------------------------------------------------------------

pub fn layout_cmd(preset: &str) {
    let (_, t) = ok_or_fail(resolve_scoped_team(None, true));
    let t = t.expect("required resolve returned no team");
    let window_target = if !t.tmux_window.is_empty() {
        t.tmux_window.clone()
    } else {
        tmux::get_current_window_target().unwrap_or_default()
    };
    if window_target.is_empty() {
        fail("Cannot determine tmux window target");
    }
    if preset == "auto" {
        match crate::layout::apply_adaptive(&window_target) {
            None => println!(
                "{}",
                py_dumps(
                    &json!({"layout": "", "window": window_target, "reason": "no-op"}),
                    true,
                    None,
                    false
                )
            ),
            Some(choice) => println!(
                "{}",
                py_dumps(
                    &json!({
                        "layout": choice.preset,
                        "orientation": choice.orientation,
                        "window": window_target,
                    }),
                    true,
                    None,
                    false
                )
            ),
        }
        return;
    }
    if preset == "main-vertical" || preset == "main-horizontal" {
        let dim = if preset == "main-vertical" {
            "main-pane-width"
        } else {
            "main-pane-height"
        };
        tmux::set_window_option(&window_target, dim, "50%");
    }
    tmux::select_layout(&window_target, preset);
    println!(
        "{}",
        py_dumps(
            &json!({"layout": preset, "window": window_target}),
            true,
            None,
            false
        )
    );
}

// ---------------------------------------------------------------------------
// flow
// ---------------------------------------------------------------------------

/// The `python3 -c` shell around the script: runpy like the Python-era
/// command, with FlowError surfaced as a clean CLI failure (`_fail`).
const FLOW_RUNNER: &str = r#"import runpy, sys
script = sys.argv[1]
sys.argv = sys.argv[1:]
try:
    runpy.run_path(script, run_name="__main__")
except SystemExit:
    raise
except Exception as exc:
    from hive.flow import FlowError
    if isinstance(exc, FlowError):
        sys.stderr.write(f"Error: {exc}\n")
        sys.exit(1)
    raise
"#;

pub fn flow_run_cmd(script: &str) {
    let _ = ok_or_fail(resolve_scoped_team(None, true));
    let script_path = std::fs::canonicalize(script)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| script.to_string());
    // ponytail: flow scripts are trusted Python programs; the binary
    // delegates to the interpreter instead of in-process runpy. Upgrade
    // path: a Rust-native flow DSL. The script's `from hive.flow import
    // agent` resolves against the materialized pylib client, which calls
    // back into this binary (hidden `flow-op` subcommands via $HIVE_BIN)
    // for every hive interaction.
    let pylib = ok_or_fail(crate::flow::materialize_pylib())
        .to_string_lossy()
        .into_owned();
    let pythonpath = match std::env::var("PYTHONPATH") {
        Ok(existing) if !existing.is_empty() => format!("{pylib}:{existing}"),
        _ => pylib,
    };
    std::env::set_var("PYTHONPATH", pythonpath);
    if let Ok(exe) = std::env::current_exe() {
        std::env::set_var("HIVE_BIN", exe);
    }
    execvp(
        "python3",
        &["-c".to_string(), FLOW_RUNNER.to_string(), script_path],
    );
}

// ---------------------------------------------------------------------------
// pr
// ---------------------------------------------------------------------------

// Replaces the bare index token in a window-status format with a conditional
// that renders `PR<n>` for windows carrying `@hive-pr`. `##I` is tmux's
// escaped literal `#I`, not the index token — left alone (the pathological
// `###I` triple is intentionally unsupported: a conservative no-replace beats
// corrupting a user's format).
pub(crate) const _PR_INDEX_TOKEN: &str = "#{?#{@hive-pr},PR#{@hive-pr},#I}";

fn _replace_index_tokens(format: &str) -> String {
    let bytes = format.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'#'
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'I'
            && (i == 0 || bytes[i - 1] != b'#')
        {
            out.extend_from_slice(_PR_INDEX_TOKEN.as_bytes());
            i += 2;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| format.to_string())
}

/// Per-window status format derived from the *global* value; None = skip.
pub(crate) fn _derive_pr_window_status(global_format: Option<&str>) -> Option<String> {
    let global_format = global_format?;
    if global_format.is_empty() {
        return None;
    }
    if global_format.contains("@hive-pr") {
        return None;
    }
    let derived = _replace_index_tokens(global_format);
    if derived == global_format {
        return None; // no replaceable #I
    }
    Some(derived)
}

pub fn pr_set_cmd(number: i64, plain: bool) {
    if !tmux::is_inside_tmux() {
        fail("must run inside tmux");
    }
    if number <= 0 {
        fail(&format!(
            "PR number must be a positive integer, got {number}"
        ));
    }
    let window = tmux::get_current_window_target().unwrap_or_default();
    if window.is_empty() {
        fail("cannot determine current window");
    }
    if tmux::get_window_option(&window, "hive-team")
        .filter(|team| !team.is_empty())
        .is_none()
    {
        fail(
            "current window is not a hive team window (no @hive-team); \
             run `hive pr set` from your team window",
        );
    }
    tmux::set_window_option(&window, "@hive-pr", &number.to_string());
    let mut display = Map::new();
    for option in ["window-status-format", "window-status-current-format"] {
        let global_format = tmux::get_global_window_option(option);
        match _derive_pr_window_status(global_format.as_deref()) {
            None => {
                let already = global_format
                    .as_deref()
                    .map(|f| !f.is_empty() && f.contains("@hive-pr"))
                    .unwrap_or(false);
                display.insert(
                    option.to_string(),
                    Value::String(
                        if already {
                            "already-global"
                        } else {
                            "skipped-no-index-token"
                        }
                        .to_string(),
                    ),
                );
            }
            Some(derived) => {
                tmux::set_window_option(&window, option, &derived);
                display.insert(option.to_string(), Value::String("derived".to_string()));
            }
        }
    }
    if plain {
        let summary = display
            .iter()
            .map(|(key, value)| match value {
                Value::String(s) => format!("{key}={s}"),
                other => format!("{key}={other}"),
            })
            .collect::<Vec<_>>()
            .join(", ");
        println!("window {window} labeled @hive-pr={number} ({summary})");
    } else {
        let result = json!({"window": window, "pr": number, "display": display});
        println!("{}", py_dumps(&result, true, Some(2), false));
    }
}

pub fn pr_clear_cmd(plain: bool) {
    if !tmux::is_inside_tmux() {
        fail("must run inside tmux");
    }
    let window = tmux::get_current_window_target().unwrap_or_default();
    if window.is_empty() {
        fail("cannot determine current window");
    }
    if tmux::get_window_option(&window, "hive-team")
        .filter(|team| !team.is_empty())
        .is_none()
    {
        fail(
            "current window is not a hive team window (no @hive-team); \
             run `hive pr clear` from your team window",
        );
    }
    let previous = tmux::get_window_option(&window, "hive-pr");
    tmux::clear_window_option(&window, "@hive-pr");
    if !plain {
        let previous_value = match &previous {
            Some(previous) => Value::String(previous.clone()),
            None => Value::Null,
        };
        println!(
            "{}",
            py_dumps(
                &json!({"window": window, "previous": previous_value}),
                true,
                Some(2),
                false
            )
        );
    } else if previous.as_deref().map_or(false, |p| !p.is_empty()) {
        println!(
            "window {window} cleared @hive-pr={}",
            previous.unwrap_or_default()
        );
    } else {
        println!("window {window} had no @hive-pr stamp to clear");
    }
}

// ---------------------------------------------------------------------------
// attach
// ---------------------------------------------------------------------------

fn _attach_launcher(cli_name: &str, quoted_sid: &str) -> Option<String> {
    match cli_name {
        "claude" => Some(format!("hive claude --resume {quoted_sid}")),
        "codex" => Some(format!("hive codex resume {quoted_sid}")),
        "grok" => Some(format!("hive grok --resume {quoted_sid}")),
        _ => None,
    }
}

fn _member_attach_command(cli_name: &str, session_id: &str, cwd: &str) -> String {
    let quoted_sid = shlex_quote(session_id);
    let launch = _attach_launcher(cli_name, &quoted_sid).expect("attachable cli");
    let cwd = if cwd.is_empty() {
        getcwd()
    } else {
        cwd.to_string()
    };
    if cli_name == "claude" && crate::adapters::claude_bg::job_row(session_id, "claude").is_none() {
        // An interactive session (desktop ccd, joined session) must NOT be
        // resumed — the launcher's resume lane would mint a forked bg job
        // that steals the member's deliveries. Render the transcript
        // read-only instead — and without the resume-hint tail, which would
        // otherwise re-adopt any same-named job on viewer exit.
        return format!("cd {} && hive view {quoted_sid}", shlex_quote(&cwd));
    }
    format!(
        "cd {} && {launch}; hive resume-hint {cli_name} 2>/dev/null || true",
        shlex_quote(&cwd)
    )
}

/// Build a window for the team: one attach pane per member, tiled.
///
/// Returns (window_target, attached_member_names, skipped_member_names).
fn _materialize_team_display(entry: &Map<String, Value>) -> (String, Vec<String>, Vec<String>) {
    let team = map_str(entry, "team");
    let ws = map_str(entry, "workspace");
    let members: Vec<Map<String, Value>> = entry
        .get("members")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.as_object().cloned())
                .collect()
        })
        .unwrap_or_default();
    let members = _sorted_member_rows(members);
    let attachable_idx: Vec<usize> = members
        .iter()
        .enumerate()
        .filter(|(_, member)| {
            truthy(member.get("sessionId"))
                && matches!(map_str(member, "cli").as_str(), "claude" | "codex" | "grok")
        })
        .map(|(index, _)| index)
        .collect();
    let mut skipped: Vec<String> = members
        .iter()
        .enumerate()
        .filter(|(index, _)| !attachable_idx.contains(index))
        .map(|(_, member)| map_str(member, "name"))
        .collect();
    if attachable_idx.is_empty() {
        fail(&format!(
            "team '{team}' has no attachable members (no recorded engine identity)"
        ));
    }

    let session_name = tmux::get_current_session_name()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "hive".to_string());
    if !tmux::has_session(&session_name) {
        let _ = tmux::new_session(&session_name, 200, 50);
    }
    let anchor_cwd = {
        let first = &members[attachable_idx[0]];
        let cwd = map_str(first, "cwd");
        if cwd.is_empty() {
            getcwd()
        } else {
            cwd
        }
    };
    let (window, first_pane) =
        tmux::new_window(&session_name, &team, Some(&anchor_cwd), true).unwrap_or_default();
    if window.is_empty() || first_pane.is_empty() {
        fail("failed to create a window for the team");
    }

    tmux::configure_hive_window(&window);
    tmux::set_window_option(&window, "@hive-team", &team);
    tmux::set_window_option(&window, "@hive-workspace", &ws);
    tmux::set_window_option(&window, "@hive-created", &map_str(entry, "createdAt"));

    let mut attached: Vec<String> = Vec::new();
    let mut prev_pane = first_pane.clone();
    for (i, index) in attachable_idx.iter().enumerate() {
        let member = &members[*index];
        let name = map_str(member, "name");
        let cli_name = map_str(member, "cli");
        let cwd = map_str(member, "cwd");
        let pane = if i == 0 {
            first_pane.clone()
        } else {
            let split = ok_or_fail(tmux::split_window(
                &prev_pane,
                crate::layout::split_horizontal(&window, i + 1),
                None,
                true,
                if cwd.is_empty() { None } else { Some(&cwd) },
            ));
            if split.is_empty() {
                skipped.push(name);
                continue;
            }
            split
        };
        tmux::set_pane_title(&pane, &format!("[{name}]"));
        tmux::tag_pane(&pane, "agent", &name, &team, &cli_name, "");
        if !ws.is_empty() {
            let _ = crate::context::save_context_for_pane(&pane, &team, &ws, &name);
        }
        ok_or_fail(tmux::send_keys(
            &pane,
            &_member_attach_command(&cli_name, &map_str(member, "sessionId"), &cwd),
            true,
        ));
        attached.push(name);
        prev_pane = pane;
    }

    let _ = crate::layout::apply_adaptive(&window);
    let _ = crate::registry::set_display(&team, &tmux::get_window_id(&window).unwrap_or_default());
    (window, attached, skipped)
}

pub fn attach_cmd(team_name: &str) {
    let entry = match crate::registry::load(team_name) {
        Some(entry) => entry,
        None => fail(&format!("team '{team_name}' not found (see `hive ls`)")),
    };

    let mut window = crate::team::_find_team_window(team_name, "")
        .map(|(window, _)| window)
        .unwrap_or_default();
    let mut built = false;
    if window.is_empty() {
        let (materialized, _attached, skipped) = _materialize_team_display(&entry);
        window = materialized;
        built = true;
        for name in skipped {
            eprintln!("! {name}: no attachable engine identity — not rendered");
        }
    }
    let ws = map_str(&entry, "workspace");
    if !ws.is_empty() {
        if let Ok(mut t) = Team::load(team_name, "") {
            let _ = _ensure_team_hived(&mut t, &ws);
        }
    }

    if tmux::is_inside_tmux() {
        tmux::select_window(&window);
        println!("{} {window}", if built { "built" } else { "found" });
        return;
    }
    let session = match window.split_once(':') {
        Some((session, _)) => session.to_string(),
        None => window.clone(),
    };
    ok_or_fail(tmux::exec_attach(&session, &window));
}

// ---------------------------------------------------------------------------
// thread / capture
// ---------------------------------------------------------------------------

pub fn thread(message_id: &str) {
    let (_, t) = ok_or_fail(resolve_scoped_team(None, true));
    let mut t = t.expect("required resolve returned no team");
    let ws = ok_or_fail(resolve_workspace(Some(&t), true));
    let _ = _ensure_team_hived(&mut t, &ws);
    let payload = crate::hived::request_thread(&ws, message_id);
    let mut payload = match payload {
        Some(payload) if !payload.is_empty() => payload,
        _ => fail("hived unavailable"),
    };
    if payload.get("ok") == Some(&Value::Bool(false)) {
        let error = match payload.get("error") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => "thread lookup failed".to_string(),
        };
        fail(&error);
    }
    payload.shift_remove("ok");
    println!("{}", json_pretty(&Value::Object(payload)));
}

pub fn capture(member_name: &str, lines: i64) {
    let (_, t) = ok_or_fail(resolve_scoped_team(None, true));
    let t = t.expect("required resolve returned no team");
    match t.get(member_name) {
        Ok(agent) => {
            let text = ok_or_fail(agent.capture(lines.max(0) as u32));
            println!("{text}");
        }
        Err(_) => fail(&format!(
            "member '{member_name}' not found in team '{}'",
            t.name
        )),
    }
}

// ---------------------------------------------------------------------------
// cvim / vim / vfork / hfork (human helpers)
// ---------------------------------------------------------------------------

fn _cvim_binary() -> PathBuf {
    // The toolkit is embedded in this binary and materialized to
    // `$HIVE_HOME/core_assets/cvim/` at first use; HIVE_CORE_ASSETS stays as
    // the dev escape hatch pointing at an external asset tree.
    let overridden = env_string("HIVE_CORE_ASSETS");
    if !overridden.is_empty() {
        return PathBuf::from(overridden).join("cvim/bin/cvim-command");
    }
    match crate::cvim::materialize_assets() {
        Ok(path) => path,
        Err(err) => fail(&format!("cannot materialize cvim assets: {err}")),
    }
}

fn _exec_cvim(mode: &str, args: &[String]) -> ! {
    // The script reads TMUX_PANE for its reply pane; inside a codex tool env
    // that variable is the shared daemon's (stripped) one, so hand it the
    // thread-resolved pane identity instead.
    if let Some(pane) = tmux::get_current_pane_id().filter(|pane| !pane.is_empty()) {
        std::env::set_var("TMUX_PANE", pane);
    }
    // The script's helper callbacks are hidden subcommands of this binary
    // (the Python original exported HIVE_PYTHON for the same reason); a bare
    // `hive` on the pane's PATH is only the script's fallback.
    if let Ok(exe) = std::env::current_exe() {
        std::env::set_var("HIVE_BIN", exe);
    }
    let mut argv: Vec<String> = vec![
        _cvim_binary().to_string_lossy().into_owned(),
        mode.to_string(),
    ];
    argv.extend(args.iter().cloned());
    execvp("bash", &argv);
}

pub fn cvim_cmd(args: &[String]) {
    _exec_cvim("cvim", args);
}

pub fn vim_cmd(args: &[String]) {
    _exec_cvim("vim", args);
}

fn _exec_fork_split(split: &str, args: &[String]) {
    // Thread-aware pane resolution: in a codex tool env TMUX_PANE is gone.
    let reply_pane = tmux::get_current_pane_id().unwrap_or_default();
    let mut command = std::process::Command::new("hive");
    command
        .arg("fork")
        .arg("-s")
        .arg(split)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let _ = command.spawn();
    if !reply_pane.is_empty() {
        tmux::run_shell_detached(&format!(
            "sleep 0.2 && tmux send-keys -t {} Escape",
            shlex_quote(&reply_pane)
        ));
    }
}

pub fn vfork_cmd(args: &[String]) {
    _exec_fork_split("v", args);
}

pub fn hfork_cmd(args: &[String]) {
    _exec_fork_split("h", args);
}

// ---------------------------------------------------------------------------
// notify
// ---------------------------------------------------------------------------

pub fn notify_cmd(message: &str) {
    let target_pane = _resolve_target_pane();
    let payload = ok_or_fail(crate::notify_ui::notify(message, &target_pane, ""));
    let value = serde_json::to_value(&payload).unwrap_or(Value::Null);
    println!("{}", py_dumps(&value, true, None, false));
}

// ---------------------------------------------------------------------------
// plugin
// ---------------------------------------------------------------------------

fn _render_plugin_mutation_result(action: &str, payload: &Map<String, Value>) -> String {
    let name = map_str(payload, "name");
    let mut lines = vec![format!("Plugin '{name}' {action}.")];
    let install_root = map_str(payload, "installRoot");
    let commands: Vec<String> = payload
        .get("commands")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| match item {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    let mut command_names: Vec<String> = Vec::new();
    for item in &commands {
        let path = Path::new(item);
        let label = if path.extension().and_then(|e| e.to_str()) == Some("md") {
            path.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            path.file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        if !command_names.contains(&label) {
            command_names.push(label);
        }
    }

    if !install_root.is_empty() {
        lines.push(format!("  install root: {install_root}"));
    }
    if !command_names.is_empty() {
        lines.push(format!("  commands: {}", command_names.join(", ")));
    }
    lines.push(
        "  note: existing Codex panes may not reload plugin settings dynamically; \
         restart them if old hooks or commands still run."
            .to_string(),
    );
    lines.join("\n")
}

pub fn plugin_list(plain: bool) {
    let rows = ok_or_fail(crate::plugin_manager::list_plugins());
    if !plain {
        println!("{}", py_dumps(&Value::Array(rows), false, None, false));
        return;
    }
    let enabled_count = rows.iter().filter(|row| truthy(row.get("enabled"))).count();
    println!("Plugins ({enabled_count}/{} enabled)", rows.len());
    if rows.is_empty() {
        return;
    }
    let name_of = |row: &Value| {
        row.get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let name_width = rows.iter().map(|row| name_of(row).len()).max().unwrap_or(0);
    for row in &rows {
        let status = if truthy(row.get("enabled")) {
            "enabled"
        } else {
            "disabled"
        };
        let description = row.get("description").and_then(Value::as_str).unwrap_or("");
        println!(
            "  {:<name_width$}  {status:<8}  {description}",
            name_of(row)
        );
    }
}

pub fn plugin_ls(plain: bool) {
    plugin_list(plain);
}

pub fn plugin_enable(name: &str, plain: bool) {
    match crate::plugin_manager::enable_plugin(name) {
        Ok(payload) => {
            if !plain {
                println!("{}", py_dumps(&payload, false, None, false));
                return;
            }
            let empty = Map::new();
            let map = payload.as_object().unwrap_or(&empty);
            println!("{}", _render_plugin_mutation_result("enabled", map));
        }
        Err(e) => fail(&e.to_string()),
    }
}

pub fn plugin_disable(name: &str, plain: bool) {
    match crate::plugin_manager::disable_plugin(name, false) {
        Ok(payload) => {
            if !plain {
                println!("{}", py_dumps(&payload, false, None, false));
                return;
            }
            let empty = Map::new();
            let map = payload.as_object().unwrap_or(&empty);
            println!("{}", _render_plugin_mutation_result("disabled", map));
        }
        Err(e) => fail(&e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// codex managed launch
// ---------------------------------------------------------------------------

// codex subcommands that are not an interactive TUI launch: hive leaves these
// completely untouched (raw codex). Kept in sync with `codex --help`.
const _CODEX_PASSTHROUGH_SUBCOMMANDS: &[&str] = &[
    "exec",
    "e",
    "review",
    "login",
    "logout",
    "mcp",
    "plugin",
    "mcp-server",
    "app-server",
    "remote-control",
    "app",
    "completion",
    "update",
    "doctor",
    "sandbox",
    "debug",
    "apply",
    "a",
    "cloud",
    "exec-server",
    "features",
    "help",
];

// Non-interactive surfaces: --help/--version never start a session.
const _CODEX_PASSTHROUGH_FLAGS: &[&str] = &["-h", "--help", "-V", "--version"];

// Global codex options that consume the following token as their value, so the
// subcommand scan does not mistake that value for the subcommand. `--opt=value`
// and `-Cvalue` are self-contained and handled separately.
const _CODEX_VALUE_OPTS: &[&str] = &[
    "-c",
    "--config",
    "-m",
    "--model",
    "-C",
    "--cd",
    "--remote",
    "--remote-auth-token-env",
    "--enable",
    "--disable",
    "-p",
    "--profile",
    "-a",
    "--ask-for-approval",
    "-s",
    "--sandbox",
];

/// Index of the first non-option token in `args` — the subcommand, if any.
pub(crate) fn _codex_subcommand_index(args: &[String]) -> Option<usize> {
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            return if i + 1 < args.len() {
                Some(i + 1)
            } else {
                None
            };
        }
        if a.starts_with('-') {
            i += if _CODEX_VALUE_OPTS.contains(&a.as_str()) && !a.contains('=') {
                2
            } else {
                1
            };
            continue;
        }
        return Some(i);
    }
    None
}

/// First positional token after the subcommand (e.g. resume's SESSION_ID).
pub(crate) fn _codex_positional_after(args: &[String], sub_index: usize) -> Option<String> {
    let mut i = sub_index + 1;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            return args.get(i + 1).cloned();
        }
        if a.starts_with('-') {
            i += if _CODEX_VALUE_OPTS.contains(&a.as_str()) && !a.contains('=') {
                2
            } else {
                1
            };
            continue;
        }
        return Some(a.clone());
    }
    None
}

/// Value of the first `--opt value` / `--opt=value` occurrence in `args`.
///
/// A following token starting with `-` is the next flag, not this option's
/// value: the option is read as bare (None) rather than swallowing it.
pub(crate) fn _codex_opt_value(args: &[String], names: &[&str]) -> Option<String> {
    for (i, a) in args.iter().enumerate() {
        if names.contains(&a.as_str()) {
            let next = args.get(i + 1).map(String::as_str).unwrap_or("");
            return if !next.is_empty() && !next.starts_with('-') {
                Some(next.to_string())
            } else {
                None
            };
        }
        for name in names {
            let prefix = if name.starts_with("--") {
                format!("{name}=")
            } else {
                (*name).to_string()
            };
            if a.starts_with(&prefix) && a != name {
                return Some(a[prefix.len()..].to_string());
            }
        }
    }
    None
}

/// `<team>.<member>` when the pane carries hive member tags, else None.
pub(crate) fn _pane_member_label_via(
    get: impl Fn(&str, &str) -> Option<String>,
    pane: &str,
) -> Option<String> {
    let team = get(pane, "hive-team").unwrap_or_default();
    let agent = get(pane, "hive-agent").unwrap_or_default();
    if !team.is_empty() && !agent.is_empty() {
        Some(format!("{team}.{agent}"))
    } else {
        None
    }
}

fn _pane_member_label(pane: &str) -> Option<String> {
    _pane_member_label_via(|target, key| tmux::get_window_option(target, key), pane)
}

/// Launcher-minted job/thread name: member identity, or a pane placeholder.
pub(crate) fn _mint_name(label: Option<String>, pane: &str) -> String {
    label.unwrap_or_else(|| {
        let stripped = pane.replace('%', "");
        format!(
            "hive-{}",
            if stripped.is_empty() {
                "pane"
            } else {
                stripped.as_str()
            }
        )
    })
}

pub(crate) fn _codex_pane_thread_name(pane: &str) -> String {
    _mint_name(_pane_member_label(pane), pane)
}

/// True when the user already passed codex's cwd flag (-C / --cd, any form).
fn _codex_args_set_cwd(args: &[String]) -> bool {
    args.iter()
        .any(|a| a == "--cd" || a.starts_with("--cd=") || a.starts_with("-C"))
}

fn _codex_raw(args: &[String]) -> ! {
    execvp("codex", args)
}

/// Replace this process with codex on the shared app-server daemon.
///
/// Degrades to raw `codex` (embedded, status quo) whenever the managed path
/// cannot apply — the caller never ends up worse than plain codex.
fn _exec_codex_managed(args: &[String]) -> ! {
    use crate::adapters::codex_app_server;

    let pane = {
        let env_pane = env_string("TMUX_PANE");
        if !env_pane.is_empty() {
            env_pane
        } else {
            tmux::get_current_pane_id().unwrap_or_default()
        }
    };
    if pane.is_empty() || !tmux::is_inside_tmux() {
        _codex_raw(args); // hive needs a tmux pane to bind a thread to
    }
    let sub_index = _codex_subcommand_index(args);
    let sub = sub_index.map(|i| args[i].as_str());
    if let Some(sub) = sub {
        if _CODEX_PASSTHROUGH_SUBCOMMANDS.contains(&sub) {
            _codex_raw(args); // a management subcommand, not an interactive TUI launch
        }
    }
    if args
        .iter()
        .any(|a| _CODEX_PASSTHROUGH_FLAGS.contains(&a.as_str()))
    {
        _codex_raw(args); // --help/--version never start a session
    }
    if args
        .iter()
        .any(|a| a == "--remote" || a.starts_with("--remote="))
    {
        _codex_raw(args); // caller already chose an endpoint
    }
    if !codex_app_server::spawn_daemon() {
        _codex_raw(args); // daemon would not bind — fall back to embedded codex
    }
    let cwd = _codex_opt_value(args, &["--cd", "-C"])
        .filter(|value| !value.is_empty())
        .unwrap_or_else(getcwd);
    let _ = codex_app_server::ensure_dir_trusted(&cwd);
    let sock = codex_app_server::shared_socket_path();
    // -c check_for_update_on_startup=false mirrors the hive-spawned path so a
    // managed launch never drops the user into codex's npm self-update prompt.
    let mut argv: Vec<String> = vec![
        "-c".to_string(),
        "check_for_update_on_startup=false".to_string(),
        "--remote".to_string(),
        format!("unix://{}", sock.to_string_lossy()),
    ];
    if !_codex_args_set_cwd(args) {
        argv.push("--cd".to_string());
        argv.push(cwd.clone());
    }

    if sub == Some("resume") {
        let sid = _codex_positional_after(args, sub_index.expect("sub implies index"));
        match sid {
            Some(sid) => {
                let _ = codex_app_server::write_pane_thread(&pane, &sid, &cwd);
            }
            None => {
                // Picker / --last: the chosen thread is unknowable up front. A
                // stale record must not keep routing hive at the previous thread.
                let _ = codex_app_server::clear_pane_thread(&pane);
            }
        }
        argv.extend(args.iter().cloned());
        execvp("codex", &argv);
    }
    if sub == Some("fork") {
        let sub_index = sub_index.expect("sub implies index");
        let source = _codex_positional_after(args, sub_index);
        let forked = source.as_deref().and_then(|source| {
            codex_app_server::fork_member_thread(source, &_codex_pane_thread_name(&pane))
        });
        if let (Some(source), Some(forked)) = (source, forked) {
            let _ = codex_app_server::write_pane_thread(&pane, &forked, &cwd);
            let mut rewritten: Vec<String> = args.to_vec();
            rewritten[sub_index] = "resume".to_string();
            if let Some(offset) = rewritten
                .iter()
                .skip(sub_index + 1)
                .position(|a| *a == source)
            {
                rewritten[sub_index + 1 + offset] = forked;
            }
            argv.extend(rewritten);
            execvp("codex", &argv);
        }
        // No source id, or the fork RPC failed: let codex fork on its own —
        // remote-attached but unrecorded, so clear any stale pane record.
        let _ = codex_app_server::clear_pane_thread(&pane);
        argv.extend(args.iter().cloned());
        execvp("codex", &argv);
    }
    // Interactive launch — no subcommand, flags only, or a bare [PROMPT]:
    // mint the pane's thread so it is born with an identity hive can read,
    // deliver to, and resume. A trailing prompt rides `resume`'s own [PROMPT]
    // positional unchanged.
    let minted = codex_app_server::start_member_thread(
        &cwd,
        &_codex_pane_thread_name(&pane),
        &_codex_opt_value(args, &["--model", "-m"]).unwrap_or_default(),
    );
    if let Some(minted) = minted {
        let _ = codex_app_server::write_pane_thread(&pane, &minted, &cwd);
        argv.push("resume".to_string());
        argv.push(minted);
        argv.extend(args.iter().cloned());
        execvp("codex", &argv);
    }
    // Mint failed (daemon just died?): remote attach unrecorded — degraded,
    // and a stale record must not point hive at a thread this TUI won't run.
    let _ = codex_app_server::clear_pane_thread(&pane);
    argv.extend(args.iter().cloned());
    execvp("codex", &argv);
}

pub fn codex_cmd(args: &[String]) {
    _exec_codex_managed(args);
}

// ---------------------------------------------------------------------------
// claude managed launch
// ---------------------------------------------------------------------------

// claude subcommands that are not an interactive TUI launch: raw passthrough.
// Hidden subcommands are only recognized at argv[1], so args[0] is the one
// place a subcommand can sit.
const _CLAUDE_PASSTHROUGH_SUBCOMMANDS: &[&str] = &[
    "agents",
    "attach",
    "logs",
    "stop",
    "respawn",
    "rm",
    "mcp",
    "plugin",
    "config",
    "doctor",
    "update",
    "install",
    "migrate-installer",
    "setup-token",
    "api",
    "bg-spare",
    "bg-pty-host",
    "daemon",
    "help",
];

// Non-interactive surfaces: --help/--version never start a session.
const _CLAUDE_PASSTHROUGH_FLAGS: &[&str] = &["-h", "--help", "-v", "--version"];

// Launch shapes the bg mapping cannot represent: headless print mode
// (rejected by --bg upstream), an explicit --bg the caller manages itself,
// and -c/--continue (which session it continues is unknowable up front).
const _CLAUDE_RAW_MODE_FLAGS: &[&str] = &["-p", "--print", "--bg", "-c", "--continue"];

/// (resume flag present, its value). `-r`/`--resume` take an optional value;
/// a bare flag opens claude's picker.
pub(crate) fn _claude_resume_arg(args: &[String]) -> (bool, Option<String>) {
    for (i, a) in args.iter().enumerate() {
        if a == "-r" || a == "--resume" {
            if let Some(next) = args.get(i + 1) {
                if !next.starts_with('-') {
                    return (true, Some(next.clone()));
                }
            }
            return (true, None);
        }
        if let Some(rest) = a.strip_prefix("--resume=") {
            return (
                true,
                if rest.is_empty() {
                    None
                } else {
                    Some(rest.to_string())
                },
            );
        }
    }
    (false, None)
}

pub(crate) fn _claude_pane_job_name(pane: &str) -> String {
    _mint_name(_pane_member_label(pane), pane)
}

/// Replace this process with a watch loop keeping the pane attached to its
/// bg job's engine. Never returns.
fn _claude_attach_loop(job_id: &str) -> ! {
    let quoted = shlex_quote(job_id);
    let script = format!(
        "set -m\n\
         while :; do\n  \
         t0=$(date +%s)\n  \
         claude attach {quoted}\n  \
         rc=$?\n  \
         if [ $rc -ge 1 ] && [ $rc -le 128 ] && [ $(( $(date +%s) - t0 )) -lt 5 ]; then\n    \
         exit $rc\n  \
         fi\n  \
         echo \"hive: viewer detached from job {quoted}; \"\\\n\
         \"reattaching in 1s (Ctrl-C to stay detached)\" >&2\n  \
         sleep 1 || exit 0\n\
         done\n"
    );
    let env = crate::adapters::claude_bg::bg_env(None);
    let err = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .env_clear()
        .envs(&env)
        .exec();
    eprintln!("Error: {err}");
    std::process::exit(1);
}

fn _claude_raw(args: &[String]) -> ! {
    execvp("claude", args)
}

/// Run claude as a hive-managed background job with this pane attached.
///
/// Degrades to raw `claude` whenever the managed path cannot apply — the
/// caller never ends up worse than plain claude.
fn _exec_claude_managed(args: &[String]) -> ! {
    use crate::adapters::claude_bg;

    if args.len() == 1 && args[0] == "channel-server" {
        // Tombstone for the retired hive-channel plugin's MCP entry: exec'ing
        // this into claude would feed it a garbage subcommand every session.
        eprintln!(
            "Error: the hive-channel plugin is retired (claude delivery now \
             uses the session's own cross-session inbox). Remove it with: \
             claude plugin uninstall hive-channel@hive"
        );
        std::process::exit(1);
    }
    let pane = env_string("TMUX_PANE");
    if pane.is_empty() || env_string("TMUX").is_empty() {
        _claude_raw(args); // hive needs a real tmux pane to bind a job to
    }
    if let Some(first) = args.first() {
        if _CLAUDE_PASSTHROUGH_SUBCOMMANDS.contains(&first.as_str()) {
            _claude_raw(args); // a management subcommand, not an interactive TUI launch
        }
    }
    if args
        .iter()
        .any(|a| _CLAUDE_PASSTHROUGH_FLAGS.contains(&a.as_str()))
    {
        _claude_raw(args);
    }
    if args
        .iter()
        .any(|a| _CLAUDE_RAW_MODE_FLAGS.contains(&a.as_str()))
    {
        _claude_raw(args);
    }

    let (resume_present, resume_val) = _claude_resume_arg(args);
    if resume_present && resume_val.is_none() {
        _claude_raw(args); // picker: the chosen session is unknowable up front
    }
    let cwd = getcwd();

    if let Some(resume_val) = resume_val
        .as_deref()
        .filter(|value| claude_bg::looks_like_job_id(value))
    {
        let mut engine = claude_bg::engine_session_for_job(resume_val);
        if engine.is_none() && claude_bg::job_exists(resume_val, "claude") {
            engine = claude_bg::ensure_engine(resume_val, None, "claude");
        }
        if engine.is_some() || claude_bg::job_exists(resume_val, "claude") {
            let session_id = engine.map(|e| e.session_id).unwrap_or_default();
            let _ = claude_bg::write_pane_job(&pane, resume_val, &session_id, &cwd);
            _claude_attach_loop(resume_val);
        }
        // Not a known job: fall through and treat the value as a session id.
    }

    let user_named = args
        .iter()
        .any(|a| a == "--name" || a.starts_with("--name="));
    let name = if user_named {
        String::new()
    } else {
        _claude_pane_job_name(&pane)
    };
    let job_id = claude_bg::spawn_job(&cwd, &name, "", args, None, "claude");
    let job_id = match job_id {
        Some(job_id) if !job_id.is_empty() => job_id,
        _ => {
            eprintln!("hive: `claude --bg` failed; launching plain claude");
            _claude_raw(args);
        }
    };
    let engine = claude_bg::wait_engine_entry(&job_id, 10.0);
    let _ = claude_bg::write_pane_job(
        &pane,
        &job_id,
        &engine.map(|e| e.session_id).unwrap_or_default(),
        &cwd,
    );
    _claude_attach_loop(&job_id);
}

pub fn claude_cmd(args: &[String]) {
    _exec_claude_managed(args);
}

// ---------------------------------------------------------------------------
// grok managed launch
// ---------------------------------------------------------------------------

// grok subcommands that are not an interactive TUI launch: hive leaves these
// completely untouched (raw grok). A subcommand is always the first token; a
// prompt is the only other thing that can sit there.
const _GROK_PASSTHROUGH_SUBCOMMANDS: &[&str] = &[
    "agent",
    "completions",
    "dashboard",
    "doctor",
    "du",
    "export",
    "help",
    "inspect",
    "leader",
    "login",
    "logout",
    "mcp",
    "memory",
    "models",
    "plugin",
    "sessions",
    "setup",
    "trace",
    "update",
    "version",
    "worktree",
    "wrap",
];

// Non-interactive surfaces: --help/--version never start a session.
const _GROK_PASSTHROUGH_FLAGS: &[&str] = &["-h", "--help", "-V", "--version"];

/// Value of the first `--opt value` / `--opt=value` occurrence in `args`.
///
/// A following token starting with `-` is the next flag, not this option's
/// value: `--resume -m grok-4` resumes grok's own picker instead of recording
/// `-m` as the pane's session id.
pub(crate) fn _grok_opt_value(args: &[String], names: &[&str]) -> Option<String> {
    for (i, a) in args.iter().enumerate() {
        if names.contains(&a.as_str()) {
            let next = args.get(i + 1).map(String::as_str).unwrap_or("");
            return if !next.is_empty() && !next.starts_with('-') {
                Some(next.to_string())
            } else {
                None
            };
        }
        for name in names {
            if let Some(rest) = a.strip_prefix(&format!("{name}=")) {
                return Some(rest.to_string());
            }
        }
    }
    None
}

/// (session id this launch will run, whether hive must pass --session-id).
pub(crate) fn _grok_launch_session(args: &[String]) -> (Option<String>, bool) {
    let explicit = _grok_opt_value(args, &["--session-id", "-s"]);
    if explicit.as_deref().map_or(false, |value| !value.is_empty()) {
        return (explicit, false);
    }
    if args
        .iter()
        .any(|a| a == "--resume" || a.starts_with("--resume="))
        && !args.iter().any(|a| a == "--fork-session")
    {
        return (_grok_opt_value(args, &["--resume"]), false);
    }
    (Some(uuid4()), true)
}

fn _grok_raw(args: &[String]) -> ! {
    execvp("grok", args)
}

/// Replace this process with grok, attached to a per-pane leader daemon.
///
/// Degrades to raw `grok` whenever the managed path cannot apply — the
/// caller never ends up worse than plain grok.
fn _exec_grok_managed(args: &[String]) -> ! {
    use crate::adapters::grok_leader;

    let pane = {
        let env_pane = env_string("TMUX_PANE");
        if !env_pane.is_empty() {
            env_pane
        } else {
            tmux::get_current_pane_id().unwrap_or_default()
        }
    };
    if pane.is_empty() || !tmux::is_inside_tmux() {
        _grok_raw(args); // hive needs a tmux pane to bind a daemon to
    }
    if let Some(first) = args.first() {
        if _GROK_PASSTHROUGH_SUBCOMMANDS.contains(&first.as_str()) {
            _grok_raw(args); // a management subcommand, not an interactive TUI launch
        }
    }
    if args
        .iter()
        .any(|a| _GROK_PASSTHROUGH_FLAGS.contains(&a.as_str()))
    {
        _grok_raw(args); // --help/--version never start a session
    }
    if !grok_leader::spawn_daemon(&pane) {
        // A raw grok drives whatever session it likes; leaving an earlier
        // record in place would have hive resolve that stale id as this pane's.
        let _ = std::fs::remove_file(grok_leader::pane_session_path(&pane));
        eprintln!("hive: grok leader did not start; launching plain grok");
        _grok_raw(args);
    }
    let (session_id, pass_flag) = _grok_launch_session(args);
    let mut argv: Vec<String> = vec![
        "--leader".to_string(),
        "--leader-socket".to_string(),
        grok_leader::pane_socket_path(&pane)
            .to_string_lossy()
            .into_owned(),
    ];
    if pass_flag {
        argv.push("--session-id".to_string());
        argv.push(session_id.clone().unwrap_or_default());
    }
    if let Some(session_id) = session_id.as_deref().filter(|value| !value.is_empty()) {
        let _ = grok_leader::write_pane_session(&pane, session_id, &getcwd());
    }
    argv.extend(args.iter().cloned());
    execvp("grok", &argv);
}

pub fn grok_cmd(args: &[String]) {
    _exec_grok_managed(args);
}

// ---------------------------------------------------------------------------
// ccd
// ---------------------------------------------------------------------------

pub fn ccd_ls_cmd() {
    let members = _live_member_pids();
    let mut rows: Vec<Value> = Vec::new();
    for s in crate::adapters::claude_sessions::list_sessions() {
        let mut row = Map::new();
        row.insert("name".to_string(), Value::String(s.name.clone()));
        row.insert("title".to_string(), Value::String(s.title.clone()));
        row.insert("pid".to_string(), Value::Number(s.pid.into()));
        row.insert("kind".to_string(), Value::String(s.kind.clone()));
        row.insert("cwd".to_string(), Value::String(s.cwd.clone()));
        if let Some((team, agent)) = members.get(&s.pid) {
            row.insert(
                "member".to_string(),
                Value::String(format!("{team}.{agent}")),
            );
        }
        rows.push(Value::Object(row));
    }
    println!("{}", json_pretty(&json!({ "sessions": rows })));
}

// ---------------------------------------------------------------------------
// resume-hint
// ---------------------------------------------------------------------------

pub fn resume_hint_cmd(cli_name: &str) {
    // Prints nothing and exits 0 on any failure: a hint must never break the
    // wrapper.
    if let Some(hint) = _resume_hint(cli_name, &getcwd()) {
        println!("{hint}");
    }
}

fn _resume_hint(cli_name: &str, cwd: &str) -> Option<String> {
    let (pane, _team, _agent) = _pane_team_identity()?;
    let (session_id, resume_cmd) = match cli_name {
        "codex" => (
            crate::adapters::codex_app_server::session_id_for_pane(&pane),
            "hive codex resume",
        ),
        "grok" => (
            crate::adapters::grok_leader::read_pane_session(&pane).map(|record| record.0),
            "hive grok --resume",
        ),
        _ => (
            crate::adapters::claude_bg::job_id_for_pane(&pane),
            "hive claude --resume",
        ),
    };
    let session_id = session_id.filter(|value| !value.is_empty())?;
    // Both fields are untrusted content headed for automatic terminal output:
    // control/non-printable bytes (ESC/OSC/BEL/newline) silence the hint. So
    // does a leading "-", which would parse as a CLI option instead of a
    // session id when pasted.
    if !py_isprintable(cwd) || !py_isprintable(&session_id) || session_id.starts_with('-') {
        return None;
    }
    let command = format!(
        "cd {} && {resume_cmd} {}",
        shlex_quote(cwd),
        shlex_quote(&session_id)
    );
    // cyan matches the CLI's own resume line; stripped whenever stdout is not
    // a real terminal (pipes, tests, logs) — click's behavior.
    let styled = if stdout_isatty() {
        format!("\x1b[36m{command}\x1b[0m")
    } else {
        command
    };
    Some(format!("Resume from anywhere:\n  {styled}"))
}

/// (pane, team, agent) when this pane is a tagged team member, else None.
fn _pane_team_identity() -> Option<(String, String, String)> {
    let pane = env_string("TMUX_PANE").trim().to_string();
    if pane.is_empty() {
        return None;
    }
    let team = tmux::get_pane_option(&pane, "hive-team").unwrap_or_default();
    let agent = tmux::get_pane_option(&pane, "hive-agent").unwrap_or_default();
    if team.is_empty() || agent.is_empty() {
        return None;
    }
    Some((pane, team, agent))
}

// ---------------------------------------------------------------------------
// shell-init
// ---------------------------------------------------------------------------

const _SHELL_INIT_POSIX: &str = r#"# hive launchers — `hcodex` / `hclaude` / `hgrok` start a hive-connected codex /
# claude / grok in the current tmux pane (shared app-server daemon for codex,
# per-pane leader for grok, supervisor-hosted bg job for claude) and print a
# cd-ready resume hint when it exits. Outside tmux, and for management subcommands / non-interactive flags,
# they run the plain binary. Plain `codex` / `claude` / `grok` are never touched.
function hcodex {
  if ! command -v hive >/dev/null 2>&1; then
    echo "hcodex: hive is not on PATH" >&2; return 127
  fi
  # The launcher always ends in an exec (managed or raw), so the status here
  # is codex's own — never a fallback signal. The if-condition keeps errexit
  # shells from bailing before the status is saved.
  if hive codex "$@"; then _hive_rc=0; else _hive_rc=$?; fi
  # print a cd-ready resume hint for the session that just ended.
  hive resume-hint codex 2>/dev/null || true
  return $_hive_rc
}

function hclaude {
  if ! command -v hive >/dev/null 2>&1; then
    echo "hclaude: hive is not on PATH" >&2; return 127
  fi
  # The launcher always ends in an exec (managed or raw), so the status here
  # is claude's own — never a fallback signal. The if-condition keeps errexit
  # shells from bailing before the status is saved.
  if hive claude "$@"; then _hive_rc=0; else _hive_rc=$?; fi
  # claude's own resume hint omits the directory; print a cd-ready one.
  hive resume-hint claude 2>/dev/null || true
  return $_hive_rc
}

function hgrok {
  if ! command -v hive >/dev/null 2>&1; then
    echo "hgrok: hive is not on PATH" >&2; return 127
  fi
  # The launcher always ends in an exec (managed or raw), so the status here
  # is grok's own — never a fallback signal. The if-condition keeps errexit
  # shells from bailing before the status is saved.
  if hive grok "$@"; then _hive_rc=0; else _hive_rc=$?; fi
  # print a cd-ready resume hint for the session that just ended.
  hive resume-hint grok 2>/dev/null || true
  return $_hive_rc
}
"#;

const _SHELL_INIT_FISH: &str = r#"# hive launchers — `hcodex` / `hclaude` / `hgrok` start a hive-connected codex /
# claude / grok in the current tmux pane (shared app-server daemon for codex,
# per-pane leader for grok, supervisor-hosted bg job for claude) and print a
# cd-ready resume hint when it exits. Outside tmux, and for management subcommands / non-interactive flags,
# they run the plain binary. Plain `codex` / `claude` / `grok` are never touched.
function hcodex
    if not type -q hive
        echo "hcodex: hive is not on PATH" >&2
        return 127
    end
    # the launcher always ends in an exec (managed or raw): the status is
    # codex's own, never a fallback signal
    hive codex $argv
    set -l _hive_rc $status
    # print a cd-ready resume hint for the session that just ended.
    hive resume-hint codex 2>/dev/null
    return $_hive_rc
end

function hclaude
    if not type -q hive
        echo "hclaude: hive is not on PATH" >&2
        return 127
    end
    # the launcher always ends in an exec (managed or raw): the status is
    # claude's own, never a fallback signal
    hive claude $argv
    set -l _hive_rc $status
    # claude's own resume hint omits the directory; print a cd-ready one.
    hive resume-hint claude 2>/dev/null
    return $_hive_rc
end

function hgrok
    if not type -q hive
        echo "hgrok: hive is not on PATH" >&2
        return 127
    end
    # the launcher always ends in an exec (managed or raw): the status is
    # grok's own, never a fallback signal
    hive grok $argv
    set -l _hive_rc $status
    # print a cd-ready resume hint for the session that just ended.
    hive resume-hint grok 2>/dev/null
    return $_hive_rc
end
"#;

pub fn shell_init_cmd(shell: &str) {
    let resolved = if shell.is_empty() {
        let env_shell = env_string("SHELL");
        if env_shell.is_empty() {
            "zsh".to_string()
        } else {
            Path::new(&env_shell)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        }
    } else {
        shell.to_string()
    };
    if resolved.trim() == "fish" {
        print!("{_SHELL_INIT_FISH}");
    } else {
        // zsh and bash share this syntax. The ksh-style `function name {` form
        // bypasses alias expansion of the name in BOTH shells, so a stray
        // alias cannot break the parse.
        print!("{_SHELL_INIT_POSIX}");
    }
}

// ---------------------------------------------------------------------------
// worktree pool
// ---------------------------------------------------------------------------

fn wt_ok<T>(result: crate::worktree::Result<T>) -> T {
    match result {
        Ok(value) => value,
        Err(e) => fail(&e.to_string()),
    }
}

/// Owner / integration context for worktree commands (pane-anchored, cwd-free).
/// Returns (owner, team, integration).
fn _worktree_context() -> (String, String, Option<String>) {
    let binding = _discover_tmux_binding();
    let window = {
        let bound = map_str(&binding, "tmuxWindow");
        if !bound.is_empty() {
            bound
        } else if tmux::is_inside_tmux() {
            tmux::get_current_window_target().unwrap_or_default()
        } else {
            String::new()
        }
    };
    let team = map_str(&binding, "team");
    let integration = if window.is_empty() {
        None
    } else {
        tmux::get_window_option(&window, "hive-integration-branch").filter(|v| !v.is_empty())
    };
    let owner = if team.is_empty() {
        "unbound".to_string()
    } else {
        format!("team:{team}")
    };
    (owner, team, integration)
}

pub fn worktree_set_base_cmd(refname: &str, plain: bool) {
    let window = tmux::get_current_window_target().unwrap_or_default();
    let team = if window.is_empty() {
        String::new()
    } else {
        tmux::get_window_option(&window, "hive-team").unwrap_or_default()
    };
    if team.is_empty() {
        fail("current window is not a hive team window (no @hive-team); run from your team window");
    }
    let cwd = getcwd();
    let anchor = wt_ok(crate::worktree::repo_anchor(Some(Path::new(&cwd))));
    let oid = wt_ok(crate::worktree::rev_parse(&anchor, refname));
    tmux::set_window_option(&window, "@hive-integration-branch", refname);
    if plain {
        println!(
            "team '{team}' integration branch set: {refname} ({})",
            &oid[..oid.len().min(12)]
        );
    } else {
        println!(
            "{}",
            py_dumps(
                &json!({
                    "team": team,
                    "integrationBranch": refname,
                    "oid": oid,
                    "window": window,
                }),
                true,
                Some(2),
                false
            )
        );
    }
}

pub fn worktree_start_cmd(feature: &str, base_ref: Option<&str>, plain: bool) {
    let cwd = getcwd();
    let anchor = wt_ok(crate::worktree::repo_anchor(Some(Path::new(&cwd))));
    let (owner, team, integration) = _worktree_context();
    let base = wt_ok(crate::worktree::resolve_base(
        &anchor,
        base_ref,
        integration.as_deref(),
    ));
    let result = wt_ok(crate::worktree::start(
        &anchor,
        feature,
        &base,
        &owner,
        &team,
        integration.as_deref(),
        None,
    ));
    if plain {
        println!("{}", result.path);
        println!(
            "mode={} branch={} base={}@{}",
            result.mode,
            result.branch,
            result.base,
            &result.base_oid[..result.base_oid.len().min(12)]
        );
        for warning in &result.warnings {
            eprintln!("warning: {warning}");
        }
    } else {
        println!("{}", py_dumps(&result.to_json(), true, Some(2), false));
    }
    if !result.ready() {
        std::process::exit(1);
    }
}

pub fn worktree_done_cmd(feature: &str, force: bool, plain: bool) {
    let cwd = getcwd();
    let anchor = wt_ok(crate::worktree::repo_anchor(Some(Path::new(&cwd))));
    let result = wt_ok(crate::worktree::done(&anchor, feature, force, &cwd));
    if !plain {
        println!("{}", py_dumps(&result.to_json(), true, Some(2), false));
        return;
    }
    if !result.status_summary.is_empty() {
        eprintln!("{}", result.status_summary);
    }
    println!("removed {}", result.removed_path);
    println!(
        "branch {} kept (delete after PR merge via normal flow)",
        result.branch
    );
}

pub fn worktree_status_cmd(feature: Option<&str>, plain: bool) {
    let cwd = getcwd();
    let anchor = wt_ok(crate::worktree::repo_anchor(Some(Path::new(&cwd))));
    let payload: Value = match feature.filter(|f| !f.is_empty()) {
        Some(feature) => {
            serde_json::to_value(wt_ok(crate::worktree::feature_status(&anchor, feature)))
                .unwrap_or(Value::Null)
        }
        None => serde_json::to_value(wt_ok(crate::worktree::pool_status(&anchor)))
            .unwrap_or(Value::Null),
    };
    if !plain {
        println!("{}", py_dumps(&payload, true, Some(2), false));
        return;
    }
    let rows: Vec<Value> = match payload {
        Value::Array(rows) => rows,
        other => vec![other],
    };
    if rows.is_empty() {
        println!("no hive-labeled worktrees or branches");
        return;
    }
    for row in rows {
        let row = match row.as_object() {
            Some(row) => row.clone(),
            None => continue,
        };
        let mut flags: Vec<String> = Vec::new();
        if truthy(row.get("dirty")) {
            flags.push("dirty".to_string());
        }
        let in_progress: Vec<String> = row
            .get("inProgress")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if !in_progress.is_empty() {
            flags.push(format!("in-progress:{}", in_progress.join(",")));
        }
        if truthy(row.get("stale")) {
            flags.push("stale".to_string());
        }
        let suffix = if flags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", flags.join(" "))
        };
        let owner_value = map_str(&row, "owner");
        let owner = if owner_value.is_empty() {
            String::new()
        } else {
            format!(" owner={owner_value}")
        };
        let line = format!(
            "{}: {}{owner} {}{suffix}",
            map_str(&row, "feature"),
            map_str(&row, "state"),
            map_str(&row, "worktreePath")
        );
        println!("{}", line.trim_end());
    }
}

// ---------------------------------------------------------------------------
// Tests (ported from tests/unit — logic-level only)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // --- tests/unit/test_launcher_opt_values.py ---

    #[test]
    fn test_a_following_flag_is_not_the_value() {
        let a = args(&["--resume", "-m", "grok-4"]);
        assert_eq!(_grok_opt_value(&a, &["--resume"]), None);
        assert_eq!(_codex_opt_value(&a, &["--resume"]), None);
    }

    #[test]
    fn test_a_trailing_bare_option_has_no_value() {
        let a = args(&["--resume"]);
        assert_eq!(_grok_opt_value(&a, &["--resume"]), None);
        assert_eq!(_codex_opt_value(&a, &["--resume"]), None);
    }

    #[test]
    fn test_a_real_value_still_reads() {
        let a = args(&["--resume", "old-sid", "-m", "grok-4"]);
        assert_eq!(
            _grok_opt_value(&a, &["--resume"]),
            Some("old-sid".to_string())
        );
        assert_eq!(
            _codex_opt_value(&a, &["--resume"]),
            Some("old-sid".to_string())
        );
    }

    #[test]
    fn test_the_equals_form_still_reads() {
        let a = args(&["--resume=old-sid"]);
        assert_eq!(
            _grok_opt_value(&a, &["--resume"]),
            Some("old-sid".to_string())
        );
        assert_eq!(
            _codex_opt_value(&a, &["--resume"]),
            Some("old-sid".to_string())
        );
    }

    #[test]
    fn test_codex_cwd_does_not_swallow_the_next_flag() {
        assert_eq!(
            _codex_opt_value(&args(&["--cd", "--model", "x"]), &["--cd", "-C"]),
            None
        );
        assert_eq!(
            _codex_opt_value(&args(&["--cd", "/tmp/w", "--model", "x"]), &["--cd", "-C"]),
            Some("/tmp/w".to_string())
        );
    }

    #[test]
    fn test_grok_resume_before_a_flag_leaves_the_pane_unrecorded() {
        // a bare --resume opens grok's picker: hive cannot know the session id,
        // so it records nothing rather than recording the next flag
        assert_eq!(
            _grok_launch_session(&args(&["--resume", "-m", "grok-4"])),
            (None, false)
        );
    }

    #[test]
    fn test_grok_resume_with_an_id_records_that_session() {
        assert_eq!(
            _grok_launch_session(&args(&["--resume", "old-sid"])),
            (Some("old-sid".to_string()), false)
        );
    }

    #[test]
    fn test_grok_bare_launch_mints_a_session_and_passes_the_flag() {
        let (sid, pass_flag) = _grok_launch_session(&args(&["-m", "grok-4"]));
        assert!(pass_flag);
        assert_eq!(sid.expect("minted session id").len(), 36);
    }

    // --- tests/unit/test_pr_window_display.py ---

    #[test]
    fn test_derives_plain_padded_format() {
        assert_eq!(
            _derive_pr_window_status(Some("  #I #W  ")),
            Some("  #{?#{@hive-pr},PR#{@hive-pr},#I} #W  ".to_string())
        );
    }

    #[test]
    fn test_preserves_style_wrappers_and_padding() {
        let derived =
            _derive_pr_window_status(Some("#[bg=yellow,fg=black,bold]  #I #W  #[default]"));
        assert_eq!(
            derived,
            Some(
                "#[bg=yellow,fg=black,bold]  #{?#{@hive-pr},PR#{@hive-pr},#I} #W  #[default]"
                    .to_string()
            )
        );
    }

    #[test]
    fn test_derives_tmux_default_format() {
        let derived = _derive_pr_window_status(Some("#I:#W#{?window_flags,#{window_flags}, }"));
        assert_eq!(
            derived,
            Some(
                "#{?#{@hive-pr},PR#{@hive-pr},#I}:#W#{?window_flags,#{window_flags}, }".to_string()
            )
        );
    }

    #[test]
    fn test_skips_when_global_already_references_hive_pr() {
        assert_eq!(
            _derive_pr_window_status(Some("#{?#{@hive-pr},PR#{@hive-pr},#I}:#W")),
            None
        );
    }

    #[test]
    fn test_skips_when_no_index_token() {
        assert_eq!(_derive_pr_window_status(Some("#W only")), None);
    }

    #[test]
    fn test_skips_empty_or_missing_global() {
        assert_eq!(_derive_pr_window_status(None), None);
        assert_eq!(_derive_pr_window_status(Some("")), None);
    }

    #[test]
    fn test_escaped_literal_hash_i_is_not_rewritten() {
        // `##I` renders a literal `#I` — not a replaceable index token, so skip.
        assert_eq!(_derive_pr_window_status(Some("##I #W")), None);
    }

    #[test]
    fn test_replaces_real_tokens_and_leaves_escaped_ones() {
        let derived = _derive_pr_window_status(Some("#I #W ##I #I"));
        assert_eq!(
            derived,
            Some(format!("{_PR_INDEX_TOKEN} #W ##I {_PR_INDEX_TOKEN}"))
        );
    }

    // --- tests/unit/test_launcher_mint_names.py ---

    fn tags_lookup<'a>(
        mapping: &'a [((&'a str, &'a str), &'a str)],
    ) -> impl Fn(&str, &str) -> Option<String> + 'a {
        move |target: &str, key: &str| {
            mapping
                .iter()
                .find(|((t, k), _)| *t == target && *k == key)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn test_a_member_pane_mints_the_member_name_for_claude() {
        let mapping = [
            (("%179", "hive-team"), "honey"),
            (("%179", "hive-agent"), "worker"),
        ];
        let label = _pane_member_label_via(tags_lookup(&mapping), "%179");
        assert_eq!(_mint_name(label, "%179"), "honey.worker");
    }

    #[test]
    fn test_a_member_pane_mints_the_member_name_for_codex() {
        let mapping = [
            (("%9", "hive-team"), "comb"),
            (("%9", "hive-agent"), "validator"),
        ];
        let label = _pane_member_label_via(tags_lookup(&mapping), "%9");
        assert_eq!(_mint_name(label, "%9"), "comb.validator");
    }

    #[test]
    fn test_an_untagged_pane_falls_back_to_the_pane_placeholder() {
        let mapping: [((&str, &str), &str); 0] = [];
        let label = _pane_member_label_via(tags_lookup(&mapping), "%42");
        assert_eq!(_mint_name(label, "%42"), "hive-42");
    }

    #[test]
    fn test_a_half_tagged_pane_is_not_a_member() {
        let mapping = [(("%7", "hive-team"), "honey")];
        let label = _pane_member_label_via(tags_lookup(&mapping), "%7");
        assert_eq!(_mint_name(label, "%7"), "hive-7");
    }

    // --- launcher scanning / resume parsing ---

    #[test]
    fn test_codex_subcommand_index_skips_global_options() {
        assert_eq!(
            _codex_subcommand_index(&args(&["-c", "k=v", "exec"])),
            Some(2)
        );
        assert_eq!(_codex_subcommand_index(&args(&["resume", "sid"])), Some(0));
        assert_eq!(_codex_subcommand_index(&args(&["-m", "gpt"])), None);
    }

    #[test]
    fn test_codex_positional_after_skips_flags() {
        let a = args(&["resume", "--model", "x", "sid-1"]);
        assert_eq!(_codex_positional_after(&a, 0), Some("sid-1".to_string()));
        assert_eq!(_codex_positional_after(&args(&["resume"]), 0), None);
    }

    #[test]
    fn test_claude_resume_arg_shapes() {
        assert_eq!(_claude_resume_arg(&args(&[])), (false, None));
        assert_eq!(_claude_resume_arg(&args(&["--resume"])), (true, None));
        assert_eq!(
            _claude_resume_arg(&args(&["-r", "abc"])),
            (true, Some("abc".to_string()))
        );
        assert_eq!(
            _claude_resume_arg(&args(&["--resume=abc"])),
            (true, Some("abc".to_string()))
        );
        assert_eq!(_claude_resume_arg(&args(&["--resume", "-m"])), (true, None));
        assert_eq!(_claude_resume_arg(&args(&["--resume="])), (true, None));
    }

    // --- fork split choice ---

    #[test]
    fn test_choose_fork_split_prefers_fitting_direction() {
        // Both fit: wide window goes horizontal only at >= 2.5x aspect.
        assert!(_choose_fork_split(300, 60));
        assert!(!_choose_fork_split(200, 100));
        // Only horizontal fits.
        assert!(_choose_fork_split(200, 30));
        // Only vertical fits.
        assert!(!_choose_fork_split(100, 60));
        // Neither fits: highest score wins (h_score 0.9875 vs v_score 0.45).
        assert!(_choose_fork_split(159, 20));
        assert!(!_choose_fork_split(80, 41));
    }

    // --- config value parsing ---

    #[test]
    fn test_parse_config_value_shapes() {
        assert_eq!(_parse_config_value("true"), Value::Bool(true));
        assert_eq!(_parse_config_value(" FALSE "), Value::Bool(false));
        assert_eq!(_parse_config_value("42"), json!(42));
        assert_eq!(_parse_config_value("1.5"), json!(1.5));
        assert_eq!(
            _parse_config_value("hello"),
            Value::String("hello".to_string())
        );
    }

    // --- python-style json dumps ---

    #[test]
    fn test_py_dumps_matches_python_separators() {
        let value = json!({"a": 1, "b": [1, 2], "c": "x"});
        assert_eq!(
            py_dumps(&value, true, None, false),
            r#"{"a": 1, "b": [1, 2], "c": "x"}"#
        );
        assert_eq!(
            py_dumps(&json!({"b": 1, "a": 2}), true, None, true),
            r#"{"a": 2, "b": 1}"#
        );
    }

    #[test]
    fn test_py_dumps_indent_matches_python() {
        let value = json!({"a": [1], "b": {}});
        assert_eq!(
            py_dumps(&value, true, Some(2), false),
            "{\n  \"a\": [\n    1\n  ],\n  \"b\": {}\n}"
        );
    }

    #[test]
    fn test_py_dumps_ensure_ascii_escapes_non_ascii() {
        assert_eq!(py_dumps(&json!("你"), true, None, false), "\"\\u4f60\"");
        assert_eq!(py_dumps(&json!("你"), false, None, false), "\"你\"");
        assert_eq!(
            py_dumps(&json!("🐝"), true, None, false),
            "\"\\ud83d\\udc1d\""
        );
    }

    // --- shlex quoting ---

    #[test]
    fn test_shlex_quote_matches_python() {
        assert_eq!(shlex_quote(""), "''");
        assert_eq!(shlex_quote("abc./_-"), "abc./_-");
        assert_eq!(shlex_quote("a b"), "'a b'");
        assert_eq!(shlex_quote("it's"), r#"'it'"'"'s'"#);
    }

    #[test]
    fn test_uuid4_shape() {
        let sid = uuid4();
        assert_eq!(sid.len(), 36);
        assert_eq!(sid.as_bytes()[14], b'4');
        assert!(matches!(sid.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
    }
}
