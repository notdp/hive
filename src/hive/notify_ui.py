from __future__ import annotations

import argparse
import os
import subprocess
import shlex
import sys
import tempfile
import time
from pathlib import Path

from . import notify_debug
from . import tmux


def _target_window_is_focused(*, session_name: str, window_target: str) -> bool:
    if not session_name or not window_target:
        return False
    active_window = tmux.get_most_recent_client_window(session_name)
    return bool(active_window and active_window == window_target)


NOTIFY_TOKEN_OPTION = "@hive-notify-token"
ORIGINAL_NAME_OPTION = "@hive-notify-original-name"
ORIGINAL_NAME_KEY = "hive-notify-original-name"
HOOK_NAME_OPTION = "@hive-notify-hook"
HOOK_NAME_KEY = "hive-notify-hook"
ATTENTION_SCRIPT_OPTION = "@hive-notify-attention"
ATTENTION_SCRIPT_KEY = "hive-notify-attention"
PANE_NOTIFY_ACTIVE_KEY = "hive-notify-active"
FLASH_STYLE = "reverse,bold"
NOTIFY_BADGE = "🤖"
# Use one stable high-index hook so each notify refreshes the same fast-path
# instead of installing per-notify hook/script pairs that can go stale.
SELECT_HOOK_NAME = "after-select-window[900001]"


_PANE_ATTENTION_PYTHON = r'''
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
'''


def _write_pane_attention_script(*, pane_id: str, token: str) -> Path:
    handle = tempfile.NamedTemporaryFile("w", suffix=".sh", prefix="hive-pane-attention-", delete=False)
    with handle:
        handle.write(f'''#!/usr/bin/env bash
set -euo pipefail

QP={shlex.quote(pane_id)}
TOKEN={shlex.quote(token)}
CLIENT="${{1:-}}"

cleanup() {{
  cur="$(tmux show-options -p -v -t "$QP" @{PANE_NOTIFY_ACTIVE_KEY} 2>/dev/null || echo '')"
  if [ "$cur" = "$TOKEN" ]; then
    tmux set-option -p -t "$QP" -u @{PANE_NOTIFY_ACTIVE_KEY} >/dev/null 2>&1 || true
  fi
  rm -f "$0"
}}
trap cleanup EXIT

tmux set-option -p -t "$QP" @{PANE_NOTIFY_ACTIVE_KEY} "$TOKEN" >/dev/null 2>&1 || true
HIVE_NOTIFY_PANE="$QP" HIVE_NOTIFY_CLIENT="$CLIENT" {shlex.quote(sys.executable)} <<'PY'
{_PANE_ATTENTION_PYTHON}
PY

sleep 0.18
''')
    path = Path(handle.name)
    path.chmod(0o755)
    return path


def _select_hook_command() -> str:
    # run-shell executes with the tmux server's environment, not this
    # process's: a source-checkout registration (PYTHONPATH=src) would
    # otherwise install a hook whose `-m hive.notify_ui` can never import
    # hive — the flash then sticks until the sidecar sweep.
    pythonpath = os.environ.get("PYTHONPATH", "")
    env_prefix = f"PYTHONPATH={shlex.quote(pythonpath)} " if pythonpath else ""
    cleanup_cmd = (
        f"{env_prefix}{shlex.quote(sys.executable)} -m hive.notify_ui "
        "--cleanup-selected '#{session_name}:#{window_index}' "
        "--client '#{client_tty}'"
    )
    # This string is parsed by tmux's hook command parser, then by run-shell.
    # Keep the attached-client e2e test in sync if this quoting changes.
    escaped = cleanup_cmd.replace("\\", "\\\\").replace('"', '\\"')
    run_cmd = f'run-shell -b "{escaped}"'
    return f"if-shell -F '#{{?@{NOTIFY_TOKEN_OPTION.lstrip('@')},1,0}}' {shlex.quote(run_cmd)}"


def ensure_notify_select_hook(session: str) -> None:
    if not session:
        return
    tmux._run(["set-hook", "-t", session, SELECT_HOOK_NAME, _select_hook_command()], check=False)


def _remove_attention_script(path: str) -> None:
    if not path:
        return
    try:
        Path(path).unlink(missing_ok=True)
    except OSError:
        return


