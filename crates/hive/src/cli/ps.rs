//! Read-only resource inventory. OS parentage and recorded ownership are
//! independent columns; collection never repairs a record or starts an engine.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Map, Value};

use crate::adapters::{claude_bg, claude_desktop, codex_app_server, grok_leader};
use crate::{hived, registry, tmux};

const UNKNOWN: &str = "unknown";

#[derive(Debug)]
struct Process {
    pid: i64,
    ppid: i64,
    born: String,
    age: Value,
    command: String,
}

fn text(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn field(value: Option<&Value>) -> Value {
    value
        .filter(|v| !v.is_null() && v.as_str() != Some(""))
        .cloned()
        .unwrap_or(json!(UNKNOWN))
}

fn string(map: &Map<String, Value>, key: &str) -> String {
    text(&field(map.get(key)))
}

fn age(born: &str, now: i64) -> Value {
    let Ok(input) = std::ffi::CString::new(born) else {
        return json!(UNKNOWN);
    };
    // ps lstart is local time. LC_ALL=C on the child fixes month names;
    // mktime applies the local timezone, including DST at the birth date.
    let timestamp = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        tm.tm_isdst = -1;
        if libc::strptime(input.as_ptr(), c"%a %b %e %H:%M:%S %Y".as_ptr(), &mut tm).is_null() {
            return json!(UNKNOWN);
        }
        libc::mktime(&mut tm)
    };
    if timestamp < 0 || timestamp as i64 > now {
        json!(UNKNOWN)
    } else {
        json!(now - timestamp as i64)
    }
}

fn parse_processes(input: &str, now: i64) -> Vec<Process> {
    input
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?.parse().ok()?;
            let ppid = parts.next()?.parse().ok()?;
            let born = (0..5)
                .map(|_| parts.next())
                .collect::<Option<Vec<_>>>()?
                .join(" ");
            let first = parts.next()?;
            let offset = first.as_ptr() as usize - line.as_ptr() as usize;
            let command = line[offset..].to_string();
            Some(Process {
                pid,
                ppid,
                age: age(&born, now),
                born,
                command,
            })
        })
        .collect()
}

fn executable(command: &str, name: &str) -> bool {
    command
        .split_whitespace()
        .next()
        .and_then(|p| Path::new(p).file_name())
        .is_some_and(|p| p == name)
}

fn option(command: &str, flag: &str) -> Option<String> {
    let mut words = command.split_whitespace();
    while let Some(word) = words.next() {
        if word == flag {
            return words.next().map(str::to_owned);
        }
        if let Some(value) = word.strip_prefix(&format!("{flag}=")) {
            return Some(value.to_owned());
        }
    }
    None
}

fn row(kind: &str, owner: Value, process: Option<&Process>) -> Value {
    json!({"kind": kind, "logicalOwner": owner,
        "pid": process.map(|p| json!(p.pid)).unwrap_or(json!(UNKNOWN)),
        "ppid": process.map(|p| json!(p.ppid)).unwrap_or(json!(UNKNOWN)),
        "startedAt": process.map(|p| json!(p.born)).unwrap_or(json!(UNKNOWN)),
        "ageSeconds": process.map(|p| p.age.clone()).unwrap_or(json!(UNKNOWN))})
}

