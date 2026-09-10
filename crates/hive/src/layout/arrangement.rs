//! The remembered arrangement of a team window, in the workspace's
//! `state/hive-arrangement/window.json`: the human's border drag (the tmux
//! layout string, the plan key it held under, the member on each leaf)
//! and the `hive mirror` choice. tmux objects die with the server; this
//! is what a rebuilt window is arranged from. Display preference only: it
//! decides no membership, names no process, and a file that does not fit
//! the window — another team instance, another plan, other members — is
//! ignored and the planner takes over.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

const DIR: &str = "hive-arrangement";
const FILE: &str = "window.json";
const SCHEMA: u64 = 1;

/// One leaf of the drag: the member whose pane sat there, with the pane's
/// role (`agent`, `mirror`; empty for a plain shell pane).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Leaf {
    pub member: String,
    pub role: String,
}

/// The drag as the hook observed it: the plan whose key the window held,
/// the window's size then, its layout string, and its panes in window
/// order — the order `select-layout` hands cells out in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Drag {
    pub plan_key: String,
    pub size: (i64, i64),
    pub layout: String,
    pub leaves: Vec<Leaf>,
}

/// Whose arrangement a window holds: the team, its workspace and the team
/// instance (`@hive-created`, the registry's `createdAt`), so a recycled
/// name's successor never inherits its predecessor's drag.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct WindowIdentity {
    pub team: String,
    pub workspace: String,
    pub instance: String,
}

impl WindowIdentity {
    /// The store file. None when no workspace resolves, or when
    /// `state/hive-arrangement` is taken by something that is not a
    /// directory (a `hive create --state hive-arrangement=…` entry): the
    /// user's file is not overwritten, the arrangement is simply not kept.
    fn path(&self) -> Option<PathBuf> {
        let workspace = if self.workspace.is_empty() {
            crate::registry::team_dir(&self.team)?
        } else {
            PathBuf::from(crate::paths::expanduser(&self.workspace))
        };
        let dir = workspace.join("state").join(DIR);
        if dir.exists() && !dir.is_dir() {
            return None;
        }
        Some(dir.join(FILE))
    }
}

/// Both sides are epoch seconds; an empty one on either side is no
/// instance check, as every registry write treats an empty key.
fn instance_matches(stored: &str, wanted: &str) -> bool {
    if stored.is_empty() || wanted.is_empty() {
        return true;
    }
    match (stored.parse::<f64>(), wanted.parse::<f64>()) {
        (Ok(a), Ok(b)) => a == b,
        _ => stored == wanted,
    }
}

#[derive(Debug, Default, PartialEq)]
struct Stored {
    mirror: Option<bool>,
    drag: Option<Drag>,
}