def _run_attention_script(path: str, client: str, *, window_target: str = "") -> None:
    if not path:
        notify_debug.emit_for_window(window_target, "attention.run", script_present=False, window=window_target or None)
        return
    if "#{client_tty}" in client:
        client = ""
    try:
        script = Path(path)
        if not script.is_file():
            notify_debug.emit_for_window(
                window_target,
                "attention.run",
                script_present=False,
                window=window_target or None,
                path=str(path),
                error="missing_file",
            )
            return
        result = subprocess.run(
            [str(script), client],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
            timeout=5,
        )
        notify_debug.emit_for_window(
            window_target,
            "attention.run",
            script_present=True,
            window=window_target or None,
            client_present=bool(client),
            returncode=result.returncode,
        )
    except subprocess.TimeoutExpired:
        notify_debug.emit_for_window(window_target, "attention.run", window=window_target or None, error="timeout")
    except Exception as exc:
        notify_debug.emit_for_window(window_target, "attention.run", window=window_target or None, error=type(exc).__name__)


def clear_stale_notify(
    window_target: str,
    panes: list[str],
    *,
    token: str = "",
    remove_attention: bool = True,
    source: str = "unknown",
    workspace: str = "",
) -> None:
    """Recover a window from durable notify state."""
    token = token or tmux.get_window_option(window_target, NOTIFY_TOKEN_OPTION.lstrip("@")) or ""
    original = tmux.get_window_option(window_target, ORIGINAL_NAME_KEY) or ""
    attention = tmux.get_window_option(window_target, ATTENTION_SCRIPT_KEY) or ""

    notify_debug.emit_for_window(
        window_target,
        "clear.start",
        workspace=workspace,
        source=source,
        window=window_target,
        token=token or None,
        panes_count=len(panes),
        remove_attention=remove_attention,
    )

    tmux.clear_window_option(window_target, "window-status-style")
    tmux.clear_window_option(window_target, "window-status-current-style")
    if original:
        tmux.rename_window(window_target, original)

    tmux.clear_window_option(window_target, NOTIFY_TOKEN_OPTION)
    tmux.clear_window_option(window_target, ORIGINAL_NAME_OPTION)
    tmux.clear_window_option(window_target, HOOK_NAME_OPTION)
    tmux.clear_window_option(window_target, ATTENTION_SCRIPT_OPTION)
    if remove_attention:
        _remove_attention_script(attention)

    pane_active_matches = 0
    if token:
        # Known boundary: only panes still in this window are reconciled here;
        # cross-window break-pane moves are not chased by notify cleanup.
        for pane_id in panes:
            if tmux.get_pane_option(pane_id, PANE_NOTIFY_ACTIVE_KEY) == token:
                tmux.clear_pane_option(pane_id, PANE_NOTIFY_ACTIVE_KEY)
                pane_active_matches += 1

    notify_debug.emit_for_window(
        window_target,
        "clear.done",
        workspace=workspace,
        source=source,
        window=window_target,
        token=token or None,
        pane_active_matches=pane_active_matches,
    )


def cleanup_selected_window(window_target: str, *, client: str = "") -> bool:
    if not window_target or "#{" in window_target:
        return False
    token = tmux.get_window_option(window_target, NOTIFY_TOKEN_OPTION.lstrip("@")) or ""
    notify_debug.emit_for_window(
        window_target,
        "cleanup_selected.start",
        window=window_target,
        client=client or None,
        token=token or None,
    )
    if not token:
        return False
    attention = tmux.get_window_option(window_target, ATTENTION_SCRIPT_KEY) or ""
    panes = tmux.list_panes(window_target)
    clear_stale_notify(
        window_target,
        panes,
        token=token,
        remove_attention=False,
        source="select_hook",
    )
    _run_attention_script(attention, client, window_target=window_target)
    return True


def _ring_terminal_bell(pane_id: str, *, window_target: str = "", workspace: str = "") -> None:
    tty_path = tmux.get_pane_tty(pane_id)
    if not tty_path:
        notify_debug.emit_for_window(
            window_target, "bell", workspace=workspace, pane=pane_id, tty_present=False, success=False
        )
        return
    try:
        with open(tty_path, "w") as handle:
            handle.write("\a")
            handle.flush()
    except OSError:
        notify_debug.emit_for_window(
            window_target, "bell", workspace=workspace, pane=pane_id, tty_present=True, success=False
        )
        return
    notify_debug.emit_for_window(
        window_target, "bell", workspace=workspace, pane=pane_id, tty_present=True, success=True
    )