fn exists(path: &Path) -> Value {
    match fs::metadata(path) {
        Ok(_) => json!(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!(false),
        Err(_) => json!(UNKNOWN),
    }
}

fn hived_args(process: &Process, entries: &[Map<String, Value>]) -> Option<(String, String)> {
    if !executable(&process.command, "hive") {
        return None;
    }
    let (_, args) = process.command.split_once(" --hived ")?;
    for entry in entries {
        let workspace = string(entry, "workspace");
        let team = string(entry, "team");
        if args.starts_with(&format!("{workspace} {team} "))
            || args == format!("{workspace} {team}")
        {
            return Some((workspace, team));
        }
    }
    // Without a roster, only an unambiguous four-argument daemon argv is
    // decoded. Whitespace inside an unknown workspace cannot be recovered.
    let args: Vec<_> = args.split_whitespace().collect();
    if (2..=4).contains(&args.len()) {
        Some((args[0].to_string(), args[1].to_string()))
    } else {
        Some((UNKNOWN.to_string(), UNKNOWN.to_string()))
    }
}

fn hash_matches(reply: Option<&Map<String, Value>>, pid: i64, hash: &str) -> Value {
    let Some(reply) = reply else {
        return json!(UNKNOWN);
    };
    if reply
        .get("hived")
        .and_then(|h| h.get("pid"))
        .and_then(Value::as_i64)
        != Some(pid)
    {
        return json!(UNKNOWN);
    }
    match reply.get("buildHash").and_then(Value::as_str) {
        Some(remote) if remote != UNKNOWN && hash != UNKNOWN => json!(remote == hash),
        _ => json!(UNKNOWN),
    }
}

fn desktop_absent(root: &Path, host: &str) -> Option<bool> {
    fn dirs(root: &Path) -> Option<Vec<PathBuf>> {
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(vec![]),
            Err(_) => return None,
        };
        let mut dirs = vec![];
        for entry in entries {
            let entry = entry.ok()?;
            if entry.file_type().ok()?.is_dir() {
                dirs.push(entry.path());
            }
        }
        Some(dirs)
    }
    for account in dirs(root)? {
        for org in dirs(&account)? {
            match fs::metadata(org.join(format!("{host}.json"))) {
                Ok(_) => return Some(false),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return None,
            }
        }
    }
    Some(true)
}

fn orch_state(entry: &Map<String, Value>) -> &'static str {
    let host = entry
        .get("members")
        .and_then(Value::as_array)
        .and_then(|members| members.iter().find(|m| m["name"] == "orch"))
        .and_then(|m| m.get("hostSessionId"))
        .and_then(Value::as_str);
    let Some(host) = host.filter(|h| {
        h.starts_with("local_")
            && h.len() > 6
            && h.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    }) else {
        return UNKNOWN;
    };
    if claude_desktop::desktop_record(host).is_some() {
        return "present";
    }
    let root = PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join("Library/Application Support/Claude/claude-code-sessions");
    if desktop_absent(&root, host) == Some(true) {
        "absent"
    } else {
        UNKNOWN
    }
}

fn owners(entries: &[Map<String, Value>], cli: &str, id: &str) -> Value {
    let mut found = vec![];
    for entry in entries {
        for member in entry
            .get("members")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if member["cli"] == cli && member["sessionId"] == id {
                if let Some(name) = member["name"].as_str() {
                    found.push(format!("{}.{name}", string(entry, "team")));
                }
            }
        }
    }
    if found.is_empty() {
        json!(UNKNOWN)
    } else {
        json!(found.join(","))
    }
}

fn job_state(engine_alive: bool, in_ledger: bool, ledger_available: bool) -> &'static str {
    if engine_alive {
        "alive"
    } else if !ledger_available {
        UNKNOWN
    } else if in_ledger {
        "asleep"
    } else {
        "gone"
    }
}

fn client_row(
    process: &Process,
    processes: &[Process],
    entries: &[Map<String, Value>],
) -> Option<Value> {
    if !executable(&process.command, "tmux")
        || !process.command.split_whitespace().any(|w| w == "-C")
        || !process
            .command
            .split_whitespace()
            .any(|w| w == "attach" || w == "attach-session")
    {
        return None;
    }
    let session = option(&process.command, "-t").unwrap_or(UNKNOWN.to_string());
    let parent_owner = processes
        .iter()
        .find(|p| p.pid == process.ppid)
        .and_then(|p| hived_args(p, entries));
    let logical = entries
        .iter()
        .find(|e| string(e, "team") == session)
        .map(|e| string(e, "team"))
        .unwrap_or_else(|| session.clone());
    let mut item = row("tmuxClient", json!(logical), Some(process));
    item["session"] = json!(session);
    item["orphan"] = json!(process.ppid == 1);
    item["parentTeam"] = parent_owner
        .map(|(_, t)| json!(t))
        .unwrap_or(json!(UNKNOWN));
    Some(item)
}

fn display_present(panes: Option<&[tmux::PaneInfo]>, server: &str, team: &str) -> Value {
    match panes {
        Some(panes) => json!(panes.iter().any(|p| p.team == team)),
        None if server == "no-server" => json!(false),
        None => json!(UNKNOWN),
    }
}

fn is_grok_leader(process: &Process) -> bool {
    executable(&process.command, "grok")
        && process
            .command
            .split_whitespace()
            .skip(1)
            .take(2)
            .eq(["agent", "leader"])
}

