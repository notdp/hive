//! Team verbs: `create` (an orch pane, a shell pane, or a Claude session
//! outside tmux), `join`, `delete`, `team`, `ls`, `doctor`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde_json::{Map, Value};

use super::util::{fail, json_pretty, ok_or_fail, parse_entries, resolve_sender};
use crate::identity;
use crate::json_fields::map_str;
use crate::paths::{expanduser, getcwd};
use crate::team::{
    add_runtime_location_fields, created_at_key, gc_dead_teams, member_registry_row,
    remember_context, resolve_scoped_team, resolve_workspace, session_member_row, start_team_hived,
    start_team_hived_or_warn, team_status_payload, Team, LEAD_AGENT_NAME,
};
use crate::tmux;

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
pub(crate) fn create(
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
    if !identity::is_inside_tmux() {
        let team_name = if name.is_empty() {
            crate::naming::pick_team_name("", "", "0")
        } else {
            name.to_string()
        };
        create_detached_team(&team_name, desc, workspace, reset_workspace, state_entries);
        return;
    }
    let current_pane = identity::current_pane_id().unwrap_or_default();
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
        let window = identity::current_window_target().unwrap_or_default();
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
        name = crate::naming::pick_team_name(
            &identity::current_session_name().unwrap_or_default(),
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
    if let Some(warning) = tmux::stale_version_warning() {
        eprintln!("{warning}");
    }
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
fn create_detached_team(
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
                && crate::registry::member_for_session(&creator.session_id, None).is_none() =>
        {
            Some(crate::team::with_host_session(
                session_member_row(LEAD_AGENT_NAME, "claude", &creator.session_id),
                creator,
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
        ok_or_fail(crate::team_display::new_team_session_window(name));
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
            ok_or_fail(crate::team_display::bind_member_viewer(
                &first_pane,
                orch,
                name,
                &ws_str,
                "mirror",
            ));
            start_team_hived_or_warn(&mut t, &ws_str);
        }
        None => {
            // No orch: the first pane is the team's own shell pane, tagged
            // the way a shell-pane create tags its pane (`Team::create`), so
            // a verb run from it finds the team through its own tags — the
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
    if let Some(warning) = tmux::stale_version_warning() {
        eprintln!("{warning}");
    }
    if orch_member.is_some() {
        println!("You are {name}.{LEAD_AGENT_NAME}.");
        println!(
            "{}",
            title_badge_hint(&format!("[{name}.{LEAD_AGENT_NAME}] "))
        );
    } else if let Some(creator) = creator.as_ref().filter(|c| !c.session_id.is_empty()) {
        if let Some((e_team, e_name)) =
            crate::registry::member_for_session(&creator.session_id, None)
        {
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

    let existing = identity::discover_tmux_binding();
    match reuse_existing_binding(&existing, name) {
        Ok(true) => return existing,
        Ok(false) => {}
        Err(message) => fail(&message),
    }

    let session_name = identity::current_session_name().unwrap_or_else(|| "hive".to_string());
    let session_name = if session_name.is_empty() {
        "hive".to_string()
    } else {
        session_name
    };
    let orch_cli = crate::agent_cli::resolve_spawn_cli_name(None);
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
        crate::naming::pick_team_name(&session_name, &final_window_id, &final_index)
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
    start_team_hived_or_warn(&mut t, &ws_str);
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
    if identity::is_codex_tool_env() {
        // Running from inside the codex TUI's own tool: pane record or
        // codex roster membership is the identity, and the shared daemon
        // must answer.
        if identity::current_codex_thread_is_hive_managed()
            && crate::adapters::codex_app_server::daemon_alive()
        {
            return;
        }
        fail(&super::codex_relaunch_message());
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

/// Join a team.
///
/// Outside tmux: the current Claude session enters TEAM's roster as a full
/// member. Inside tmux: the current pane (or --pane) registers into the
/// window's team.
pub(crate) fn join_cmd(
    team_arg: &str,
    name_override: &str,
    pane_override: &str,
    notify: bool,
    group_name: &str,
) {
    if !identity::is_inside_tmux() {
        if !pane_override.is_empty() {
            fail("--pane needs tmux; outside tmux `hive join <team>` joins this session");
        }
        join_as_ccd(team_arg, name_override);
        return;
    }

    let binding = identity::discover_tmux_binding();
    let team_name = if team_arg.is_empty() {
        map_str(&binding, "team")
    } else {
        team_arg.to_string()
    };
    if team_name.is_empty() {
        fail("no team in scope — pass a team (see `hive ls`) or run from a bound window");
    }
    let pane_id = if pane_override.is_empty() {
        identity::current_pane_id().unwrap_or_default()
    } else {
        pane_override.to_string()
    };
    if pane_id.is_empty() {
        fail("cannot determine current pane");
    }

    let mut t = match Team::load(&team_name, &identity::current_pane_id().unwrap_or_default()) {
        Ok(t) => t,
        Err(e) => fail(&e.to_string()),
    };
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

    let mut seen_names = crate::naming::window_seen_names(&t, &panes);
    if let Err(error) = crate::naming::claim_member_name(name_override, &mut seen_names) {
        fail(&error);
    }

    let (role, pane_cli) = crate::agent_cli::classify_pane(&target_pane);
    if role != "agent" {
        fail(&format!(
            "pane '{pane_id}' is not running an agent CLI; only agent panes can be registered"
        ));
    }
    let agent_name = if name_override.is_empty() {
        crate::naming::derive_agent_name(&mut seen_names)
    } else {
        name_override.to_string()
    };
    let cwd = tmux::display_value(&pane_id, "#{pane_current_path}")
        .filter(|c| !c.is_empty())
        .unwrap_or_else(getcwd);
    ok_or_fail(crate::team::register_agent_member(
        &mut t,
        &pane_id,
        &team_name,
        &agent_name,
        &pane_cli,
        &cwd,
        notify,
        group_name,
    ));
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
fn join_as_ccd(team_name: &str, name_override: &str) {
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
    if let Some((e_team, e_name)) = crate::registry::member_for_session(&guest.session_id, None) {
        if e_team == team_name {
            println!("already a member: {e_team}.{e_name}");
            return;
        }
        fail(&format!(
            "this session is already {e_team}.{e_name}; leave with `hive kill {e_team}.{e_name}` first"
        ));
    }
    let mut seen = crate::naming::roster_names(&entry);
    seen.insert(LEAD_AGENT_NAME.to_string());
    if let Err(error) = crate::naming::claim_member_name(name_override, &mut seen) {
        fail(&error);
    }
    let member_name = if name_override.is_empty() {
        crate::naming::derive_agent_name(&mut seen)
    } else {
        name_override.to_string()
    };
    let row = crate::team::with_host_session(
        session_member_row(&member_name, "claude", &guest.session_id),
        &guest,
    );
    let _ = crate::registry::record_member(team_name, &row, "");
    // Eager display: the joined session gets its mirror pane now, not at
    // the next attach.
    if let Some(entry) = crate::registry::load(team_name) {
        ok_or_fail(crate::team_display::ensure_team_display(&entry));
    }
    println!("joined: {team_name}.{member_name}");
    println!(
        "{}",
        title_badge_hint(&format!("[{team_name}.{member_name}] "))
    );
}

/// Show team overview.
pub(crate) fn team_cmd(team_arg: &str) {
    gc_dead_teams();
    let scoped = if team_arg.is_empty() {
        identity::default_team().unwrap_or_default()
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
    if !identity::is_inside_tmux() {
        fail("no team in scope — pass -t <team> (see `hive ls`)");
    }
    let mut result = Map::new();
    result.insert("team".to_string(), Value::Null);
    let session_name = identity::current_session_name();
    let window_target = identity::current_window_target();
    let current_pane = identity::current_pane_id();
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
    if let Some((major, minor)) = tmux::version() {
        tmux_payload.insert(
            "version".to_string(),
            Value::from(format!("{major}.{minor}")),
        );
    }
    if let Some(warning) = tmux::stale_version_warning() {
        tmux_payload.insert("warning".to_string(), Value::from(warning));
    }
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

/// Diagnose agent connectivity and session state.
///
/// The report is always JSON on stdout: with no reachable hived it still
/// carries the workspace's `runDir` and `logs` map (the debugging entry
/// points) next to a `hived` section saying why, and the exit status is 1.
pub(crate) fn doctor(agent_name: &str) {
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
fn doctor_report(t: &mut Team, ws: &str, target_name: &str) -> (Map<String, Value>, bool) {
    doctor_answer(t, ws, target_name)
}

fn doctor_answer(t: &mut Team, ws: &str, target_name: &str) -> (Map<String, Value>, bool) {
    if let Err(err) = start_team_hived(t, ws) {
        return (hived_down_report(ws, &err.to_string()), false);
    }
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

/// Delete a team and clean up; `--down` retires every member first and
/// kills the team's tmux session after.
pub(crate) fn delete(name: &str, workspace: &str, delete_workspace: bool, down: bool) {
    ok_or_fail(crate::team::delete_team(
        name,
        workspace,
        delete_workspace,
        down,
    ));
}

mod ls;

pub(crate) use ls::ls_cmd;

#[cfg(test)]
mod tests;
