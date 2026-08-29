//! Notify UI: tmux window flash, terminal bell, and the pane-attention popup.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

#[cfg(test)]
use self::tests::fake_tmux as tmux;
use crate::notify_debug;
#[cfg(not(test))]
use crate::tmux;

pub const NOTIFY_TOKEN_OPTION: &str = "@hive-notify-token";
pub const ORIGINAL_NAME_OPTION: &str = "@hive-notify-original-name";
pub const ORIGINAL_NAME_KEY: &str = "hive-notify-original-name";
pub const HOOK_NAME_OPTION: &str = "@hive-notify-hook";
#[allow(dead_code)]
pub const HOOK_NAME_KEY: &str = "hive-notify-hook";
pub const ATTENTION_SCRIPT_OPTION: &str = "@hive-notify-attention";
pub const ATTENTION_SCRIPT_KEY: &str = "hive-notify-attention";
pub const PANE_NOTIFY_ACTIVE_KEY: &str = "hive-notify-active";
pub const FLASH_STYLE: &str = "reverse,bold";
pub const NOTIFY_BADGE: &str = "\u{1F916}";
// Use one stable high-index hook so each notify refreshes the same fast-path
// instead of installing per-notify hook/script pairs that can go stale.
pub const SELECT_HOOK_NAME: &str = "after-select-window[900001]";

pub const _PANE_ATTENTION_PYTHON: &str = r##"
from __future__ import annotations

import os
import shlex

from hive import tmux


POPUP_CODE = r"""
from __future__ import annotations

import os
import random
import sys
import time
import shutil

cols, rows = shutil.get_terminal_size((80, 24))
cols = max(50, cols)
rows = max(16, rows)
random.seed(4143)

agent = os.environ.get("HIVE_NOTIFY_AGENT", "").strip() or "target"
window_target = os.environ.get("HIVE_NOTIFY_WINDOW", "").strip() or "unknown"
pane_id = os.environ.get("HIVE_NOTIFY_PANE_ID", "").strip() or "unknown"
label = f"TARGET LOCKED: {agent.upper()}"
chars = "01ABCDEF/%#@{}[]"
cx, cy = cols // 2, rows // 2
SCAN_FRAMES = 14
SCAN_DELAY = 0.032
PULSE_FRAMES = 4
PULSE_DELAY = 0.055
FINAL_HOLD = 0.1
COLLAPSE_DELAY = 0.032


def clear() -> None:
    sys.stdout.write("\033[?25l\033[H\033[2J")


def at(y: int, x: int, text: str) -> None:
    if 0 <= y < rows and x < cols:
        sys.stdout.write(f"\033[{y + 1};{max(0, x) + 1}H{text[:max(0, cols - x)]}")


def corner(y: int, x: int, sx: int, sy: int, color: int = 46) -> None:
    glyph = "┌" if sx > 0 and sy > 0 else "┐" if sx < 0 and sy > 0 else "└" if sx > 0 else "┘"
    at(y, x, f"\033[38;5;{color};1m{glyph}\033[0m")
    at(y, x + sx, f"\033[38;5;{color};1m" + "━" * 8 + "\033[0m")
    for i in range(1, 5):
        at(y + sy * i, x, f"\033[38;5;{color};1m┃\033[0m")


for frame in range(SCAN_FRAMES):
    clear()
    t = frame / (SCAN_FRAMES - 1)
    ease = 1 - (1 - t) ** 3
    margin_x = int((cols // 2 - 18) * ease)
    margin_y = int((rows // 2 - 6) * ease)
    lx, rx = margin_x + 2, cols - margin_x - 3
    ty, by = margin_y + 1, rows - margin_y - 2
    corner(ty, lx, 1, 1)
    corner(ty, rx, -9, 1)
    corner(by, lx, 1, -1)
    corner(by, rx, -9, -1)
    for _ in range(10):
        text = "".join(random.choice(chars) for _ in range(random.randint(4, 12)))
        at(
            random.randint(max(0, ty - 2), min(rows - 1, by + 2)),
            random.randint(max(0, lx), max(0, rx - 12)),
            "\033[38;5;28m" + text + "\033[0m",
        )
    if frame >= SCAN_FRAMES // 2:
        scan = "SCAN " + "".join(random.choice(chars) for _ in range(12))
        at(cy, cx - len(scan) // 2, "\033[38;5;82m" + scan + "\033[0m")
    sys.stdout.flush()
    time.sleep(SCAN_DELAY)

for pulse in range(PULSE_FRAMES):
    clear()
    color = 220 if pulse % 2 == 0 else 46
    box_w = min(cols - 4, max(len(label) + 6, 28))
    x = max(0, cx - box_w // 2)
    inner_w = box_w - 2
    clipped_label = label[: max(0, inner_w - 4)]
    at(cy - 2, x, f"\033[38;5;{color};1m╔" + "═" * inner_w + "╗\033[0m")
    at(cy - 1, x, f"\033[38;5;{color};1m║" + " " * inner_w + "║\033[0m")
    left_pad = max(0, (inner_w - len(clipped_label)) // 2)
    right_pad = max(0, inner_w - left_pad - len(clipped_label))
    at(
        cy,
        x,
        f"\033[38;5;{color};1m║"
        + " " * left_pad
        + f"\033[48;5;{color}m\033[38;5;232;1m{clipped_label}\033[0m"
        + f"\033[38;5;{color};1m"
        + " " * right_pad
        + "║\033[0m",
    )
    at(cy + 1, x, f"\033[38;5;{color};1m║" + " " * inner_w + "║\033[0m")
    at(cy + 2, x, f"\033[38;5;{color};1m╚" + "═" * inner_w + "╝\033[0m")
    diagnostic = f"window={window_target} pane={pane_id}"
    at(cy + 4, cx - len(diagnostic) // 2, "\033[38;5;245m" + diagnostic + "\033[0m")
    sys.stdout.flush()
    time.sleep(PULSE_DELAY)

time.sleep(FINAL_HOLD)
for width in [40, 24, 10, 2]:
    clear()
    width = min(width, cols - 4)
    at(cy, cx - width // 2, "\033[38;5;46;1m" + "━" * width + "\033[0m")
    sys.stdout.flush()
    time.sleep(COLLAPSE_DELAY)

clear()
sys.stdout.write("\033[?25h")
sys.stdout.flush()
"""