fn leader_matches(process: &Process, socket: &Path, recorded: &[i64]) -> bool {
    is_grok_leader(process)
        && match option(&process.command, "--leader-socket") {
            Some(named) => Path::new(&named) == socket,
            None => recorded.contains(&process.pid),
        }
}

fn collect() -> Result<Vec<Value>, String> {
    let output = Command::new("ps")
        .args(["-axo", "pid,ppid,lstart,command"])
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| format!("cannot read process table: {e}"))?;
    if !output.status.success() {
        return Err("cannot read process table".to_string());
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs() as i64;
    let processes = parse_processes(&String::from_utf8_lossy(&output.stdout), now);
    let entries = registry::list_entries();
    let (panes, server) = tmux::list_panes_all_status();
    let mut rows = vec![];
    let mut hived_teams = BTreeSet::new();
    let hash = hived::hived_build_hash();
    for process in &processes {
        if let Some((workspace, team)) = hived_args(process, &entries) {
            let mut item = row("hived", json!(team), Some(process));
            item["team"] = json!(team);
            item["workspace"] = json!(workspace);
            item["tmuxServer"] = json!(server);
            let reply = if workspace != UNKNOWN {
                let socket = hived::socket_path(&workspace);
                item["socket"] = json!(socket);
                item["socketExists"] = exists(&socket);
                hived::request_ping_impl(&workspace, 0.25)
            } else {
                item["socket"] = json!(UNKNOWN);
                item["socketExists"] = json!(UNKNOWN);
                None
            };
            item["buildMatches"] = hash_matches(reply.as_ref(), process.pid, hash);
            hived_teams.insert((team, workspace));
            rows.push(item);
        }
        if let Some(item) = client_row(process, &processes, &entries) {
            rows.push(item);
        }
    }

    let mut codex_sockets = BTreeSet::new();
    for process in &processes {
        if executable(&process.command, "codex")
            && process
                .command
                .split_whitespace()
                .any(|w| w == "app-server")
        {
            let socket = option(&process.command, "--listen")
                .and_then(|s| s.strip_prefix("unix://").map(PathBuf::from));
            let mut item = row("codexDaemon", json!(UNKNOWN), Some(process));
            item["alive"] = json!(true);
            item["socket"] = socket.as_ref().map(|s| json!(s)).unwrap_or(json!(UNKNOWN));
            item["codexHome"] = socket
                .as_ref()
                .filter(|s| s.file_name().is_some_and(|n| n == "hive-shared.sock"))
                .and_then(|s| s.parent()?.parent())
                .map(|p| json!(p))
                .unwrap_or(json!(UNKNOWN));
            item["logicalOwner"] = item["codexHome"].clone();
            if let Some(socket) = socket {
                codex_sockets.insert(socket);
            }
            rows.push(item);
        }
    }
    let socket = codex_app_server::shared_socket_path();
    if !codex_sockets.contains(&socket)
        && (socket.exists() || codex_app_server::shared_pidfile_path().exists())
    {
        let mut item = row("codexDaemon", json!(codex_app_server::codex_home()), None);
        item["codexHome"] = json!(codex_app_server::codex_home());
        item["socket"] = json!(socket);
        item["alive"] = json!(codex_app_server::daemon_alive());
        rows.push(item);
    }

    let mut leaders: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();
    for key in grok_leader::list_daemon_keys() {
        leaders
            .entry(grok_leader::socket_path_for_key(&key))
            .or_default()
            .insert(key);
    }
    for process in &processes {
        if is_grok_leader(process) {
            if let Some(socket) = option(&process.command, "--leader-socket") {
                let socket = PathBuf::from(socket);
                if let Some(key) = socket.file_stem().and_then(|s| s.to_str()) {
                    leaders
                        .entry(socket.clone())
                        .or_default()
                        .insert(key.to_string());
                }
            }
        }
    }
    let mut listed_leaders = BTreeSet::new();
    for (socket, keys) in leaders {
        let recorded_pids: Vec<i64> = ["pid", "lock"]
            .iter()
            .filter_map(|ext| {
                fs::read_to_string(socket.with_extension(ext))
                    .ok()?
                    .trim()
                    .parse()
                    .ok()
            })
            .collect();
        let matching: Vec<_> = processes
            .iter()
            .filter(|p| leader_matches(p, &socket, &recorded_pids))
            .collect();
        let bindings: Vec<_> = keys
            .iter()
            .filter_map(|k| grok_leader::member_from_key(k))
            .collect();
        let owner = if bindings.is_empty() {
            json!(UNKNOWN)
        } else {
            json!(bindings
                .iter()
                .map(|(t, m)| format!("{t}.{m}"))
                .collect::<Vec<_>>()
                .join(","))
        };
        let roster_present = bindings.iter().any(|(team, member)| {
            entries.iter().any(|e| {
                e.get("team").and_then(Value::as_str) == Some(team)
                    && e.get("members")
                        .and_then(Value::as_array)
                        .is_some_and(|ms| ms.iter().any(|m| m["name"] == *member))
            })
        });
        let alive = grok_leader::probe_socket(&socket);
        let candidates: Vec<_> = if matching.is_empty() {
            vec![None]
        } else {
            matching.into_iter().map(Some).collect()
        };
        for process in candidates {
            if let Some(process) = process {
                listed_leaders.insert(process.pid);
            }
            let mut item = row("grokLeader", owner.clone(), process);
            item["key"] = json!(keys.iter().cloned().collect::<Vec<_>>().join(","));
            item["socket"] = json!(socket);
            item["alive"] = if process.is_some() {
                json!(true)
            } else if alive {
                json!(UNKNOWN)
            } else {
                json!(false)
            };
            item["socketAlive"] = json!(alive);
            item["rosterPresent"] = if bindings.is_empty() {
                json!(UNKNOWN)
            } else {
                json!(roster_present)
            };
            rows.push(item);
        }
    }
    for process in processes
        .iter()
        .filter(|p| is_grok_leader(p) && !listed_leaders.contains(&p.pid))
    {
        let mut item = row("grokLeader", json!(UNKNOWN), Some(process));
        item["key"] = json!(UNKNOWN);
        item["socket"] = json!(UNKNOWN);
        item["alive"] = json!(true);
        item["socketAlive"] = json!(UNKNOWN);
        item["rosterPresent"] = json!(UNKNOWN);
        rows.push(item);
    }

    let ledger = claude_bg::list_jobs("claude");
    let mut jobs = BTreeSet::new();
    for entry in &entries {
        for member in entry
            .get("members")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if member["cli"] == "claude" {
                if let Some(id) = member["sessionId"].as_str().filter(|id| !id.is_empty()) {
                    jobs.insert(id.to_string());
                }
            }
        }
    }
    // A desktop/interactive Claude session is not a background job. Only
    // durable job ids and ids backed by a pane job record belong here.
    let recorded: BTreeSet<_> = claude_bg::list_recorded_panes()
        .iter()
        .filter_map(|p| claude_bg::read_pane_job(p).map(|r| r.job_id))
        .collect();
    jobs.retain(|id| recorded.contains(id) || claude_bg::looks_like_job_id(id));
    jobs.extend(recorded);
    for job in ledger.iter().flatten() {
        if let Some(id) = job.get("id").and_then(Value::as_str) {
            jobs.insert(id.to_string());
        }
    }
    for id in jobs {
        let engine = claude_bg::engine_session_for_job(&id);
        let process = engine
            .as_ref()
            .and_then(|e| processes.iter().find(|p| p.pid == i64::from(e.pid)));
        let in_ledger = ledger.as_ref().is_some_and(|jobs| {
            jobs.iter()
                .any(|j| j.get("id").and_then(Value::as_str) == Some(&id))
        });
        let mut item = row("claudeJob", owners(&entries, "claude", &id), process);
        if let Some(engine) = &engine {
            item["pid"] = json!(engine.pid);
        }
        item["jobId"] = json!(id);
        item["state"] = json!(job_state(engine.is_some(), in_ledger, ledger.is_some()));
        rows.push(item);
    }
    for entry in &entries {
        let team = string(entry, "team");
        let workspace = string(entry, "workspace");
        let mut item = row("team", json!(team), None);
        item["team"] = json!(team);
        item["workspace"] = json!(workspace);
        item["memberCount"] = entry
            .get("members")
            .and_then(Value::as_array)
            .map(|ms| json!(ms.len()))
            .unwrap_or(json!(UNKNOWN));
        item["hivedPresent"] = if workspace == UNKNOWN {
            json!(UNKNOWN)
        } else {
            json!(hived_teams.contains(&(team.clone(), workspace)))
        };
        item["displayPresent"] = display_present(panes.as_deref(), server, &team);
        item["orchSession"] = json!(orch_state(entry));
        rows.push(item);
    }
    Ok(rows)
}

