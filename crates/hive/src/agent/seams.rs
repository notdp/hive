// ---------------------------------------------------------------------------
// Seams: every cross-module effect goes through one wrapper so the unit tests
// can intercept it the way the Python suite monkeypatches the module globals.
// Without an installed test hook each wrapper is a plain passthrough.
// ---------------------------------------------------------------------------

use std::collections::HashMap;
use std::time::Duration;

#[cfg(test)]
use anyhow::bail;

use crate::adapters::claude_bg::{EngineSession, KeyResult};
use crate::adapters::claude_sessions;

use super::support::{wait_codex_attached, wait_grok_session_ready, AGENT_STARTUP_TIMEOUT};
#[cfg(test)]
use super::testhook;

pub(super) fn hooked_is_inside_tmux() -> bool {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| h.is_inside_tmux) {
        return v;
    }
    crate::tmux::is_inside_tmux()
}

pub(super) fn hooked_split_window(
    target: &str,
    horizontal: bool,
    size: Option<&str>,
) -> anyhow::Result<String> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.event_order.push(format!("split:{target}"));
        h.split_window_result
            .clone()
            .unwrap_or_else(|| Ok(target.to_string()))
    }) {
        return v.map_err(|msg| anyhow::anyhow!(msg));
    }
    crate::tmux::split_window(target, horizontal, size, true, None)
}

pub(super) fn hooked_get_pane_window_target(pane_id: &str) -> String {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| h.pane_window_target.clone()) {
        return v;
    }
    crate::tmux::get_pane_window_target(pane_id).unwrap_or_default()
}

pub(super) fn hooked_set_pane_title(pane_id: &str, title: &str) {
    #[cfg(test)]
    if testhook::with(|h| h.titles.push((pane_id.to_string(), title.to_string()))).is_some() {
        return;
    }
    crate::tmux::set_pane_title(pane_id, title);
}

pub(super) fn hooked_tag_pane(pane_id: &str, role: &str, agent: &str, team: &str, cli: &str) {
    #[cfg(test)]
    if testhook::with(|h| {
        h.tags.push((
            pane_id.to_string(),
            role.to_string(),
            agent.to_string(),
            team.to_string(),
        ))
    })
    .is_some()
    {
        return;
    }
    crate::tmux::tag_pane(pane_id, role, agent, team, cli, "");
}

pub(super) fn hooked_kill_pane(pane_id: &str) {
    #[cfg(test)]
    if testhook::with(|h| {
        h.event_order.push(format!("pane:{pane_id}"));
        h.killed.push(pane_id.to_string())
    })
    .is_some()
    {
        return;
    }
    crate::tmux::kill_pane(pane_id);
}

/// The pid tmux runs in the pane, read while the pane still exists.
pub(super) fn hooked_pane_pid(pane_id: &str) -> Option<u32> {
    #[cfg(test)]
    if let Some(pid) = testhook::with(|h| {
        h.event_order.push(format!("pid:{pane_id}"));
        h.pane_pid
    }) {
        return pid;
    }
    crate::tmux::pane_pid(pane_id)
}