def tmux_value(target: str, fmt: str) -> str:
    return tmux.display_value(target, fmt) or ""


pane = os.environ.get("HIVE_NOTIFY_PANE", "").strip()
client = os.environ.get("HIVE_NOTIFY_CLIENT", "").strip()
if not pane:
    raise SystemExit(0)

try:
    left_s, top_s, width_s, height_s = tmux_value(
        pane,
        "#{pane_left} #{pane_top} #{pane_width} #{pane_height}",
    ).split()
    left = int(left_s)
    top = int(top_s)
    width = int(width_s)
    height = int(height_s)
except Exception:
    raise SystemExit(0)

popup_w = width
popup_h = height
# Numeric tmux popup -y anchors the popup bottom edge; use tmux's
# pane-aware popup formats so a lower split starts at the target pane top.
x = "#{popup_pane_left}"
y = "#{popup_pane_top}"

agent = tmux_value(pane, "#{@hive-agent}") or "target"
window_target = tmux_value(pane, "#{session_name}:#{window_index}") or ""

payload = (
    "HIVE_NOTIFY_AGENT="
    + shlex.quote(agent)
    + " HIVE_NOTIFY_WINDOW="
    + shlex.quote(window_target)
    + " HIVE_NOTIFY_PANE_ID="
    + shlex.quote(pane)
    + " python3 - <<'PYPOPUP'\n"
    + POPUP_CODE
    + "\nPYPOPUP"
)

tmux.display_popup(
    pane,
    payload,
    client=client,
    x=x,
    y=y,
    width=str(popup_w),
    height=str(popup_h),
    borderless=True,
    close_on_exit=True,
)
"##;

/// Serialized form matches the Python `notify()` return dict exactly.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NotifyPayload {
    pub agent: String,
    #[serde(rename = "paneId")]
    pub pane_id: String,
    pub window: String,
    pub tab: String,
    pub message: String,
    #[serde(rename = "clientMode")]
    pub client_mode: String,
    pub surface: String,
    pub suppressed: bool,
    #[serde(rename = "suppressionReason", skip_serializing_if = "Option::is_none")]
    pub suppression_reason: Option<String>,
}

fn or_null(value: &str) -> Value {
    if value.is_empty() {
        Value::Null
    } else {
        Value::String(value.to_string())
    }
}

/// Python `shlex.quote` (POSIX single-quote quoting, ASCII-safe charset).
fn shlex_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    let safe = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if value.chars().all(safe) {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

// Python used sys.executable; the Rust binary has no interpreter path.
// ponytail: python3 stand-in keeps the select hook and attention script
// runnable against the installed Python hive package; the integration pass
// owns re-pointing these entrypoints at the Rust binary.
fn sys_executable() -> String {
    "python3".to_string()
}

fn _target_window_is_focused(session_name: &str, window_target: &str) -> bool {
    if session_name.is_empty() || window_target.is_empty() {
        return false;
    }
    match tmux::get_most_recent_client_window(Some(session_name)) {
        Some(active) => !active.is_empty() && active == window_target,
        None => false,
    }
}

fn _write_pane_attention_script(pane_id: &str, token: &str) -> anyhow::Result<PathBuf> {
    let content = format!(
        r#"#!/usr/bin/env bash
set -euo pipefail

QP={qp}
TOKEN={tok}
CLIENT="${{1:-}}"

cleanup() {{
  cur="$(tmux show-options -p -v -t "$QP" @{active_key} 2>/dev/null || echo '')"
  if [ "$cur" = "$TOKEN" ]; then
    tmux set-option -p -t "$QP" -u @{active_key} >/dev/null 2>&1 || true
  fi
  rm -f "$0"
}}
trap cleanup EXIT

tmux set-option -p -t "$QP" @{active_key} "$TOKEN" >/dev/null 2>&1 || true
HIVE_NOTIFY_PANE="$QP" HIVE_NOTIFY_CLIENT="$CLIENT" {exe} <<'PY'
{py}
PY

sleep 0.18
"#,
        qp = shlex_quote(pane_id),
        tok = shlex_quote(token),
        active_key = PANE_NOTIFY_ACTIVE_KEY,
        exe = shlex_quote(&sys_executable()),
        py = _PANE_ATTENTION_PYTHON,
    );
    let dir = std::env::temp_dir();
    // ponytail: NamedTemporaryFile stand-in — pid+nanos+attempt is unique
    // enough for one script per flash; revisit only if collisions appear.
    for attempt in 0..8 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = dir.join(format!(
            "hive-pane-attention-{}-{}-{}.sh",
            std::process::id(),
            nanos,
            attempt
        ));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(mut handle) => {
                handle.write_all(content.as_bytes())?;
                drop(handle);
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
                return Ok(path);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.into()),
        }
    }
    anyhow::bail!("could not create pane attention script")
}

