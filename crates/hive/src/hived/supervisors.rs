// --------------------------------------------------------------------------
// supervisors
// --------------------------------------------------------------------------

use std::collections::{HashMap, HashSet};
use std::fs;

use serde_json::{Map, Value};

use crate::agent::Agent;

use super::*;

/// Reap grok leader daemons that nothing owns any more.
///
/// Two lifecycles, told apart by key shape:
///
/// - ``m-<team>.<member>`` — registry-driven: the engine belongs to a team
///   member, so a dead pane means nothing (the display closed). Reap only
///   when the team's registry file is *valid and lists no such member*
///   (kill/delete removed it), or the file is *missing entirely* (the team
///   was deleted/archived). An unreadable entry is never grounds to kill a
///   daemon, and a young pidfile gets a grace window so a spawn's
///   registration in flight cannot be raced.
/// - ``p<slug>`` — a raw ``hive grok`` pane outside any team keeps the old
///   pane lifecycle: pane gone, daemon reaped.
///
/// The leader directory is global while a registry is scoped to one
/// `$HIVE_HOME`, so a member key is only this hived's business when it names
/// this hived's own team. A hived running against a disposable `$HIVE_HOME`
/// (the acceptance lane, a dev sandbox) otherwise reads the live team's key,
/// finds no entry for it in its own registry, and reaps a member that is
/// serving someone.
///
/// Killing a leader takes its attached TUI down with it, so every reap is
/// logged; ``is_pane_alive`` only reports dead panes from a successful tmux
/// listing, never from a transient tmux failure.
pub(crate) fn cleanup_dead_daemons(workspace: &str, team: &str) {
    for key in hooked_gl_list_daemon_keys() {
        let binding = crate::adapters::grok_leader::member_from_key(&key);
        match binding {
            None => {
                let slug = &key[1.min(key.len())..];
                if slug.is_empty() || !slug.chars().all(|c| c.is_ascii_digit()) {
                    continue;
                }
                let pane = format!("%{slug}");
                if hooked_is_pane_alive(&pane) {
                    continue;
                }
            }
            Some((key_team, member)) => {
                if key_team != team {
                    continue; // another team's engine, another hived's call
                }
                let Some(path) = crate::registry::entry_path(&key_team) else {
                    continue;
                };

                if path.is_file() {
                    let Some(entry) = crate::registry::load(&key_team) else {
                        continue; // unreadable is not proof of absence
                    };
                    let listed = entry
                        .get("members")
                        .and_then(Value::as_array)
                        .map(|members| {
                            members.iter().any(|m| {
                                m.get("name").and_then(Value::as_str) == Some(member.as_str())
                            })
                        })
                        .unwrap_or(false);
                    if listed {
                        continue;
                    }
                }
                // Missing registry file, or a valid roster without this
                // member: the engine is an orphan — but never a newborn one.
                let pidfile = hooked_gl_socket_path_for_key(&key).with_extension("pid");
                let Ok(metadata) = fs::metadata(&pidfile) else {
                    continue; // no pidfile yet: daemon mid-start
                };
                let Ok(mtime) = metadata.modified() else {
                    continue;
                };
                let age = std::time::SystemTime::now()
                    .duration_since(mtime)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                if age < _GROK_REAP_GRACE_SECONDS {
                    continue;
                }
            }
        }
        hooked_notify_debug_emit(
            workspace,
            "daemon.reap",
            &[("key", Value::from(key.clone()))],
        );
        // Drop the pool's client BEFORE killing the daemon: a grok stdio
        // client that outlives its leader auto-spawns a replacement on the
        // same socket, resurrecting an orphan mid-reap.
        hooked_gl_pool_drop_key(&key);
        hooked_gl_kill_daemon_key(&key);
    }
}