/// Block until the pane's process is gone, or *timeout* passes.
///
/// Waiting on tmux's listing is not enough: `is_pane_alive` answers no the
/// instant kill-pane drops the pane record, while the TUI it hosted is
/// still on its way out — and a dying grok TUI still raises a leader. So
/// the caller reads `#{pane_pid}` before the kill and this waits on that
/// process. Without a pid (tmux never answered) the listing is all there
/// is, and it is used as the weaker signal it is.
pub(super) fn hooked_wait_pane_exit(pane_id: &str, pid: Option<u32>, timeout: f64) {
    #[cfg(test)]
    if testhook::with(|h| {
        h.event_order.push(format!("wait:{pane_id}"));
        h.waited_pane_gone.push(pane_id.to_string())
    })
    .is_some()
    {
        return;
    }
    let deadline = std::time::Instant::now() + Duration::from_secs_f64(timeout.max(0.0));
    loop {
        let gone = match pid {
            Some(pid) => !process_alive(pid),
            None => !crate::tmux::is_pane_alive(pane_id),
        };
        if gone || std::time::Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// True while the pid names a process, signal 0 being the "is it there"
/// probe. `EPERM` is a live process this user may not signal, not an
/// absence; only `ESRCH` says it is gone.
fn process_alive(pid: u32) -> bool {
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

pub(super) fn hooked_clear_pane_tags(pane_id: &str) {
    #[cfg(test)]
    if testhook::with(|h| h.cleared_tags.push(pane_id.to_string())).is_some() {
        return;
    }
    crate::tmux::clear_pane_tags(pane_id);
}

pub(super) fn hooked_send_keys(pane_id: &str, text: &str, enter: bool) -> anyhow::Result<()> {
    #[cfg(test)]
    if testhook::with(|h| {
        h.event_order.push(format!("launch:{pane_id}"));
        h.calls.push(text.to_string());
        if enter {
            h.calls.push("<Enter>".to_string());
        }
    })
    .is_some()
    {
        return Ok(());
    }
    crate::tmux::send_keys(pane_id, text, enter)
}

pub(super) fn hooked_send_key(pane_id: &str, key: &str) -> anyhow::Result<()> {
    #[cfg(test)]
    if testhook::with(|h| h.calls.push(format!("<{key}>"))).is_some() {
        return Ok(());
    }
    crate::tmux::send_key(pane_id, key)
}

pub(super) fn hooked_is_pane_in_mode(pane_id: &str) -> bool {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| h.is_pane_in_mode) {
        return v;
    }
    crate::tmux::is_pane_in_mode(pane_id)
}

pub(super) fn hooked_cancel_pane_mode(pane_id: &str) {
    #[cfg(test)]
    if testhook::with(|h| h.cancelled_modes.push(pane_id.to_string())).is_some() {
        return;
    }
    crate::tmux::cancel_pane_mode(pane_id);
}

pub(super) fn hooked_load_buffer(name: &str, data: &str) -> anyhow::Result<()> {
    #[cfg(test)]
    if let Some(fails) = testhook::with(|h| {
        if !h.load_buffer_fails {
            h.buffers_loaded.push((name.to_string(), data.to_string()));
        }
        h.load_buffer_fails
    }) {
        if fails {
            bail!("tmux load-buffer timed out");
        }
        return Ok(());
    }
    crate::tmux::load_buffer(name, data)
}

pub(super) fn hooked_paste_buffer(name: &str, target: &str, bracketed: bool) {
    #[cfg(test)]
    if testhook::with(|h| h.pasted.push((name.to_string(), target.to_string()))).is_some() {
        return;
    }
    crate::tmux::paste_buffer(name, target, bracketed);
}

pub(super) fn hooked_delete_buffer(name: &str) {
    #[cfg(test)]
    if testhook::with(|h| h.deleted_buffers.push(name.to_string())).is_some() {
        return;
    }
    crate::tmux::delete_buffer(name);
}

pub(super) fn hooked_capture_pane(pane_id: &str, lines: u32) -> anyhow::Result<String> {
    #[cfg(test)]
    if testhook::with(|h| h.captured.push((pane_id.to_string(), lines))).is_some() {
        return Ok(String::new());
    }
    crate::tmux::capture_pane(pane_id, lines, false)
}

pub(super) fn hooked_sleep(seconds: f64) {
    #[cfg(test)]
    if testhook::with(|h| h.sleeps.push(seconds)).is_some() {
        return;
    }
    std::thread::sleep(Duration::from_secs_f64(seconds.max(0.0)));
}

pub(super) fn hooked_supported_profile(profile_name: &str) -> bool {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| h.supported_profile) {
        return v;
    }
    crate::draft_guard::supported_profile(profile_name)
}

pub(super) fn hooked_parse_draft(pane_id: &str, profile_name: &str) -> anyhow::Result<String> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| h.parse_draft.clone()) {
        return Ok(v.unwrap_or_default());
    }
    crate::draft_guard::parse_draft(pane_id, profile_name)
}

