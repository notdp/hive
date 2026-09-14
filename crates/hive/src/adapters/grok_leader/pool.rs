use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Result;

use super::binding::binding_holds;
use super::client::{GrokStdioClient, PromptResult, SessionRuntime};
use super::daemon::{kill_daemon_key, probe_socket, spawn_member_daemon};
use super::keys::{
    member_from_key, member_key, read_pane_session, read_session_key, resolve_pane_key,
    socket_path_for_key, write_session_key, RecordBinding,
};
use super::{CANCEL_SENT, CONNECT_COOLDOWN, PROMPT_QUEUED};

// --------------------------------------------------------------------------
// per-key client pool (hived-side)
// --------------------------------------------------------------------------

/// A request id is unique only within one connection. These handles live
/// in the hived process and must not be restored across daemon restarts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptId {
    pub generation: u64,
    pub rid: u64,
}

/// The identity a revival confirmed, handed to the caller that asked for
/// it and checked again at the submission boundary: the member and the
/// team instance (the record's binding, as the registry then agreed to
/// it), the session the client loaded, the leader socket the connection
/// reaches (the key's canonical socket as the revive resolved it — the
/// launch's, for a member bound from one), and the connection itself
/// (the client generation). The submission it was made for carries it to
/// the prompt; a later revive on the same key hands its own caller
/// another one and changes nothing about this one — a submission on a
/// confirmation whose identity is no longer the key's is refused, whether
/// the binding stopped holding, a valid later binding replaced it, or the
/// key resolves to another leader by now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirmation {
    pub key: String,
    pub binding: RecordBinding,
    pub session_id: String,
    pub socket_path: String,
    pub generation: u64,
}

/// What a revival left: whether it raised the leader (`false` for a member
/// that was online — a no-op, not a failure), the session's state as the
/// handshake's `session/load` replayed it, and the identity it confirmed.
/// The state is a snapshot the caller's own gate reads for itself; nothing
/// here says the turn is closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revival {
    pub raised: bool,
    pub input_state: String,
    pub turn_open: Option<bool>,
    pub confirmation: Confirmation,
}

/// Why a revival did not end with a client on the member's session. Told
/// apart because a caller may act on them differently: a binding that does
/// not hold is a dead member, a leader that did not start or answer the
/// handshake is one to report, never to retire on this evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviveFailure {
    NotRetained(String),
    LeaderStart(String),
    Handshake(String),
}

impl fmt::Display for ReviveFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReviveFailure::NotRetained(reason) => write!(f, "not retained: {reason}"),
            ReviveFailure::LeaderStart(reason) => write!(f, "leader did not start: {reason}"),
            ReviveFailure::Handshake(reason) => write!(f, "handshake failed: {reason}"),
        }
    }
}

/// What the pool's delivery paths need from a client. GrokStdioClient is the
/// only production implementation; tests substitute fakes through
/// `acting_client`'s override. `Err` is a transport failure: the client
/// could not reach its leader at all.
pub trait LeaderClient: Send + Sync {
    fn generation(&self) -> u64 {
        unreachable!("generation not expected on this client")
    }
    fn session_id(&self) -> Option<String> {
        unreachable!("session_id not expected on this client")
    }
    fn socket_path(&self) -> String {
        unreachable!("socket_path not expected on this client")
    }
    fn prompt(&self, _text: &str) -> Result<bool> {
        unreachable!("prompt not expected on this client")
    }
    fn prompt_tracked(&self, _text: &str) -> Result<u64> {
        unreachable!("prompt_tracked not expected on this client")
    }
    fn prompt_result(&self, _rid: u64) -> Option<PromptResult> {
        unreachable!("prompt_result not expected on this client")
    }
    fn cancel(&self) -> Result<bool> {
        unreachable!("cancel not expected on this client")
    }
    fn compact(&self) -> &'static str {
        unreachable!("compact not expected on this client")
    }
    fn runtime(&self) -> Option<SessionRuntime> {
        unreachable!("runtime not expected on this client")
    }
    fn turn_open(&self) -> Option<bool> {
        unreachable!("turn_open not expected on this client")
    }
}

