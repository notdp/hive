//! CLI plumbing the handler modules share: the `fail` exit lane, process
//! and tty helpers, JSON printing, the `--artifact` and target-pane
//! resolvers. Domain logic lives in the crate, not here.

use std::path::Path;

use anyhow::Result;
use serde_json::{Map, Value};

use crate::identity;
use crate::paths::expanduser;

// ---------------------------------------------------------------------------
// Small shared utilities
// ---------------------------------------------------------------------------

/// Print `Error: msg` to stderr, exit 1.
pub(crate) fn fail(msg: &str) -> ! {
    eprintln!("Error: {msg}");
    std::process::exit(1);
}

/// Bridge for anyhow-returning helpers used from CLI handlers: any Err takes
/// the `fail` exit lane.
pub(crate) fn ok_or_fail<T>(result: Result<T>) -> T {
    match result {
        Ok(value) => value,
        Err(err) => fail(&err.to_string()),
    }
}

/// Replace this process with *program*; print the error and exit 1 when
/// the exec fails.
pub(crate) fn execvp(program: &str, args: &[String]) -> ! {
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(program).args(args).exec();
    eprintln!("Error: {err}");
    std::process::exit(1);
}

/// No control characters or line separators: safe to echo into a terminal.
// ponytail: the control-char gate covers the documented threats (ESC/OSC/BEL/
// newline); the full Unicode C*/Z* table is overkill.
pub(crate) fn is_printable(s: &str) -> bool {
    s.chars()
        .all(|c| !c.is_control() && c != '\u{2028}' && c != '\u{2029}')
}

pub(crate) fn stdout_isatty() -> bool {
    unsafe { libc::isatty(1) == 1 }
}

/// A settings value as an environment-variable string: strings bare, null
/// empty, anything else its JSON text.
pub(crate) fn value_as_env_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// `json.dumps(payload, indent=2, ensure_ascii=False)`.
pub(crate) fn json_pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// `secrets.token_urlsafe(4)` — 4 random bytes, base64url, no padding.
fn token_urlsafe4() -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let data = crate::naming::os_random_bytes(4);
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6 & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 63) as usize] as char);
        }
    }
    out
}

fn stdin_isatty() -> bool {
    unsafe { libc::isatty(0) == 1 }
}

pub(crate) fn resolve_sender(agent_name: Option<&str>) -> String {
    identity::resolve_sender(agent_name).unwrap_or_else(|| {
        fail(
            "cannot resolve own member identity: this engine is on no roster \
             (a codex thread, grok session or Claude session not recorded by \
             any team) — join a team first, or run from a bound pane",
        )
    })
}

// ---------------------------------------------------------------------------
// `fail` wrappers over the domain modules' `Result`/`Option` seams
// ---------------------------------------------------------------------------

pub(crate) fn validate_root_send_protocol(body: &str) {
    if let Some(err) = crate::send::root_send_protocol_error(body) {
        fail(&err);
    }
}

pub(crate) fn parse_entries(entries: &[String]) -> Map<String, Value> {
    match crate::bus::parse_key_value(entries) {
        Ok(map) => map,
        Err(err) => fail(&err.to_string()),
    }
}

pub(crate) fn resolve_target_pane() -> String {
    match identity::current_pane_id() {
        Some(current) if !current.is_empty() => current,
        _ => fail("cannot determine target pane (run inside tmux)"),
    }
}

pub(crate) fn resolve_artifact_path(artifact: &str, workspace: &str) -> String {
    if artifact.is_empty() {
        return String::new();
    }
    if artifact == "-" {
        // Read from stdin, save to workspace artifacts
        if workspace.is_empty() {
            fail("--artifact - requires a workspace (run inside a team)");
        }
        let heredoc_recipe = "  hive <cmd> <args> --artifact - <<'EOF'\n  # details\n  EOF";
        if stdin_isatty() {
            fail(&format!(
                "--artifact - expects piped stdin but a terminal is attached; \
                 use a heredoc instead:\n{heredoc_recipe}"
            ));
        }
        let mut content = String::new();
        use std::io::Read;
        let _ = std::io::stdin().read_to_string(&mut content);
        if content.is_empty() {
            fail(&format!(
                "--artifact - received empty stdin; pipe content in or use a heredoc:\n{heredoc_recipe}"
            ));
        }
        let ws_artifacts = Path::new(workspace).join("artifacts");
        let _ = std::fs::create_dir_all(&ws_artifacts);
        // Short random id — file name is never parsed by downstream code.
        let filename = format!("{}.md", token_urlsafe4());
        let path = ws_artifacts.join(filename);
        let _ = std::fs::write(&path, &content);
        return path.to_string_lossy().into_owned();
    }
    let resolved_artifact = expanduser(artifact);
    if !Path::new(&resolved_artifact).exists() {
        fail(&format!("artifact not found: {resolved_artifact}"));
    }
    resolved_artifact
}

// ---------------------------------------------------------------------------
// Tests (ported from tests/unit — logic-level only)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_urlsafe4_shape() {
        let token = token_urlsafe4();
        assert_eq!(token.len(), 6);
        assert!(token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }
}