pub(super) fn hooked_clear_input(pane_id: &str, profile_name: &str) -> anyhow::Result<()> {
    #[cfg(test)]
    if let Some(fails) = testhook::with(|h| {
        if !h.clear_input_fails {
            h.draft_cleared.push(pane_id.to_string());
        }
        h.clear_input_fails
    }) {
        if fails {
            bail!("tmux clear-input timed out");
        }
        return Ok(());
    }
    crate::draft_guard::clear_input(pane_id, profile_name)
}

pub(super) fn hooked_wait_input_empty(
    pane_id: &str,
    profile_name: &str,
    timeout: f64,
) -> anyhow::Result<bool> {
    #[cfg(test)]
    if testhook::with(|_h| ()).is_some() {
        return Ok(true);
    }
    crate::draft_guard::wait_input_empty(pane_id, profile_name, Duration::from_secs_f64(timeout))
}

pub(super) fn hooked_resolve_session_id_for_pane(pane_id: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.resolved_session_panes.push(pane_id.to_string());
        h.session_ids_by_pane.get(pane_id).cloned()
    }) {
        return v;
    }
    crate::agent_cli::resolve_session_id_for_pane(pane_id, None)
}

pub(super) fn hooked_detect_cli_process_for_pane(
    pane_id: &str,
) -> Option<crate::agent_cli::CLIProfile> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        if !h.cli_probe_seq.is_empty() {
            let name = h.cli_probe_seq.remove(0);
            return name.and_then(|n| crate::agent_cli::get_profile(&n));
        }
        match &h.cli_probe {
            Some(name) if !name.is_empty() => crate::agent_cli::get_profile(name),
            _ => None,
        }
    }) {
        return v.cloned();
    }
    crate::agent_cli::detect_cli_process_for_pane(pane_id).cloned()
}

pub(super) fn hooked_interactive_claude_pid(pane_id: &str) -> Option<i32> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| h.interactive_claude_pid) {
        return v;
    }
    crate::adapters::claude_view::interactive_claude_pid(pane_id)
}

pub(super) fn hooked_wait_codex_attached(pane_id: &str) -> bool {
    #[cfg(test)]
    if let Some(Some(v)) = testhook::with(|h| {
        h.wait_codex_attached.inspect(|_| {
            h.waited_codex.push(pane_id.to_string());
        })
    }) {
        return v;
    }
    wait_codex_attached(pane_id, AGENT_STARTUP_TIMEOUT, 0.5)
}

pub(super) fn hooked_wait_grok_session_ready(pane_id: &str, session_id: &str) -> bool {
    #[cfg(test)]
    if let Some(Some(v)) = testhook::with(|h| {
        h.wait_grok_ready.inspect(|_| {
            h.waited_grok
                .push((pane_id.to_string(), session_id.to_string()));
            h.event_order.push(format!("ready:{pane_id}"));
        })
    }) {
        return v;
    }
    wait_grok_session_ready(pane_id, session_id, AGENT_STARTUP_TIMEOUT, 0.5)
}

// --- claude_bg seams -------------------------------------------------------

pub(super) fn hooked_job_id_for_pane(pane_id: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.pane_job_lookups.push(pane_id.to_string());
        h.job_id_for_pane.clone()
    }) {
        return v;
    }
    crate::adapters::claude_bg::job_id_for_pane(pane_id)
}

pub(super) fn hooked_engine_session_for_job(job_id: &str) -> Option<EngineSession> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.seen_jobs.push(job_id.to_string());
        h.engines_by_job.get(job_id).cloned()
    }) {
        return v;
    }
    crate::adapters::claude_bg::engine_session_for_job(job_id)
}

pub(super) fn hooked_job_row(job_id: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        if h.job_row_ids.iter().any(|j| j == job_id) {
            let mut row = serde_json::Map::new();
            row.insert(
                "id".to_string(),
                serde_json::Value::String(job_id.to_string()),
            );
            Some(row)
        } else {
            None
        }
    }) {
        return v;
    }
    crate::adapters::claude_bg::job_row(job_id, "claude")
}

