use anyhow::bail;

use crate::adapters::claude_sessions;

use super::seams::*;
use super::spawn::Agent;
use super::support::DeliveryError;

impl Agent {
    // --- Control ---

    /// Send a prompt to the agent; return the accepted-transport class.
    ///
    /// Delivery is native-transport-only: codex goes through the shared
    /// daemon's `turn/start` RPC on the member's recorded thread, grok
    /// through its per-pane leader's `session/prompt`, claude through its
    /// session's own inbox socket. None of them touches the composer, and
    /// there is no keystroke fallback on any failure — a transport that did
    /// not accept the message raises `DeliveryError` (callers surface it as
    /// an explicit submit failure). The returned classification names which
    /// transport boundary was crossed (`turnStartAccepted` /
    /// `sessionPromptQueued` / `udsWriteAccepted`); none of them proves the
    /// agent processed the message — that final confirmation only ever comes
    /// from the target's transcript.
    pub fn send(&self, text: &str) -> Result<String, DeliveryError> {
        // No sender known: the origin label falls back to the team, never to
        // the target's own address (see `send_from`).
        self.send_from(text, "")
    }

    /// `send` with the message's real origin. *sender* is what a claude
    /// inbox frame shows as `from` — the human's message card reads it —
    /// so it must be the message's author (`<team>.<member>`), not the
    /// recipient. Empty falls back to the bare team name: a truthful
    /// origin when hive itself speaks (join notices, spawn prompts).
    pub fn send_from(&self, text: &str, sender: &str) -> Result<String, DeliveryError> {
        let sender = self.origin_label(sender);
        // A claude member's engine is not on the pane TTY at all: the pane's
        // job record is its address, and a parked engine (supervisor idles
        // jobs after ~1h) is woken in-line — so a probe that sees nothing is
        // still a deliverable claude member. That record is an address only
        // for a member hive spawned as claude, and only while the pane shows
        // no *other* live CLI: a recycled pane id whose member is codex must
        // never route into a stale `hive-pane-<n>.job`, whichever way the
        // probe happens to read that pane.
        if self.pane_id.is_empty() {
            return self._send_headless(text, &sender);
        }
        let probe = hooked_detect_cli_process_for_pane(&self.pane_id);
        let profile_name = probe.as_ref().map(|p| p.name).unwrap_or_default();
        let claude_member =
            self.cli == "claude" && (profile_name.is_empty() || profile_name == "claude");
        if probe.is_none() && !claude_member {
            return Err(DeliveryError(format!(
                "no live CLI process on pane {} (cli_exited): \
                 refusing native transport to a retained shell",
                self.pane_id
            )));
        }
        if claude_member {
            if let Some(job_id) = hooked_job_id_for_pane(&self.pane_id).filter(|j| !j.is_empty()) {
                return self._deliver_claude_job(&job_id, text, &sender);
            }
        }
        if profile_name == "codex" {
            return match hooked_codex_send_to_pane(&self.pane_id, text) {
                Some(accepted) => Ok(accepted.to_string()),
                None => Err(DeliveryError(format!(
                    "codex pane {} did not accept the turn \
                     (no recorded thread, daemon down, RPC error, or \
                     connection failure)",
                    self.pane_id
                ))),
            };
        }
        if profile_name == "grok" {
            return match hooked_grok_send_to_pane(&self.pane_id, text) {
                Some(accepted) => Ok(accepted.to_string()),
                None => Err(DeliveryError(format!(
                    "grok pane {} did not accept the prompt \
                     (no leader/session, RPC error, or connection failure)",
                    self.pane_id
                ))),
            };
        }
        if claude_member {
            // The pane may be a display-only mirror (hive attach renders an
            // interactive member — a joined ccd — as a read-only viewer and
            // tags the pane with the member's name). The engine identity is
            // the roster sessionId; when it names a live interactive session
            // rather than a bg job, deliver there — the pane was never the
            // address, only the picture.
            let sid = self.session_id.clone().unwrap_or_default();
            if !sid.is_empty() && hooked_job_row(&sid).is_none() {
                return self._deliver_claude_session(&sid, text, &sender);
            }
            return Err(DeliveryError(format!(
                "claude pane {} has no bg job record; a hive \
                 claude member runs as a background job (relaunch it with \
                 `hive claude`) — hive does not deliver to a bare claude TUI",
                self.pane_id
            )));
        }
        if profile_name == "claude" {
            return Err(DeliveryError(format!(
                "pane {} shows claude but its member '{}' \
                 is a {} member (recycled pane id, or a stale job \
                 record); hive does not deliver across CLIs",
                self.pane_id, self.name, self.cli
            )));
        }
        Err(DeliveryError(format!(
            "pane {} runs no supported agent CLI \
             (profile={}); hive delivers over \
             native transports only",
            self.pane_id,
            if profile_name.is_empty() {
                "unknown"
            } else {
                &profile_name
            }
        )))
    }

