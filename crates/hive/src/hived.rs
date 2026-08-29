// pending port (wave 3)

/// Ask the workspace hived to (re)connect the codex app-server client.
/// Wave-3 hived port replaces this stub; agent.rs ignores the result.
pub fn request_connect_codex(_workspace: &str) {}

/// Ask the workspace hived to (re)connect the grok leader for a pane.
pub fn request_connect_grok(_workspace: &str, _pane_id: &str) {}