pub(super) fn hooked_ensure_engine(job_id: &str, timeout: Option<f64>) -> Option<EngineSession> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.wakes.push(job_id.to_string());
        match &h.ensure_engine {
            None => Some(testhook::fake_engine(4321, job_id, "sess-registry")),
            Some(v) => v.clone(),
        }
    }) {
        return v;
    }
    crate::adapters::claude_bg::ensure_engine(job_id, timeout, "claude")
}

pub(super) fn hooked_wait_engine_entry(job_id: &str, timeout: f64) -> Option<EngineSession> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| h.wait_engine_entry.clone()) {
        return v;
    }
    crate::adapters::claude_bg::wait_engine_entry(job_id, timeout)
}

pub(super) fn hooked_spawn_job(
    cwd: &str,
    name: &str,
    prompt: &str,
    extra_args: &[String],
    extra_env: &HashMap<String, String>,
) -> Option<String> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.spawns.push(testhook::SpawnRecord {
            cwd: cwd.to_string(),
            name: name.to_string(),
            prompt: prompt.to_string(),
            extra_args: extra_args.to_vec(),
            extra_env: extra_env.clone(),
        });
        h.spawn_job_result.clone()
    }) {
        return v;
    }
    crate::adapters::claude_bg::spawn_job(cwd, name, prompt, extra_args, Some(extra_env), "claude")
}

pub(super) fn hooked_write_pane_job(
    pane_id: &str,
    job_id: &str,
    session_id: &str,
    cwd: &str,
) -> anyhow::Result<()> {
    #[cfg(test)]
    if testhook::with(|h| {
        h.records.push((
            pane_id.to_string(),
            job_id.to_string(),
            session_id.to_string(),
            cwd.to_string(),
        ))
    })
    .is_some()
    {
        return Ok(());
    }
    Ok(crate::adapters::claude_bg::write_pane_job(
        pane_id, job_id, session_id, cwd,
    )?)
}

pub(super) fn hooked_stop_job(job_id: &str) {
    #[cfg(test)]
    if testhook::with(|h| h.stopped.push(job_id.to_string())).is_some() {
        return;
    }
    crate::adapters::claude_bg::stop_job(job_id, "claude");
}

pub(super) fn hooked_type_into_job(job_id: &str, text: &str) -> KeyResult {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.type_into_job_result.clone().inspect(|_| {
            h.typed.push((job_id.to_string(), text.to_string()));
        })
    })
    .flatten()
    {
        return v;
    }
    crate::adapters::claude_bg::type_into_job(job_id, text, "claude")
}

pub(super) fn hooked_interrupt_job(job_id: &str) -> KeyResult {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.interrupt_job_result.clone().inspect(|_| {
            h.interrupted_jobs.push(job_id.to_string());
        })
    })
    .flatten()
    {
        return v;
    }
    crate::adapters::claude_bg::interrupt_job(job_id, "claude")
}

// --- claude_sessions seams -------------------------------------------------

pub(super) fn hooked_daemon_reply(session_id: &str, text: &str) -> Option<&'static str> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.daemon_replies
            .push((session_id.to_string(), text.to_string()));
        h.daemon_reply
    }) {
        return v;
    }
    claude_sessions::daemon_reply(session_id, text)
}

pub(super) fn hooked_claude_sessions_send(
    sock_path: &str,
    text: &str,
    sender: &str,
    session_id: &str,
) -> Option<&'static str> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.inbox_writes.push((
            sock_path.to_string(),
            text.to_string(),
            sender.to_string(),
            session_id.to_string(),
        ));
        h.sessions_send
    }) {
        return v;
    }
    claude_sessions::send(sock_path, text, sender, session_id)
}

pub(super) fn hooked_list_sessions() -> Vec<claude_sessions::ClaudeSession> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| h.list_sessions.clone()) {
        return v;
    }
    claude_sessions::list_sessions()
}