/// Keep this team's codex members riding the shared daemon.
pub(crate) fn codex_supervisor_tick(workspace: &str, team: &str) {
    let panes = hooked_list_panes_all();
    if panes.is_empty() {
        return; // an empty listing is a tmux failure, not an empty server
    }
    let live_panes: HashSet<String> = panes.iter().map(|p| p.pane_id.clone()).collect();
    for pane in hooked_cas_list_recorded_panes() {
        if !live_panes.contains(&pane) {
            hooked_cas_clear_pane_thread(&pane);
            codex_reattach_at()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&pane);
        }
    }

    let Ok(t) = hooked_team_load(team) else {
        return;
    };
    let members: Vec<&Agent> = t
        .agents
        .iter()
        .filter(|a| a.cli == "codex" && live_panes.contains(&a.pane_id))
        .collect();
    if members.is_empty() {
        return;
    }

    if !hooked_cas_daemon_alive() {
        hooked_cas_drop_client();
        let respawned = hooked_cas_spawn_daemon();
        hooked_notify_debug_emit(
            workspace,
            "codex.daemon.respawn",
            &[("ok", Value::Bool(respawned))],
        );
        if !respawned {
            return;
        }
    }

    let now = monotonic();
    for agent in members {
        let Some(thread_id) = hooked_cas_thread_id_for_pane(&agent.pane_id) else {
            continue;
        };
        if thread_id.is_empty() {
            continue;
        }
        if hooked_detect_cli_process_for_pane(&agent.pane_id).is_some() {
            continue; // CLI (codex or another agent) is on the TTY — leave it
        }
        let last = codex_reattach_at()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&agent.pane_id)
            .copied()
            .unwrap_or(f64::NEG_INFINITY);
        if now - last < _CODEX_REATTACH_COOLDOWN_SECONDS {
            continue;
        }
        let command =
            hooked_display_value(&agent.pane_id, "#{pane_current_command}").unwrap_or_default();
        if !crate::agent_cli::is_shell_command(&command) {
            continue; // not at a shell prompt (vim, ssh, …): never type into it
        }
        codex_reattach_at()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(agent.pane_id.clone(), now);
        hooked_notify_debug_emit(
            workspace,
            "codex.member.reattach",
            &[
                ("pane", Value::from(agent.pane_id.clone())),
                ("agent", Value::from(agent.name.clone())),
                ("thread", Value::from(thread_id.clone())),
            ],
        );
        hooked_send_keys(&agent.pane_id, &format!("hive codex resume {thread_id}"));
    }
}

/// Prune claude pane job records whose pane died; park the orphans.
///
/// Records are machine-level (like codex's thread records), so staleness
/// must never rebind a recycled pane id to a foreign job. A record whose
/// pane is gone also means nobody is watching that engine any more:
/// ``claude stop`` parks it — the job stays in the ledger and ``hive claude
/// --resume <jobId>`` (or a delivery) can still wake it, so nothing is lost,
/// but no orphan engine keeps burning in the background.
///
/// No respawn/reattach half: the engine's life is claude's own supervisor's
/// business (wake happens on demand at delivery), and the pane viewer
/// self-heals through the managed launcher's attach loop — a user who
/// deliberately left the loop must not be typed at.
pub(crate) fn claude_supervisor_tick(workspace: &str) {
    let panes = hooked_list_panes_all();
    if panes.is_empty() {
        return; // an empty listing is a tmux failure, not an empty server
    }
    let live_panes: HashSet<String> = panes.iter().map(|p| p.pane_id.clone()).collect();
    for pane in hooked_cb_list_recorded_panes() {
        if live_panes.contains(&pane) {
            continue;
        }
        let record = hooked_cb_read_pane_job(&pane);
        hooked_cb_clear_pane_job(&pane);
        if let Some(record) = record {
            hooked_notify_debug_emit(
                workspace,
                "claude.job.park",
                &[
                    ("pane", Value::from(pane.clone())),
                    ("job", Value::from(record.job_id.clone())),
                ],
            );
            hooked_cb_stop_job(&record.job_id);
        }
    }
}

/// Shared per-loop state for the claude name/view ticks.
#[derive(Debug, Default)]
pub struct ClaudeTickState {
    pub named: HashSet<String>,
    #[allow(clippy::type_complexity)]
    pub signature: Option<(Vec<String>, Vec<(String, String)>)>,
    pub labels: HashMap<String, String>,
}

/// Keep each claude member's job labelled `<team>.<member>`.
///
/// A member spawned by hive is minted under that name already; one adopted
/// from a pane that was running claude first (join, `--resume`) was minted
/// before the pane carried any tag, so its job keeps a `hive-<pane>`
/// placeholder. The engine's registry entry — read anyway on every tick —
/// carries the current label, so the comparison is free and the rename fires
/// at most once per job.
///
/// The rename is one control frame, but its confirmation polls the registry
/// for up to a few seconds, so it goes to a thread: identity repair must not
/// stall delivery.
pub(crate) fn claude_name_tick(
    members: &[(String, Map<String, Value>)],
    team: &str,
    state: &mut ClaudeTickState,
) {
    let mut sorted: Vec<&(String, Map<String, Value>)> = members.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for (member, binding) in sorted {
        if binding.get("cli").and_then(Value::as_str) != Some("claude") {
            continue;
        }
        let pane = map_get_str(binding, "pane");
        let job_id = hooked_cb_job_id_for_pane(&pane).unwrap_or_default();
        let want = format!("{team}.{member}");
        if job_id.is_empty() || state.named.contains(&job_id) {
            continue;
        }
        let Some(engine) = hooked_cb_engine_session_for_job(&job_id) else {
            continue; // asleep or gone: retry on a later tick
        };
        state.named.insert(job_id.clone());
        if engine.name == want {
            continue;
        }
        hooked_ensure_job_named_thread(&job_id, &want);
    }
}