fn render_json(rows: &[Value]) -> String {
    format!(
        "[\n{}\n]\n",
        rows.iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join(",\n")
    )
}

fn render_table(rows: &[Value]) -> String {
    let columns = ["kind", "logicalOwner", "pid", "ppid", "ageSeconds"];
    let mut cells: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            columns
                .iter()
                .map(|k| text(&field(r.get(k))).replace(['\n', '\r', '\t'], " "))
                .collect()
        })
        .collect();
    cells.insert(
        0,
        vec![
            "KIND".into(),
            "LOGICAL OWNER".into(),
            "PID".into(),
            "OS PARENT".into(),
            "AGE(s)".into(),
        ],
    );
    let widths: Vec<_> = (0..columns.len())
        .map(|i| cells.iter().map(|r| r[i].len()).max().unwrap_or(0))
        .collect();
    let mut lines = vec![];
    for (index, cells) in cells.iter().enumerate() {
        let mut line = cells
            .iter()
            .zip(&widths)
            .map(|(c, w)| format!("{c:<w$}"))
            .collect::<Vec<_>>()
            .join("  ");
        if index == 0 {
            line.push_str("  DETAILS");
        } else if let Some(map) = rows[index - 1].as_object() {
            for (key, value) in map {
                if !columns.contains(&key.as_str()) {
                    line.push_str(&format!(
                        "  {key}={}",
                        text(value).replace(['\n', '\r', '\t'], " ")
                    ));
                }
            }
        }
        lines.push(line);
    }
    format!("{}\n", lines.join("\n"))
}