impl LeaderClient for GrokStdioClient {
    fn generation(&self) -> u64 {
        GrokStdioClient::generation(self)
    }
    fn session_id(&self) -> Option<String> {
        GrokStdioClient::session_id(self)
    }
    fn socket_path(&self) -> String {
        self.socket_path.clone()
    }
    fn prompt(&self, text: &str) -> Result<bool> {
        Ok(GrokStdioClient::prompt(self, text))
    }

    fn prompt_tracked(&self, text: &str) -> Result<u64> {
        GrokStdioClient::prompt_tracked(self, text).map_err(|e| anyhow::anyhow!(e))
    }

    fn prompt_result(&self, rid: u64) -> Option<PromptResult> {
        GrokStdioClient::prompt_result(self, rid)
    }

    fn cancel(&self) -> Result<bool> {
        Ok(GrokStdioClient::cancel(self))
    }

    fn compact(&self) -> &'static str {
        GrokStdioClient::compact(self)
    }

    fn runtime(&self) -> Option<SessionRuntime> {
        GrokStdioClient::runtime(self)
    }

    fn turn_open(&self) -> Option<bool> {
        GrokStdioClient::turn_open(self)
    }
}

/// True unless *key* names a member the registry no longer lists.
///
/// A grok client raises a leader of its own the moment it finds none on the
/// socket, so binding one for a killed member resurrects the engine the kill
/// just took down — in the hived, whose pool outlives every kill hive runs.
/// The roster is the arbiter, as it is for the hived's own orphan reap. A
/// pane key answers to its pane, not to a roster, and is always live here;
/// an unreadable entry reads as gone, which only postpones a reconnect to
/// the next tick.
fn key_is_rostered(key: &str) -> bool {
    let Some((team, member)) = member_from_key(key) else {
        return true;
    };
    crate::registry::load(&team)
        .and_then(|entry| {
            let members = entry.get("members")?.as_array()?.clone();
            Some(members.iter().any(|m| {
                m.get("name").and_then(serde_json::Value::as_str) == Some(member.as_str())
            }))
        })
        .unwrap_or(false)
}

#[derive(Default)]
pub(super) struct PoolState {
    pub(super) clients: HashMap<String, Arc<GrokStdioClient>>,
    pub(super) cooldown: HashMap<String, Instant>,
}

/// One persistent stdio client per daemon key.
///
/// The hived reads runtime every tick; each client's reader thread keeps
/// its session state current between calls. Clients are created lazily the
/// first time a read finds both a socket and a session record, and a dead
/// one is dropped and retried after a cooldown so a missing daemon does not
/// storm subprocess spawns.
#[cfg(test)]
type ClientOverride = Box<dyn Fn(&str) -> Option<Arc<dyn LeaderClient>> + Send>;

pub struct GrokClientPool {
    pub(super) state: Mutex<PoolState>,
    /// One gate per key around `client_for_key`'s get-or-connect: two
    /// callers connecting one key at once — two submissions reviving one
    /// cold member — would each spawn a stdio client and the second's
    /// insert would close the first's, under its caller's feet. The second
    /// waits at the gate and takes the client the first connected.
    connects: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    #[cfg(test)]
    pub(super) client_override: Mutex<Option<ClientOverride>>,
}

impl GrokClientPool {
    pub fn new() -> GrokClientPool {
        GrokClientPool {
            connects: Mutex::new(HashMap::new()),
            state: Mutex::new(PoolState::default()),
            #[cfg(test)]
            client_override: Mutex::new(None),
        }
    }

