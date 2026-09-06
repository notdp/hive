//! Registry-truth core verbs: create, join, send, team, ls, view, doctor,
//! interrupt, kill, delete.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde_json::{Map, Value};

use super::*;
use crate::team::{Team, LEAD_AGENT_NAME};
use crate::tmux;

// ---------------------------------------------------------------------------
// create
// ---------------------------------------------------------------------------

/// Create a team.
///
/// NAME is optional everywhere (pool-picked by default). Outside tmux: the
/// session named after the team (created detached when missing) holds its
/// window, and a Claude session creator is its orch. Inside tmux on an agent pane: that pane
/// becomes the orch. Inside tmux on a shell pane: the window binds the team
/// without an orch.
///
/// The workspace defaults to the team's own directory under the registry
/// store (`$HIVE_HOME/teams/<team>/`, beside its `team.json`); `--workspace`
/// puts it elsewhere and the registry entry records where.
pub fn create(
    name: &str,
    desc: &str,
    workspace: &str,
    reset_workspace: bool,
    state_entries: &[String],
) {
    if reset_workspace && workspace.is_empty() {
        fail("--reset-workspace requires --workspace (the default workspace is always reset)");
    }
    // Before the registry row exists: a workspace whose hived socket path
    // cannot bind would otherwise leave a registered team nobody can reach.
    if !workspace.is_empty() {
        if let Err(reason) = crate::devlog::check_socket_path_len(Path::new(&expanduser(workspace)))
        {
            fail(&reason);
        }
    }
    if !tmux::is_inside_tmux() {
        let team_name = if name.is_empty() {
            pick_team_name("", "", "0")
        } else {
            name.to_string()
        };
        create_detached_team(&team_name, desc, workspace, reset_workspace, state_entries);
        return;
    }
    let current_pane = tmux::get_current_pane_id().unwrap_or_default();
    if !current_pane.is_empty()
        && crate::agent_cli::detect_profile_for_pane(&current_pane).is_some()
    {
        // Agent pane: this pane becomes the orch of the fresh team.
        if !workspace.is_empty() || !state_entries.is_empty() || reset_workspace {
            fail("an orch create uses the team directory; run from a shell pane for --workspace");
        }
        require_daemon_backed(&current_pane);
        let result = create_orch_team(&current_pane, name);
        println!("{}", json_pretty(&Value::Object(result)));
        return;
    }
    let mut name = name.to_string();
    if name.is_empty() {
        let window = tmux::get_current_window_target().unwrap_or_default();
        let window_id = if window.is_empty() {
            String::new()
        } else {
            tmux::get_window_id(&window).unwrap_or_default()
        };
        let index = if window.contains(':') {
            window.rsplit(':').next().unwrap_or("0").to_string()
        } else {
            "0".to_string()
        };
        name = pick_team_name(
            &tmux::get_current_session_name().unwrap_or_default(),
            &window_id,
            &index,
        );
    }
    if let Err(e) = check_explicit_workspace(&name, workspace) {
        fail(&e.to_string());
    }
    let ws_str = if workspace.is_empty() {
        team_workspace(&name)
    } else {
        expanduser(workspace)
    };
    let t = match Team::create(&name, desc, &ws_str) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };
    // The lead joins the roster only when its pane actually runs an
    // agent — a shell-pane create has no engine to register (same
    // authority the pane tagging uses).
    let lead = t.lead_agent();
    let lead_is_agent = lead
        .as_ref()
        .map(|lead| crate::agent_cli::member_role_for_pane(&lead.pane_id) == "agent")
        .unwrap_or(false);
    let members: Vec<Map<String, Value>> = if lead_is_agent {
        vec![member_registry_row(lead.as_ref().expect("lead checked"))]
    } else {
        Vec::new()
    };
    let _ = crate::registry::record_team(
        &t.name,
        &ws_str,
        &created_at_key(t.created_at),
        &members,
        &t.tmux_window_id,
    );
    if let Err(e) = prepare_workspace(&name, &ws_str, reset_workspace, state_entries) {
        fail(&e.to_string());
    }
    remember_context(&name, &ws_str, LEAD_AGENT_NAME);
    println!("Team '{name}' created.");
    println!("Workspace initialized: {ws_str}");
}

/// The default workspace: the team's own directory under the registry
/// store, `$HIVE_HOME/teams/<team>/`, where its `team.json` also lives.
fn team_workspace(name: &str) -> String {
    match crate::registry::team_dir(name) {
        Some(dir) => dir.to_string_lossy().into_owned(),
        None => fail(&format!(
            "team name '{name}' is invalid: not a safe registry name"
        )),
    }
}

/// One shape for a workspace path: `~` expanded, absolute against the
/// cwd, trailing separators and `.` components dropped — so `--workspace
/// ~/.hive/teams/honey/` names the team directory as surely as the default.
/// `..` is left alone; the path need not exist yet.
fn normalized_workspace(ws: &str) -> PathBuf {
    let path = PathBuf::from(expanduser(ws));
    let absolute = if path.is_absolute() {
        path
    } else {
        PathBuf::from(getcwd()).join(path)
    };
    absolute.components().collect()
}

/// Whether *ws* is the team's own directory under the registry store.
fn is_team_dir(name: &str, ws: &str) -> bool {
    normalized_workspace(ws) == normalized_workspace(&team_workspace(name))
}

/// Refuse an explicit `--workspace` inside the registry store that is not
/// this team's own directory: every directory there is some team's, and
/// `hive delete --delete-workspace` removes the workspace whole.
fn check_explicit_workspace(name: &str, ws: &str) -> Result<()> {
    if ws.is_empty() || is_team_dir(name, ws) {
        return Ok(());
    }
    let store = normalized_workspace(&crate::registry::store_dir().to_string_lossy());
    if normalized_workspace(ws).starts_with(&store) {
        bail!(
            "--workspace {ws} is inside the registry store {}; only team '{name}'s own              directory may live there (drop --workspace for the default)",
            store.display()
        );
    }
    Ok(())
}

/// Bring a fresh team's workspace up and seed its `--state` entries.
///
/// The team directory (the default) is always reset: a pool name recycled
/// after `hive delete` must not inherit its predecessor's bus, event log
/// or artifacts, and `team.json` is the one file the reset leaves alone.
/// An explicit `--workspace` keeps whatever it holds unless
/// `--reset-workspace` asks for the wipe.
fn prepare_workspace(name: &str, ws: &str, reset: bool, state_entries: &[String]) -> Result<()> {
    let path = Path::new(ws);
    if is_team_dir(name, ws) {
        crate::hived::stop_hived(ws);
        crate::bus::reset_workspace(path)?;
    } else {
        if path.exists() && reset {
            std::fs::remove_dir_all(path)?;
        }
        crate::bus::init_workspace(path)?;
    }
    for (key, value) in parse_entries(state_entries) {
        let _ = std::fs::write(path.join("state").join(&key), map_entry_str(&value));
    }
    Ok(())
}

fn map_entry_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The protocol asks a tmux-less Claude session to badge its own title so
/// the human and `hive ccd ls` can tell it apart; there is no API hive can
/// call for that, so the command output carries the reminder — an orch that
/// skipped it was the first thing a human noticed.
fn title_badge_hint(badge: &str) -> String {
    format!(
        "Rename this session now: prefix its title with `{badge}` (set_session_title \
         or your host's rename), and drop the prefix when you leave the team."
    )
}