fn _select_hook_command() -> String {
    // run-shell executes with the tmux server's environment, not this
    // process's: a source-checkout registration (PYTHONPATH=src) would
    // otherwise install a hook whose `-m hive.notify_ui` can never import
    // hive — the flash then sticks until the hived sweep.
    let pythonpath = std::env::var("PYTHONPATH").unwrap_or_default();
    let env_prefix = if pythonpath.is_empty() {
        String::new()
    } else {
        format!("PYTHONPATH={} ", shlex_quote(&pythonpath))
    };
    let cleanup_cmd = format!(
        "{}{} -m hive.notify_ui \
         --cleanup-selected '#{{session_name}}:#{{window_index}}' \
         --client '#{{client_tty}}'",
        env_prefix,
        shlex_quote(&sys_executable())
    );
    // This string is parsed by tmux's hook command parser, then by run-shell.
    // Keep the attached-client e2e test in sync if this quoting changes.
    let escaped = cleanup_cmd.replace('\\', "\\\\").replace('"', "\\\"");
    let run_cmd = format!("run-shell -b \"{}\"", escaped);
    format!(
        "if-shell -F '#{{?@{},1,0}}' {}",
        NOTIFY_TOKEN_OPTION.trim_start_matches('@'),
        shlex_quote(&run_cmd)
    )
}

pub fn ensure_notify_select_hook(session: &str) {
    if session.is_empty() {
        return;
    }
    let hook_command = _select_hook_command();
    let _ = tmux::_run(
        &[
            "set-hook",
            "-t",
            session,
            SELECT_HOOK_NAME,
            hook_command.as_str(),
        ],
        false,
        5,
    );
}

fn _remove_attention_script(path: &str) {
    if path.is_empty() {
        return;
    }
    let _ = fs::remove_file(path);
}

fn _run_attention_script(path: &str, client: &str, window_target: &str) {
    if path.is_empty() {
        notify_debug::emit_for_window(
            window_target,
            "attention.run",
            "",
            &[
                ("script_present", json!(false)),
                ("window", or_null(window_target)),
            ],
        );
        return;
    }
    let client = if client.contains("#{client_tty}") {
        ""
    } else {
        client
    };
    let script = Path::new(path);
    if !script.is_file() {
        notify_debug::emit_for_window(
            window_target,
            "attention.run",
            "",
            &[
                ("script_present", json!(false)),
                ("window", or_null(window_target)),
                ("path", json!(path)),
                ("error", json!("missing_file")),
            ],
        );
        return;
    }
    let mut child = match Command::new(script)
        .arg(client)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            notify_debug::emit_for_window(
                window_target,
                "attention.run",
                "",
                &[
                    ("window", or_null(window_target)),
                    ("error", json!(format!("{:?}", err.kind()))),
                ],
            );
            return;
        }
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let returncode = status
                    .code()
                    .or_else(|| status.signal().map(|sig| -sig))
                    .unwrap_or(-1);
                notify_debug::emit_for_window(
                    window_target,
                    "attention.run",
                    "",
                    &[
                        ("script_present", json!(true)),
                        ("window", or_null(window_target)),
                        ("client_present", json!(!client.is_empty())),
                        ("returncode", json!(returncode)),
                    ],
                );
                return;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    notify_debug::emit_for_window(
                        window_target,
                        "attention.run",
                        "",
                        &[
                            ("window", or_null(window_target)),
                            ("error", json!("timeout")),
                        ],
                    );
                    return;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                notify_debug::emit_for_window(
                    window_target,
                    "attention.run",
                    "",
                    &[
                        ("window", or_null(window_target)),
                        ("error", json!(format!("{:?}", err.kind()))),
                    ],
                );
                return;
            }
        }
    }
}

/// Recover a window from durable notify state.
pub fn clear_stale_notify(
    window_target: &str,
    panes: &[String],
    token: &str,
    remove_attention: bool,
    source: &str,
    workspace: &str,
) {
    let token = if token.is_empty() {
        tmux::get_window_option(window_target, NOTIFY_TOKEN_OPTION.trim_start_matches('@'))
            .unwrap_or_default()
    } else {
        token.to_string()
    };
    let original = tmux::get_window_option(window_target, ORIGINAL_NAME_KEY).unwrap_or_default();
    let attention =
        tmux::get_window_option(window_target, ATTENTION_SCRIPT_KEY).unwrap_or_default();

    notify_debug::emit_for_window(
        window_target,
        "clear.start",
        workspace,
        &[
            ("source", json!(source)),
            ("window", json!(window_target)),
            ("token", or_null(&token)),
            ("panes_count", json!(panes.len())),
            ("remove_attention", json!(remove_attention)),
        ],
    );

    tmux::clear_window_option(window_target, "window-status-style");
    tmux::clear_window_option(window_target, "window-status-current-style");
    if !original.is_empty() {
        tmux::rename_window(window_target, &original);
    }

    tmux::clear_window_option(window_target, NOTIFY_TOKEN_OPTION);
    tmux::clear_window_option(window_target, ORIGINAL_NAME_OPTION);
    tmux::clear_window_option(window_target, HOOK_NAME_OPTION);
    tmux::clear_window_option(window_target, ATTENTION_SCRIPT_OPTION);
    if remove_attention {
        _remove_attention_script(&attention);
    }

    let mut pane_active_matches = 0;
    if !token.is_empty() {
        // Known boundary: only panes still in this window are reconciled here;
        // cross-window break-pane moves are not chased by notify cleanup.
        for pane_id in panes {
            if tmux::get_pane_option(pane_id, PANE_NOTIFY_ACTIVE_KEY).as_deref()
                == Some(token.as_str())
            {
                tmux::clear_pane_option(pane_id, PANE_NOTIFY_ACTIVE_KEY);
                pane_active_matches += 1;
            }
        }
    }

    notify_debug::emit_for_window(
        window_target,
        "clear.done",
        workspace,
        &[
            ("source", json!(source)),
            ("window", json!(window_target)),
            ("token", or_null(&token)),
            ("pane_active_matches", json!(pane_active_matches)),
        ],
    );
}

