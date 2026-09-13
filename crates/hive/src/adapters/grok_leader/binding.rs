//! Whether a member's session record still names the member it was
//! written for — the one question every revival asks before it raises a
//! leader, and the `retained` the runtime reports for a member whose
//! leader is gone.
//!
//! A leader exits on its own once its last client disconnects (grok's
//! default; hive passes no `--no-exit-on-disconnect`), and what it leaves
//! behind is the key's session record. That record is only worth loading
//! again while the registry still has the team instance and the roster row
//! it was written for: a same-named team recreated since, a member
//! respawned onto another session, a row killed off, or a record that
//! never carried a binding (a launch nobody bound, a record from before
//! the binding existed) is not this member's session, and a same name is
//! no evidence. The hived re-runs the check at every submission; a
//! runtime field that reported `retained` a moment ago is a snapshot.

use serde_json::Value;

use super::daemon::probe_socket;
use super::keys::{member_from_key, read_session_key, socket_path_for_key, RecordBinding};

/// The binding a revival may act on: the record's, once it has been
/// checked against the registry's current instance of the team and the
/// member's roster row. `Err` says which check failed.
pub fn binding_holds(key: &str) -> Result<RecordBinding, String> {
    let Some((team, member)) = member_from_key(key) else {
        return Err(format!("{key} is not a member key"));
    };
    let Some(record) = read_session_key(key) else {
        return Err(format!("no session record for {key}"));
    };
    let Some(binding) = record.binding else {
        return Err(format!("the session record for {key} names no member"));
    };
    if binding.team != team || binding.member != member {
        return Err(format!(
            "the session record for {key} is bound to {}.{}",
            binding.team, binding.member
        ));
    }
    let Some(entry) = crate::registry::load(&team) else {
        return Err(format!("team '{team}' is not in the registry"));
    };
    if !same_instance(entry.get("createdAt"), &binding.created_at) {
        return Err(format!(
            "team '{team}' is another instance than the one {key} was bound to"
        ));
    }
    let row = entry
        .get("members")
        .and_then(Value::as_array)
        .and_then(|rows| {
            rows.iter()
                .filter_map(Value::as_object)
                .find(|row| row.get("name").and_then(Value::as_str) == Some(member.as_str()))
        });
    let Some(row) = row else {
        return Err(format!("'{member}' is not on the roster of '{team}'"));
    };
    if row.get("cli").and_then(Value::as_str) != Some("grok") {
        return Err(format!("'{team}.{member}' is not a grok member"));
    }
    if row.get("sessionId").and_then(Value::as_str) != Some(record.session_id.as_str()) {
        return Err(format!(
            "the roster row of '{team}.{member}' names another session than its record"
        ));
    }
    Ok(binding)
}

/// Both sides are epoch seconds; the registry stores its as a string or a
/// number, the record as the string `team::created_at_key` made.
fn same_instance(stored: Option<&Value>, bound: &str) -> bool {
    let stored = match stored {
        Some(Value::String(text)) => text.parse::<f64>().ok(),
        Some(Value::Number(number)) => number.as_f64(),
        _ => None,
    };
    matches!((stored, bound.parse::<f64>().ok()), (Some(a), Some(b)) if a == b)
}

/// A member whose leader is gone but whose session a submission may load
/// again: the binding holds and nothing listens on the key's socket.
pub fn retained(key: &str) -> bool {
    binding_holds(key).is_ok() && !probe_socket(&socket_path_for_key(key))
}