/// Create a team from outside tmux: its window in the session named after
/// it (created detached when missing), a registry entry, its workspace.
pub(crate) fn create_detached_team(
    name: &str,
    desc: &str,
    workspace: &str,
    reset_workspace: bool,
    state_entries: &[String],
) {
    let error = crate::team::validate_team_name(name);
    if !error.is_empty() {
        fail(&error);
    }
    if crate::registry::load(name).is_some() {
        fail(&format!(
            "team '{name}' already exists (hive delete removes it)"
        ));
    }
    if let Err(e) = check_explicit_workspace(name, workspace) {
        fail(&e.to_string());
    }
    // The creator is the orch when it is an agent: a Claude session outside
    // tmux joins its own roster, same as an agent pane does inside tmux.
    // A session already on another team's roster stays a guest here.
    let creator = crate::adapters::claude_sessions::self_session();
    let orch_member: Option<Map<String, Value>> = match creator.as_ref() {
        Some(creator)
            if !creator.session_id.is_empty()
                && registry_member_for_session(&creator.session_id).is_none() =>
        {
            Some(session_member_row(
                LEAD_AGENT_NAME,
                "claude",
                &creator.session_id,
            ))
        }
        _ => None,
    };
    // A tagged window with no registry entry is a leftover (a create that
    // died mid-way); building a second window for the name would let the
    // orphan shadow the real display in `find_team_window`.
    if let Ok((orphan, _)) = crate::team::find_team_window(name, "") {
        if !orphan.is_empty() {
            fail(&format!(
                "window {orphan} is still tagged for team '{name}' — `hive delete {name}` clears it"
            ));
        }
    }
    let (window, first_pane, created_session) =
        ok_or_fail(crate::cli::rest::new_team_session_window(name));
    let undo_window = || {
        if created_session {
            tmux::kill_session(&format!("={name}"));
        } else {
            // killing the only pane closes the window
            tmux::kill_pane(&first_pane);
        }
    };
    let window_id = tmux::get_window_id(&window).unwrap_or_default();
    let ws_str = if workspace.is_empty() {
        team_workspace(name)
    } else {
        expanduser(workspace)
    };
    // The "auto-init from " prefix keeps `should_show_description` quiet.
    let description = if desc.is_empty() {
        format!("auto-init from hive create ({window})")
    } else {
        desc.to_string()
    };
    let members: Vec<Map<String, Value>> = orch_member.clone().into_iter().collect();
    // Everything between the window and the registry entry rolls the
    // window back on failure, so a half-made team leaves no tagged window
    // behind for a retry to trip over.
    let built = (|| -> Result<Team> {
        let t = Team::create_for_window(
            name,
            &window,
            &first_pane,
            LEAD_AGENT_NAME,
            &description,
            &ws_str,
            false,
        )?;
        prepare_workspace(name, &ws_str, reset_workspace, state_entries)?;
        crate::registry::record_team(
            &t.name,
            &ws_str,
            &created_at_key(t.created_at),
            &members,
            &window_id,
        )?;
        Ok(t)
    })();
    let mut t = match built {
        Ok(t) => t,
        Err(e) => {
            undo_window();
            fail(&e.to_string());
        }
    };
    match orch_member.as_ref() {
        Some(orch) => {
            // The ccd creator's read-only mirror is the first pane (a fresh
            // window records no `off`, so `pane_role` is `mirror`); an
            // orch that will send needs the hived up, as in
            // `create_orch_team`.
            crate::cli::rest::bind_member_viewer(&first_pane, orch, name, &ws_str, "mirror");
            let _ = start_team_hived(&mut t, &ws_str);
        }
        None => {
            // No orch: the first pane is the team's dock, tagged the way a
            // shell-pane create tags its pane (`Team::create`), so a verb
            // run from it finds the team through its own tags — the
            // window's `@hive-team` is display, not binding.
            tmux::tag_pane(
                &first_pane,
                crate::agent_cli::member_role_for_pane(&first_pane),
                LEAD_AGENT_NAME,
                name,
                "",
                "",
            );
        }
    }
    remember_context(name, &ws_str, LEAD_AGENT_NAME);
    println!("Team '{name}' created (tmux window {window} — `hive attach {name}` opens it).");
    if orch_member.is_some() {
        println!("You are {name}.{LEAD_AGENT_NAME}.");
        println!("{}", title_badge_hint(&format!("[{name}] ")));
    } else if let Some(creator) = creator.as_ref().filter(|c| !c.session_id.is_empty()) {
        if let Some((e_team, e_name)) = registry_member_for_session(&creator.session_id) {
            println!("You are already {e_team}.{e_name} — orchestrating '{name}' as a guest.");
        }
    }
    if !ws_str.is_empty() {
        println!("Workspace initialized: {ws_str}");
    }
}

/// Whether `hive create` should hand back *existing* instead of creating.
///
/// A tagged pane is idempotent whatever *name* says: the pane is the team's
/// display and cannot be two teams. A binding that came from the session
/// row (no pane) is the engine's identity; reusing it for a different
/// *name* would silently answer with a team the caller did not ask for, so
/// that case is refused by name.
fn reuse_existing_binding(existing: &Map<String, Value>, name: &str) -> Result<bool, String> {
    let team = map_str(existing, "team");
    if team.is_empty() {
        return Ok(false);
    }
    if !map_str(existing, "pane").is_empty() || name.is_empty() || name == team {
        return Ok(true);
    }
    Err(format!(
        "this session is already {team}.{} — `hive create {name}` needs a session on no team",
        map_str(existing, "agent")
    ))
}

/// Bind the current pane as the orch of a fresh team.
///
/// Spawns nobody — members come later via `hive spawn`, driven by the orch.
/// Placement: a lone pane binds its window in place; a crowded window
/// breaks the orch pane out to a fresh one first, so team identity
/// derives from the final window (Bug A). *name* overrides the pool pick.
/// Idempotent: an already-bound pane returns its existing binding; a
/// session bound by its own row or env is refused when *name* asks for
/// another team (see [`reuse_existing_binding`]).
fn create_orch_team(current_pane: &str, name: &str) -> Map<String, Value> {
    gc_dead_teams();

    let existing = discover_tmux_binding();
    match reuse_existing_binding(&existing, name) {
        Ok(true) => return existing,
        Ok(false) => {}
        Err(message) => fail(&message),
    }

    let session_name = tmux::get_current_session_name().unwrap_or_else(|| "hive".to_string());
    let session_name = if session_name.is_empty() {
        "hive".to_string()
    } else {
        session_name
    };
    let orch_cli = resolve_spawn_cli_name(None);
    let mut window = tmux::get_pane_window_target(current_pane).unwrap_or_default();
    if window.is_empty() {
        fail("cannot determine current window");
    }
    let panes = match tmux::list_panes_full_or_none(&window) {
        Some(panes) => panes,
        None => fail(&format!(
            "tmux did not answer the pane listing for {window}; rerun create"
        )),
    };
    if !panes.iter().any(|p| p.pane_id == current_pane) {
        fail(&format!(
            "current pane {current_pane} missing from {window} listing; rerun create"
        ));
    }

    let mut orch_pane = current_pane.to_string();
    if panes.len() >= 2 {
        // Crowded window — isolate the orch so the team owns its window.
        let (new_window, new_pane) = match tmux::break_pane(current_pane, "", true, None) {
            Ok(pair) => pair,
            Err(e) => fail(&e.to_string()),
        };
        if new_window.is_empty() {
            fail("failed to break out into a new window");
        }
        window = new_window;
        orch_pane = new_pane;
    }

    let final_window_id = tmux::get_window_id(&window).unwrap_or_default();
    let final_index = if window.contains(':') {
        window.rsplit(':').next().unwrap_or("0").to_string()
    } else {
        "0".to_string()
    };

    let team_name = if name.is_empty() {
        pick_team_name(&session_name, &final_window_id, &final_index)
    } else {
        name.to_string()
    };
    prepare_window_for_new_team(&window, &orch_pane);
    claim_team_name(&team_name, &window, !name.is_empty());

    let ws_str = team_workspace(&team_name);
    if let Err(e) = prepare_workspace(&team_name, &ws_str, false, &[]) {
        fail(&e.to_string());
    }

    let mut t = match Team::create_for_window(
        &team_name,
        &window,
        &orch_pane,
        LEAD_AGENT_NAME,
        &format!("auto-init from tmux {session_name} ({window})"),
        &ws_str,
        false,
    ) {
        Ok(t) => t,
        Err(e) => fail(&e.to_string()),
    };

    tmux::rename_window(&window, &t.name);
    tmux::configure_hive_window(&window);
    tmux::set_pane_option(&orch_pane, "hive-role", "agent");
    tmux::set_pane_option(&orch_pane, "hive-agent", LEAD_AGENT_NAME);
    tmux::set_pane_option(&orch_pane, "hive-team", &t.name);
    tmux::set_pane_option(&orch_pane, "hive-cli", &orch_cli);
    let _ = crate::context::save_context_for_pane(&orch_pane, &t.name, &ws_str, LEAD_AGENT_NAME);
    remember_context(&t.name, &ws_str, LEAD_AGENT_NAME);
    let member = session_member_row(
        LEAD_AGENT_NAME,
        &orch_cli,
        t.lead_session_id.as_deref().unwrap_or_default(),
    );
    let _ = crate::registry::record_team(
        &t.name,
        &ws_str,
        &created_at_key(t.created_at),
        &[member],
        &t.tmux_window_id,
    );
    let _ = start_team_hived(&mut t, &ws_str);
    tmux::select_window(&window);

    let mut result = Map::new();
    result.insert("team".to_string(), Value::String(t.name.clone()));
    result.insert("window".to_string(), Value::String(window));
    let mut orch = Map::new();
    orch.insert("pane".to_string(), Value::String(orch_pane));
    orch.insert(
        "name".to_string(),
        Value::String(LEAD_AGENT_NAME.to_string()),
    );
    orch.insert("cli".to_string(), Value::String(orch_cli));
    result.insert("orch".to_string(), Value::Object(orch));
    result.insert("workspace".to_string(), Value::String(ws_str));
    result.insert(
        "protocol".to_string(),
        Value::String("/hive:hive".to_string()),
    );
    result
}

/// Clear a stale `@hive-team` tag on *window_target* so a new team can bind.
///
/// Fails (rather than clobbering) when the window still hosts live members
/// that the current pane isn't part of — that window owns a real team.
fn prepare_window_for_new_team(window_target: &str, current_pane: &str) {
    let existing = match tmux::get_window_option(window_target, "hive-team") {
        Some(existing) if !existing.is_empty() => existing,
        _ => return,
    };
    if crate::team::window_has_live_team_members(window_target, &existing) {
        let cur_team = if current_pane.is_empty() {
            None
        } else {
            tmux::get_pane_option(current_pane, "hive-team")
        };
        if cur_team.as_deref() != Some(existing.as_str()) {
            fail(&format!(
                "tmux window '{window_target}' already hosts live Hive team \
                 '{existing}' — run from a team pane, or start the team elsewhere."
            ));
        }
        return;
    }
    crate::team::clear_window_tags(window_target);
}

