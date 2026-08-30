//! Behavioral tests for the materialized flow pylib client
//! (`assets/pylib/hive/flow.py`): a tiny flow script run under python3
//! against a shimmed $HIVE_BIN that records the op protocol and answers
//! canned JSON. Asserts the client's subprocess protocol (op order and
//! payloads) and the byte-level `[flow]` log lines. A second test drives
//! the real binary's hidden `flow-op` dispatch (guard + unknown op),
//! which never touches tmux or a team.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

const PYLIB_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/pylib");

/// Fake `hive` binary: logs every `flow-op` call as JSONL, answers canned
/// results, and emits one `[flow] … retry …` progress line on spawn to
/// prove the client streams op-side logs through in order.
const SHIM: &str = r#"#!/usr/bin/env python3
import json, os, sys
from pathlib import Path

log = Path(os.environ["SHIM_LOG"])
assert sys.argv[1] == "flow-op", sys.argv
op = sys.argv[2]
args = json.loads(sys.argv[3]) if len(sys.argv) > 3 else {}
prior = log.read_text().splitlines() if log.exists() else []
with log.open("a") as fh:
    fh.write(json.dumps({"op": op, "args": args}) + "\n")


def ok(**fields):
    print(json.dumps({"ok": True, **fields}))


if op == "context":
    ok(teamName="t-x", workspace=os.environ["SHIM_WS"])
elif op == "spawn":
    print(f"[flow] {args['name']} spawn failed (mint refused); retry 2/3")
    ok(pane="%7", cli=args.get("cli") or "claude")
elif op == "ready":
    ok()
elif op == "dispatch":
    n = sum(1 for line in prior if json.loads(line)["op"] == "dispatch") + 1
    ok(msgId=f"m{n}")
elif op == "wait-reply":
    ok(body="done, see file", artifact="/tmp/f.md", msgId="r1")
elif op == "kill":
    ok()
else:
    print(json.dumps({"ok": False, "error": f"unknown flow op '{op}'"}))
    sys.exit(1)
"#;

const SCRIPT: &str = r#"from hive.flow import FLOW_SENDER, FlowError, agent, parallel

assert FLOW_SENDER == "flow.run"
m = agent("do the thing\nfully", name="impl")
print("SUMMARY", m.summary, flush=True)
print("PANE", m.pane, flush=True)
m.ask("short follow-up")
m.kill()
try:
    m.ask("again")
except FlowError as exc:
    print("DEAD", exc, flush=True)


def boom():
    raise FlowError("boom")


try:
    parallel(boom, lambda: 42)
except FlowError as exc:
    print("PARALLEL_ERR", exc, flush=True)
"#;

fn write_executable(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

#[test]
fn test_flow_client_speaks_the_op_protocol_and_logs() {
    let tmp = tempfile::TempDir::new().unwrap();
    let shim = tmp.path().join("hive-shim");
    write_executable(&shim, SHIM);
    let ws = tmp.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    let script = tmp.path().join("plan.py");
    fs::write(&script, SCRIPT).unwrap();
    let log = tmp.path().join("ops.jsonl");

    let out = Command::new("python3")
        .arg(&script)
        .env("PYTHONPATH", PYLIB_DIR)
        .env("HIVE_BIN", &shim)
        .env("SHIM_LOG", &log)
        .env("SHIM_WS", &ws)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");

    let expected = "\
[flow] impl spawn failed (mint refused); retry 2/3
[flow] impl spawned in %7
[flow] impl dispatched (m1); waiting for reply…
[flow] impl replied (r1)
SUMMARY done, see file
PANE %7
[flow] impl asked (m2); waiting…
[flow] impl answered (r1)
[flow] impl retired
DEAD member 'impl' was killed; spawn a new one
PARALLEL_ERR boom
";
    assert_eq!(stdout, expected);

    let ops: Vec<Value> = fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let names: Vec<&str> = ops.iter().map(|o| o["op"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "spawn",
            "ready",
            "context",
            "dispatch",
            "wait-reply",
            "dispatch",
            "wait-reply",
            "kill"
        ]
    );
    assert_eq!(
        ops[0]["args"],
        serde_json::json!({"name": "impl", "cli": null, "model": ""})
    );
    assert_eq!(
        ops[1]["args"],
        serde_json::json!({"name": "impl", "cli": "claude"})
    );
    // the task prompt rides an artifact under <workspace>/artifacts/tasks/
    let dispatch = &ops[3]["args"];
    assert_eq!(
        dispatch["body"].as_str().unwrap(),
        "flow-mailbox dispatch: impl.md (not a member; hive send flow.run, then stop)"
    );
    let artifact = dispatch["artifact"].as_str().unwrap();
    assert!(artifact.ends_with("artifacts/tasks/impl.md"), "{artifact}");
    assert_eq!(fs::read_to_string(artifact).unwrap(), "do the thing\nfully");
    assert_eq!(
        ops[4]["args"],
        serde_json::json!({"name": "impl", "msgId": "m1"})
    );
    // short single-line follow-up rides the body, no artifact file
    assert_eq!(
        ops[5]["args"],
        serde_json::json!({"name": "impl", "body": "short follow-up", "artifact": ""})
    );
    assert_eq!(ops[7]["args"], serde_json::json!({"name": "impl"}));
}

#[test]
fn test_flow_op_guard_and_unknown_op_through_the_real_binary() {
    let tmp = tempfile::TempDir::new().unwrap();
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_hive"))
            .args(args)
            .env("HIVE_HOME", tmp.path())
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output()
            .unwrap();
        let last = String::from_utf8_lossy(&out.stdout)
            .lines()
            .last()
            .unwrap_or_default()
            .to_string();
        (out.status.code(), serde_json::from_str::<Value>(&last).unwrap())
    };

    // the flow/flow.* name guard fires before any team/tmux resolution
    let (code, v) = run(&["flow-op", "spawn", r#"{"name":"flow.run","model":""}"#]);
    assert_eq!(code, Some(1));
    assert_eq!(v["ok"], Value::Bool(false));
    assert!(
        v["error"].as_str().unwrap().contains("mailbox address kind"),
        "{v}"
    );

    let (code, v) = run(&["flow-op", "bogus", "{}"]);
    assert_eq!(code, Some(1));
    assert_eq!(v["error"].as_str().unwrap(), "unknown flow op 'bogus'");
}
