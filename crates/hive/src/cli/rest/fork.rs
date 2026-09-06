use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Stdio;

use serde_json::{Map, Value};

use super::*;
use crate::agent::Agent;
use crate::team::Team;
use crate::tmux;

// ---------------------------------------------------------------------------
// fork
// ---------------------------------------------------------------------------

const FORK_MIN_COLS: i64 = 80;
const FORK_MIN_ROWS: i64 = 20;

/// True for horizontal (left/right) split, false for vertical (top/bottom).
///
/// Accounts for the 1-cell tmux separator consumed by the split.
pub(crate) fn choose_fork_split(width: i64, height: i64) -> bool {
    let h_half = (width - 1) / 2;
    let v_half = (height - 1) / 2;
    let can_h = h_half >= FORK_MIN_COLS;
    let can_v = v_half >= FORK_MIN_ROWS;
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
        h_half as f64 / FORK_MIN_COLS as f64,
        height as f64 / FORK_MIN_ROWS as f64,
    );
    let v_score = f64::min(
        width as f64 / FORK_MIN_COLS as f64,
        v_half as f64 / FORK_MIN_ROWS as f64,
    );
    h_score >= v_score
}

pub fn fork_cmd(pane_id: &str, split: &str, join_as: &str, prompt: &str) {
    let target = resolve_pane_target(pane_id);
    if !target.is_team_bound {
        // Non-team pane: clone it bare — no member registration, no @hive-* tags.
        // The clone is an independent agent that belongs to no Hive team.
        if !join_as.is_empty() {
            fail("--join-as requires a team-bound pane");
        }
        let new_pane = fork_orphan_clone(&target.pane_id, split, prompt);
        let mut payload = Map::new();
        payload.insert("pane".to_string(), Value::String(new_pane));
        payload.insert("registered".to_string(), Value::Null);
        payload.insert("team".to_string(), Value::Null);
        println!("{}", json_pretty(&Value::Object(payload)));
        return;
    }

    // Team-bound fork: register the clone as a new team member.
    let mut target_team = if !pane_id.is_empty() {
        ok_or_fail(load_team(&target.team_name, ""))
    } else {
        ok_or_fail(resolve_scoped_team(None, true))
            .1
            .expect("required resolve returned no team")
    };

    let join_as = if join_as.is_empty() {
        let window_target = if !target_team.tmux_window.is_empty() {
            target_team.tmux_window.clone()
        } else {
            identity::current_window_target().unwrap_or_default()
        };
        let panes = if window_target.is_empty() {
            Vec::new()
        } else {
            tmux::list_panes_full(&window_target)
        };
        let mut seen_names = window_seen_names(&target_team, &panes);
        derive_agent_name(&mut seen_names)
    } else {
        join_as.to_string()
    };

    let (_registered_agent, new_pane) =
        fork_registered_agent(&mut target_team, pane_id, split, &join_as, prompt);
    let mut payload = Map::new();
    payload.insert("pane".to_string(), Value::String(new_pane));
    payload.insert("registered".to_string(), Value::String(join_as));
    payload.insert("team".to_string(), Value::String(target_team.name.clone()));
    println!("{}", json_pretty(&Value::Object(payload)));
}