pub fn cleanup_selected_window(window_target: &str, client: &str) -> bool {
    if window_target.is_empty() || window_target.contains("#{") {
        return false;
    }
    let token = tmux::get_window_option(window_target, NOTIFY_TOKEN_OPTION.trim_start_matches('@'))
        .unwrap_or_default();
    notify_debug::emit_for_window(
        window_target,
        "cleanup_selected.start",
        "",
        &[
            ("window", json!(window_target)),
            ("client", or_null(client)),
            ("token", or_null(&token)),
        ],
    );
    if token.is_empty() {
        return false;
    }
    let attention =
        tmux::get_window_option(window_target, ATTENTION_SCRIPT_KEY).unwrap_or_default();
    let panes = tmux::list_panes(window_target);
    clear_stale_notify(window_target, &panes, &token, false, "select_hook", "");
    _run_attention_script(&attention, client, window_target);
    true
}

fn _ring_terminal_bell(pane_id: &str, window_target: &str, workspace: &str) {
    let tty_path = tmux::get_pane_tty(pane_id).unwrap_or_default();
    if tty_path.is_empty() {
        notify_debug::emit_for_window(
            window_target,
            "bell",
            workspace,
            &[
                ("pane", json!(pane_id)),
                ("tty_present", json!(false)),
                ("success", json!(false)),
            ],
        );
        return;
    }
    let written = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tty_path)
        .and_then(|mut handle| {
            handle.write_all(b"\x07")?;
            handle.flush()
        });
    if written.is_err() {
        notify_debug::emit_for_window(
            window_target,
            "bell",
            workspace,
            &[
                ("pane", json!(pane_id)),
                ("tty_present", json!(true)),
                ("success", json!(false)),
            ],
        );
        return;
    }
    notify_debug::emit_for_window(
        window_target,
        "bell",
        workspace,
        &[
            ("pane", json!(pane_id)),
            ("tty_present", json!(true)),
            ("success", json!(true)),
        ],
    );
}

pub fn show_window_flash(
    _message: &str,
    pane_id: &str,
    window_target: &str,
    window_name: &str,
    agent_name: &str,
    animate_on_arrival: bool,
    workspace: &str,
) -> anyhow::Result<()> {
    let session = window_target
        .rsplit_once(':')
        .map(|(head, _)| head)
        .unwrap_or("");

    ensure_notify_select_hook(session);

    let existing =
        tmux::get_window_option(window_target, ORIGINAL_NAME_KEY).filter(|value| !value.is_empty());
    let original_present = existing.is_some();
    let original = match existing {
        Some(original) => original,
        None => {
            tmux::set_window_option(window_target, ORIGINAL_NAME_OPTION, window_name);
            window_name.to_string()
        }
    };

    let flash_name = if agent_name.is_empty() {
        format!("{} \u{B7} {}", original, NOTIFY_BADGE)
    } else {
        format!("{} \u{B7} {} {}", original, NOTIFY_BADGE, agent_name)
    };
    tmux::rename_window(window_target, &flash_name);

    let hook_idx = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
        % 1_000_000_000;
    let token = format!("{}:{}", pane_id, hook_idx);
    let old_token =
        tmux::get_window_option(window_target, NOTIFY_TOKEN_OPTION.trim_start_matches('@'))
            .unwrap_or_default();
    notify_debug::emit_for_window(
        window_target,
        "flash.start",
        workspace,
        &[
            ("window", json!(window_target)),
            ("pane", json!(pane_id)),
            ("old_token", or_null(&old_token)),
            ("new_token", json!(token)),
            ("original_name_present", json!(original_present)),
            ("animate", json!(animate_on_arrival)),
        ],
    );
    let attention_script = if animate_on_arrival {
        Some(_write_pane_attention_script(pane_id, &token)?)
    } else {
        None
    };

    tmux::set_window_option(window_target, NOTIFY_TOKEN_OPTION, &token);
    tmux::set_window_option(window_target, HOOK_NAME_OPTION, SELECT_HOOK_NAME);
    if let Some(script) = &attention_script {
        tmux::set_window_option(
            window_target,
            ATTENTION_SCRIPT_OPTION,
            &script.to_string_lossy(),
        );
    }
    if attention_script.is_some() {
        tmux::set_pane_option(pane_id, PANE_NOTIFY_ACTIVE_KEY, &token);
    }

    tmux::set_window_option(window_target, "window-status-style", FLASH_STYLE);
    tmux::set_window_option(window_target, "window-status-current-style", FLASH_STYLE);
    notify_debug::emit_for_window(
        window_target,
        "flash.done",
        workspace,
        &[
            ("window", json!(window_target)),
            ("pane", json!(pane_id)),
            ("new_token", json!(token)),
            (
                "attention_script_created",
                json!(attention_script.is_some()),
            ),
        ],
    );
    Ok(())
}