    /// Origin label for a claude inbox frame: the sender as given, or the
    /// team name when nobody is named. Never `self` — this agent is the
    /// recipient.
    fn origin_label(&self, sender: &str) -> String {
        if sender.is_empty() {
            self.team_name.clone()
        } else {
            sender.to_string()
        }
    }

    fn _deliver_claude_job(
        &self,
        job_id: &str,
        text: &str,
        sender: &str,
    ) -> Result<String, DeliveryError> {
        let where_ = if !self.pane_id.is_empty() {
            format!("pane {}", self.pane_id)
        } else {
            "headless".to_string()
        };
        let mut engine = hooked_engine_session_for_job(job_id);
        if engine.is_none() && hooked_job_row(job_id).is_some() {
            // Asleep, not dead: the job ledger still lists it, and a
            // tty-less attach revives the engine (same jobId and
            // sessionId, fresh pid) — then re-read its new entry.
            engine = hooked_ensure_engine(job_id, None);
        }
        let Some(engine) = engine else {
            return Err(DeliveryError(format!(
                "claude job '{job_id}' ({where_}) is gone (removed from the \
                 job ledger, or the wake failed); the message stays on the bus"
            )));
        };
        // Primary lane: the supervisor daemon's reply channel — the
        // typed-keystroke lane, no peer wrapper in any state. Any
        // failure falls back to the inbox socket, which still
        // delivers (wrapped) with today's error semantics.
        if let Some(accepted) = hooked_daemon_reply(&engine.session_id, text) {
            return Ok(accepted.to_string());
        }
        let accepted =
            hooked_claude_sessions_send(&engine.socket_path, text, sender, &engine.session_id);
        match accepted {
            Some(a) if a == claude_sessions::WRITE_TIMED_OUT => Err(DeliveryError(format!(
                "claude job '{job_id}' ({where_}) accepted the connection \
                 but did not drain the message in time"
            ))),
            Some(a) => Ok(a.to_string()),
            None => Err(DeliveryError(format!(
                "claude job '{job_id}' ({where_}) is not listening on its \
                 inbox; the message stays on the bus"
            ))),
        }
    }

    /// Deliver to a joined interactive Claude session (no bg job).
    ///
    /// Same two lanes as a job engine: the supervisor reply channel first,
    /// the session's own inbox socket as fallback.
    fn _deliver_claude_session(
        &self,
        session_id: &str,
        text: &str,
        sender: &str,
    ) -> Result<String, DeliveryError> {
        if let Some(accepted) = hooked_daemon_reply(session_id, text) {
            return Ok(accepted.to_string());
        }
        let sid8: String = session_id.chars().take(8).collect();
        let live = hooked_list_sessions()
            .into_iter()
            .find(|s| s.session_id == session_id);
        let Some(live) = live else {
            return Err(DeliveryError(format!(
                "claude member '{}' (session {sid8}) has no \
                 live session; the message stays on the bus",
                self.name
            )));
        };
        let accepted = hooked_claude_sessions_send(&live.socket_path, text, sender, session_id);
        match accepted {
            Some(a) if a != claude_sessions::WRITE_TIMED_OUT => Ok(a.to_string()),
            _ => Err(DeliveryError(format!(
                "claude member '{}' (session {sid8}) did not \
                 accept the frame; the message stays on the bus",
                self.name
            ))),
        }
    }