pub(super) fn run(json: bool) {
    match collect() {
        Ok(rows) => print!(
            "{}",
            if json {
                render_json(&rows)
            } else {
                render_table(&rows)
            }
        ),
        Err(error) => super::util::fail(&error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_orphan_client_keeps_logical_owner_separate_from_os_parent() {
        let processes = parse_processes("PID PPID STARTED COMMAND\n123 1 Fri Sep 11 10:00:00 2026 /usr/bin/tmux -C attach -t cedar", 2_000_000_000);
        let item = client_row(&processes[0], &processes, &[]).unwrap();
        assert_eq!(item["orphan"], true);
        assert_eq!(item["logicalOwner"], "cedar");
        assert_eq!(item["ppid"], 1);
        let output = render_json(&[item.clone()]);
        assert_eq!(serde_json::from_str::<Value>(&output).unwrap()[0], item);
        assert_eq!(output.lines().count(), 3);
        let table = render_table(&[item]);
        assert!(table.contains("OS PARENT"));
        assert!(table.contains("orphan=true"));
    }

    #[test]
    fn test_hived_hash_mismatch_requires_matching_socket_pid() {
        let reply = json!({"hived":{"pid":42}, "buildHash":"old"});
        assert_eq!(hash_matches(reply.as_object(), 42, "new"), false);
        assert_eq!(hash_matches(reply.as_object(), 43, "new"), UNKNOWN);
        assert_eq!(hash_matches(None, 42, "new"), UNKNOWN);
        assert_eq!(hash_matches(reply.as_object(), 42, UNKNOWN), UNKNOWN);
    }

    #[test]
    fn test_asleep_job_is_distinct_from_gone_and_failed_ledger() {
        assert_eq!(job_state(false, true, true), "asleep");
        assert_eq!(job_state(false, false, true), "gone");
        assert_eq!(job_state(false, false, false), UNKNOWN);
        assert_eq!(job_state(true, false, false), "alive");
    }

    #[test]
    fn test_no_server_and_unknown_observations_stay_distinct() {
        assert_eq!(display_present(None, "no-server", "cedar"), false);
        assert_eq!(display_present(None, "unknown", "cedar"), UNKNOWN);
        let item = row("claudeJob", json!(UNKNOWN), None);
        let parsed: Value = serde_json::from_str(&render_json(&[item])).unwrap();
        for key in ["pid", "ppid", "ageSeconds", "startedAt"] {
            assert_eq!(parsed[0][key], UNKNOWN);
        }
        assert_eq!(age("invalid", 2_000_000_000), UNKNOWN);
    }

    #[test]
    fn test_registry_fixture_preserves_workspace_and_job_ownership() {
        let root = tempfile::tempdir().unwrap();
        let mut env = crate::testenv::EnvGuard::new();
        env.set("HIVE_HOME", root.path());
        let dir = root.path().join("teams/cedar");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("team.json"), json!({"team":"cedar", "workspace":"/tmp/a  workspace", "members":[{"name":"worker", "cli":"claude", "sessionId":"abcdef12"}]}).to_string()).unwrap();
        let entries = registry::list_entries();
        let ps = parse_processes("42 1 Fri Sep 11 10:00:00 2026 /usr/bin/hive --hived /tmp/a  workspace cedar cedar:0 @1", 2_000_000_000);
        assert_eq!(
            hived_args(&ps[0], &entries),
            Some(("/tmp/a  workspace".into(), "cedar".into()))
        );
        assert_eq!(owners(&entries, "claude", "abcdef12"), "cedar.worker");
    }

    #[test]
    fn test_desktop_absence_requires_complete_readable_search() {
        let root = tempfile::tempdir().unwrap();
        let org = root.path().join("account/org");
        fs::create_dir_all(&org).unwrap();
        assert_eq!(desktop_absent(root.path(), "local_missing"), Some(true));
        fs::write(org.join("local_missing.json"), "broken").unwrap();
        assert_eq!(desktop_absent(root.path(), "local_missing"), Some(false));
        assert_eq!(
            desktop_absent(&org.join("local_missing.json"), "local_missing"),
            None
        );
    }

    #[test]
    fn test_hived_argv_ignores_shell_commands_mentioning_daemons() {
        let ps = parse_processes(
            "42 1 Fri Sep 11 10:00:00 2026 /bin/sh -c hive --hived /tmp/ws cedar",
            2_000_000_000,
        );
        assert_eq!(hived_args(&ps[0], &[]), None);
    }

    #[test]
    fn test_grok_recorded_pid_cannot_override_explicit_other_socket() {
        let ps = parse_processes("42 1 Fri Sep 11 10:00:00 2026 grok agent leader --leader-socket /custom/hive/l-abc.sock", 2_000_000_000);
        assert!(leader_matches(
            &ps[0],
            Path::new("/custom/hive/l-abc.sock"),
            &[]
        ));
        assert!(!leader_matches(
            &ps[0],
            Path::new("/default/hive/l-abc.sock"),
            &[42]
        ));
    }

    #[test]
    fn test_collect_uses_one_process_snapshot_and_one_ledger_without_writes() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let mut env = crate::testenv::EnvGuard::new();
        let homes = root.path().join("homes");
        env.set("HOME", &homes);
        env.set("HIVE_HOME", homes.join("hive"));
        env.set("CODEX_HOME", homes.join("codex"));
        env.set("GROK_HOME", homes.join("grok"));
        env.set("CLAUDE_CONFIG_DIR", homes.join("claude"));
        let bin = root.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        env.set("PATH", &bin);
        env.set("PS_TEST_TRACE", root.path().join("trace"));
        for (name, body) in [
            ("ps", "printf 'ps %s\\n' \"$*\" >> \"$PS_TEST_TRACE\"\nprintf '%s\\n' '42 1 Fri Sep 11 10:00:00 2026 tmux -C attach -t cedar'"),
            ("claude", "printf 'claude %s\\n' \"$*\" >> \"$PS_TEST_TRACE\"\nprintf '%s\\n' '[{\"id\":\"abcdef12\"},{\"id\":\"abcdef13\"}]'"),
            ("tmux", "printf 'tmux %s\\n' \"$*\" >> \"$PS_TEST_TRACE\"\nprintf '%s\\n' 'no server running on fixture' >&2\nexit 1"),
        ] {
            let path = bin.join(name);
            fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let rows = collect().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["orphan"], true);
        assert!(rows[1..].iter().all(|r| r["state"] == "asleep"));
        assert!(!homes.exists(), "observation created a home directory");
        let trace = fs::read_to_string(root.path().join("trace")).unwrap();
        assert_eq!(
            trace
                .lines()
                .filter(|l| l.starts_with("ps "))
                .collect::<Vec<_>>(),
            ["ps -axo pid,ppid,lstart,command"]
        );
        assert_eq!(
            trace
                .lines()
                .filter(|l| l.starts_with("claude "))
                .collect::<Vec<_>>(),
            ["claude agents --json --all"]
        );
        assert_eq!(trace.lines().count(), 3);
    }
}