// --- codex_app_server seams ------------------------------------------------

pub(super) fn hooked_codex_spawn_daemon() -> bool {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.codex_started.push(());
        h.codex_spawn_daemon
    }) {
        return v;
    }
    crate::adapters::codex_app_server::spawn_daemon()
}

pub(super) fn hooked_ensure_dir_trusted(cwd: &str) -> anyhow::Result<()> {
    #[cfg(test)]
    if let Some(err) = testhook::with(|h| {
        h.codex_trusted.push(cwd.to_string());
        h.ensure_dir_trusted_error.clone()
    }) {
        return match err {
            Some(msg) => bail!(msg),
            None => Ok(()),
        };
    }
    crate::adapters::codex_app_server::ensure_dir_trusted(cwd)
}

pub(super) fn hooked_start_member_thread(cwd: &str, name: &str, model: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.codex_minted
            .push((cwd.to_string(), name.to_string(), model.to_string()));
        h.start_member_thread.clone()
    }) {
        return v;
    }
    crate::adapters::codex_app_server::start_member_thread(cwd, name, model)
}

pub(super) fn hooked_write_pane_thread(
    pane_id: &str,
    thread_id: &str,
    cwd: &str,
) -> anyhow::Result<()> {
    #[cfg(test)]
    if testhook::with(|h| {
        h.codex_records
            .push((pane_id.to_string(), thread_id.to_string(), cwd.to_string()))
    })
    .is_some()
    {
        return Ok(());
    }
    crate::adapters::codex_app_server::write_pane_thread(pane_id, thread_id, cwd)
}

pub(super) fn hooked_codex_send_to_pane(pane_id: &str, text: &str) -> Option<&'static str> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.codex_sent.push((pane_id.to_string(), text.to_string()));
        h.codex_send_to_pane
    }) {
        return v;
    }
    crate::adapters::codex_app_server::send_to_pane(pane_id, text)
}

pub(super) fn hooked_codex_send_to_thread(thread_id: &str, text: &str) -> Option<&'static str> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.codex_sent_thread
            .push((thread_id.to_string(), text.to_string()));
        h.codex_send_to_thread
    }) {
        return v;
    }
    crate::adapters::codex_app_server::send_to_thread(thread_id, text)
}

pub(super) fn hooked_codex_interrupt_pane(pane_id: &str) -> Option<&'static str> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.codex_interrupted_panes.push(pane_id.to_string());
        h.codex_interrupt_pane
    }) {
        return v;
    }
    crate::adapters::codex_app_server::interrupt_pane(pane_id)
}

pub(super) fn hooked_codex_interrupt_thread(thread_id: &str) -> Option<&'static str> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.codex_interrupted_threads.push(thread_id.to_string());
        h.codex_interrupt_thread
    }) {
        return v;
    }
    crate::adapters::codex_app_server::interrupt_thread(thread_id)
}

pub(super) fn hooked_codex_daemon_alive() -> bool {
    #[cfg(test)]
    if let Some(Some(v)) = testhook::with(|h| h.codex_daemon_alive) {
        return v;
    }
    crate::adapters::codex_app_server::daemon_alive()
}

// --- grok_leader seams -----------------------------------------------------

/// The member's leader daemon, raised by identity (no pane).
pub(super) fn hooked_grok_spawn_member_daemon(team: &str, member: &str) -> bool {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.event_order.push(format!("leader:{team}.{member}"));
        h.grok_leaders.push((team.to_string(), member.to_string()));
        h.grok_spawn_member_daemon
    }) {
        return v;
    }
    crate::adapters::grok_leader::spawn_member_daemon(team, member)
}

