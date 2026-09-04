use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Result;

use super::client::{GrokStdioClient, SessionRuntime};
use super::daemon::{probe_socket, spawn_member_daemon};
use super::keys::{
    member_from_key, member_key, read_pane_session, read_session_key, resolve_pane_key,
    socket_path_for_key, write_session_key,
};
use super::{CANCEL_SENT, PROMPT_QUEUED, _CONNECT_COOLDOWN};

// --------------------------------------------------------------------------
// per-pane client pool (hived-side)
// --------------------------------------------------------------------------

/// What the pool's delivery paths need from a client. GrokStdioClient is the
/// only production implementation; tests substitute fakes (the Python
/// per-instance `_client_for_key` monkeypatch). `Err` models a raising
/// client — any client failure is a transport failure.
pub trait LeaderClient: Send + Sync {
    fn prompt(&self, _text: &str) -> Result<bool> {
        unreachable!("prompt not expected on this client")
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
}

impl LeaderClient for GrokStdioClient {
    fn prompt(&self, text: &str) -> Result<bool> {
        Ok(GrokStdioClient::prompt(self, text))
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
fn _key_is_rostered(key: &str) -> bool {
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
    clients: HashMap<String, Arc<GrokStdioClient>>,
    pub(super) cooldown: HashMap<String, Instant>,
}

/// One persistent stdio client per daemon key.
///
/// The hived reads runtime every tick; each client's reader thread keeps
/// its session state current between calls. Clients are created lazily the
/// first time a read finds both a socket and a session record, and a dead
/// one is dropped and retried after a cooldown so a missing daemon does not
/// storm subprocess spawns.
pub struct GrokClientPool {
    pub(super) state: Mutex<PoolState>,
    #[cfg(test)]
    pub(super) client_override:
        Mutex<Option<Box<dyn Fn(&str) -> Option<Arc<dyn LeaderClient>> + Send>>>,
}

impl GrokClientPool {
    pub fn new() -> GrokClientPool {
        GrokClientPool {
            state: Mutex::new(PoolState::default()),
            #[cfg(test)]
            client_override: Mutex::new(None),
        }
    }

    pub fn runtime_for_key(&self, key: &str) -> Option<SessionRuntime> {
        self._acting_client(key)?.runtime()
    }

    /// Bring the stdio client online for a key (called at spawn time).
    pub fn connect_key(&self, key: &str) -> bool {
        self._acting_client(key).is_some()
    }

    /// Deliver text as a prompt over the key's leader.
    ///
    /// Returns [`PROMPT_QUEUED`] when the leader echoed the prompt back, else
    /// None: no daemon, no session record, an rpc error, or an ack timeout.
    /// A busy session is not bounced — the leader queues the prompt FIFO and
    /// runs it when the current turn ends, the same as typing into the TUI.
    pub fn send_to_key(&self, key: &str, text: &str) -> Option<&'static str> {
        let client = self._acting_client(key)?;
        match client.prompt(text) {
            Ok(true) => Some(PROMPT_QUEUED),
            _ => None,
        }
    }

    /// Cancel the running turn over the key's leader.
    ///
    /// Returns [`CANCEL_SENT`] when the notification went out on a loaded
    /// session, else None: no daemon, no session record, or a dead pipe.
    pub fn interrupt_key(&self, key: &str) -> Option<&'static str> {
        let client = self._acting_client(key)?;
        match client.cancel() {
            Ok(true) => Some(CANCEL_SENT),
            _ => None,
        }
    }

    pub fn compact_key(&self, key: &str) -> &'static str {
        match self._acting_client(key) {
            Some(client) => client.compact(),
            None => "unavailable",
        }
    }

    /// The Python per-instance `_client_for_key` monkeypatch seam.
    fn _acting_client(&self, key: &str) -> Option<Arc<dyn LeaderClient>> {
        #[cfg(test)]
        {
            if let Some(factory) = self.client_override.lock().unwrap().as_ref() {
                return factory(key);
            }
        }
        self._client_for_key(key)
            .map(|client| client as Arc<dyn LeaderClient>)
    }

    pub(super) fn _client_for_key(&self, key: &str) -> Option<Arc<GrokStdioClient>> {
        // A relaunched grok on the same key mints a new session id, so the
        // record — not just the client's liveness — decides whether the bound
        // client is still the key's.
        let record = read_session_key(key);
        {
            let mut state = self.state.lock().unwrap();
            if let Some(client) = state.clients.get(key).cloned() {
                if client.is_alive()
                    && record.is_some()
                    && client.session_id().as_deref()
                        == record.as_ref().map(|(sid, _cwd)| sid.as_str())
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

        if record.is_none() || !probe_socket(&socket_path_for_key(key)) || !_key_is_rostered(key) {
            self._set_cooldown(key);
            return None;
        }
        let client = match GrokStdioClient::new(key) {
            Ok(client) => Arc::new(client),
            Err(_) => {
                self._set_cooldown(key);
                return None;
            }
        };
        if !client.handshake() {
            client.close();
            self._set_cooldown(key);
            return None;
        }
        self.state
            .lock()
            .unwrap()
            .clients
            .insert(key.to_string(), client.clone());
        Some(client)
    }

    fn _set_cooldown(&self, key: &str) {
        self.state.lock().unwrap().cooldown.insert(
            key.to_string(),
            Instant::now() + Duration::from_secs_f64(_CONNECT_COOLDOWN),
        );
    }

    pub fn drop(&self, pane: &str) {
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

    /// `create_member_session`'s adopt path: Python pokes `pool()._clients`
    /// directly under `pool()._lock`.
    fn _adopt_client(&self, key: &str, client: Arc<GrokStdioClient>) {
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

static _POOL: OnceLock<GrokClientPool> = OnceLock::new();

pub fn pool() -> &'static GrokClientPool {
    _POOL.get_or_init(GrokClientPool::new)
}

pub fn runtime_for_pane(pane: &str) -> Option<SessionRuntime> {
    pool().runtime_for_key(&resolve_pane_key(pane))
}

pub fn runtime_for_key(key: &str) -> Option<SessionRuntime> {
    pool().runtime_for_key(key)
}

pub fn connect_pane(pane: &str) -> bool {
    pool().connect_key(&resolve_pane_key(pane))
}

pub fn connect_key(key: &str) -> bool {
    pool().connect_key(key)
}

pub fn send_to_pane(pane: &str, text: &str) -> Option<&'static str> {
    pool().send_to_key(&resolve_pane_key(pane), text)
}

pub fn send_to_key(key: &str, text: &str) -> Option<&'static str> {
    pool().send_to_key(key, text)
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
    read_pane_session(pane).map(|(session_id, _cwd)| session_id)
}

/// Materialize the member's session on its leader — the headless spawn.
///
/// Ensures the member daemon, asks it for `session/new` with hive's
/// minted id, and records the session beside the socket on success. The
/// creating client stays in the pool, already bound and folding the
/// session's notifications.
pub fn create_member_session(team: &str, member: &str, session_id: &str, cwd: &str) -> bool {
    if !spawn_member_daemon(team, member) {
        return false;
    }
    let key = member_key(team, member);
    let client = match GrokStdioClient::new(&key) {
        Ok(client) => Arc::new(client),
        Err(_) => return false,
    };
    if !client.new_session(session_id, cwd) {
        client.close();
        return false;
    }
    if write_session_key(&key, session_id, cwd).is_err() {
        return false;
    }
    pool()._adopt_client(&key, client);
    true
}