/// Guard a default/explicit team name that another window already owns.
///
/// A stale duplicate (no live member panes) is cleared so the name can be
/// claimed; a live duplicate is a hard error — names are never silently
/// suffixed or clobbered.
fn claim_team_name(team_name: &str, this_window: &str, explicit: bool) {
    let (existing_wt, _) = crate::team::find_team_window(team_name, "").unwrap_or_default();
    if existing_wt.is_empty() || existing_wt == this_window {
        return;
    }
    if crate::team::window_has_live_team_members(&existing_wt, team_name) {
        let hint = if explicit {
            "choose a different --name"
        } else {
            "rerun from that window, or run `hive doctor`"
        };
        fail(&format!(
            "team '{team_name}' already lives in tmux window '{existing_wt}' — {hint}."
        ));
    }
    crate::team::gc_stale_team_windows(team_name, this_window, &[existing_wt]);
}

// ---------------------------------------------------------------------------
// daemon-backed gates (orch create)
// ---------------------------------------------------------------------------

/// The `/hive` entry as *cli* types it — claude keeps the plugin-qualified
/// name, codex and grok take the bare skill name (the spawn prompt rule).
fn hive_skill_entry(cli: &str) -> String {
    let profile = crate::agent_cli::get_profile(cli).expect("known agent cli");
    profile.skill_cmd_for(if cli == "claude" { "hive:hive" } else { "hive" })
}

/// Refuse an engine hive does not manage (a codex thread off the shared
/// daemon, a bare interactive claude, a grok without a leader) as the orch
/// of a new team; each refusal says how to relaunch.
fn require_daemon_backed(pane: &str) {
    if is_codex_tool_env() {
        // Running from inside the codex TUI's own tool: pane record or
        // codex roster membership is the identity, and the shared daemon
        // must answer.
        if crate::cli::team_ops::codex_thread_is_hive_managed(&crate::cli::util::env_string(
            "CODEX_THREAD_ID",
        )) && crate::adapters::codex_app_server::daemon_alive()
        {
            return;
        }
        fail(&codex_relaunch_message());
    }
    if pane.is_empty() {
        return;
    }
    let profile = match crate::agent_cli::detect_profile_for_pane(pane) {
        Some(profile) => profile,
        None => return,
    };
    if profile.name == "grok" {
        require_grok_leader_backed(pane);
        return;
    }
    if profile.name == "claude" {
        require_claude_job_backed(pane);
        return;
    }
    if profile.name != "codex" {
        return;
    }
    if crate::adapters::codex_app_server::thread_id_for_pane(pane).is_some()
        && crate::adapters::codex_app_server::daemon_alive()
    {
        return; // recorded thread on a live shared daemon — hive-managed, fine
    }
    fail(&format!(
        "this codex is not hive-managed; hive needs its thread on the shared \
         app-server daemon for native runtime, so it can't join yet.\n\
         for future launches use hcodex (one-time setup, any shell):\n  \
         grep -q 'hive shell-init' ~/.zshrc || \
         echo 'eval \"$(hive shell-init zsh)\"' >> ~/.zshrc\n\
         for this session now (your session is preserved):\n  \
         1) exit codex: press Ctrl-C (twice)\n  \
         2) run: hive codex resume <session-id>   (or `hive codex resume` \
         for the picker)\n\
         then re-run {}.",
        hive_skill_entry("codex")
    ));
}

/// Refuse a bare interactive claude pane: hive claude members run as bg jobs.
fn require_claude_job_backed(pane: &str) {
    if crate::adapters::claude_bg::job_id_for_pane(pane).is_some() {
        return;
    }
    fail(&format!(
        "this claude is not hive-managed; hive claude members run as \
         background jobs (`claude --bg`) with the pane attached as a viewer, \
         so it can't join yet.\n\
         for future launches use hclaude (one-time setup, any shell):\n  \
         grep -q 'hive shell-init' ~/.zshrc || \
         echo 'eval \"$(hive shell-init zsh)\"' >> ~/.zshrc\n\
         for this session now (your session is preserved):\n  \
         1) note your session id (`claude --resume` lists it), exit claude\n  \
         2) run: hive claude -r <session-id>\n\
         then re-run {}.",
        hive_skill_entry("claude")
    ));
}

/// Refuse a plain grok pane: hive delivers only through the pane leader.
fn require_grok_leader_backed(pane: &str) {
    let sock = crate::adapters::grok_leader::pane_socket_path(pane);
    if sock.exists() && crate::adapters::grok_leader::probe_socket(&sock) {
        return;
    }
    let sid = crate::adapters::grok_leader::session_id_for_pane(pane).unwrap_or_default();
    let resume = if sid.is_empty() {
        "hive grok".to_string()
    } else {
        format!("hive grok --resume {sid}")
    };
    fail(&format!(
        "this grok has no hive leader; hive delivers to grok only through the \
         pane leader, so it can't join yet.\n\
         for future launches use hgrok (one-time setup, any shell):\n  \
         grep -q 'hive shell-init' ~/.zshrc || \
         echo 'eval \"$(hive shell-init zsh)\"' >> ~/.zshrc\n\
         for this session now (your session is preserved):\n  \
         1) exit grok: /exit\n  \
         2) run: {resume}\n\
         then re-run {}.",
        hive_skill_entry("grok")
    ));
}

// ---------------------------------------------------------------------------
// join
// ---------------------------------------------------------------------------

/// Join a team.
///
/// Outside tmux: the current Claude session enters TEAM's roster as a full
/// member. Inside tmux: the current pane (or --pane) registers into the
/// window's team.
pub fn join_cmd(
    team_arg: &str,
    name_override: &str,
    pane_override: &str,
    notify: bool,
    group_name: &str,
) {
    if !tmux::is_inside_tmux() {
        if !pane_override.is_empty() {
            fail("--pane needs tmux; outside tmux `hive join <team>` joins this session");
        }
        join_as_ccd(team_arg, name_override);
        return;
    }

    let binding = discover_tmux_binding();
    let team_name = if team_arg.is_empty() {
        map_str(&binding, "team")
    } else {
        team_arg.to_string()
    };
    if team_name.is_empty() {
        fail("no team in scope — pass a team (see `hive ls`) or run from a bound window");
    }
    let pane_id = if pane_override.is_empty() {
        tmux::get_current_pane_id().unwrap_or_default()
    } else {
        pane_override.to_string()
    };
    if pane_id.is_empty() {
        fail("cannot determine current pane");
    }

    let mut t = match Team::load(&team_name, &tmux::get_current_pane_id().unwrap_or_default()) {
        Ok(t) => t,
        Err(e) => fail(&e.to_string()),
    };
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

    let target_pane = match panes.iter().find(|pane| pane.pane_id == pane_id) {
        Some(pane) => pane.clone(),
        None => fail(&format!(
            "pane '{pane_id}' not found in window '{window_target}'"
        )),
    };

    if target_pane.team == team_name && !target_pane.agent.is_empty() {
        fail(&format!(
            "pane '{pane_id}' is already registered as '{}'",
            target_pane.agent
        ));
    }

    let mut seen_names = window_seen_names(&t, &panes);
    claim_member_name(name_override, &mut seen_names);

    let (role, pane_cli) = classify_pane(&target_pane);
    if role != "agent" {
        fail(&format!(
            "pane '{pane_id}' is not running an agent CLI; only agent panes can be registered"
        ));
    }
    let agent_name = if name_override.is_empty() {
        derive_agent_name(&mut seen_names)
    } else {
        name_override.to_string()
    };
    let cwd = tmux::display_value(&pane_id, "#{pane_current_path}")
        .filter(|c| !c.is_empty())
        .unwrap_or_else(getcwd);
    register_agent_member(
        &mut t,
        &pane_id,
        &team_name,
        &agent_name,
        &pane_cli,
        &cwd,
        notify,
        group_name,
    );
    let member_name = agent_name;

    let mut result_payload = Map::new();
    result_payload.insert("joined".to_string(), Value::String(member_name));
    result_payload.insert("role".to_string(), Value::String(role.to_string()));
    result_payload.insert("pane".to_string(), Value::String(pane_id));
    result_payload.insert("team".to_string(), Value::String(team_name));
    if !group_name.is_empty() {
        result_payload.insert("group".to_string(), Value::String(group_name.to_string()));
    }
    println!("{}", json_pretty(&Value::Object(result_payload)));
}