fn parse_leaf(leaf: &Value) -> Option<Leaf> {
    let leaf = leaf.as_object()?;
    let text = |key: &str| {
        leaf.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    Some(Leaf {
        member: text("member"),
        role: text("role"),
    })
}

fn parse_drag(drag: &Map<String, Value>) -> Option<Drag> {
    let plan_key = drag.get("planKey")?.as_str()?.to_string();
    let layout = drag.get("layout")?.as_str()?.to_string();
    let size = drag
        .get("size")
        .and_then(Value::as_array)
        .and_then(|size| Some((size.first()?.as_i64()?, size.get(1)?.as_i64()?)))
        .unwrap_or((0, 0));
    let leaves = drag
        .get("leaves")?
        .as_array()?
        .iter()
        .map(parse_leaf)
        .collect::<Option<Vec<Leaf>>>()?;
    if plan_key.is_empty() || layout.is_empty() || leaves.is_empty() {
        return None;
    }
    Some(Drag {
        plan_key,
        size,
        layout,
        leaves,
    })
}

/// What the file holds for *instance*: nothing when it is missing, not
/// this schema, corrupt, or another instance's.
fn read(path: &Path, instance: &str) -> Stored {
    let Ok(text) = fs::read_to_string(path) else {
        return Stored::default();
    };
    let Ok(Value::Object(doc)) = serde_json::from_str::<Value>(&text) else {
        return Stored::default();
    };
    if doc.get("schema").and_then(Value::as_u64) != Some(SCHEMA) {
        return Stored::default();
    }
    let stored_instance = doc
        .get("instance")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !instance_matches(stored_instance, instance) {
        return Stored::default();
    }
    Stored {
        mirror: match doc.get("mirror").and_then(Value::as_str) {
            Some("on") => Some(true),
            Some("off") => Some(false),
            _ => None,
        },
        drag: doc
            .get("drag")
            .and_then(Value::as_object)
            .and_then(parse_drag),
    }
}

fn drag_value(drag: &Drag) -> Value {
    let leaves: Vec<Value> = drag
        .leaves
        .iter()
        .map(|leaf| {
            let mut row = Map::new();
            row.insert("member".to_string(), Value::from(leaf.member.as_str()));
            row.insert("role".to_string(), Value::from(leaf.role.as_str()));
            Value::Object(row)
        })
        .collect();
    let mut out = Map::new();
    out.insert("planKey".to_string(), Value::from(drag.plan_key.as_str()));
    out.insert(
        "size".to_string(),
        Value::Array(vec![Value::from(drag.size.0), Value::from(drag.size.1)]),
    );
    out.insert("layout".to_string(), Value::from(drag.layout.as_str()));
    out.insert("leaves".to_string(), Value::Array(leaves));
    Value::Object(out)
}

/// Write *stored* for *identity* with an atomic rename; an empty record
/// removes the file. Whether the file now says what was asked.
fn write(path: &Path, identity: &WindowIdentity, stored: &Stored) -> bool {
    if stored.mirror.is_none() && stored.drag.is_none() {
        return match fs::remove_file(path) {
            Ok(()) => true,
            Err(e) => e.kind() == std::io::ErrorKind::NotFound,
        };
    }
    let mut doc = Map::new();
    doc.insert("schema".to_string(), Value::from(SCHEMA));
    doc.insert("team".to_string(), Value::from(identity.team.as_str()));
    doc.insert(
        "instance".to_string(),
        Value::from(identity.instance.as_str()),
    );
    if let Some(on) = stored.mirror {
        doc.insert(
            "mirror".to_string(),
            Value::from(if on { "on" } else { "off" }),
        );
    }
    if let Some(drag) = &stored.drag {
        doc.insert("drag".to_string(), drag_value(drag));
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    if fs::create_dir_all(parent).is_err() {
        return false;
    }
    let Ok((mut file, tmp)) = crate::paths::mkstemp_in(parent, ".arrangement.", ".tmp") else {
        return false;
    };
    let mut text = serde_json::to_string_pretty(&Value::Object(doc)).unwrap_or_default();
    text.push('\n');
    let written = file
        .write_all(text.as_bytes())
        .and_then(|_| fs::rename(&tmp, path))
        .is_ok();
    if !written {
        let _ = fs::remove_file(&tmp);
    }
    written
}

/// The remembered drag of *identity*'s window, if any.
pub(crate) fn drag(identity: &WindowIdentity) -> Option<Drag> {
    read(&identity.path()?, &identity.instance).drag
}

/// Remember *drag*, keeping the mirror choice. False when nothing changed
/// or the store is unavailable.
pub(crate) fn remember_drag(identity: &WindowIdentity, drag: &Drag) -> bool {
    let Some(path) = identity.path() else {
        return false;
    };
    let mut stored = read(&path, &identity.instance);
    if stored.drag.as_ref() == Some(drag) {
        return false;
    }
    stored.drag = Some(drag.clone());
    write(&path, identity, &stored)
}

/// Forget the drag, keeping the mirror choice.
pub(crate) fn forget_drag(identity: &WindowIdentity) {
    let Some(path) = identity.path() else {
        return;
    };
    let mut stored = read(&path, &identity.instance);
    if stored.drag.is_none() {
        return;
    }
    stored.drag = None;
    write(&path, identity, &stored);
}

/// The remembered `hive mirror` choice: `Some(false)` withholds the mirror.
pub(crate) fn mirror_preference(identity: &WindowIdentity) -> Option<bool> {
    read(&identity.path()?, &identity.instance).mirror
}

/// Remember the `hive mirror` choice, keeping the drag.
pub(crate) fn remember_mirror(identity: &WindowIdentity, on: bool) {
    let Some(path) = identity.path() else {
        return;
    };
    let mut stored = read(&path, &identity.instance);
    if stored.mirror == Some(on) {
        return;
    }
    stored.mirror = Some(on);
    write(&path, identity, &stored);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(workspace: &Path, instance: &str) -> WindowIdentity {
        WindowIdentity {
            team: "honey".to_string(),
            workspace: workspace.to_string_lossy().into_owned(),
            instance: instance.to_string(),
        }
    }

    fn a_drag() -> Drag {
        Drag {
            plan_key: "landscape/m2/mirror-half/2x1".to_string(),
            size: (220, 60),
            layout: "7851,220x60,0,0{60x60,0,0,1,159x60,61,0[159x10,61,0,2,159x49,61,11,3]}"
                .to_string(),
            leaves: vec![
                Leaf {
                    member: "orch".to_string(),
                    role: "mirror".to_string(),
                },
                Leaf {
                    member: "scout".to_string(),
                    role: "agent".to_string(),
                },
                Leaf {
                    member: "sage".to_string(),
                    role: "agent".to_string(),
                },
            ],
        }
    }

    fn file(workspace: &Path) -> PathBuf {
        workspace.join("state").join(DIR).join(FILE)
    }

    #[test]
    fn test_drag_and_mirror_round_trip_through_one_file_and_forget_each_other_not() {
        let tmp = tempfile::tempdir().unwrap();
        let me = identity(tmp.path(), "100.0");
        assert_eq!(drag(&me), None);
        assert_eq!(mirror_preference(&me), None);

        assert!(remember_drag(&me, &a_drag()));
        // the same drag again is not a write
        assert!(!remember_drag(&me, &a_drag()));
        remember_mirror(&me, false);
        assert_eq!(drag(&me), Some(a_drag()));
        assert_eq!(mirror_preference(&me), Some(false));

        let doc: Value =
            serde_json::from_str(&fs::read_to_string(file(tmp.path())).unwrap()).unwrap();
        assert_eq!(doc["schema"], Value::from(1));
        assert_eq!(doc["team"], Value::from("honey"));
        assert_eq!(doc["instance"], Value::from("100.0"));
        assert_eq!(doc["mirror"], Value::from("off"));
        assert_eq!(doc["drag"]["planKey"], Value::from(a_drag().plan_key));
        assert_eq!(doc["drag"]["size"], serde_json::json!([220, 60]));
        assert_eq!(doc["drag"]["leaves"][0]["member"], Value::from("orch"));
        assert_eq!(doc["drag"]["leaves"][0]["role"], Value::from("mirror"));

        // forgetting the drag keeps the mirror choice
        forget_drag(&me);
        assert_eq!(drag(&me), None);
        assert_eq!(mirror_preference(&me), Some(false));
        remember_mirror(&me, true);
        assert_eq!(mirror_preference(&me), Some(true));
        assert!(file(tmp.path()).exists());
    }

    #[test]
    fn test_a_record_with_nothing_left_in_it_is_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        let me = identity(tmp.path(), "100.0");
        assert!(remember_drag(&me, &a_drag()));
        assert!(file(tmp.path()).exists());
        forget_drag(&me);
        assert!(!file(tmp.path()).exists());
        // forgetting what is not there is fine
        forget_drag(&me);
    }

    #[test]
    fn test_another_instance_reads_nothing_and_its_write_replaces_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let old = identity(tmp.path(), "100.0");
        assert!(remember_drag(&old, &a_drag()));
        remember_mirror(&old, false);

        // the recycled name's successor inherits neither
        let new = identity(tmp.path(), "200.0");
        assert_eq!(drag(&new), None);
        assert_eq!(mirror_preference(&new), None);
        assert!(remember_drag(&new, &a_drag()));
        assert_eq!(mirror_preference(&new), None);
        let doc: Value =
            serde_json::from_str(&fs::read_to_string(file(tmp.path())).unwrap()).unwrap();
        assert_eq!(doc["instance"], Value::from("200.0"));
        assert!(doc.get("mirror").is_none());
        // the old instance now sees nothing of its own
        assert_eq!(drag(&old), None);
    }

    #[test]
    fn test_instances_compare_as_numbers_and_an_empty_one_checks_nothing() {
        assert!(instance_matches("100", "100.0"));
        assert!(instance_matches("1757500000.5", "1757500000.5"));
        assert!(!instance_matches("100.0", "100.5"));
        assert!(instance_matches("", "100.0"));
        assert!(instance_matches("100.0", ""));
        assert!(!instance_matches("a", "b"));
    }

    #[test]
    fn test_a_state_entry_named_like_the_store_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        fs::create_dir_all(&state).unwrap();
        // `hive create --state hive-arrangement=x` wrote this file
        fs::write(state.join(DIR), "x").unwrap();
        let me = identity(tmp.path(), "100.0");

        assert!(!remember_drag(&me, &a_drag()));
        remember_mirror(&me, false);
        forget_drag(&me);
        assert_eq!(drag(&me), None);
        assert_eq!(mirror_preference(&me), None);
        assert_eq!(fs::read_to_string(state.join(DIR)).unwrap(), "x");
    }

    #[test]
    fn test_a_corrupt_or_foreign_file_reads_as_nothing_and_is_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let me = identity(tmp.path(), "100.0");
        fs::create_dir_all(file(tmp.path()).parent().unwrap()).unwrap();
        for text in ["{not json", "[]", "{\"schema\": 2, \"drag\": {}}"] {
            fs::write(file(tmp.path()), text).unwrap();
            assert_eq!(drag(&me), None, "{text}");
            assert_eq!(mirror_preference(&me), None, "{text}");
        }
        // a drag missing its layout is no drag
        fs::write(
            file(tmp.path()),
            r#"{"schema":1,"instance":"100.0","drag":{"planKey":"k","leaves":[{"member":"a","role":"agent"}]}}"#,
        )
        .unwrap();
        assert_eq!(drag(&me), None);
        assert!(remember_drag(&me, &a_drag()));
        assert_eq!(drag(&me), Some(a_drag()));
    }

    #[test]
    fn test_an_empty_workspace_means_the_team_directory() {
        let mut env = crate::testenv::EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        env.set("HIVE_HOME", tmp.path());
        let me = WindowIdentity {
            team: "honey".to_string(),
            workspace: String::new(),
            instance: "100.0".to_string(),
        };
        assert!(remember_drag(&me, &a_drag()));
        assert!(file(&tmp.path().join("teams").join("honey")).exists());
        assert_eq!(drag(&me), Some(a_drag()));
        // a name that could escape the store has no file
        let evil = WindowIdentity {
            team: "../evil".to_string(),
            ..me
        };
        assert!(!remember_drag(&evil, &a_drag()));
    }
}
