//! Name allocation: the team-name pool and its window-id overflow, the
//! random member-name pool, and the claim that keeps a chosen name unique
//! within a team (window panes, lead, registry roster).

use std::collections::HashSet;

use serde_json::{Map, Value};

use crate::json_fields::map_str;
use crate::team::{Team, LEAD_AGENT_NAME};
use crate::tmux;
use crate::tmux::PaneInfo;

const TEAM_NAME_POOL: [&str; 10] = [
    "honey", "comb", "wasp", "bumble", "hornet", "nectar", "pollen", "amber", "clover", "sage",
];

const RANDOM_AGENT_NAMES: [&str; 10] = [
    "yoyo", "lulu", "nini", "bobo", "kiki", "dodo", "pipi", "toto", "momo", "coco",
];

/// os.urandom-grade bytes for name picks and artifact filenames.
pub(crate) fn os_random_bytes(n: usize) -> Vec<u8> {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(&mut buf).is_ok() {
            return buf;
        }
    }
    // ponytail: nanos fallback only fires when /dev/urandom is unreadable —
    // uniqueness is what matters here, not cryptographic strength.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    for (i, b) in buf.iter_mut().enumerate() {
        *b = ((nanos >> ((i % 16) * 8)) & 0xff) as u8;
    }
    buf
}

/// `secrets.choice(seq)`.
fn random_choice<'a>(options: &[&'a str]) -> &'a str {
    let idx = os_random_bytes(1)[0] as usize % options.len();
    options[idx]
}

/// Stable per-window slug. Uses the tmux window id (`@42` → `w42`); falls
/// back to the mutable window index only when no id is available.
fn window_id_slug(window_id: &str, fallback_index: &str) -> String {
    let raw = window_id.trim_start_matches('@');
    let raw = if raw.is_empty() {
        if fallback_index.is_empty() {
            "0"
        } else {
            fallback_index
        }
    } else {
        raw
    };
    format!("w{raw}")
}

/// Window-id-derived team name — the overflow scheme behind the pool.
fn default_team_name_for_window(session_name: &str, window_id: &str, window_index: &str) -> String {
    format!("{session_name}-{}", window_id_slug(window_id, window_index))
}

/// Group tags and qualified `@hive-agent` prefixes claimed by live panes.
fn claimed_group_namespaces() -> HashSet<String> {
    let mut claimed = HashSet::new();
    for pane in tmux::list_panes_all() {
        let group = pane.group.trim();
        if !group.is_empty() {
            claimed.insert(group.to_string());
        }
        let agent = pane.agent.trim();
        if let Some((prefix, _)) = agent.split_once('.') {
            if !prefix.is_empty() {
                claimed.insert(prefix.to_string());
            }
        }
    }
    claimed
}

/// Short memorable name for a new team; window-id scheme as overflow.
pub(crate) fn pick_team_name(session_name: &str, window_id: &str, window_index: &str) -> String {
    let mut used: HashSet<String> = tmux::list_panes_all()
        .into_iter()
        .filter(|p| !p.team.is_empty())
        .map(|p| p.team)
        .collect();
    used.extend(claimed_group_namespaces());
    // The registry is the name authority: a team whose window is gone owns
    // its name until `hive delete` — a pool pick must never clobber it.
    for entry in crate::registry::list_entries() {
        let team = map_str(&entry, "team");
        if !team.is_empty() {
            used.insert(team);
        }
    }
    for candidate in TEAM_NAME_POOL {
        if !used.contains(candidate) {
            return candidate.to_string();
        }
    }
    default_team_name_for_window(session_name, window_id, window_index)
}

fn names_used_in_window(panes: &[PaneInfo]) -> HashSet<String> {
    panes
        .iter()
        .map(|pane| pane.agent.trim().to_string())
        .filter(|agent| !agent.is_empty())
        .collect()
}