    /// Inspect only this process's clients; never connect or spawn to decide sleep.
    /// Unknown turn evidence is an outstanding obligation.
    pub(crate) fn idle_owned_keys(&self, team: &str) -> Option<Vec<String>> {
        let state = self.state.lock().unwrap();
        state
            .clients
            .iter()
            .filter(|(key, _)| member_from_key(key).is_some_and(|(owner, _)| owner == team))
            .map(|(key, client)| {
                let still_bound = client.socket_path == socket_path_for_key(key).to_string_lossy()
                    && read_session_key(key).is_some_and(|record| {
                        client.session_id().as_deref() == Some(record.session_id.as_str())
                    });
                (still_bound && client.idle_for_sleep()).then(|| key.clone())
            })
            .collect()
    }

    #[cfg(test)]
    pub(super) fn hold_for_test(&self, key: &str, client: Arc<GrokStdioClient>) {
        self.adopt_client(key, client);
    }

    pub fn runtime_for_key(&self, key: &str) -> Option<SessionRuntime> {
        self.acting_client(key)?.runtime()
    }

    /// The key's turn evidence: `None` with no client on the key (no
    /// daemon, no session record), `Some(None)` from a client that has
    /// seen no turn event, else the evidence.
    pub fn turn_open_for_key(&self, key: &str) -> Option<Option<bool>> {
        Some(self.acting_client(key)?.turn_open())
    }

    /// Bring the stdio client online for a key (called at spawn time).
    pub fn connect_key(&self, key: &str) -> bool {
        self.acting_client(key).is_some()
    }