/// The engine-first mint: leader + `session/new` + record, all by identity.
pub(super) fn hooked_grok_create_member_session(
    team: &str,
    member: &str,
    session_id: &str,
    cwd: &str,
) -> bool {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.event_order
            .push(format!("mint:{team}.{member}:{session_id}:{cwd}"));
        h.grok_minted.push((
            team.to_string(),
            member.to_string(),
            session_id.to_string(),
            cwd.to_string(),
        ));
        h.grok_create_member_session
    }) {
        return v;
    }
    crate::adapters::grok_leader::create_member_session(team, member, session_id, cwd)
}

/// The session record on a daemon key (resume/fork lanes, where the TUI —
/// not `session/new` — materializes the session).
pub(super) fn hooked_grok_write_session_key(
    key: &str,
    session_id: &str,
    cwd: &str,
) -> anyhow::Result<()> {
    #[cfg(test)]
    if testhook::with(|h| {
        h.event_order.push(format!("record:{key}:{session_id}"));
        h.grok_sessions
            .push((key.to_string(), session_id.to_string(), cwd.to_string()))
    })
    .is_some()
    {
        return Ok(());
    }
    crate::adapters::grok_leader::write_session_key(key, session_id, cwd)
}

pub(super) fn hooked_grok_send_to_pane(pane_id: &str, text: &str) -> Option<&'static str> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.grok_sent.push((pane_id.to_string(), text.to_string()));
        h.grok_send_to_pane
    }) {
        return v;
    }
    crate::adapters::grok_leader::send_to_pane(pane_id, text)
}

pub(super) fn hooked_grok_send_to_key(key: &str, text: &str) -> Option<&'static str> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.grok_sent_key.push((key.to_string(), text.to_string()));
        h.grok_send_to_key
    }) {
        return v;
    }
    crate::adapters::grok_leader::send_to_key(key, text)
}

pub(super) fn hooked_grok_interrupt_pane(pane_id: &str) -> Option<&'static str> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.grok_interrupted_panes.push(pane_id.to_string());
        h.grok_interrupt_pane
    }) {
        return v;
    }
    crate::adapters::grok_leader::interrupt_pane(pane_id)
}

pub(super) fn hooked_grok_interrupt_key(key: &str) -> Option<&'static str> {
    #[cfg(test)]
    if let Some(v) = testhook::with(|h| {
        h.grok_interrupted_keys.push(key.to_string());
        h.grok_interrupt_key
    }) {
        return v;
    }
    crate::adapters::grok_leader::interrupt_key(key)
}

pub(super) fn hooked_grok_pool_drop_key(key: &str) {
    #[cfg(test)]
    if testhook::with(|h| h.event_order.push(format!("pool:{key}"))).is_some() {
        return;
    }
    crate::adapters::grok_leader::pool().drop_key(key);
}

pub(super) fn hooked_grok_kill_daemon_key(key: &str) {
    #[cfg(test)]
    if testhook::with(|h| {
        h.event_order.push(format!("daemon:{key}"));
        h.grok_killed_keys.push(key.to_string());
    })
    .is_some()
    {
        return;
    }
    crate::adapters::grok_leader::kill_daemon_key(key);
}

pub(super) fn hooked_grok_probe_socket(socket_path: &std::path::Path) -> bool {
    #[cfg(test)]
    if let Some(Some(v)) = testhook::with(|h| h.grok_probe_socket) {
        return v;
    }
    crate::adapters::grok_leader::probe_socket(socket_path)
}

// --- hived seams -----------------------------------------------------------

pub(super) fn hooked_request_connect_codex(workspace: &str) {
    #[cfg(test)]
    if testhook::with(|h| h.connects_codex.push(workspace.to_string())).is_some() {
        return;
    }
    let _ = crate::hived::request_connect_codex(workspace);
}

pub(super) fn hooked_request_connect_grok(workspace: &str, pane_id: &str) {
    #[cfg(test)]
    if testhook::with(|h| {
        h.connects_grok
            .push((workspace.to_string(), pane_id.to_string()));
        h.event_order.push(format!("connect:{workspace}:{pane_id}"));
    })
    .is_some()
    {
        return;
    }
    let _ = crate::hived::request_connect_grok(workspace, pane_id);
}