pub fn notify(message: &str, pane_id: &str, workspace: &str) -> anyhow::Result<NotifyPayload> {
    let window_target = tmux::get_pane_window_target(pane_id).unwrap_or_default();
    let window_name = tmux::get_pane_window_name(pane_id)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "target".to_string());
    let agent_name = tmux::get_pane_option(pane_id, "hive-agent").unwrap_or_default();
    let session_name = tmux::get_pane_session_name(pane_id).unwrap_or_default();
    let client_mode = tmux::get_client_mode(Some(pane_id));
    let suppressed = _target_window_is_focused(&session_name, &window_target);
    notify_debug::emit_for_window(
        &window_target,
        "notify.call",
        workspace,
        &[
            ("pane", json!(pane_id)),
            ("window", or_null(&window_target)),
            ("agent", or_null(&agent_name)),
            ("client_mode", json!(client_mode)),
            ("suppressed", json!(suppressed)),
        ],
    );
    if suppressed {
        return Ok(NotifyPayload {
            agent: agent_name,
            pane_id: pane_id.to_string(),
            window: window_target,
            tab: window_name,
            message: message.to_string(),
            client_mode,
            surface: "suppressed".to_string(),
            suppressed: true,
            suppression_reason: Some("focused_window".to_string()),
        });
    }

    if !window_target.is_empty() {
        show_window_flash(
            message,
            pane_id,
            &window_target,
            &window_name,
            &agent_name,
            true,
            workspace,
        )?;
    }
    _ring_terminal_bell(pane_id, &window_target, workspace);
    Ok(NotifyPayload {
        agent: agent_name,
        pane_id: pane_id.to_string(),
        window: window_target,
        tab: window_name,
        message: message.to_string(),
        client_mode,
        surface: "fired".to_string(),
        suppressed: false,
        suppression_reason: None,
    })
}

