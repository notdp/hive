//! Behavioral tests for the embedded `cvim-command` bash asset, driven the
//! way `tests/unit/test_cvim_command.py` drives the Python-era copy: a fake
//! tmux/ps/editor on PATH, the real hive binary as $HIVE_BIN for the hidden
//! `cvim-*` helper subcommands.
//!
//! `test_popup_schedules_post_after_popup_exits` is the AGENTS.md-pinned
//! regression guard: `run-shell` must be scheduled only after the popup has
//! torn down, or the returned edit payload is swallowed.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

const CVIM_COMMAND: &str = include_str!("../assets/cvim/bin/cvim-command");
const MENU_VIM: &str = include_str!("../assets/cvim/resources/menu.vim");
const PROTOCOL_JSON: &str = include_str!("../assets/cvim/resources/cvim_edit_protocol.json");

/// Same fake tmux as tests/unit/test_cvim_command.py::_write_fake_tmux.
const FAKE_TMUX: &str = r##"#!/usr/bin/env python3
import json
import os
import subprocess
import sys
import time
from pathlib import Path

state = json.loads(os.environ["FAKE_TMUX_STATE"])
log_path = Path(os.environ["FAKE_TMUX_LOG"])
actions_path = Path(os.environ["FAKE_TMUX_ACTIONS"]) if os.environ.get("FAKE_TMUX_ACTIONS") else None
args = sys.argv[1:]
cmd = args[0]
panes = {pane["id"]: pane for pane in state["panes"]}


def _print(value):
    sys.stdout.write(f"{value}\n")


def _append(event):
    if actions_path is None:
        return
    event.setdefault("ts", time.monotonic())
    with actions_path.open("a") as fh:
        fh.write(json.dumps(event) + "\n")


def _target_pane():
    if "-t" in args:
        return args[args.index("-t") + 1]
    return state["current_pane"]


if cmd == "display-message":
    if "-c" in args:
        fmt = args[-1]
        if fmt == "#{client_width}":
            _print(state["client_width"])
            raise SystemExit(0)
        if fmt == "#{client_height}":
            _print(state["client_height"])
            raise SystemExit(0)
        raise SystemExit(1)

    pane = panes[_target_pane()]
    fmt = args[-1]
    values = {
        "#{pane_id}": pane["id"],
        "#{pane_current_path}": pane.get("cwd", "/repo"),
        "#{pane_current_command}": pane.get("command", "claude"),
        "#{pane_title}": pane.get("title", ""),
        "#{session_id}": state.get("session_id", "$1"),
        "#{window_id}": state.get("window_id", "@1"),
        "#{pane_pid}": pane.get("pid", 1234),
        "#{pane_tty}": pane.get("tty", "/dev/ttys001"),
        "#{client_tty}": pane.get("client_tty", "/dev/ttys010"),
        "#{client_pid}": pane.get("client_pid", 4321),
        "#{@hive-workspace}": state.get("workspace", ""),
        "#{pane_left}": pane["left"],
        "#{pane_top}": pane["top"],
        "#{pane_width}": pane["width"],
        "#{pane_height}": pane["height"],
    }
    value = values.get(fmt)
    if value is None:
        raise SystemExit(1)
    _print(value)
    raise SystemExit(0)

if cmd == "list-commands":
    _print("display-popup")
    raise SystemExit(0)

if cmd == "list-panes":
    fmt = args[args.index("-F") + 1] if "-F" in args else ""
    for pane in state["panes"]:
        if fmt == "#{pane_id} #{pane_left} #{pane_top} #{pane_width} #{pane_height}":
            _print(f'{pane["id"]} {pane["left"]} {pane["top"]} {pane["width"]} {pane["height"]}')
        else:
            _print(pane["id"])
    raise SystemExit(0)

if cmd == "display-popup":
    event = {"cmd": cmd, "args": args}
    log_path.write_text(json.dumps(event))
    _append(event)
    if os.environ.get("FAKE_TMUX_EXEC_POPUP") == "1":
        popup_cmd = args[args.index("-E") + 1]
        popup_env = os.environ.copy()
        if os.environ.get("FAKE_TMUX_MARK_POPUP_CONTEXT") == "1":
            popup_env["FAKE_TMUX_IN_POPUP"] = "1"
        subprocess.run(popup_cmd, shell=True, check=True, env=popup_env)
    raise SystemExit(0)

if cmd == "capture-pane":
    event = {"cmd": cmd, "args": args}
    _append(event)
    sys.stdout.write(os.environ.get("FAKE_TMUX_CAPTURE_PANE_TEXT", ""))
    raise SystemExit(0)