    /// Deliver text as a prompt over the key's leader, from a pool that
    /// never revived the key — a spawning or joining CLI's own, or the
    /// cvim sendback — binding once to a leader already listening
    /// (`client_for_key` never spawns).
    ///
    /// Returns [`PROMPT_QUEUED`] when the leader echoed the prompt back, else
    /// None: no daemon, no session record, an rpc error, or an ack timeout.
    /// A busy session is not bounced — the leader queues the prompt FIFO and
    /// runs it when the current turn ends, the same as typing into the TUI.
    /// A leader that is gone is not raised here: that is `revive_key`.
    pub fn send_to_key(&self, key: &str, text: &str) -> Option<&'static str> {
        let client = self.acting_client(key)?;
        match client.prompt(text) {
            Ok(true) => Some(PROMPT_QUEUED),
            _ => None,
        }
    }

    /// `send_to_key` on the identity a revive confirmed: the prompt goes
    /// out on that connection, to that session, for that member of that
    /// team instance, or not at all (`confirmed_client`).
    pub fn send_confirmed(&self, confirmation: &Confirmation, text: &str) -> Option<&'static str> {
        let client = self.confirmed_client(confirmation)?;
        match client.prompt(text) {
            Ok(true) => Some(PROMPT_QUEUED),
            _ => None,
        }
    }

    /// A workflow node's task as a tracked prompt on the identity a revive
    /// confirmed: the connection and request ids let `prompt_result_for_key`
    /// read the turn's outcome. `Err` covers an identity that is no longer
    /// the key's, a leader gone, and a client rebound since.
    pub fn dispatch_confirmed(
        &self,
        confirmation: &Confirmation,
        text: &str,
    ) -> Result<PromptId, String> {
        let client = self.confirmed_client(confirmation).ok_or_else(|| {
            format!(
                "no grok leader client on {} for the identity its revive confirmed",
                confirmation.key
            )
        })?;
        let rid = client.prompt_tracked(text).map_err(|e| e.to_string())?;
        Ok(PromptId {
            generation: client.generation(),
            rid,
        })
    }

    /// The outcome of a prompt `dispatch_confirmed` sent; None with no client
    /// on the key or one that never sent it (reconnected since).
    pub fn prompt_result_for_key(&self, key: &str, id: PromptId) -> Option<PromptResult> {
        let client = self.acting_client(key)?;
        if client.generation() != id.generation {
            return None;
        }
        client.prompt_result(id.rid)
    }

    /// Cancel the running turn over the key's leader.
    ///
    /// Returns [`CANCEL_SENT`] when the notification went out on a loaded
    /// session, else None: no daemon, no session record, or a dead pipe.
    pub fn interrupt_key(&self, key: &str) -> Option<&'static str> {
        let client = self.acting_client(key)?;
        match client.cancel() {
            Ok(true) => Some(CANCEL_SENT),
            _ => None,
        }
    }

    pub fn compact_key(&self, key: &str) -> &'static str {
        match self.acting_client(key) {
            Some(client) => client.compact(),
            None => "unavailable",
        }
    }

    /// `client_for_key` behind the test override, so delivery paths can
    /// run against a fake client.
    fn acting_client(&self, key: &str) -> Option<Arc<dyn LeaderClient>> {
        #[cfg(test)]
        {
            if let Some(factory) = self.client_override.lock().unwrap().as_ref() {
                return factory(key);
            }
        }
        self.client_for_key(key)
            .map(|client| client as Arc<dyn LeaderClient>)
    }

    pub(crate) fn client_for_key(&self, key: &str) -> Option<Arc<GrokStdioClient>> {
        let gate = self.connect_gate(key);
        let _connecting = gate.lock().unwrap_or_else(|e| e.into_inner());
        // A relaunched grok on the same key mints a new session id, and a
        // member rebound to another launch resolves to another socket, so
        // the record and the key's canonical socket — not just the client's
        // liveness — decide whether the pooled client is still the key's.
        let record = read_session_key(key);
        let sock = socket_path_for_key(key);
        {
            let mut state = self.state.lock().unwrap();
            if let Some(client) = state.clients.get(key).cloned() {
                if client.is_alive()
                    && client.socket_path == sock.to_string_lossy()
                    && record.is_some()
                    && client.session_id().as_deref()
                        == record.as_ref().map(|r| r.session_id.as_str())
                {
                    return Some(client);
                }
                client.close();
                state.clients.remove(key);
            }
            if let Some(until) = state.cooldown.get(key) {
                if Instant::now() < *until {
                    return None;
                }
            }
        }

        if record.is_none() || !probe_socket(&sock) || !key_is_rostered(key) {
            self.set_cooldown(key);
            return None;
        }
        let client = match GrokStdioClient::new(key) {
            Ok(client) => Arc::new(client),
            Err(_) => {
                self.set_cooldown(key);
                return None;
            }
        };
        if !client.handshake() {
            client.close();
            self.set_cooldown(key);
            return None;
        }
        self.state
            .lock()
            .unwrap()
            .clients
            .insert(key.to_string(), client.clone());
        Some(client)
    }

    fn connect_gate(&self, key: &str) -> Arc<Mutex<()>> {
        self.connects
            .lock()
            .unwrap()
            .entry(key.to_string())
            .or_default()
            .clone()
    }

    /// The client a confirmed submission goes out on: the connection the
    /// revive confirmed (the pooled client of that generation, alive, on
    /// the confirmed session and the confirmed socket), and the key's
    /// identity here and now still the confirmed one — the key resolving
    /// to that socket, the record naming that session and bound to that
    /// member of that team instance, the registry agreeing
    /// (`binding_holds`). Nothing is reinterpreted down here: a client
    /// rebound since (another generation) is the runtime's and is left
    /// alone; a dead client, one whose record names another session, or
    /// one on a socket the key no longer resolves to (the member's alias
    /// rebound to another launch — the same team, instance and session
    /// can all survive that) is closed; a binding that stopped holding,
    /// or a valid later binding of the same key — the same name, another
    /// team instance — refuses the submission while the client stays for
    /// whoever revives next. Nothing on this path loads a session or
    /// raises a leader.
    fn confirmed_client(&self, confirmation: &Confirmation) -> Option<Arc<dyn LeaderClient>> {
        #[cfg(test)]
        {
            if let Some(factory) = self.client_override.lock().unwrap().as_ref() {
                return factory(&confirmation.key);
            }
        }
        let key = confirmation.key.as_str();
        let client = self.state.lock().unwrap().clients.get(key).cloned()?;
        if client.generation() != confirmation.generation {
            return None;
        }
        let on_session = client.session_id().as_deref() == Some(confirmation.session_id.as_str());
        let on_socket = client.socket_path == confirmation.socket_path
            && socket_path_for_key(key).to_string_lossy() == confirmation.socket_path;
        let on_record = read_session_key(key)
            .is_some_and(|record| record.session_id == confirmation.session_id);
        if !client.is_alive() || !on_session || !on_socket || !on_record {
            client.close();
            self.state.lock().unwrap().clients.remove(key);
            return None;
        }
        if binding_holds(key).ok().as_ref() != Some(&confirmation.binding) {
            return None;
        }
        Some(client)
    }

    /// Bring a member's session back under a client before a submission:
    /// the one path that raises a leader for a member.
    ///
    /// Only for a member whose record still names it (`binding_holds`,
    /// checked here and now — a runtime's `retained` is a snapshot). A
    /// leader already listening is reused, so an online member is a
    /// no-op that reports `raised: false`; otherwise the member daemon is
    /// raised on the record's key. Either way the connect cooldown a cold
    /// runtime read may have left is cleared before the handshake, which
    /// `session/load`s the recorded session. No prompt goes out and no
    /// bus row is written: the caller's gate reads the loaded state next,
    /// and its submission carries the `Confirmation` — the binding this
    /// check passed, the session the client loaded, the socket the client
    /// reaches (the key's canonical one when the client was taken — a
    /// pooled client on a socket the key no longer resolves to is closed
    /// and replaced by `client_for_key`, never confirmed), the client's
    /// generation — which is the caller's alone: nothing in the pool
    /// remembers it, so a second revive on the key confirms for its own
    /// caller and cannot replace what this one confirmed.
    pub fn revive_key(&self, key: &str) -> Result<Revival, ReviveFailure> {
        let binding = binding_holds(key).map_err(ReviveFailure::NotRetained)?;
        let raised = if probe_socket(&socket_path_for_key(key)) {
            false
        } else {
            if !spawn_member_daemon(&binding.team, &binding.member) {
                return Err(ReviveFailure::LeaderStart(format!(
                    "the leader for {key} did not come up"
                )));
            }
            true
        };
        self.state.lock().unwrap().cooldown.remove(key);
        let client = self.acting_client(key).ok_or_else(|| {
            ReviveFailure::Handshake(format!("no client came up on {key} after the handshake"))
        })?;
        let session_id = client.session_id().ok_or_else(|| {
            ReviveFailure::Handshake(format!("the client on {key} loaded no session"))
        })?;
        Ok(Revival {
            raised,
            input_state: client
                .runtime()
                .map(|runtime| runtime.input_state)
                .unwrap_or_else(|| "unknown".to_string()),
            turn_open: client.turn_open(),
            confirmation: Confirmation {
                key: key.to_string(),
                binding,
                session_id,
                socket_path: client.socket_path(),
                generation: client.generation(),
            },
        })
    }

    fn set_cooldown(&self, key: &str) {
        self.state.lock().unwrap().cooldown.insert(
            key.to_string(),
            Instant::now() + Duration::from_secs_f64(CONNECT_COOLDOWN),
        );
    }

    pub fn drop_pane(&self, pane: &str) {
        self.drop_key(&resolve_pane_key(pane));
    }

    /// Drop every client attached to *key*'s socket (reap path).
    pub fn drop_key(&self, key: &str) {
        let sock = socket_path_for_key(key).to_string_lossy().into_owned();
        let doomed: Vec<Arc<GrokStdioClient>> = {
            let mut state = self.state.lock().unwrap();
            let keys: Vec<String> = state
                .clients
                .iter()
                .filter(|(_key, client)| client.socket_path == sock)
                .map(|(key, _client)| key.clone())
                .collect();
            keys.into_iter()
                .filter_map(|key| state.clients.remove(&key))
                .collect()
        };
        for client in doomed {
            client.close();
        }
    }

    /// Bind *client* as *key*'s pooled client (the mint's adopt path),
    /// closing whatever the pool held for the key before.
    fn adopt_client(&self, key: &str, client: Arc<GrokStdioClient>) {
        let existing = {
            let mut state = self.state.lock().unwrap();
            let existing = state.clients.remove(key);
            state.clients.insert(key.to_string(), client);
            existing
        };
        if let Some(existing) = existing {
            existing.close();
        }
    }
}