    /// Deliver to a member with no pane: the engine is the only address.
    ///
    /// Identity comes from the registry row (claude jobId / codex threadId /
    /// grok member key) — there is no pane to probe, and nothing to guard
    /// against pane-id recycling.
    fn _send_headless(&self, text: &str, sender: &str) -> Result<String, DeliveryError> {
        if self.cli == "claude" {
            let sid = self.session_id.clone().unwrap_or_default();
            if sid.is_empty() {
                return Err(DeliveryError(format!(
                    "claude member '{}' has no recorded engine identity; \
                     the message stays on the bus",
                    self.name
                )));
            }
            if hooked_job_row(&sid).is_some() {
                return self._deliver_claude_job(&sid, text, sender);
            }
            return self._deliver_claude_session(&sid, text, sender);
        }
        if self.cli == "codex" {
            let thread_id = self.session_id.clone().unwrap_or_default();
            let accepted = if !thread_id.is_empty() {
                hooked_codex_send_to_thread(&thread_id, text)
            } else {
                None
            };
            return match accepted {
                Some(a) => Ok(a.to_string()),
                None => Err(DeliveryError(format!(
                    "codex member '{}' did not accept the turn \
                     (no recorded thread, daemon down, RPC error, or \
                     connection failure)",
                    self.name
                ))),
            };
        }
        if self.cli == "grok" {
            let key = crate::adapters::grok_leader::member_key(&self.team_name, &self.name);
            return match hooked_grok_send_to_key(&key, text) {
                Some(a) => Ok(a.to_string()),
                None => Err(DeliveryError(format!(
                    "grok member '{}' did not accept the prompt \
                     (no leader/session, RPC error, or connection failure)",
                    self.name
                ))),
            };
        }
        Err(DeliveryError(format!(
            "member '{}' runs '{}', which hive has no \
             headless transport for",
            self.name, self.cli
        )))
    }

    /// Abort the member's running turn over its CLI's native transport.
    ///
    /// Every branch is addressed to the engine, never to the pane: claude's
    /// Escape rides the same pipe as its text, codex takes `turn/interrupt`
    /// on its recorded thread and grok the ACP `session/cancel` on its
    /// recorded session. So the abort lands on *that* turn whatever the
    /// pane's viewer happens to be showing, and a member whose transport is
    /// gone is a refusal — never an Escape into a pager, a copy-mode scroll
    /// or somebody else's session.
    pub fn interrupt(&self) -> anyhow::Result<()> {
        if self.cli == "claude" {
            let mut job_id = if !self.pane_id.is_empty() {
                hooked_job_id_for_pane(&self.pane_id).unwrap_or_default()
            } else {
                String::new()
            };
            if job_id.is_empty() {
                job_id = self.session_id.clone().unwrap_or_default();
            }
            if job_id.is_empty() {
                bail!(
                    "claude member '{}' has no bg job record \
                     to interrupt; hive never send-keys a member pane",
                    self.name
                );
            }
            let result = hooked_interrupt_job(&job_id);
            if !result.ok {
                bail!("claude job {job_id} was not interrupted: {}", result.why);
            }
            return Ok(());
        }
        if self.cli == "codex" {
            let accepted = if !self.pane_id.is_empty() {
                hooked_codex_interrupt_pane(&self.pane_id)
            } else if let Some(sid) = self.session_id.as_deref().filter(|s| !s.is_empty()) {
                hooked_codex_interrupt_thread(sid)
            } else {
                None
            };
            if accepted.is_none() {
                bail!(
                    "codex pane {} did not accept turn/interrupt \
                     (no recorded thread, daemon down, RPC error, or \
                     connection failure)",
                    self.pane_id
                );
            }
            return Ok(());
        }
        if self.cli == "grok" {
            let accepted = if !self.pane_id.is_empty() {
                hooked_grok_interrupt_pane(&self.pane_id)
            } else {
                hooked_grok_interrupt_key(&crate::adapters::grok_leader::member_key(
                    &self.team_name,
                    &self.name,
                ))
            };
            if accepted.is_none() {
                bail!(
                    "grok pane {} did not accept session/cancel \
                     (no leader/session, or connection failure)",
                    self.pane_id
                );
            }
            return Ok(());
        }
        bail!(
            "member '{}' runs '{}', which hive has no native \
             interrupt for; hive never send-keys a member pane",
            self.name,
            self.cli
        );
    }