if cmd in {"send-keys", "load-buffer", "paste-buffer"}:
    event = {"cmd": cmd, "args": args}
    _append(event)
    raise SystemExit(0)

if cmd == "run-shell":
    event = {"cmd": cmd, "args": args}
    if os.environ.get("FAKE_TMUX_DROP_RUN_SHELL_IN_POPUP") == "1" and os.environ.get("FAKE_TMUX_IN_POPUP") == "1":
        event["dropped"] = True
        _append(event)
        raise SystemExit(0)
    _append(event)
    subprocess.run(args[-1], shell=True, check=True, env=os.environ.copy())
    raise SystemExit(0)

raise SystemExit(1)
"##;

/// Fake ps: default answer is a plain interactive claude on the pane tty.
const FAKE_PS: &str = r#"#!/usr/bin/env python3
import os
import sys

out = os.environ.get("FAKE_PS_OUTPUT", "456 claude claude")
if out:
    sys.stdout.write(out.rstrip("\n") + "\n")
"#;

const FAKE_EDITOR: &str = r#"#!/usr/bin/env python3
import os
import sys
from pathlib import Path

path = Path(sys.argv[1])
append_text = os.environ.get("FAKE_EDITOR_APPEND_TEXT")
if append_text:
    existing = path.read_text() if path.exists() else ""
    with path.open("a") as fh:
        if existing and not existing.endswith("\n"):
            fh.write("\n")
        fh.write(append_text)
"#;

/// Capturing fake vim for menu-mode assertions (log path via CAPTURE_LOG).
const FAKE_CAPTURING_VIM: &str = r#"#!/usr/bin/env python3
import json
import os
import sys

log_path = os.environ["CAPTURE_LOG"]
menu_json = os.environ.get("CVIM_MENU_JSON", "")
menu = None
if menu_json and os.path.isfile(menu_json):
    with open(menu_json) as fh:
        menu = json.load(fh)
msg_file = sys.argv[-1] if len(sys.argv) > 1 else ""
msg_content = ""
if msg_file and os.path.isfile(msg_file):
    with open(msg_file) as fh:
        msg_content = fh.read()
record = {
    "argv": sys.argv,
    "env": {k: os.environ.get(k, "") for k in (
        "CVIM_MENU_JSON", "CVIM_SEEDS_DIR", "CVIM_MSG_FILE",
        "CVIM_ORIG_FILE", "CVIM_OFFSET_FILE", "CVIM_MENU_SELECTED_FILE",
    )},
    "menu": menu,
    "seeds": sorted(os.listdir(os.environ.get("CVIM_SEEDS_DIR", "") or "."))
        if os.environ.get("CVIM_SEEDS_DIR") and os.path.isdir(os.environ["CVIM_SEEDS_DIR"]) else [],
    "msg_content": msg_content,
}
with open(log_path, "w") as fh:
    json.dump(record, fh)
"#;