def show_window_flash(
    message: str,
    pane_id: str,
    window_target: str,
    window_name: str,
    *,
    agent_name: str = "",
    animate_on_arrival: bool = True,
    workspace: str = "",
) -> None:
    parts = window_target.rsplit(":", 1)
    session = parts[0] if len(parts) == 2 else ""

    ensure_notify_select_hook(session)

    original = tmux.get_window_option(window_target, ORIGINAL_NAME_KEY)
    original_present = bool(original)
    if not original:
        original = window_name
        tmux.set_window_option(window_target, ORIGINAL_NAME_OPTION, original)

    flash_name = f"{original} · {NOTIFY_BADGE} {agent_name}" if agent_name else f"{original} · {NOTIFY_BADGE}"
    tmux.rename_window(window_target, flash_name)

    hook_idx = int(time.time() * 1000) % 1_000_000_000
    token = f"{pane_id}:{hook_idx}"
    old_token = tmux.get_window_option(window_target, NOTIFY_TOKEN_OPTION.lstrip("@")) or ""
    notify_debug.emit_for_window(
        window_target,
        "flash.start",
        workspace=workspace,
        window=window_target,
        pane=pane_id,
        old_token=old_token or None,
        new_token=token,
        original_name_present=original_present,
        animate=animate_on_arrival,
    )
    attention_script = None
    if animate_on_arrival:
        attention_script = _write_pane_attention_script(pane_id=pane_id, token=token)

    tmux.set_window_option(window_target, NOTIFY_TOKEN_OPTION, token)
    tmux.set_window_option(window_target, HOOK_NAME_OPTION, SELECT_HOOK_NAME)
    if attention_script is not None:
        tmux.set_window_option(window_target, ATTENTION_SCRIPT_OPTION, str(attention_script))
    if attention_script is not None:
        tmux.set_pane_option(pane_id, PANE_NOTIFY_ACTIVE_KEY, token)

    tmux.set_window_option(window_target, "window-status-style", FLASH_STYLE)
    tmux.set_window_option(window_target, "window-status-current-style", FLASH_STYLE)
    notify_debug.emit_for_window(
        window_target,
        "flash.done",
        workspace=workspace,
        window=window_target,
        pane=pane_id,
        new_token=token,
        attention_script_created=attention_script is not None,
    )


def notify(
    message: str,
    pane_id: str,
    *,
    workspace: str = "",
) -> dict[str, object]:
    window_target = tmux.get_pane_window_target(pane_id) or ""
    window_name = tmux.get_pane_window_name(pane_id) or "target"
    agent_name = tmux.get_pane_option(pane_id, "hive-agent") or ""
    session_name = tmux.get_pane_session_name(pane_id) or ""
    client_mode = tmux.get_client_mode(pane_id)
    suppressed = _target_window_is_focused(
        session_name=session_name,
        window_target=window_target,
    )
    notify_debug.emit_for_window(
        window_target,
        "notify.call",
        workspace=workspace,
        pane=pane_id,
        window=window_target or None,
        agent=agent_name or None,
        client_mode=client_mode,
        suppressed=suppressed,
    )
    if suppressed:
        return {
            "agent": agent_name,
            "paneId": pane_id,
            "window": window_target,
            "tab": window_name,
            "message": message,
            "clientMode": client_mode,
            "surface": "suppressed",
            "suppressed": True,
            "suppressionReason": "focused_window",
        }

    if window_target:
        show_window_flash(
            message,
            pane_id,
            window_target,
            window_name,
            agent_name=agent_name,
            animate_on_arrival=True,
            workspace=workspace,
        )
    _ring_terminal_bell(pane_id, window_target=window_target, workspace=workspace)
    return {
        "agent": agent_name,
        "paneId": pane_id,
        "window": window_target,
        "tab": window_name,
        "message": message,
        "clientMode": client_mode,
        "surface": "fired",
        "suppressed": False,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--cleanup-selected", default="")
    parser.add_argument("--client", default="")
    args = parser.parse_args(argv)
    if args.cleanup_selected:
        cleanup_selected_window(args.cleanup_selected, client=args.client)
        return 0
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