impl Default for GrokClientPool {
    fn default() -> Self {
        GrokClientPool::new()
    }
}

static POOL: OnceLock<GrokClientPool> = OnceLock::new();

pub fn pool() -> &'static GrokClientPool {
    POOL.get_or_init(GrokClientPool::new)
}

pub fn runtime_for_pane(pane: &str) -> Option<SessionRuntime> {
    pool().runtime_for_key(&resolve_pane_key(pane))
}

pub fn runtime_for_key(key: &str) -> Option<SessionRuntime> {
    pool().runtime_for_key(key)
}

pub fn turn_open_for_key(key: &str) -> Option<Option<bool>> {
    pool().turn_open_for_key(key)
}

pub fn connect_pane(pane: &str) -> bool {
    pool().connect_key(&resolve_pane_key(pane))
}

pub fn send_to_pane(pane: &str, text: &str) -> Option<&'static str> {
    pool().send_to_key(&resolve_pane_key(pane), text)
}

pub fn send_to_key(key: &str, text: &str) -> Option<&'static str> {
    pool().send_to_key(key, text)
}

pub fn dispatch_confirmed(confirmation: &Confirmation, text: &str) -> Result<PromptId, String> {
    pool().dispatch_confirmed(confirmation, text)
}

pub fn prompt_result_for_key(key: &str, id: PromptId) -> Option<PromptResult> {
    pool().prompt_result_for_key(key, id)
}