/// `python -m hive.notify_ui` equivalent entrypoint.
pub fn main(argv: &[String]) -> i32 {
    let mut cleanup_selected = String::new();
    let mut client = String::new();
    let mut index = 0;
    while index < argv.len() {
        let arg = &argv[index];
        if let Some(value) = arg.strip_prefix("--cleanup-selected=") {
            cleanup_selected = value.to_string();
        } else if arg == "--cleanup-selected" {
            index += 1;
            cleanup_selected = argv.get(index).cloned().unwrap_or_default();
        } else if let Some(value) = arg.strip_prefix("--client=") {
            client = value.to_string();
        } else if arg == "--client" {
            index += 1;
            client = argv.get(index).cloned().unwrap_or_default();
        }
        index += 1;
    }
    if !cleanup_selected.is_empty() {
        cleanup_selected_window(&cleanup_selected, &client);
        return 0;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Test stand-in for `crate::tmux` (monkeypatch equivalent).
    pub mod fake_tmux {
        use std::cell::RefCell;
        use std::collections::HashMap;

        #[derive(Default)]
        pub struct FakeState {
            pub window_options: HashMap<String, String>,
            pub pane_options: HashMap<(String, String), String>,
            /// (kind, target-or-pane, option-or-name, value)
            pub actions: Vec<(String, String, String, String)>,
            pub run_calls: Vec<Vec<String>>,
            pub pane_window_name: Option<String>,
            pub pane_window_target: Option<String>,
            pub pane_session_name: Option<String>,
            pub pane_agent: Option<String>,
            pub client_mode: Option<String>,
            pub most_recent_client_window: Option<String>,
            pub pane_tty: Option<String>,
            pub panes: Vec<String>,
        }

        thread_local! {
            static STATE: RefCell<FakeState> = RefCell::new(FakeState::default());
        }

        pub fn reset() {
            STATE.with(|state| *state.borrow_mut() = FakeState::default());
        }

        pub fn with_state<R>(f: impl FnOnce(&mut FakeState) -> R) -> R {
            STATE.with(|state| f(&mut state.borrow_mut()))
        }

        fn strip(option: &str) -> String {
            option.trim_start_matches('@').to_string()
        }

        pub fn get_most_recent_client_window(_session: Option<&str>) -> Option<String> {
            with_state(|state| state.most_recent_client_window.clone())
        }

        pub fn get_client_mode(_target: Option<&str>) -> String {
            with_state(|state| state.client_mode.clone()).unwrap_or_else(|| "unknown".to_string())
        }

        pub fn get_window_option(_target: &str, key: &str) -> Option<String> {
            with_state(|state| state.window_options.get(key).cloned())
        }

        pub fn set_window_option(target: &str, option: &str, value: &str) {
            with_state(|state| {
                state.actions.push((
                    "set-window".to_string(),
                    target.to_string(),
                    option.to_string(),
                    value.to_string(),
                ));
                state
                    .window_options
                    .insert(strip(option), value.to_string());
            });
        }

        pub fn clear_window_option(target: &str, option: &str) {
            with_state(|state| {
                state.actions.push((
                    "clear-window".to_string(),
                    target.to_string(),
                    option.to_string(),
                    String::new(),
                ));
                state.window_options.remove(&strip(option));
            });
        }

        pub fn rename_window(target: &str, name: &str) {
            with_state(|state| {
                state.actions.push((
                    "rename-window".to_string(),
                    target.to_string(),
                    name.to_string(),
                    String::new(),
                ));
            });
        }

        pub fn list_panes(_target: &str) -> Vec<String> {
            with_state(|state| state.panes.clone())
        }

        pub fn get_pane_option(pane: &str, key: &str) -> Option<String> {
            with_state(|state| {
                if key == "hive-agent" {
                    if let Some(agent) = &state.pane_agent {
                        return Some(agent.clone());
                    }
                }
                state
                    .pane_options
                    .get(&(pane.to_string(), key.to_string()))
                    .cloned()
            })
        }

        pub fn set_pane_option(pane: &str, key: &str, value: &str) {
            with_state(|state| {
                state.actions.push((
                    "set-pane".to_string(),
                    pane.to_string(),
                    key.to_string(),
                    value.to_string(),
                ));
                state
                    .pane_options
                    .insert((pane.to_string(), key.to_string()), value.to_string());
            });
        }

        pub fn clear_pane_option(pane: &str, key: &str) {
            with_state(|state| {
                state.actions.push((
                    "clear-pane".to_string(),
                    pane.to_string(),
                    key.to_string(),
                    String::new(),
                ));
                state
                    .pane_options
                    .remove(&(pane.to_string(), key.to_string()));
            });
        }

        pub fn get_pane_tty(_pane: &str) -> Option<String> {
            with_state(|state| state.pane_tty.clone())
        }

        pub fn get_pane_window_target(_pane: &str) -> Option<String> {
            with_state(|state| state.pane_window_target.clone())
        }

        pub fn get_pane_window_name(_pane: &str) -> Option<String> {
            with_state(|state| state.pane_window_name.clone())
        }

        pub fn get_pane_session_name(_pane: &str) -> Option<String> {
            with_state(|state| state.pane_session_name.clone())
        }

        pub fn _run(args: &[&str], _check: bool, _timeout: u64) {
            with_state(|state| {
                state
                    .run_calls
                    .push(args.iter().map(|arg| arg.to_string()).collect())
            });
        }
    }

    fn mock_tmux_basics() {
        fake_tmux::reset();
        fake_tmux::with_state(|state| {
            state.pane_window_name = Some("dev".to_string());
            state.pane_window_target = Some("dev:1".to_string());
            state.pane_agent = Some("orch".to_string());
            state.pane_session_name = Some("dev".to_string());
            state.most_recent_client_window = Some("dev:9".to_string());
            state.client_mode = Some("terminal".to_string());
        });
    }

    /// Route notify_debug workspace resolution to a temp dir so no test
    /// writes under the real cache dir.
    fn route_debug_logs(workspace: &std::path::Path) {
        crate::notify_debug::tests::fake_tmux::reset();
        crate::notify_debug::tests::fake_tmux::set_workspace_value(Some(
            workspace.to_string_lossy().into_owned(),
        ));
    }

    fn actions3() -> Vec<(String, String, String)> {
        fake_tmux::with_state(|state| {
            state
                .actions
                .iter()
                .map(|(kind, a, b, _)| (kind.clone(), a.clone(), b.clone()))
                .collect()
        })
    }

    fn rename_calls() -> Vec<(String, String)> {
        fake_tmux::with_state(|state| {
            state
                .actions
                .iter()
                .filter(|(kind, _, _, _)| kind == "rename-window")
                .map(|(_, target, name, _)| (target.clone(), name.clone()))
                .collect()
        })
    }

    fn set_window_calls() -> Vec<(String, String, String)> {
        fake_tmux::with_state(|state| {
            state
                .actions
                .iter()
                .filter(|(kind, _, _, _)| kind == "set-window")
                .map(|(_, target, option, value)| (target.clone(), option.clone(), value.clone()))
                .collect()
        })
    }

    fn pane_set_calls() -> Vec<(String, String, String)> {
        fake_tmux::with_state(|state| {
            state
                .actions
                .iter()
                .filter(|(kind, _, _, _)| kind == "set-pane")
                .map(|(_, pane, key, value)| (pane.clone(), key.clone(), value.clone()))
                .collect()
        })
    }

    fn run_calls() -> Vec<Vec<String>> {
        fake_tmux::with_state(|state| state.run_calls.clone())
    }

    fn set_window_value(option: &str) -> String {
        fake_tmux::with_state(|state| {
            state
                .actions
                .iter()
                .find(|(kind, _, opt, _)| kind == "set-window" && opt == option)
                .map(|(_, _, _, value)| value.clone())
        })
        .unwrap_or_else(|| panic!("expected a set for {}", option))
    }

    fn cleanup_attention_scripts() {
        for (kind, _, option, value) in fake_tmux::with_state(|state| state.actions.clone()) {
            if kind == "set-window" && option == ATTENTION_SCRIPT_OPTION {
                let _ = fs::remove_file(value);
            }
        }
    }

    fn owned3(items: &[(&str, &str, &str)]) -> Vec<(String, String, String)> {
        items
            .iter()
            .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
            .collect()
    }

    #[test]
    fn test_notify_fires_flash_and_bell() {
        let tmp = TempDir::new().unwrap();
        mock_tmux_basics();
        route_debug_logs(&tmp.path().join("ws"));
        let tty = tmp.path().join("tty");
        fs::write(&tty, "").unwrap();
        fake_tmux::with_state(|state| state.pane_tty = Some(tty.to_string_lossy().into_owned()));

        let payload = notify("回来确认", "%9", "").unwrap();

        assert_eq!(payload.surface, "fired");
        assert!(!payload.suppressed);
        assert_eq!(payload.message, "回来确认");
        assert_eq!(payload.pane_id, "%9");
        assert_eq!(payload.window, "dev:1");
        assert_eq!(payload.tab, "dev");
        assert_eq!(payload.agent, "orch");
        // flash fired against the target window with agent name + animation
        assert_eq!(
            rename_calls(),
            vec![("dev:1".to_string(), "dev · 🤖 orch".to_string())]
        );
        let token = set_window_value(NOTIFY_TOKEN_OPTION);
        assert!(token.starts_with("%9:"));
        assert_eq!(
            pane_set_calls(),
            vec![(
                "%9".to_string(),
                "hive-notify-active".to_string(),
                token.clone()
            )]
        );
        // bell hit the pane tty
        assert_eq!(fs::read(&tty).unwrap(), b"\x07");
        cleanup_attention_scripts();
    }

    #[test]
    fn test_notify_is_silent_when_target_window_is_focused() {
        let tmp = TempDir::new().unwrap();
        mock_tmux_basics();
        route_debug_logs(tmp.path());
        fake_tmux::with_state(|state| state.most_recent_client_window = Some("dev:1".to_string()));

        let payload = notify("回来确认", "%9", "").unwrap();

        assert_eq!(payload.surface, "suppressed");
        assert!(payload.suppressed);
        assert_eq!(
            payload.suppression_reason.as_deref(),
            Some("focused_window")
        );
        assert!(actions3().is_empty());
        assert!(run_calls().is_empty());
    }

    #[test]
    fn test_show_window_flash_renames_sets_reverse_bold_and_hook() {
        let tmp = TempDir::new().unwrap();
        fake_tmux::reset();
        route_debug_logs(tmp.path());

        show_window_flash("Agent finished", "%9", "dev:1", "dev", "orch", true, "").unwrap();

        assert_eq!(
            rename_calls(),
            vec![("dev:1".to_string(), "dev · 🤖 orch".to_string())]
        );
        let token = set_window_value(NOTIFY_TOKEN_OPTION);
        assert!(token.starts_with("%9:"));
        assert_eq!(
            pane_set_calls(),
            vec![(
                "%9".to_string(),
                "hive-notify-active".to_string(),
                token.clone()
            )]
        );
        let attention = set_window_value(ATTENTION_SCRIPT_OPTION);
        let script_body = fs::read_to_string(&attention).unwrap();
        assert!(script_body.contains("QP=%9"));
        assert!(script_body.contains(&format!("TOKEN={}", token)));
        let _ = fs::remove_file(&attention);
        let hook_name_value = set_window_value(HOOK_NAME_OPTION);
        assert_eq!(hook_name_value, SELECT_HOOK_NAME);
        assert_eq!(
            set_window_calls(),
            owned3(&[
                ("dev:1", "@hive-notify-original-name", "dev"),
                ("dev:1", "@hive-notify-token", token.as_str()),
                ("dev:1", "@hive-notify-hook", SELECT_HOOK_NAME),
                ("dev:1", "@hive-notify-attention", attention.as_str()),
                ("dev:1", "window-status-style", "reverse,bold"),
                ("dev:1", "window-status-current-style", "reverse,bold"),
            ])
        );
        let runs = run_calls();
        assert_eq!(runs.len(), 1);
        let hook_cmd = &runs[0];
        assert_eq!(
            hook_cmd[..4].to_vec(),
            vec![
                "set-hook".to_string(),
                "-t".to_string(),
                "dev".to_string(),
                SELECT_HOOK_NAME.to_string()
            ]
        );
        assert!(!hook_cmd[4].contains("set-hook -ut"));
        assert!(!hook_cmd[4].contains("/tmp/hive-notify-"));
        assert!(hook_cmd[4].contains("-m hive.notify_ui --cleanup-selected"));
        assert!(hook_cmd[4].contains("'#{client_tty}'"));
    }

    #[test]
    fn test_show_window_flash_can_skip_arrival_animation() {
        let tmp = TempDir::new().unwrap();
        fake_tmux::reset();
        route_debug_logs(tmp.path());

        show_window_flash("Agent finished", "%9", "dev:1", "dev", "orch", false, "").unwrap();

        let token = set_window_value(NOTIFY_TOKEN_OPTION);
        assert_eq!(
            rename_calls(),
            vec![("dev:1".to_string(), "dev · 🤖 orch".to_string())]
        );
        assert!(token.starts_with("%9:"));
        assert!(pane_set_calls().is_empty());
        assert!(!set_window_calls()
            .iter()
            .any(|(_, option, _)| option == ATTENTION_SCRIPT_OPTION));
        assert_eq!(run_calls().len(), 1);
    }

    #[test]
    fn test_show_window_flash_without_agent_name_uses_bare_flag() {
        let tmp = TempDir::new().unwrap();
        fake_tmux::reset();
        route_debug_logs(tmp.path());

        show_window_flash("Agent finished", "%9", "dev:1", "dev", "", true, "").unwrap();

        assert_eq!(
            rename_calls(),
            vec![("dev:1".to_string(), "dev · 🤖".to_string())]
        );
        cleanup_attention_scripts();
    }

    #[test]
    fn test_double_notify_preserves_original_and_does_not_rewrite_original_option() {
        let tmp = TempDir::new().unwrap();
        fake_tmux::reset();
        route_debug_logs(tmp.path());

        show_window_flash("m1", "%9", "dev:1", "dev", "orch", true, "").unwrap();
        show_window_flash("m2", "%9", "dev:1", "dev · 🤖 orch", "orch", true, "").unwrap();

        assert_eq!(
            rename_calls(),
            vec![
                ("dev:1".to_string(), "dev · 🤖 orch".to_string()),
                ("dev:1".to_string(), "dev · 🤖 orch".to_string()),
            ]
        );
        let original_writes: Vec<String> = set_window_calls()
            .into_iter()
            .filter(|(_, option, _)| option == ORIGINAL_NAME_OPTION)
            .map(|(_, _, value)| value)
            .collect();
        assert_eq!(original_writes, vec!["dev".to_string()]);
        cleanup_attention_scripts();
    }

    #[test]
    fn test_clear_stale_notify_restores_window_options_and_matching_pane() {
        let tmp = TempDir::new().unwrap();
        fake_tmux::reset();
        route_debug_logs(tmp.path());
        let missing = tmp.path().join("missing-attention.sh");
        fake_tmux::with_state(|state| {
            state
                .window_options
                .insert("hive-notify-token".to_string(), "%9:old-fire".to_string());
            state
                .window_options
                .insert("hive-notify-original-name".to_string(), "dev".to_string());
            state
                .window_options
                .insert("hive-notify-hook".to_string(), SELECT_HOOK_NAME.to_string());
            state.window_options.insert(
                "hive-notify-attention".to_string(),
                missing.to_string_lossy().into_owned(),
            );
            state.pane_options.insert(
                ("%9".to_string(), "hive-notify-active".to_string()),
                "%9:old-fire".to_string(),
            );
            state.pane_options.insert(
                ("%10".to_string(), "hive-notify-active".to_string()),
                "%10:new-fire".to_string(),
            );
        });

        clear_stale_notify(
            "dev:1",
            &["%9".to_string(), "%10".to_string()],
            "",
            true,
            "unknown",
            "",
        );

        assert_eq!(
            actions3(),
            owned3(&[
                ("clear-window", "dev:1", "window-status-style"),
                ("clear-window", "dev:1", "window-status-current-style"),
                ("rename-window", "dev:1", "dev"),
                ("clear-window", "dev:1", "@hive-notify-token"),
                ("clear-window", "dev:1", "@hive-notify-original-name"),
                ("clear-window", "dev:1", "@hive-notify-hook"),
                ("clear-window", "dev:1", "@hive-notify-attention"),
                ("clear-pane", "%9", "hive-notify-active"),
            ])
        );
        fake_tmux::with_state(|state| {
            assert!(state.window_options.is_empty());
            assert_eq!(state.pane_options.len(), 1);
            assert_eq!(
                state
                    .pane_options
                    .get(&("%10".to_string(), "hive-notify-active".to_string())),
                Some(&"%10:new-fire".to_string())
            );
        });
    }

    #[test]
    fn test_cleanup_selected_window_clears_current_token_and_runs_attention() {
        let tmp = TempDir::new().unwrap();
        fake_tmux::reset();
        route_debug_logs(tmp.path());
        let script = tmp.path().join("hive-pane-attention.sh");
        let called = tmp.path().join("called");
        fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s' \"$1\" > {}\n", called.display()),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        fake_tmux::with_state(|state| {
            state
                .window_options
                .insert("hive-notify-token".to_string(), "%9:old-fire".to_string());
            state
                .window_options
                .insert("hive-notify-original-name".to_string(), "dev".to_string());
            state.window_options.insert(
                "hive-notify-attention".to_string(),
                script.to_string_lossy().into_owned(),
            );
            state.pane_options.insert(
                ("%9".to_string(), "hive-notify-active".to_string()),
                "%9:old-fire".to_string(),
            );
            state.panes = vec!["%9".to_string()];
        });

        assert!(cleanup_selected_window("dev:1", "/dev/ttys050"));

        fake_tmux::with_state(|state| {
            assert!(state.window_options.is_empty());
            assert!(state.pane_options.is_empty());
        });
        assert_eq!(fs::read_to_string(&called).unwrap(), "/dev/ttys050");
    }

    #[test]
    fn test_notify_with_workspace_writes_ui_events() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("ws");
        mock_tmux_basics();
        crate::notify_debug::tests::fake_tmux::reset();

        notify("回来确认", "%9", workspace.to_str().unwrap()).unwrap();

        let log = workspace.join("run").join("notify.jsonl");
        let text = fs::read_to_string(&log).unwrap();
        let events: Vec<String> = text
            .lines()
            .map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["event"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert!(events.contains(&"notify.call".to_string()));
        cleanup_attention_scripts();
    }

    #[test]
    fn test_pane_attention_popup_covers_target_pane() {
        assert!(_PANE_ATTENTION_PYTHON.contains("popup_w = width"));
        assert!(_PANE_ATTENTION_PYTHON.contains("popup_h = height"));
        assert!(_PANE_ATTENTION_PYTHON.contains("x = \"#{popup_pane_left}\""));
        assert!(_PANE_ATTENTION_PYTHON.contains("y = \"#{popup_pane_top}\""));
        assert!(_PANE_ATTENTION_PYTHON.contains("TARGET LOCKED:"));
    }

    #[test]
    fn test_pane_attention_animation_timing_is_fast() {
        assert!(_PANE_ATTENTION_PYTHON.contains("SCAN_FRAMES = 14"));
        assert!(_PANE_ATTENTION_PYTHON.contains("SCAN_DELAY = 0.032"));
        assert!(_PANE_ATTENTION_PYTHON.contains("PULSE_FRAMES = 4"));
        assert!(_PANE_ATTENTION_PYTHON.contains("PULSE_DELAY = 0.055"));
        let script = _write_pane_attention_script("%9", "tok").unwrap();
        let body = fs::read_to_string(&script).unwrap();
        let _ = fs::remove_file(&script);
        assert!(body.contains("sleep 0.18"));
    }

    #[test]
    fn test_pane_attention_script_executes_via_facade() {
        // the embedded script imports hive.tmux, so it must run end-to-end
        // under an interpreter that has the package (PYTHONPATH=src contract)
        let tmp = TempDir::new().unwrap();
        let repo_src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../src");
        let bin_dir = tmp.path().join("bin");
        fs::create_dir(&bin_dir).unwrap();
        let log = tmp.path().join("tmux.log");
        let fake = bin_dir.join("tmux");
        fs::write(
            &fake,
            format!(
                "#!/bin/sh\necho \"$@\" >> {log}\ncase \"$*\" in\n  *pane_left*) echo '1 2 40 20' ;;\n  *) echo 'stub' ;;\nesac\n",
                log = log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();

        let path_env = format!(
            "{}:{}",
            bin_dir.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let output = Command::new("python3")
            .arg("-c")
            .arg(_PANE_ATTENTION_PYTHON)
            .env("PATH", path_env)
            .env("PYTHONPATH", &repo_src)
            .env("HIVE_NOTIFY_PANE", "%7")
            .env("HIVE_NOTIFY_CLIENT", "")
            .output()
            .expect("python3 must be runnable");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let entries = fs::read_to_string(&log).unwrap();
        let popup: Vec<&str> = entries
            .lines()
            .filter(|line| line.starts_with("display-popup"))
            .collect();
        assert_eq!(popup.len(), 1);
        assert!(popup[0].contains(" -B ") && popup[0].contains(" -E "));
        assert!(popup[0].contains("PYPOPUP")); // inner animation heredoc still delivered
    }
}