    /// Capture pane output.
    pub fn capture(&self, lines: u32) -> anyhow::Result<String> {
        hooked_capture_pane(&self.pane_id, lines)
    }

    pub fn is_alive(&self) -> bool {
        if !self.pane_id.is_empty() {
            return crate::tmux::is_pane_alive(&self.pane_id);
        }
        self._engine_alive()
    }

    /// A pane-less member is alive iff its engine answers for it.
    fn _engine_alive(&self) -> bool {
        if self.cli == "claude" {
            let job_id = self.session_id.clone().unwrap_or_default();
            if job_id.is_empty() {
                return false;
            }
            if hooked_engine_session_for_job(&job_id).is_some() || hooked_job_row(&job_id).is_some()
            // asleep is not dead
            {
                return true;
            }
            // A joined interactive session: alive while its channel is live.
            return hooked_list_sessions()
                .iter()
                .any(|s| s.session_id == job_id);
        }
        if self.cli == "codex" {
            return self
                .session_id
                .as_deref()
                .map(|s| !s.is_empty())
                .unwrap_or(false)
                && hooked_codex_daemon_alive();
        }
        if self.cli == "grok" {
            let key = crate::adapters::grok_leader::member_key(&self.team_name, &self.name);
            return hooked_grok_probe_socket(&crate::adapters::grok_leader::socket_path_for_key(
                &key,
            ));
        }
        false
    }

    /// Force kill the pane — and, for a claude member, park its engine.
    ///
    /// The engine lives on claude's supervisor, not in the pane, so killing
    /// the pane alone would leave an orphan job running headless. `claude
    /// stop` parks it: the job stays in the ledger and a managed
    /// `hive claude --resume <jobId>` launch can still wake it.
    pub fn kill(&self) {
        if self.cli == "claude" {
            let mut job_id = if !self.pane_id.is_empty() {
                hooked_job_id_for_pane(&self.pane_id).unwrap_or_default()
            } else {
                String::new()
            };
            if job_id.is_empty() {
                job_id = self.session_id.clone().unwrap_or_default();
            }
            // A joined interactive session is not hive's engine to stop:
            // kill only removes it from the roster.
            if !job_id.is_empty() && hooked_job_row(&job_id).is_some() {
                hooked_stop_job(&job_id);
            }
            if !self.pane_id.is_empty() {
                crate::adapters::claude_bg::clear_pane_job(&self.pane_id);
            }
        } else if self.cli == "grok" {
            // The member's leader daemon is the engine; a kill removes the
            // member, so the engine goes with it — deterministically, not on
            // the hived's next orphan sweep. Resolve while the pane tags
            // still exist; a pane-less member is addressed by its member key.
            let key = if !self.pane_id.is_empty() {
                crate::adapters::grok_leader::resolve_pane_key(&self.pane_id)
            } else {
                crate::adapters::grok_leader::member_key(&self.team_name, &self.name)
            };
            crate::adapters::grok_leader::pool().drop_key(&key);
            crate::adapters::grok_leader::kill_daemon_key(&key);
        }
        if !self.pane_id.is_empty() {
            hooked_kill_pane(&self.pane_id);
        }
    }
}