/// Resolve the fork source pane: (pane, profile, session_id, horizontal, cwd).
fn fork_source_details(
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
    if !identity::is_inside_tmux() {
        fail("hive fork requires tmux");
    }
    let current_pane = if !pane_id.is_empty() {
        pane_id.to_string()
    } else {
        identity::current_pane_id().unwrap_or_default()
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
        choose_fork_split(width, height)
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
                some => is_set(some),
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

pub(crate) const FORK_NEW_TASK_MARKER: &str = "NEW TASK FOR THIS FORK:";
pub(crate) const FORK_BOUNDARY_TEXT: &str =
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
pub(crate) const FORK_ORPHAN_BOUNDARY_TEXT: &str =
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
fn fork_boundary_prompt(team_bound: bool) -> &'static str {
    if team_bound {
        FORK_BOUNDARY_TEXT
    } else {
        FORK_ORPHAN_BOUNDARY_TEXT
    }
}

/// Cached static boundary file under `$HIVE_HOME`; rewritten on drift.
fn fork_boundary_file(team_bound: bool) -> PathBuf {
    let text = fork_boundary_prompt(team_bound);
    let filename = if team_bound {
        "fork-boundary.txt"
    } else {
        "fork-boundary-orphan.txt"
    };
    let path = crate::paths::hive_home().join(filename);
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

fn fork_registered_agent(
    t: &mut Team,
    pane_id: &str,
    split: &str,
    join_as: &str,
    prompt: &str,
) -> (Agent, String) {
    ensure_pane_in_scope(t, pane_id);
    let window_target = if !t.tmux_window.is_empty() {
        t.tmux_window.clone()
    } else {
        identity::current_window_target().unwrap_or_default()
    };
    let panes = if window_target.is_empty() {
        Vec::new()
    } else {
        tmux::list_panes_full(&window_target)
    };
    let mut seen_names = window_seen_names(t, &panes);
    claim_member_name(join_as, &mut seen_names);

    let workspace = t.workspace.clone();
    let (current_pane, profile, session_id, horizontal, source_cwd) =
        fork_source_details(pane_id, split, &workspace);

    // Both clones launch through hive's managed launcher; boundary text is
    // static, so cache it under $HIVE_HOME and expand via shell command
    // substitution when there is no prompt. With --prompt we inline boundary +
    // marker + prompt together so the fork sees both in one user message.
    let cmd_base = profile.fork_cmd_for(&session_id);
    let launch_cmd = if !prompt.is_empty() {
        let composed = format!(
            "{}\n\n{}\n{}",
            fork_boundary_prompt(true),
            FORK_NEW_TASK_MARKER,
            prompt
        );
        format!("{cmd_base} {}", shlex_quote(&composed))
    } else {
        format!(
            "{cmd_base} \"$(cat {})\"",
            shlex_quote(&fork_boundary_file(true).to_string_lossy())
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
    let registered_agent = register_agent_member(
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
fn fork_orphan_clone(pane_id: &str, split: &str, prompt: &str) -> String {
    let (current_pane, profile, session_id, horizontal, source_cwd) =
        fork_source_details(pane_id, split, "");
    let cmd_base = profile.fork_cmd_for(&session_id);
    let launch_cmd = if !prompt.is_empty() {
        let composed = format!(
            "{}\n\n{}\n{}",
            fork_boundary_prompt(false),
            FORK_NEW_TASK_MARKER,
            prompt
        );
        format!("{cmd_base} {}", shlex_quote(&composed))
    } else {
        format!(
            "{cmd_base} \"$(cat {})\"",
            shlex_quote(&fork_boundary_file(false).to_string_lossy())
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
// cvim / vim / vfork / hfork (human helpers)
// ---------------------------------------------------------------------------

fn cvim_binary() -> PathBuf {
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

fn exec_cvim(mode: &str, args: &[String]) -> ! {
    // The script reads TMUX_PANE for its reply pane; inside a codex tool env
    // that variable is the shared daemon's (stripped) one, so hand it the
    // thread-resolved pane identity instead.
    if let Some(pane) = identity::current_pane_id().filter(|pane| !pane.is_empty()) {
        std::env::set_var("TMUX_PANE", pane);
    }
    // The script's helper callbacks are hidden subcommands of this binary;
    // a bare `hive` on the pane's PATH is only the script's fallback.
    if let Ok(exe) = std::env::current_exe() {
        std::env::set_var("HIVE_BIN", exe);
    }
    let mut argv: Vec<String> = vec![
        cvim_binary().to_string_lossy().into_owned(),
        mode.to_string(),
    ];
    argv.extend(args.iter().cloned());
    execvp("bash", &argv);
}

pub fn cvim_cmd(args: &[String]) {
    exec_cvim("cvim", args);
}

pub fn vim_cmd(args: &[String]) {
    exec_cvim("vim", args);
}

fn exec_fork_split(split: &str, args: &[String]) {
    // Thread-aware pane resolution: in a codex tool env TMUX_PANE is gone.
    let reply_pane = identity::current_pane_id().unwrap_or_default();
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
    exec_fork_split("v", args);
}

pub fn hfork_cmd(args: &[String]) {
    exec_fork_split("h", args);
}