/// Follow the human's attach-panel switches on this team's claude panes.
///
/// A member pane is an attach viewer: pressing the panel key inside it opens
/// any other bg session, and the pane keeps its member tags while the screen
/// shows something else. Each pane's ``@hive-view`` tag carries what is
/// really on screen (empty while it shows its own member) and the border
/// renders it; a switch onto *another* hive member is also logged, which is
/// what a whole-window follow would key on later.
///
/// Two cheap signals gate the work: the attach journal's entry set (an entry
/// appears/disappears on every attach, switch and detach) and the panes'
/// titles (the panel writes the viewed session's name). Probing costs a ps
/// per pane, so it only runs when one of those changed.
pub(crate) fn claude_view_tick(
    workspace: &str,
    team: &str,
    members: &[(String, Map<String, Value>)],
    state: &mut ClaudeTickState,
) {
    let panes = hooked_list_panes_all();
    if panes.is_empty() {
        return; // an empty listing is a tmux failure, not an empty server
    }
    let titles: HashMap<String, String> = panes
        .iter()
        .filter(|p| p.cli == "claude")
        .map(|p| (p.pane_id.clone(), p.title.clone()))
        .collect();
    let mut sorted_titles: Vec<(String, String)> =
        titles.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    sorted_titles.sort();
    let signature = (hooked_cv_journal_signature(), sorted_titles);
    if state.signature.as_ref() == Some(&signature) {
        return;
    }
    state.signature = Some(signature);

    let mut sorted: Vec<&(String, Map<String, Value>)> = members.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, binding) in sorted {
        let pane_id = map_get_str(binding, "pane");
        if binding.get("cli").and_then(Value::as_str) != Some("claude")
            || !titles.contains_key(&pane_id)
        {
            continue;
        }
        let own_job = hooked_cb_job_id_for_pane(&pane_id).unwrap_or_default();
        let view = hooked_cv_view_for_pane(&pane_id, Some(panes.as_slice()));
        let label = crate::adapters::claude_view::view_label(&view, &own_job);
        if state.labels.get(&pane_id) == Some(&label) {
            continue;
        }
        state.labels.insert(pane_id.clone(), label.clone());
        hooked_set_pane_option(&pane_id, "hive-view", &label);
        if view.kind == "member_view" && view.job_id != own_job {
            let other_team = view.member.split('.').next().unwrap_or("") != team;
            hooked_notify_debug_emit(
                workspace,
                "claude.view.foreign_member",
                &[
                    ("team", Value::from(team)),
                    ("member", Value::from(name.clone())),
                    ("pane", Value::from(pane_id.clone())),
                    ("viewing", Value::from(view.member.clone())),
                    ("viewingJob", Value::from(view.job_id.clone())),
                    ("otherTeam", Value::Bool(other_team)),
                    ("certainty", Value::from(view.certainty.clone())),
                ],
            );
        }
    }
}

/// Backfill the team's registry entry from live observation.
///
/// Refreshes fields of members the registry already knows (model switch,
/// cwd change, a sessionId learned late) and the display cache. The cwd
/// comes from `Team::load`'s pane merge, which reads `#{pane_current_path}`
/// off the live pane, so this is the only lane that follows a member's `cd`
/// into the registry. It never adds or removes a roster name — membership
/// belongs to the CLI writers, and the whole read-merge-write runs under the
/// store lock so an observation racing a `hive kill` cannot resurrect the
/// killed member.
pub(crate) fn write_registry_backfill(workspace: &str, team: &str) {
    let Ok(t) = hooked_team_load(team) else {
        return;
    };
    if t.name.is_empty() || t.agents.is_empty() {
        return;
    }
    let mut observed: Vec<Map<String, Value>> = Vec::new();
    let mut sorted_agents: Vec<&Agent> = t.agents.iter().collect();
    sorted_agents.sort_by(|a, b| a.name.cmp(&b.name));
    for agent in sorted_agents {
        if agent.pane_id.is_empty() {
            continue; // registry-only member: nothing on screen to observe
        }
        let mut session_id = hooked_fresh_snapshot_session_id(&agent.pane_id, None);
        if session_id.is_empty() {
            session_id = agent.session_id.clone().unwrap_or_default();
        }
        if session_id.is_empty() && agent.cli == "grok" {
            // Daemon-family runtimes never reach the transcript-probe path
            // that feeds runtime snapshots, so a grok member's session id
            // must come straight from its leader record.
            session_id = hooked_gl_session_id_for_pane(&agent.pane_id).unwrap_or_default();
        }
        let model = hooked_resolve_model_for_pane(&agent.pane_id, &agent.cli, "");
        let mut row = Map::new();
        row.insert("name".to_string(), Value::from(agent.name.clone()));
        row.insert("cli".to_string(), Value::from(agent.cli.clone()));
        row.insert(
            "model".to_string(),
            Value::from(if model.is_empty() {
                agent.model.clone()
            } else {
                model
            }),
        );
        row.insert("sessionId".to_string(), Value::from(session_id));
        row.insert("cwd".to_string(), Value::from(agent.cwd.clone()));
        observed.push(row);
    }

    let _ = crate::registry::backfill(
        &t.name,
        &observed,
        &py_float_str(t.created_at),
        &t.tmux_window_id,
        workspace,
    );
}