fn write_script(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

/// Materialize the embedded asset tree into `dir` and return cvim-command.
fn write_bundle(dir: &Path) -> PathBuf {
    fs::create_dir_all(dir.join("bin")).unwrap();
    fs::create_dir_all(dir.join("resources")).unwrap();
    write_script(&dir.join("bin/cvim-command"), CVIM_COMMAND);
    fs::write(dir.join("resources/menu.vim"), MENU_VIM).unwrap();
    fs::write(dir.join("resources/cvim_edit_protocol.json"), PROTOCOL_JSON).unwrap();
    dir.join("bin/cvim-command")
}

struct Harness {
    tmp: tempfile::TempDir,
    command: PathBuf,
    actions_path: PathBuf,
}

impl Harness {
    fn new() -> Harness {
        let tmp = tempfile::TempDir::new().unwrap();
        write_script(&tmp.path().join("tmux"), FAKE_TMUX);
        write_script(&tmp.path().join("ps"), FAKE_PS);
        let command = write_bundle(&tmp.path().join("cvim-bundle"));
        let actions_path = tmp.path().join("tmux-actions.jsonl");
        Harness {
            tmp,
            command,
            actions_path,
        }
    }

    fn path(&self) -> &Path {
        self.tmp.path()
    }

    /// Run `bash cvim-command vim` with one 200x100 pane; extra env on top of
    /// the isolated defaults (delays zeroed unless `default_delays`).
    fn run(&self, default_delays: bool, extra_env: &[(&str, String)]) {
        let state = json!({
            "current_pane": "%1",
            "client_width": 200,
            "client_height": 100,
            "panes": [{"id": "%1", "left": 0, "top": 0, "width": 200, "height": 100}],
        });
        let editor = self.path().join("fake-editor");
        if !editor.exists() {
            write_script(&editor, FAKE_EDITOR);
        }
        let mut cmd = Command::new("bash");
        cmd.arg(&self.command)
            .arg("vim")
            .current_dir(self.path())
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.path().display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env("TMUX", "/tmp/tmux-test")
            .env("TMUX_PANE", "%1")
            .env("CVIM_SEED_MODE", "blank")
            .env("CVIM_OUTPUT_MODE", "text")
            .env("CVIM_EDITOR", &editor)
            .env("HIVE_BIN", env!("CARGO_BIN_EXE_hive"))
            .env("HIVE_HOME", self.path().join("hive-home"))
            .env("XDG_CACHE_HOME", self.path().join("cache"))
            .env("CLAUDE_HOME", self.path().join("claude-home"))
            .env("CODEX_HOME", self.path().join("codex-home"))
            .env("GROK_HOME", self.path().join("grok-home"))
            .env("FAKE_TMUX_STATE", state.to_string())
            .env("FAKE_TMUX_LOG", self.path().join("tmux-log.json"))
            .env("FAKE_TMUX_ACTIONS", &self.actions_path)
            .env("FAKE_TMUX_EXEC_POPUP", "1");
        if !default_delays {
            cmd.env("CVIM_PASTE_DELAY", "0")
                .env("CVIM_INTERRUPT_SETTLE_DELAY", "0")
                .env("CVIM_SUBMIT_DELAY", "0");
        }
        for (key, value) in extra_env {
            cmd.env(key, value);
        }
        let status = cmd.status().unwrap();
        assert!(status.success(), "cvim-command exited nonzero");
    }

    fn actions(&self) -> Vec<Value> {
        let content = fs::read_to_string(&self.actions_path).unwrap_or_default();
        content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

fn cmd_of(event: &Value) -> &str {
    event["cmd"].as_str().unwrap_or("")
}

fn last_arg(event: &Value) -> &str {
    event["args"]
        .as_array()
        .and_then(|a| a.last())
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// AGENTS.md-pinned: run-shell is scheduled only after popup teardown; a
/// run-shell issued from inside the popup context would be swallowed.
#[test]
fn test_popup_schedules_post_after_popup_exits() {
    let harness = Harness::new();
    harness.run(
        true,
        &[
            ("FAKE_EDITOR_APPEND_TEXT", "new line added".to_string()),
            (
                "FAKE_TMUX_CAPTURE_PANE_TEXT",
                "ready for input\n> [<comment on=\"previous_reply\"> pasted]".to_string(),
            ),
            ("FAKE_TMUX_MARK_POPUP_CONTEXT", "1".to_string()),
            ("FAKE_TMUX_DROP_RUN_SHELL_IN_POPUP", "1".to_string()),
        ],
    );
    let actions = harness.actions();
    let run_shell: Vec<&Value> = actions.iter().filter(|e| cmd_of(e) == "run-shell").collect();
    assert_eq!(run_shell.len(), 1);
    assert!(!run_shell[0]["dropped"].as_bool().unwrap_or(false));
    assert!(actions.iter().any(|e| cmd_of(e) == "paste-buffer"));
    assert!(actions
        .iter()
        .any(|e| cmd_of(e) == "send-keys" && last_arg(e) == "Enter"));
}

#[test]
fn test_edited_save_interrupts_before_paste_and_submits() {
    let harness = Harness::new();
    harness.run(
        false,
        &[
            ("FAKE_EDITOR_APPEND_TEXT", "new line added".to_string()),
            (
                "FAKE_TMUX_CAPTURE_PANE_TEXT",
                "ready for input\n> [<comment on=\"previous_reply\"> pasted]".to_string(),
            ),
        ],
    );
    let actions = harness.actions();
    let escape_indexes: Vec<usize> = actions
        .iter()
        .enumerate()
        .filter(|(_, e)| cmd_of(e) == "send-keys" && last_arg(e) == "Escape")
        .map(|(i, _)| i)
        .collect();
    let paste_index = actions
        .iter()
        .position(|e| cmd_of(e) == "paste-buffer")
        .expect("paste-buffer event");
    let enter_index = actions
        .iter()
        .position(|e| cmd_of(e) == "send-keys" && last_arg(e) == "Enter")
        .expect("Enter event");
    assert_eq!(escape_indexes.len(), 1);
    assert!(actions.iter().any(|e| cmd_of(e) == "load-buffer"));
    assert!(escape_indexes[0] < paste_index && paste_index < enter_index);
}

#[test]
fn test_unedited_save_interrupts_without_paste_or_submit() {
    let harness = Harness::new();
    harness.run(false, &[]);
    let actions = harness.actions();
    let escapes = actions
        .iter()
        .filter(|e| cmd_of(e) == "send-keys" && last_arg(e) == "Escape")
        .count();
    assert_eq!(escapes, 1);
    assert!(!actions
        .iter()
        .any(|e| matches!(cmd_of(e), "load-buffer" | "paste-buffer")));
    assert!(!actions
        .iter()
        .any(|e| cmd_of(e) == "send-keys" && last_arg(e) == "Enter"));
}

/// No job record and the claude on the tty is an attach viewer: the sendback
/// refuses rather than typing into a stranger's composer.
#[test]
fn test_claude_post_refuses_when_the_pane_only_holds_an_attach_viewer() {
    let harness = Harness::new();
    harness.run(
        false,
        &[
            ("FAKE_EDITOR_APPEND_TEXT", "new line added".to_string()),
            (
                "FAKE_PS_OUTPUT",
                "456 claude claude attach beef5678".to_string(),
            ),
        ],
    );
    let actions = harness.actions();
    assert!(!actions
        .iter()
        .any(|e| matches!(cmd_of(e), "send-keys" | "load-buffer" | "paste-buffer")));
}

/// Menu mode: cvim-session (shimmed), cvim-list and cvim-seed wire the
/// picker assets into the popup, with the newest message pre-seeded.
#[test]
fn test_cvim_menu_mode_activates_with_session_seed_and_no_offset() {
    let harness = Harness::new();
    let transcript = harness.path().join("session.jsonl");
    let rows = [
        json!({"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "answer A"}]}}),
        json!({"type": "message", "message": {"role": "user", "content": [{"type": "text", "text": "u"}]}}),
        json!({"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "answer B"}]}}),
    ];
    let content: String = rows.iter().map(|r| format!("{r}\n")).collect();
    fs::write(&transcript, content).unwrap();

    // HIVE_BIN wrapper: cvim-session answers with the fixture transcript
    // (the Python test overwrote the cvim-session script the same way);
    // everything else goes to the real binary.
    let wrapper = harness.path().join("hive-wrapper");
    write_script(
        &wrapper,
        &format!(
            "#!/usr/bin/env bash\n\
             if [[ \"$1\" == \"cvim-session\" ]]; then printf '%s\\n' {transcript}; exit 0; fi\n\
             exec {hive} \"$@\"\n",
            transcript = transcript.display(),
            hive = env!("CARGO_BIN_EXE_hive"),
        ),
    );
    let editor_log = harness.path().join("editor.json");
    let fake_vim = harness.path().join("fake-vim");
    write_script(&fake_vim, FAKE_CAPTURING_VIM);

    harness.run(
        false,
        &[
            ("CVIM_SEED_MODE", "session".to_string()),
            ("CVIM_EDITOR", fake_vim.display().to_string()),
            ("HIVE_BIN", wrapper.display().to_string()),
            ("CAPTURE_LOG", editor_log.display().to_string()),
        ],
    );

    let record: Value =
        serde_json::from_str(&fs::read_to_string(&editor_log).unwrap()).unwrap();
    let argv: Vec<&str> = record["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(argv.contains(&"-S"));
    assert!(argv.iter().any(|a| a.ends_with("menu.vim")));
    let env = &record["env"];
    assert!(env["CVIM_MENU_JSON"].as_str().unwrap().ends_with("/menu.json"));
    assert!(env["CVIM_SEEDS_DIR"].as_str().unwrap().ends_with("/seeds"));
    assert!(env["CVIM_MSG_FILE"].as_str().unwrap().ends_with("/message.md"));
    assert!(env["CVIM_ORIG_FILE"].as_str().unwrap().ends_with("/original.md"));
    assert!(env["CVIM_OFFSET_FILE"].as_str().unwrap().ends_with("/offset"));
    assert!(env["CVIM_MENU_SELECTED_FILE"]
        .as_str()
        .unwrap()
        .ends_with("/menu_selected"));
    let menu = record["menu"].as_array().expect("menu json");
    let offsets: Vec<i64> = menu.iter().map(|m| m["offset"].as_i64().unwrap()).collect();
    assert_eq!(offsets, vec![0, 1]);
    assert!(menu[0]["label"].as_str().unwrap().contains("answer B"));
    assert!(menu[1]["label"].as_str().unwrap().contains("answer A"));
    let seeds: Vec<&str> = record["seeds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(seeds, vec!["0.md", "1.md"]);
    // menu mode pre-seeds msg_file with the newest assistant message so a
    // failed popup_menu render still leaves a usable buffer.
    assert!(record["msg_content"].as_str().unwrap().contains("answer B"));
}
