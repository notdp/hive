//! create/join for a managed session still viewed in its original terminal.

use anyhow::{bail, Result};
use serde_json::{json, Map, Value};

use crate::json_fields::map_str;
use crate::team::{created_at_key, session_member_row, Team, LEAD_AGENT_NAME};
use crate::terminal_handoff::{Client, Target};
use crate::{registry, tmux};

fn require_unbound(client: &Client) -> Result<()> {
    if let Some((team, member)) =
        registry::member_for_session(&client.session.id, Some(client.session.cli))
    {
        bail!("this session is already {team}.{member}; resume its team window");
    }
    Ok(())
}

fn bind(
    client: &Client,
    target: &Target,
    workspace: &str,
    group: &str,
) -> Result<Map<String, Value>> {
    client.session.bind(target)?;
    tmux::tag_pane(
        &target.pane,
        "agent",
        &target.member,
        &target.team,
        client.session.cli,
        group,
    );
    crate::context::save_context_for_pane(&target.pane, &target.team, workspace, &target.member)?;
    if tmux::get_pane_option(&target.pane, "hive-agent").as_deref() != Some(&target.member)
        || tmux::get_pane_option(&target.pane, "hive-team").as_deref() != Some(&target.team)
    {
        bail!("team pane disappeared while binding the engine session");
    }
    let mut row = session_member_row(&target.member, client.session.cli, &client.session.id);
    row.insert("cwd".into(), Value::String(client.session.cwd.clone()));
    Ok(row)
}

fn finish(
    client: &mut Client,
    target: &Target,
    team: &mut Team,
    workspace: &str,
) -> (&'static str, &'static str) {
    // Registry commit is authoritative even if the launcher died before it
    // could acknowledge. Never undo membership or reopen the local viewer.
    let result = match client.commit() {
        Ok(()) => (
            "transferred",
            "Continue the current task. The original terminal opens the team window automatically; do not ask the user to run hive attach.",
        ),
        Err(error) => {
            eprintln!("hive: {error}");
            if let Err(error) = crate::terminal_handoff::recover_viewer(target, &client.session)
            {
                eprintln!("hive: {error}");
            }
            (
                "team committed; terminal transfer needs recovery",
                "The team is registered, but terminal transfer needs recovery. Use hive attach with this team name to open its window.",
            )
        }
    };
    crate::team::start_team_hived_or_warn(team, workspace);
    crate::team::remember_context(&target.team, workspace, &target.member);
    let _ = crate::layout::ensure(&target.window, false);
    result
}

pub(super) fn create(mut client: Client, name: &str, description: &str) -> Result<Value> {
    require_unbound(&client)?;
    if registry::load(name).is_some() {
        bail!("team '{name}' already exists");
    }
    if let Ok((window, _)) = crate::team::find_team_window(name, "") {
        if !window.is_empty() {
            bail!("window {window} is still tagged for team '{name}'");
        }
    }
    let workspace = super::team_workspace(name);
    crate::devlog::check_socket_path_len(std::path::Path::new(&workspace))
        .map_err(anyhow::Error::msg)?;
    let (window, pane, new_session) =
        crate::team_display::new_team_session_window(name, &client.session.cwd)?;
    let mut prepared: Option<Target> = None;
    let built = (|| -> Result<Team> {
        let window_id = tmux::display_value(&pane, "#{window_id}")
            .ok_or_else(|| anyhow::anyhow!("team window disappeared"))?;
        let desc = if description.is_empty() {
            format!("auto-init from managed terminal ({window})")
        } else {
            description.to_string()
        };
        let t = Team::create_for_window(
            name,
            &window,
            &pane,
            LEAD_AGENT_NAME,
            &desc,
            &workspace,
            false,
        )?;
        let target = Target {
            team: name.into(),
            member: LEAD_AGENT_NAME.into(),
            created_at: created_at_key(t.created_at),
            pane: pane.clone(),
            window: window_id,
            token: crate::agent::uuid4(),
            owns_window: true,
            new_session,
        };
        target.mark()?;
        prepared = Some(target.clone());
        super::prepare_workspace(name, &workspace, false, &[])?;
        client.begin(&target)?;
        let row = bind(&client, &target, &workspace, "")?;
        if registry::create_team(name, &workspace, &target.created_at, &[row], &target.window)?
            != "written"
        {
            bail!("team '{name}' was created concurrently; nothing was enrolled");
        }
        Ok(t)
    })();
    let mut team = match built {
        Ok(team) => team,
        Err(error) => {
            if let Some(target) = prepared {
                target.rollback(&client.session);
            } else {
                tmux::kill_pane(&pane);
            }
            return Err(error);
        }
    };
    let target = prepared.expect("successful build prepared target");
    let (status, next_step) = finish(&mut client, &target, &mut team, &workspace);
    Ok(
        json!({"team":name, "window":window, "orch":{"pane":pane,"name":LEAD_AGENT_NAME,"cli":client.session.cli},
        "workspace":workspace, "protocol":"/hive:hive", "handoff":status, "nextStep":next_step}),
    )
}

pub(super) fn join(
    mut client: Client,
    entry: &Map<String, Value>,
    name: &str,
    _notify: bool,
    group: &str,
) -> Result<Value> {
    require_unbound(&client)?;
    let team_name = map_str(entry, "team");
    let workspace = map_str(entry, "workspace");
    let mut seen = crate::naming::roster_names(entry);
    seen.insert(LEAD_AGENT_NAME.to_string());
    crate::naming::claim_member_name(name, &mut seen).map_err(anyhow::Error::msg)?;
    let member = if name.is_empty() {
        crate::naming::derive_agent_name(&mut seen)
    } else {
        name.to_string()
    };
    let (window, _) = crate::team_display::ensure_team_display(entry)?;
    let mut team = Team::load(&team_name, "")?;
    let anchor = tmux::list_panes_full(&window)
        .first()
        .map(|p| p.pane_id.clone())
        .ok_or_else(|| anyhow::anyhow!("team has no live window"))?;
    let pane = tmux::split_window(
        &anchor,
        crate::layout::split_horizontal(&window),
        None,
        true,
        Some(&client.session.cwd),
    )?;
    let target = Target {
        team: team_name.clone(),
        member: member.clone(),
        created_at: created_at_key(team.created_at),
        pane: pane.clone(),
        window: team.tmux_window_id.clone(),
        token: crate::agent::uuid4(),
        owns_window: false,
        new_session: false,
    };
    if let Err(error) = target.mark() {
        tmux::kill_pane(&pane);
        return Err(error);
    }
    let built = (|| -> Result<()> {
        client.begin(&target)?;
        let row = bind(&client, &target, &workspace, group)?;
        // Reserve the full row in one write: a concurrent join cannot
        // overwrite another member between name selection and commit.
        if registry::reserve_member(&team_name, &row, &target.created_at)? != "reserved" {
            bail!("team or member changed during join; nothing was enrolled");
        }
        Ok(())
    })();
    if let Err(error) = built {
        target.rollback(&client.session);
        return Err(error);
    }
    let (status, next_step) = finish(&mut client, &target, &mut team, &workspace);
    // This invocation is the joining engine's own tool. The result gives
    // its identity directly, without sending a second turn into that job.
    Ok(
        json!({"joined":member,"role":"agent","pane":pane,"team":team_name,"group":group,"handoff":status, "nextStep":next_step}),
    )
}