pub fn interrupt_pane(pane: &str) -> Option<&'static str> {
    pool().interrupt_key(&resolve_pane_key(pane))
}

pub fn interrupt_key(key: &str) -> Option<&'static str> {
    pool().interrupt_key(key)
}

pub fn compact_pane(pane: &str) -> &'static str {
    pool().compact_key(&resolve_pane_key(pane))
}

/// Session id hive minted for this pane, from its session record.
pub fn session_id_for_pane(pane: &str) -> Option<String> {
    read_pane_session(pane).map(|record| record.session_id)
}

/// Materialize the member's session on its leader — the engine-first mint.
///
/// Raises the member daemon by identity, asks it for `session/new` with
/// hive's minted id, and records the session beside the socket on success,
/// all before any pane exists: a pane attaching later (`hive grok --resume
/// <sid>`) is one more client of this engine. The record carries the
/// member and the team instance (*created_at*, a `team::created_at_key`)
/// it was minted for, which is what a later revival checks it against.
/// The creating client stays in the pool, already bound and folding the
/// session's notifications.
///
/// A failure after the daemon came up takes a leader this mint raised down
/// with it: the spawn gives the pane back, and a leader with no record and
/// no roster row would otherwise sit unaddressable until the hived's orphan
/// reap. A leader that was already listening is reused, never killed.
pub fn create_member_session(
    team: &str,
    created_at: &str,
    member: &str,
    session_id: &str,
    cwd: &str,
) -> bool {
    let key = member_key(team, member);
    let binding = RecordBinding {
        team: team.to_string(),
        created_at: created_at.to_string(),
        member: member.to_string(),
    };
    let raised_here = !probe_socket(&socket_path_for_key(&key));
    if !spawn_member_daemon(team, member) {
        return false;
    }
    let undo = |client: Option<&GrokStdioClient>| {
        if let Some(client) = client {
            client.close();
        }
        if raised_here {
            kill_daemon_key(&key);
        }
        false
    };
    let client = match GrokStdioClient::new(&key) {
        Ok(client) => Arc::new(client),
        Err(_) => return undo(None),
    };
    if !client.new_session(session_id, cwd) {
        return undo(Some(&client));
    }
    if write_session_key(&key, session_id, cwd, Some(&binding)).is_err() {
        return undo(Some(&client));
    }
    pool().adopt_client(&key, client);
    true
}