/// Join the current outside-tmux Claude session into a team's roster.
///
/// The session's own id becomes the member's engine identity; delivery
/// rides the same session channel `ccd.<name>` already uses. Idempotent:
/// an already-joined session reports its membership.
pub(crate) fn join_as_ccd(team_name: &str, name_override: &str) {
    if team_name.is_empty() {
        fail("join outside tmux needs a team: hive join <team> (see `hive ls`)");
    }
    let entry = match crate::registry::load(team_name) {
        Some(entry) => entry,
        None => fail(&format!("team '{team_name}' not found (see `hive ls`)")),
    };
    let guest = crate::adapters::claude_sessions::self_session();
    let guest = match guest {
        Some(guest) if !guest.session_id.is_empty() => guest,
        _ => fail(
            "join outside tmux needs a live Claude session channel; \
             codex/grok TUIs have none — join from a team pane instead",
        ),
    };
    if let Some((e_team, e_name)) = registry_member_for_session(&guest.session_id) {
        if e_team == team_name {
            println!("already a member: {e_team}.{e_name}");
            return;
        }
        fail(&format!(
            "this session is already {e_team}.{e_name}; leave with `hive kill {e_team}.{e_name}` first"
        ));
    }
    let mut seen = roster_names(&entry);
    seen.insert(LEAD_AGENT_NAME.to_string());
    claim_member_name(name_override, &mut seen);
    let member_name = if name_override.is_empty() {
        derive_agent_name(&mut seen)
    } else {
        name_override.to_string()
    };
    let row = session_member_row(&member_name, "claude", &guest.session_id);
    let _ = crate::registry::record_member(team_name, &row, "");
    // Eager display: the joined session gets its mirror pane now, not at
    // the next attach.
    if let Some(entry) = crate::registry::load(team_name) {
        let _ = crate::cli::rest::ensure_team_display(&entry);
    }
    println!("joined: {team_name}.{member_name}");
    println!(
        "{}",
        title_badge_hint(&format!("[{team_name}.{member_name}] "))
    );
}

// ---------------------------------------------------------------------------
// send
// ---------------------------------------------------------------------------

/// Send a message to another agent — the only message verb.
pub fn send(to_agent: &str, body: &str, artifact: &str) {
    if let Some(label) = to_agent.strip_prefix("ccd.") {
        send_to_ccd_session(label, body, artifact);
        return;
    }
    // A dot splits the address only when the prefix names an existing team
    // (`honey.worker`); otherwise the address stays whole for qualified-name
    // resolution across pane tags.
    let (explicit_team, to_agent) = split_team_address(to_agent);
    // The root gate admitted this call because the process runs inside a
    // Claude session (that session is the sender and its inbox socket is its
    // identity), or a codex/grok member's tool whose own session id keys
    // its roster row. The latter take the member lane, where the identity
    // ladder's session rung resolves them.
    let guest = if tmux::is_inside_tmux() {
        None
    } else {
        crate::adapters::claude_sessions::self_session()
    };
    let (t, sender) = if let Some(guest) = guest {
        let (_team_name, t) = resolve_guest_send_target(&to_agent, &explicit_team);
        let sender = match registry_member_for_session(&guest.session_id) {
            // A joined session is a full member: its roster name is the
            // reply address, not the ccd guest label.
            Some((m_team, m_name)) => format!("{m_team}.{m_name}"),
            // The session NAME, never the title: a title may contain spaces,
            // which would break `<HIVE from=...>` attribute tokenization
            // downstream. The name addresses the session in
            // `hive send ccd.<name>` just the same.
            None => format!("ccd.{}", guest.name),
        };
        (t, sender)
    } else {
        if !explicit_team.is_empty() && explicit_team != default_team().unwrap_or_default() {
            // Copying a teammate's `from=<team>.<member>` verbatim must just
            // work, so an own-team prefix reads as the bare name; only a
            // foreign-team prefix is refused.
            fail(
                "team members address teammates by bare name; \
                 `<team>.<member>` is for a Claude session outside tmux",
            );
        }
        let (_team_name, t) = resolve_send_target_team(&to_agent);
        (t, resolve_sender(None))
    };
    let ws = ok_or_fail(resolve_workspace(Some(&t), true));
    // Auto-anchor: the latest unanswered inbound from the recipient makes
    // this send its reply; senders never handle msgIds. Anything else is a
    // new thread and rides the root protocol. An unreadable bus (guest
    // sender, fresh workspace) just means no anchor — delivery still goes,
    // and a truly broken bus fails loudly in the send itself.
    let mut reply_to = String::new();
    let latest = crate::bus::latest_inbound_send_event(&ws, &sender, &to_agent)
        .ok()
        .flatten();
    if let Some(latest) = latest {
        let candidate = latest.msg_id;
        if !candidate.is_empty()
            && !crate::bus::has_send_reply_to(&ws, &candidate, &sender, &to_agent).unwrap_or(false)
        {
            reply_to = candidate;
        }
    }
    if reply_to.is_empty() {
        validate_root_send_protocol(body);
    }
    let resolved_artifact = resolve_artifact_path(artifact, &ws);
    let payload = match request_send_payload(
        &ws,
        &t,
        &sender,
        &to_agent,
        body,
        &resolved_artifact,
        &reply_to,
        "send",
        true,
    ) {
        Ok(payload) => payload,
        Err(e) => fail(&e.to_string()),
    };
    if is_set(payload.get("mailbox")) {
        // A mailbox has no peer runtime to go silent about: say so once,
        // in the sender's own tool result, so nobody invents a follow-up.
        println!(
            "delivered to flow mailbox msgId={} (not a member; no ack will arrive)",
            map_str(&payload, "msgId")
        );
    }
    // Peer sends stay silent (rule of silence). The bus row carries the
    // identity; `hive thread` reads it back.
}