/// Pick a short random peer name while avoiding collisions in this window.
pub(crate) fn derive_agent_name(seen: &mut HashSet<String>) -> String {
    let available: Vec<&str> = RANDOM_AGENT_NAMES
        .iter()
        .copied()
        .filter(|name| !seen.contains(*name))
        .collect();
    let candidate = if !available.is_empty() {
        random_choice(&available).to_string()
    } else {
        let mut suffix = 1;
        let mut candidate = format!("agent-{suffix}");
        while seen.contains(&candidate) {
            suffix += 1;
            candidate = format!("agent-{suffix}");
        }
        candidate
    };
    seen.insert(candidate.clone());
    candidate
}

/// Names taken in the team: the window's tagged panes, the lead, and the
/// registry roster (a member whose pane is gone owns its name too).
pub(crate) fn window_seen_names(t: &Team, panes: &[PaneInfo]) -> HashSet<String> {
    let mut seen_names = names_used_in_window(panes);
    if let Some(entry) = crate::registry::load(&t.name) {
        seen_names.extend(roster_names(&entry));
    }
    seen_names.insert(if t.lead_name.is_empty() {
        LEAD_AGENT_NAME.to_string()
    } else {
        t.lead_name.clone()
    });
    seen_names
}

/// Member names in a registry entry's roster.
pub(crate) fn roster_names(entry: &Map<String, Value>) -> HashSet<String> {
    entry
        .get("members")
        .and_then(Value::as_array)
        .map(|members| {
            members
                .iter()
                .filter_map(Value::as_object)
                .map(|m| map_str(m, "name"))
                .collect()
        })
        .unwrap_or_default()
}

/// Why *name_override* cannot be claimed against *seen_names*, or None.
fn member_name_conflict(name_override: &str, seen_names: &HashSet<String>) -> Option<String> {
    if name_override == "flow" || name_override.starts_with("flow.") {
        return Some(format!(
            "'{name_override}' collides with the flow runner's mailbox address kind (flow.run), not a member name"
        ));
    }
    if seen_names.contains(name_override) {
        return Some(format!(
            "name '{name_override}' is already taken in this team"
        ));
    }
    None
}

pub(crate) fn claim_member_name(
    name_override: &str,
    seen_names: &mut HashSet<String>,
) -> Result<(), String> {
    if name_override.is_empty() {
        return Ok(());
    }
    if let Some(error) = member_name_conflict(name_override, seen_names) {
        return Err(error);
    }
    seen_names.insert(name_override.to_string());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testenv::iso;
    use crate::testkit::registry_team;

    #[test]
    fn test_window_id_slug_prefers_window_id() {
        assert_eq!(window_id_slug("@42", "3"), "w42");
        assert_eq!(window_id_slug("", "3"), "w3");
        assert_eq!(window_id_slug("", ""), "w0");
    }

    #[test]
    fn test_default_team_name_for_window_uses_slug() {
        assert_eq!(default_team_name_for_window("dev", "@7", "1"), "dev-w7");
        assert_eq!(default_team_name_for_window("dev", "", "5"), "dev-w5");
    }

    #[test]
    fn test_derive_agent_name_avoids_seen_and_falls_back() {
        let mut seen: HashSet<String> = ["yoyo", "lulu"].iter().map(|s| s.to_string()).collect();
        let name = derive_agent_name(&mut seen);
        assert!(RANDOM_AGENT_NAMES.contains(&name.as_str()));
        assert_ne!(name, "yoyo");
        assert_ne!(name, "lulu");
        assert!(seen.contains(&name));

        let mut all: HashSet<String> = RANDOM_AGENT_NAMES.iter().map(|s| s.to_string()).collect();
        assert_eq!(derive_agent_name(&mut all), "agent-1");
        assert_eq!(derive_agent_name(&mut all), "agent-2");
    }

    #[test]
    fn test_seen_names_include_registry_only_members() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _env = iso(tmp.path());
        let pool: Vec<&str> = RANDOM_AGENT_NAMES.to_vec();
        let t = registry_team("honey", 100.0, &pool);

        let mut seen = window_seen_names(&t, &[]);

        for name in &pool {
            assert!(seen.contains(*name), "{name}");
        }
        assert_eq!(
            member_name_conflict(pool[0], &seen).unwrap(),
            format!("name '{}' is already taken in this team", pool[0])
        );
        assert_eq!(derive_agent_name(&mut seen), "agent-1");
    }
}