/// `hive send ccd.<session>`: a member pushes into an outside Claude
/// session's cross-session inbox.
fn send_to_ccd_session(label: &str, message: &str, artifact: &str) {
    let team = default_team();
    let agent = default_agent();
    let (team, agent) = match (team, agent) {
        (Some(team), Some(agent)) => (team, agent),
        _ => fail(
            "`ccd.<session>` is a team member's outbound address; another \
             Claude session is messaged with the native SendMessage tool",
        ),
    };
    if !artifact.is_empty() {
        fail("a session push carries no --artifact; put the path in the body");
    }
    if message.is_empty() {
        fail("message body required");
    }
    let matches = crate::adapters::claude_sessions::resolve(label);
    if matches.is_empty() {
        fail(&format!(
            "no live Claude session named, titled or numbered '{label}' (see `hive ccd ls`)"
        ));
    }
    if matches.len() > 1 {
        let where_ = matches
            .iter()
            .map(|s| {
                format!(
                    "{} (pid {}, {})",
                    s.name,
                    s.pid,
                    if s.cwd.is_empty() { "?" } else { &s.cwd }
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        fail(&format!(
            "{} live sessions answer to '{label}': {where_}; use the name or pid",
            matches.len()
        ));
    }
    let target = &matches[0];
    if let Some((m_team, m_agent)) = live_member_pids().get(&target.pid) {
        if *m_team == team {
            fail(&format!(
                "'{label}' is your teammate {m_agent}; members talk over \
                 the bus: `hive send {m_agent}`"
            ));
        }
        fail(&format!(
            "'{label}' is {m_team}.{m_agent}, a member of another team, not an outside session"
        ));
    }
    let sender = format!("{team}.{agent}");
    // The frame's `from` reaches only the human's message card; the receiving
    // model sees just the text. Wrap the body in the ordinary <HIVE> envelope
    // so the sender travels in band and the receiver answers by copying it
    // verbatim: `hive send <team>.<agent>`. No msgId: this is not a bus thread.
    let envelope = crate::message::format_hive_envelope(
        &sender,
        &format!("ccd.{}", target.name),
        message,
        "",
        "",
        "",
    );
    let outcome = crate::adapters::claude_sessions::send(
        &target.socket_path,
        &envelope,
        &sender,
        &target.session_id,
    );
    match outcome {
        None => fail(&format!(
            "session '{}' (pid {}) is not listening on {}; it may have just exited",
            target.name, target.pid, target.socket_path
        )),
        Some(outcome) if outcome == crate::adapters::claude_sessions::WRITE_TIMED_OUT => {
            fail(&format!(
                "session '{}' (pid {}) accepted the connection but did \
                 not read the message (~{} KB) in time; it looks \
                 stalled and may hold a truncated frame — retry once it is responsive",
                target.name,
                target.pid,
                std::cmp::max(1, message.len() / 1024)
            ))
        }
        // Fire-and-forget: success is silent (rule of silence); failures above
        // already exited non-zero with the reason.
        Some(_) => {}
    }
}

// ---------------------------------------------------------------------------
// team
// ---------------------------------------------------------------------------

/// Show team overview.
pub fn team_cmd(team_arg: &str) {
    gc_dead_teams();
    let scoped = if team_arg.is_empty() {
        default_team().unwrap_or_default()
    } else {
        team_arg.to_string()
    };
    if !scoped.is_empty() {
        let (_, t) = ok_or_fail(resolve_scoped_team(Some(&scoped), false));
        if let Some(mut t) = t {
            println!(
                "{}",
                json_pretty(&Value::Object(team_status_payload(&mut t)))
            );
            return;
        }
    }
    if !tmux::is_inside_tmux() {
        fail("no team in scope — pass -t <team> (see `hive ls`)");
    }
    let mut result = Map::new();
    result.insert("team".to_string(), Value::Null);
    let session_name = tmux::get_current_session_name();
    let window_target = tmux::get_current_window_target();
    let current_pane = tmux::get_current_pane_id();
    let panes = match window_target.as_deref() {
        Some(window) if !window.is_empty() => tmux::list_panes_full(window),
        _ => Vec::new(),
    };
    let opt_value = |v: &Option<String>| match v {
        Some(s) => Value::String(s.clone()),
        None => Value::Null,
    };
    let mut tmux_payload = Map::new();
    tmux_payload.insert("session".to_string(), opt_value(&session_name));
    tmux_payload.insert("window".to_string(), opt_value(&window_target));
    tmux_payload.insert("currentPane".to_string(), opt_value(&current_pane));
    let pane_rows: Vec<Value> = panes
        .iter()
        .map(|p| {
            let mut row = Map::new();
            row.insert("id".to_string(), Value::String(p.pane_id.clone()));
            row.insert("command".to_string(), Value::String(p.command.clone()));
            row.insert(
                "role".to_string(),
                Value::String(if p.role.is_empty() {
                    crate::agent_cli::member_role_for_pane(&p.pane_id).to_string()
                } else {
                    p.role.clone()
                }),
            );
            row.insert("agent".to_string(), Value::String(p.agent.clone()));
            row.insert("team".to_string(), Value::String(p.team.clone()));
            Value::Object(row)
        })
        .collect();
    tmux_payload.insert("panes".to_string(), Value::Array(pane_rows));
    tmux_payload.insert("paneCount".to_string(), Value::from(panes.len()));
    result.insert("tmux".to_string(), Value::Object(tmux_payload));
    result.insert(
        "hint".to_string(),
        Value::String(
            "No team bound. Run `hive create` to make this pane the orch of a fresh team, \
             then spawn members with `hive spawn <name> --task <artifact>`."
                .to_string(),
        ),
    );
    add_runtime_location_fields(&mut result);
    println!("{}", json_pretty(&Value::Object(result)));
}

// ---------------------------------------------------------------------------
// ls
// ---------------------------------------------------------------------------

/// Registry entries with their current display state.
fn build_ls_payload() -> Map<String, Value> {
    let (panes, pane_status) = tmux::list_panes_all_status();
    let (windows, win_status) = tmux::list_team_windows_status();
    let tmux_status = if pane_status == "ok" && win_status == "ok" {
        "ok"
    } else if pane_status == "unknown" || win_status == "unknown" {
        "unknown"
    } else {
        "no-server"
    };

    // team -> agent -> pane, insertion ordered.
    let mut live_members: Vec<(String, Vec<(String, tmux::PaneInfo)>)> = Vec::new();
    let mut win_by_team: HashMap<String, tmux::TeamWindow> = HashMap::new();
    if tmux_status == "ok" {
        for p in panes.unwrap_or_default() {
            if !p.team.is_empty() && !p.agent.is_empty() {
                let team_idx = match live_members.iter().position(|(team, _)| *team == p.team) {
                    Some(idx) => idx,
                    None => {
                        live_members.push((p.team.clone(), Vec::new()));
                        live_members.len() - 1
                    }
                };
                let team_slot = &mut live_members[team_idx].1;
                match team_slot.iter().position(|(agent, _)| *agent == p.agent) {
                    Some(idx) => team_slot[idx].1 = p.clone(),
                    None => team_slot.push((p.agent.clone(), p.clone())),
                }
            }
        }
        for w in windows.unwrap_or_default() {
            win_by_team.entry(w.team.clone()).or_insert(w);
        }
    }

    let live_for = |team: &str| -> Option<&Vec<(String, tmux::PaneInfo)>> {
        live_members
            .iter()
            .find(|(name, _)| name == team)
            .map(|(_, members)| members)
    };

    let mut teams: Vec<Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for entry in crate::registry::list_entries() {
        let team_name = map_str(&entry, "team");
        seen.insert(team_name.clone());
        if is_set(entry.get("corrupt")) {
            let mut row = Map::new();
            row.insert("team".to_string(), Value::String(team_name));
            row.insert("state".to_string(), Value::String("corrupt".to_string()));
            teams.push(Value::Object(row));
            continue;
        }
        let member_rows: Vec<Map<String, Value>> = entry
            .get("members")
            .and_then(Value::as_array)
            .map(|members| {
                members
                    .iter()
                    .filter_map(Value::as_object)
                    .map(|m| {
                        let mut row = Map::new();
                        row.insert(
                            "name".to_string(),
                            m.get("name")
                                .cloned()
                                .unwrap_or(Value::String(String::new())),
                        );
                        row.insert(
                            "cli".to_string(),
                            m.get("cli")
                                .cloned()
                                .unwrap_or(Value::String(String::new())),
                        );
                        row.insert(
                            "model".to_string(),
                            m.get("model")
                                .cloned()
                                .unwrap_or(Value::String(String::new())),
                        );
                        row.insert(
                            "session".to_string(),
                            Value::Bool(is_set(m.get("sessionId"))),
                        );
                        row
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut row = Map::new();
        row.insert("team".to_string(), Value::String(team_name.clone()));
        row.insert(
            "workspace".to_string(),
            entry
                .get("workspace")
                .cloned()
                .unwrap_or(Value::String(String::new())),
        );
        let mut sorted_rows = sorted_member_rows(member_rows);
        if tmux_status == "unknown" {
            row.insert(
                "members".to_string(),
                Value::Array(sorted_rows.into_iter().map(Value::Object).collect()),
            );
            row.insert("state".to_string(), Value::String("unknown".to_string()));
        } else {
            let live = live_for(&team_name);
            match live {
                Some(live) if !live.is_empty() => {
                    for m in sorted_rows.iter_mut() {
                        let name = map_str(m, "name");
                        m.insert(
                            "live".to_string(),
                            Value::Bool(live.iter().any(|(agent, _)| *agent == name)),
                        );
                    }
                    let missing: Vec<String> = entry
                        .get("members")
                        .and_then(Value::as_array)
                        .map(|members| {
                            members
                                .iter()
                                .filter_map(Value::as_object)
                                .map(|m| map_str(m, "name"))
                                .filter(|name| !live.iter().any(|(agent, _)| agent == name))
                                .collect()
                        })
                        .unwrap_or_default();
                    row.insert(
                        "members".to_string(),
                        Value::Array(sorted_rows.into_iter().map(Value::Object).collect()),
                    );
                    row.insert(
                        "window".to_string(),
                        Value::String(
                            win_by_team
                                .get(&team_name)
                                .map(|w| w.window.clone())
                                .unwrap_or_default(),
                        ),
                    );
                    row.insert(
                        "state".to_string(),
                        Value::String(
                            if missing.is_empty() {
                                "live-complete"
                            } else {
                                "live-incomplete"
                            }
                            .to_string(),
                        ),
                    );
                }
                _ => {
                    // The display is gone; the engines may still be running.
                    row.insert(
                        "members".to_string(),
                        Value::Array(sorted_rows.into_iter().map(Value::Object).collect()),
                    );
                    row.insert("state".to_string(), Value::String("detached".to_string()));
                }
            }
        }
        teams.push(Value::Object(row));
    }

    if tmux_status == "ok" {
        // Teams alive only in tmux (predating the registry writers).
        let mut tmux_only: Vec<&(String, Vec<(String, tmux::PaneInfo)>)> = live_members
            .iter()
            .filter(|(team, _)| !seen.contains(team))
            .collect();
        tmux_only.sort_by(|a, b| a.0.cmp(&b.0));
        for (team_name, members) in tmux_only {
            let win = win_by_team.get(team_name);
            let member_rows: Vec<Map<String, Value>> = members
                .iter()
                .map(|(n, p)| {
                    let mut row = Map::new();
                    row.insert("name".to_string(), Value::String(n.clone()));
                    row.insert("cli".to_string(), Value::String(p.cli.clone()));
                    row.insert("live".to_string(), Value::Bool(true));
                    row
                })
                .collect();
            let mut row = Map::new();
            row.insert("team".to_string(), Value::String(team_name.clone()));
            row.insert(
                "state".to_string(),
                Value::String("live-complete".to_string()),
            );
            row.insert(
                "window".to_string(),
                Value::String(win.map(|w| w.window.clone()).unwrap_or_default()),
            );
            row.insert(
                "workspace".to_string(),
                Value::String(win.map(|w| w.workspace.clone()).unwrap_or_default()),
            );
            row.insert(
                "members".to_string(),
                Value::Array(
                    sorted_member_rows(member_rows)
                        .into_iter()
                        .map(Value::Object)
                        .collect(),
                ),
            );
            teams.push(Value::Object(row));
        }
    }

    let mut payload = Map::new();
    payload.insert("tmux".to_string(), Value::String(tmux_status.to_string()));
    payload.insert("teams".to_string(), Value::Array(teams));
    payload
}

/// List hive teams from the registry, with their display state.
pub fn ls_cmd(plain: bool) {
    let payload = build_ls_payload();
    if !plain {
        println!("{}", json_pretty(&Value::Object(payload)));
        return;
    }
    for line in format_ls_human(&payload) {
        println!("{line}");
    }
}

fn ls_roster(entry: &Map<String, Value>) -> String {
    entry
        .get("members")
        .and_then(Value::as_array)
        .map(|members| {
            members
                .iter()
                .filter_map(Value::as_object)
                .map(|m| {
                    let cli = map_str(m, "cli");
                    if !cli.is_empty() {
                        return cli;
                    }
                    let name = map_str(m, "name");
                    if !name.is_empty() {
                        return name;
                    }
                    "?".to_string()
                })
                .collect::<Vec<_>>()
                .join("+")
        })
        .unwrap_or_default()
}

fn format_ls_human(payload: &Map<String, Value>) -> Vec<String> {
    let teams: Vec<&Map<String, Value>> = payload
        .get("teams")
        .and_then(Value::as_array)
        .map(|teams| teams.iter().filter_map(Value::as_object).collect())
        .unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    if teams.is_empty() {
        lines.push("no hive teams".to_string());
        return lines;
    }
    if map_str(payload, "tmux") == "unknown" {
        lines.push("! tmux did not answer — live/detached state unknown this pass".to_string());
    }

    let state_of = |e: &Map<String, Value>| map_str(e, "state");
    let live: Vec<&Map<String, Value>> = teams
        .iter()
        .copied()
        .filter(|e| {
            let s = state_of(e);
            s == "live-complete" || s == "live-incomplete"
        })
        .collect();
    let detached: Vec<&Map<String, Value>> = teams
        .iter()
        .copied()
        .filter(|e| state_of(e) == "detached")
        .collect();
    let other: Vec<&Map<String, Value>> = teams
        .iter()
        .copied()
        .filter(|e| {
            let s = state_of(e);
            s != "live-complete" && s != "live-incomplete" && s != "detached"
        })
        .collect();

    if !live.is_empty() {
        lines.push("LIVE".to_string());
        let mut live_sorted = live;
        live_sorted.sort_by_key(|e| map_str(e, "window"));
        for e in live_sorted {
            let window = map_str(e, "window");
            let mut row = format!(
                "  {}  {} · {}",
                if window.is_empty() { "?" } else { &window },
                map_str(e, "team"),
                ls_roster(e)
            );
            if state_of(e) == "live-incomplete" {
                let missing: Vec<String> = e
                    .get("members")
                    .and_then(Value::as_array)
                    .map(|members| {
                        members
                            .iter()
                            .filter_map(Value::as_object)
                            .filter(|m| !is_set(m.get("live")))
                            .map(|m| map_str(m, "name"))
                            .collect()
                    })
                    .unwrap_or_default();
                row.push_str(&format!("  ! missing {}", missing.join("+")));
            }
            lines.push(row);
        }
    }
    if !detached.is_empty() {
        if !lines.is_empty() && lines.last().map(String::as_str) != Some("") {
            lines.push(String::new());
        }
        lines.push("DETACHED  — no tmux display".to_string());
        for e in detached {
            lines.push(format!("  {} · {}", map_str(e, "team"), ls_roster(e)));
        }
    }
    if !other.is_empty() {
        if !lines.is_empty() && lines.last().map(String::as_str) != Some("") {
            lines.push(String::new());
        }
        lines.push("OTHER".to_string());
        for e in other {
            let what = if state_of(e) == "corrupt" {
                "unreadable registry entry".to_string()
            } else {
                state_of(e)
            };
            lines.push(format!(
                "  {}  {}  {}",
                map_str(e, "team"),
                what,
                ls_roster(e)
            ));
        }
    }
    lines
}

// ---------------------------------------------------------------------------
// view
// ---------------------------------------------------------------------------

/// Read-only viewer for a Claude session transcript (follows live): the
/// TUI on a terminal, a plain ANSI stream into a pipe.
pub fn view_cmd(session_id: &str) {
    let Some(path) = crate::transcript_view::transcript_path(session_id) else {
        println!("no transcript for session '{session_id}'");
        std::process::exit(1);
    };
    let code = if unsafe { libc::isatty(libc::STDOUT_FILENO) } == 1 {
        match crate::transcript_tui::run(&path) {
            Ok(code) => code,
            Err(err) => {
                eprintln!("{}: {}", path.display(), err);
                1
            }
        }
    } else {
        crate::transcript_view::follow_plain(session_id, &path)
    };
    std::process::exit(code);
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

/// Diagnose agent connectivity and session state.
///
/// The report is always JSON on stdout: with no reachable hived it still
/// carries the workspace's `runDir` and `logs` map (the debugging entry
/// points) next to a `hived` section saying why, and the exit status is 1.
pub fn doctor(agent_name: &str) {
    let (_, t) = ok_or_fail(resolve_scoped_team(None, true));
    let mut t = t.expect("required resolve returned no team");
    let ws = ok_or_fail(resolve_workspace(Some(&t), true));
    let target_name = if agent_name.is_empty() {
        resolve_sender(None)
    } else {
        agent_name.to_string()
    };
    let (payload, healthy) = doctor_report(&mut t, &ws, &target_name);
    println!("{}", json_pretty(&Value::Object(payload)));
    if !healthy {
        std::process::exit(1);
    }
}

/// `(report, healthy)` for *target_name* on team *t* in workspace *ws*.
///
/// Healthy: the hived's verbose doctor answer, `ok` stripped and
/// `duplicateTeams` added when the team is bound twice. Otherwise — no
/// hived answering on the workspace socket, or an `ok: false` answer — the
/// report is built here: `workspace`, `runDir`, `logs`, and a `hived`
/// section with `ok: false` and the reason.
pub(crate) fn doctor_report(
    t: &mut Team,
    ws: &str,
    target_name: &str,
) -> (Map<String, Value>, bool) {
    doctor_answer(t, ws, target_name)
}

fn doctor_answer(t: &mut Team, ws: &str, target_name: &str) -> (Map<String, Value>, bool) {
    let _ = start_team_hived(t, ws);
    let answer = crate::hived::request_doctor(ws, &t.name, target_name, true);
    let mut payload = match answer {
        Some(payload) if !payload.is_empty() => payload,
        _ => {
            return (
                hived_down_report(ws, &crate::devlog::hived_unavailable_message(Path::new(ws))),
                false,
            )
        }
    };
    if payload.get("ok") == Some(&Value::Bool(false)) {
        let error = match payload.get("error") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => "doctor failed".to_string(),
        };
        return (hived_down_report(ws, &error), false);
    }
    payload.shift_remove("ok");
    let dupes = crate::team::duplicate_team_bindings().unwrap_or_default();
    if !dupes.is_empty() {
        payload.insert(
            "duplicateTeams".to_string(),
            Value::Array(dupes.into_iter().map(Value::Object).collect()),
        );
    }
    (payload, true)
}

/// The doctor report when the hived cannot answer: the same `runDir` and
/// `logs` the hived's own verbose answer carries (`hived/payloads.rs`),
/// computed here from the workspace, plus the failure.
fn hived_down_report(ws: &str, error: &str) -> Map<String, Value> {
    let workspace = Path::new(ws);
    let mut payload = Map::new();
    payload.insert("workspace".to_string(), Value::from(ws));
    payload.insert(
        "runDir".to_string(),
        Value::from(
            crate::devlog::run_dir(workspace)
                .to_string_lossy()
                .into_owned(),
        ),
    );
    payload.insert(
        "logs".to_string(),
        Value::Object(crate::devlog::log_paths(workspace)),
    );
    let mut hived = Map::new();
    hived.insert("ok".to_string(), Value::Bool(false));
    hived.insert("error".to_string(), Value::from(error));
    payload.insert("hived".to_string(), Value::Object(hived));
    payload
}

// ---------------------------------------------------------------------------
// interrupt
// ---------------------------------------------------------------------------

/// Interrupt an agent's running turn.
pub fn interrupt(agent_name: &str) {
    let (_, t) = ok_or_fail(resolve_scoped_team(None, true));
    let t = t.expect("required resolve returned no team");
    let agent = match t.get(agent_name) {
        Ok(agent) => agent,
        Err(_) => fail(&format!(
            "member '{agent_name}' not found in team '{}'",
            t.name
        )),
    };
    if let Err(e) = agent.interrupt() {
        fail(&e.to_string());
    }
    let mut result = Map::new();
    result.insert("member".to_string(), Value::String(agent_name.to_string()));
    result.insert("action".to_string(), Value::String("interrupt".to_string()));
    result.insert("pane".to_string(), Value::String(agent.pane_id.clone()));
    result.insert("success".to_string(), Value::Bool(true));
    println!("{}", json_pretty(&Value::Object(result)));
}

// ---------------------------------------------------------------------------
// kill
// ---------------------------------------------------------------------------

/// (team, bare member) a kill addresses; empty team means "the pane's".
///
/// `-t` is the caller's own intent, so it outranks a team prefix in the
/// address; without it the `<team>.<member>` form still names its own team.
fn kill_address(agent_name: &str, team_arg: &str) -> (String, String) {
    let (address_team, bare_name) = split_team_address(agent_name);
    if team_arg.is_empty() {
        (address_team, bare_name)
    } else {
        (team_arg.to_string(), bare_name)
    }
}

/// Kill an agent pane and remove it from the team.
pub fn kill(agent_name: &str, team_arg: &str) {
    let (explicit_team, bare_name) = kill_address(agent_name, team_arg);
    let (mut t, agent_name) = if !explicit_team.is_empty() {
        (ok_or_fail(load_team(&explicit_team, "")), bare_name)
    } else {
        let (_, t) = resolve_send_target_team(agent_name);
        (t, agent_name.to_string())
    };
    let agent = match t.get(&agent_name) {
        Ok(agent) => agent,
        Err(_) => fail(&format!("agent '{agent_name}' not found")),
    };
    // Team::retire is the one retirement path (roster + registry + layout).
    let removed_from_team = t.retire(&agent_name);
    let mut result = Map::new();
    result.insert("member".to_string(), Value::String(agent_name));
    result.insert("action".to_string(), Value::String("kill".to_string()));
    result.insert("pane".to_string(), Value::String(agent.pane_id.clone()));
    result.insert(
        "removedFromTeam".to_string(),
        Value::Bool(removed_from_team),
    );
    result.insert("success".to_string(), Value::Bool(true));
    println!("{}", json_pretty(&Value::Object(result)));
}

// ---------------------------------------------------------------------------
// delete
// ---------------------------------------------------------------------------

/// Grok leader keys serving *team*, as the leader directory has them.
fn team_grok_daemon_keys(team: &str) -> Vec<String> {
    let mut keys: Vec<String> = crate::adapters::grok_leader::list_daemon_keys()
        .into_iter()
        .filter(|key| {
            crate::adapters::grok_leader::member_from_key(key)
                .map(|(key_team, _)| key_team == team)
                .unwrap_or(false)
        })
        .collect();
    keys.sort();
    keys
}

/// Stop every grok leader that served *team* and clear its key files.
fn sweep_team_grok_daemons(team: &str) {
    for key in team_grok_daemon_keys(team) {
        crate::adapters::grok_leader::pool().drop_key(&key);
        crate::adapters::grok_leader::kill_daemon_key(&key);
    }
}

/// Delete a team and clean up.
pub fn delete(name: &str, workspace: &str, delete_workspace: bool) {
    ok_or_fail(delete_team(name, workspace, delete_workspace));
}

/// The delete body; refuses an unsafe name before touching anything, since
/// the team directory is joined onto the registry store from it.
///
/// Without `--delete-workspace` only `team.json` goes: the team directory
/// keeps its bus, run dir and artifacts for reading until the name is
/// recycled (the next create resets them). With it, the workspace — the
/// team directory, or the external one the entry records — is removed,
/// and the team directory with it. An external workspace is never removed
/// without the flag.
pub(crate) fn delete_team(name: &str, workspace: &str, delete_workspace: bool) -> Result<()> {
    let error = crate::team::validate_team_name(name);
    if !error.is_empty() {
        bail!("cannot delete: {error}");
    }
    let mut team_workspace = String::new();
    let mut team_window = String::new();
    let mut team_window_id = String::new();
    if let Ok(t) = Team::load(name, "") {
        team_workspace = t.workspace.clone();
        team_window = t.tmux_window.clone();
        team_window_id = t.tmux_window_id.clone();
        t.cleanup();
    }

    // Read before the tags go: a window hive built itself (`@hive-built`,
    // in the team session or the caller's) is hive's to close; a window
    // the human's session lent the team (in-tmux create) keeps their pane.
    // The last window going drops the session with it.
    let hive_built =
        !team_window.is_empty() && tmux::get_window_option(&team_window, "hive-built").is_some();
    if !team_window.is_empty() {
        crate::team::clear_window_tags(&team_window);
    }
    // A parked mirror pane (`hive mirror off`) sits in a hidden window hive
    // made; it goes first, or it would keep the team session alive after
    // the team window closes.
    for hidden in tmux::hidden_mirror_windows(name) {
        tmux::kill_window(&hidden);
    }
    let caller_window = tmux::get_current_window_id().unwrap_or_default();
    if hive_built && !team_window_id.is_empty() && caller_window != team_window_id {
        tmux::kill_window(&team_window_id);
    }

    // Explicit -w, else the entry's workspace; with neither there is no
    // workspace to stop or remove, and the team-dir sweep below still
    // clears a leftover directory.
    let resolved_workspace = if !workspace.is_empty() {
        workspace.to_string()
    } else {
        team_workspace
    };

    // Stop hived before workspace cleanup.
    if !resolved_workspace.is_empty() {
        crate::hived::stop_hived(&resolved_workspace);
    }

    if !resolved_workspace.is_empty() && delete_workspace {
        let ws = expanduser(&resolved_workspace);
        if Path::new(&ws).exists() {
            std::fs::remove_dir_all(&ws)?;
            println!("Workspace removed: {ws}");
        }
    }

    let current = crate::context::load_current_context();
    if current.get("team").map(String::as_str) == Some(name) {
        let _ = crate::context::clear_current_context();
    }

    // The registry entry is the team's authoritative existence: removing it
    // is what makes the team deleted (readers and the hived's registry-gone
    // exit key on it).
    crate::registry::delete_team(name)?;
    if delete_workspace {
        // The team directory is the default workspace; with an external
        // one it held only the entry, gone above with its directory.
        if let Some(dir) = crate::registry::team_dir(name) {
            if dir.exists() {
                std::fs::remove_dir_all(&dir)?;
            }
        }
    }

    // Last, because it is the point of no return for the engines: the hived
    // reaps orphan leaders only for its own team, and a deleted team has no
    // hived — an unswept leader would outlive every trace of who it served.
    sweep_team_grok_daemons(name);

    println!("Team '{name}' deleted.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testenv::EnvGuard;
    use serde_json::json;

    fn as_map(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            _ => panic!("expected object"),
        }
    }

    #[test]
    fn test_delete_refuses_unsafe_names_before_touching_disk() {
        let mut env = EnvGuard::new();
        let tmp = tempfile::TempDir::new().unwrap();
        let hive_home = tmp.path().join("hive");
        env.set("HIVE_HOME", &hive_home);
        // what `teams/../evil` and an absolute name would have resolved to
        let sibling = hive_home.join("evil");
        let outside = tmp.path().join("outside");
        for dir in [&sibling, &outside] {
            std::fs::create_dir_all(dir.join("marker")).unwrap();
        }

        for name in ["../evil", outside.to_str().unwrap(), "a.b", ""] {
            let err = delete_team(name, "", true).unwrap_err().to_string();
            assert!(err.starts_with("cannot delete:"), "{name}: {err}");
        }

        assert!(sibling.join("marker").is_dir());
        assert!(outside.join("marker").is_dir());
    }

    #[test]
    fn test_format_ls_human_empty_payload() {
        let payload = as_map(json!({"tmux": "ok", "teams": []}));
        assert_eq!(format_ls_human(&payload), vec!["no hive teams"]);
    }

    #[test]
    fn test_format_ls_human_groups_live_detached_and_other() {
        let payload = as_map(json!({
            "tmux": "ok",
            "teams": [
                {
                    "team": "honey",
                    "state": "live-incomplete",
                    "window": "dev:1",
                    "members": [
                        {"name": "orch", "cli": "claude", "live": true},
                        {"name": "dodo", "cli": "codex", "live": false},
                    ],
                },
                {
                    "team": "comb",
                    "state": "detached",
                    "members": [{"name": "orch", "cli": "claude"}],
                },
                {"team": "wasp", "state": "corrupt"},
            ],
        }));
        let lines = format_ls_human(&payload);
        assert_eq!(
            lines,
            vec![
                "LIVE".to_string(),
                "  dev:1  honey · claude+codex  ! missing dodo".to_string(),
                String::new(),
                "DETACHED  — no tmux display".to_string(),
                "  comb · claude".to_string(),
                String::new(),
                "OTHER".to_string(),
                "  wasp  unreadable registry entry  ".to_string(),
            ]
        );
    }

    #[test]
    fn test_format_ls_human_flags_unknown_tmux() {
        let payload = as_map(json!({
            "tmux": "unknown",
            "teams": [{"team": "honey", "state": "unknown", "members": []}],
        }));
        let lines = format_ls_human(&payload);
        assert_eq!(
            lines[0],
            "! tmux did not answer — live/detached state unknown this pass"
        );
        assert!(lines.contains(&"OTHER".to_string()));
        assert!(lines.contains(&"  honey  unknown  ".to_string()));
    }

    #[test]
    fn test_ls_roster_prefers_cli_then_name() {
        let entry = as_map(json!({
            "members": [
                {"name": "orch", "cli": "claude"},
                {"name": "dodo", "cli": ""},
                {"name": "", "cli": ""},
            ]
        }));
        assert_eq!(ls_roster(&entry), "claude+dodo+?");
    }

    #[test]
    fn test_hive_skill_entry_is_each_clis_own_form() {
        assert_eq!(hive_skill_entry("claude"), "/hive:hive");
        assert_eq!(hive_skill_entry("codex"), "$hive");
        assert_eq!(hive_skill_entry("grok"), "/hive");
    }

    #[test]
    fn test_team_workspace_is_the_team_dir_under_the_registry_store() {
        let mut env = EnvGuard::new();
        let tmp = tempfile::TempDir::new().unwrap();
        env.set("HIVE_HOME", tmp.path().join(".hive"));
        assert_eq!(
            team_workspace("hornet"),
            tmp.path()
                .join(".hive")
                .join("teams")
                .join("hornet")
                .to_string_lossy()
                .as_ref()
        );
        assert_eq!(
            std::path::Path::new(&team_workspace("hornet"))
                .join("team.json")
                .to_string_lossy()
                .as_ref(),
            crate::registry::entry_path("hornet")
                .unwrap()
                .to_string_lossy()
                .as_ref()
        );
    }

    #[test]
    fn test_prepare_workspace_resets_the_team_dir_but_keeps_team_json() {
        let mut env = EnvGuard::new();
        let tmp = tempfile::TempDir::new().unwrap();
        env.set("HIVE_HOME", tmp.path().join(".hive"));
        crate::registry::record_team("hornet", "", "1.0", &[], "").unwrap();
        let ws = team_workspace("hornet");
        let dir = std::path::Path::new(&ws);
        // a deleted predecessor's leftovers
        std::fs::create_dir_all(dir.join("artifacts")).unwrap();
        std::fs::write(dir.join("artifacts").join("old.md"), "x").unwrap();
        std::fs::write(dir.join("hive.db"), "stale").unwrap();

        prepare_workspace("hornet", &ws, false, &["k=v".to_string()]).unwrap();

        assert!(crate::registry::load("hornet").is_some());
        assert!(!dir.join("artifacts").join("old.md").exists());
        assert!(dir.join("hive.db").is_file());
        assert_ne!(std::fs::read(dir.join("hive.db")).unwrap(), b"stale");
        assert!(dir.join("run").is_dir());
        assert_eq!(
            std::fs::read_to_string(dir.join("state").join("k")).unwrap(),
            "v"
        );
    }

    #[test]
    fn test_is_team_dir_reads_through_a_trailing_slash_and_a_tilde() {
        let mut env = EnvGuard::new();
        let tmp = tempfile::TempDir::new().unwrap();
        env.set("HOME", tmp.path());
        env.set("HIVE_HOME", tmp.path().join(".hive"));
        let ws = team_workspace("hornet");
        assert!(is_team_dir("hornet", &ws));
        assert!(is_team_dir("hornet", &format!("{ws}/")));
        assert!(is_team_dir("hornet", &format!("{ws}/.")));
        assert!(is_team_dir("hornet", "~/.hive/teams/hornet/"));
        assert!(!is_team_dir("hornet", "~/.hive/teams/comb"));
        assert!(!is_team_dir("hornet", &format!("{ws}-2")));
    }

    #[test]
    fn test_check_explicit_workspace_refuses_another_teams_dir_under_the_store() {
        let mut env = EnvGuard::new();
        let tmp = tempfile::TempDir::new().unwrap();
        env.set("HIVE_HOME", tmp.path().join(".hive"));
        let store = tmp.path().join(".hive").join("teams");
        assert!(check_explicit_workspace("hornet", "").is_ok());
        assert!(check_explicit_workspace("hornet", &team_workspace("hornet")).is_ok());
        assert!(
            check_explicit_workspace("hornet", &format!("{}/", team_workspace("hornet"))).is_ok()
        );
        assert!(
            check_explicit_workspace("hornet", tmp.path().join("elsewhere").to_str().unwrap())
                .is_ok()
        );
        for inside in [
            store.join("comb"),
            store.join("comb").join("artifacts"),
            store.clone(),
        ] {
            let error = check_explicit_workspace("hornet", inside.to_str().unwrap())
                .unwrap_err()
                .to_string();
            assert!(error.contains("registry store"), "{error}");
        }
    }

    #[test]
    fn test_prepare_workspace_resets_the_team_dir_given_with_a_trailing_slash() {
        let mut env = EnvGuard::new();
        let tmp = tempfile::TempDir::new().unwrap();
        env.set("HIVE_HOME", tmp.path().join(".hive"));
        crate::registry::record_team("hornet", "", "1.0", &[], "").unwrap();
        let ws = team_workspace("hornet");
        let dir = std::path::Path::new(&ws);
        std::fs::create_dir_all(dir.join("artifacts")).unwrap();
        std::fs::write(dir.join("artifacts").join("old.md"), "x").unwrap();

        prepare_workspace("hornet", &format!("{ws}/"), false, &[]).unwrap();

        assert!(crate::registry::load("hornet").is_some());
        assert!(!dir.join("artifacts").join("old.md").exists());
        assert!(dir.join("hive.db").is_file());
    }

    #[test]
    fn test_prepare_workspace_keeps_an_explicit_dir_unless_reset() {
        let mut env = EnvGuard::new();
        let tmp = tempfile::TempDir::new().unwrap();
        env.set("HIVE_HOME", tmp.path().join(".hive"));
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(ws.join("artifacts")).unwrap();
        std::fs::write(ws.join("artifacts").join("keep.md"), "x").unwrap();
        let ws_str = ws.to_string_lossy().into_owned();

        prepare_workspace("hornet", &ws_str, false, &[]).unwrap();
        assert!(ws.join("artifacts").join("keep.md").is_file());
        assert!(ws.join("hive.db").is_file());

        prepare_workspace("hornet", &ws_str, true, &[]).unwrap();
        assert!(!ws.join("artifacts").join("keep.md").exists());
        assert!(ws.join("hive.db").is_file());
    }

    #[test]
    fn test_team_grok_daemon_keys_selects_only_this_teams_members() {
        let mut env = EnvGuard::new();
        let tmp = tempfile::TempDir::new().unwrap();
        env.set("GROK_HOME", tmp.path());
        let hive = tmp.path().join("hive");
        std::fs::create_dir_all(&hive).unwrap();
        for name in [
            "m-hornet.ant.sock",
            "m-hornet.bee.sock",
            "m-comb.ant.sock",
            "p19.sock",
        ] {
            std::fs::write(hive.join(name), "").unwrap();
        }

        assert_eq!(
            team_grok_daemon_keys("hornet"),
            vec!["m-hornet.ant".to_string(), "m-hornet.bee".to_string()]
        );
    }

    #[test]
    fn test_sweep_team_grok_daemons_clears_only_this_teams_keys() {
        let mut env = EnvGuard::new();
        let tmp = tempfile::TempDir::new().unwrap();
        env.set("GROK_HOME", tmp.path());
        let hive = tmp.path().join("hive");
        std::fs::create_dir_all(&hive).unwrap();
        for name in [
            "m-hornet.ant.sock",
            "m-hornet.ant.pid",
            "m-hornet.ant.lock",
            "m-hornet.ant.session",
            "m-comb.ant.sock",
            "p19.sock",
        ] {
            std::fs::write(hive.join(name), "").unwrap();
        }

        sweep_team_grok_daemons("hornet");

        for gone in [
            "m-hornet.ant.sock",
            "m-hornet.ant.pid",
            "m-hornet.ant.lock",
            "m-hornet.ant.session",
        ] {
            assert!(!hive.join(gone).exists(), "{gone} survived the sweep");
        }
        assert!(hive.join("m-comb.ant.sock").exists());
        assert!(hive.join("p19.sock").exists());
    }

    #[test]
    fn test_reuse_existing_binding_refuses_another_name_for_a_paneless_session() {
        let mut bound = Map::new();
        bound.insert("team".to_string(), Value::String("honey".to_string()));
        bound.insert("agent".to_string(), Value::String("rex".to_string()));
        bound.insert("pane".to_string(), Value::String(String::new()));
        assert_eq!(reuse_existing_binding(&Map::new(), "wasp"), Ok(false));
        assert_eq!(reuse_existing_binding(&bound, ""), Ok(true));
        assert_eq!(reuse_existing_binding(&bound, "honey"), Ok(true));
        let refused = reuse_existing_binding(&bound, "wasp").unwrap_err();
        assert!(refused.contains("honey.rex") && refused.contains("wasp"));
        // a tagged pane is the team's display: idempotent whatever the name
        bound.insert("pane".to_string(), Value::String("%7".to_string()));
        assert_eq!(reuse_existing_binding(&bound, "wasp"), Ok(true));
    }

    #[test]
    fn test_kill_address_prefers_the_explicit_team_over_the_prefix() {
        let mut env = EnvGuard::new();
        let tmp = tempfile::TempDir::new().unwrap();
        env.set("HIVE_HOME", tmp.path().join("hive"));
        crate::registry::record_team("hornet", "/tmp/ws-hn", "1.0", &[], "").unwrap();

        // bare name: the pane's team decides, unless -t names one
        assert_eq!(kill_address("ant", ""), (String::new(), "ant".to_string()));
        assert_eq!(
            kill_address("ant", "hornet"),
            ("hornet".to_string(), "ant".to_string())
        );
        // the qualified form keeps working on its own
        assert_eq!(
            kill_address("hornet.ant", ""),
            ("hornet".to_string(), "ant".to_string())
        );
    }
}
