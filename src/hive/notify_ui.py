from __future__ import annotations

import shlex

from . import notify_state
from . import tmux


def _user_is_already_in_target_window(pane_id: str, *, session_name: str, window_target: str) -> bool:
    if not session_name or not window_target:
        return False
    active_window = tmux.get_most_recent_client_window(session_name)
    return bool(active_window and active_window == window_target)


FLASH_SCRIPT_TEMPLATE = r'''
QT={qt}; QP={qp}; FLASH={flash_style}; ORIG={orig_style}; QNAME={qname}
WIN_IDX={win_idx}
SESSION={session}
CLIENT=$(tmux list-clients -t "$SESSION" -F '#{{client_tty}}' 2>/dev/null | head -1)
cleanup() {{
  tmux set-window-option -t "$QT" -u window-status-style 2>/dev/null || true
  tmux set-window-option -t "$QT" -u window-status-current-style 2>/dev/null || true
  tmux rename-window -t "$QT" "$QNAME" 2>/dev/null || true
}}
is_active() {{
  [ -z "$CLIENT" ] && return 1
  CUR=$(tmux display-message -c "$CLIENT" -p '#{{window_index}}' 2>/dev/null || echo '')
  [ "$CUR" = "$WIN_IDX" ]
}}
on_arrive() {{
  cleanup
  tmux select-pane -t "$QP" 2>/dev/null || true
  exit 0
}}
elapsed=0
while [ "$elapsed" -lt {duration} ]; do
  if is_active; then on_arrive; fi
  tmux set-window-option -t "$QT" window-status-style "$FLASH" 2>/dev/null || true
  tmux set-window-option -t "$QT" window-status-current-style "$FLASH" 2>/dev/null || true
  sleep 0.5; elapsed=$((elapsed + 1))
  if is_active; then on_arrive; fi
  tmux set-window-option -t "$QT" window-status-style "$ORIG" 2>/dev/null || true
  tmux set-window-option -t "$QT" window-status-current-style "$ORIG" 2>/dev/null || true
  sleep 0.5
done
cleanup
'''


def show_window_flash(
    message: str,
    pane_id: str,
    window_target: str,
    window_name: str,
    seconds: int = 12,
) -> None:
    flash_name = f"\U0001f916 {window_name} \u00b7 {message}"
    tmux.rename_window(window_target, flash_name)

    orig_title = tmux.get_pane_title(pane_id) or ""
    badge_title = f"\U0001f916 {orig_title} \u00b7 done"
    tmux.set_pane_title(pane_id, badge_title)

    duration = max(1, int(seconds))
    parts = window_target.rsplit(":", 1)
    session = parts[0] if len(parts) == 2 else ""
    win_idx = parts[1] if len(parts) == 2 else ""
    script = FLASH_SCRIPT_TEMPLATE.format(
        qt=shlex.quote(window_target),
        qp=shlex.quote(pane_id),
        flash_style=shlex.quote("fg=white,bg=#ff5f87,bold"),
        orig_style=shlex.quote("fg=default"),
        qname=shlex.quote(window_name),
        win_idx=shlex.quote(win_idx),
        session=shlex.quote(session),
        duration=duration,
    )
    tmux._run(["run-shell", "-b", script], check=False)

    restore_title = shlex.quote(orig_title)
    qp = shlex.quote(pane_id)
    tmux._run([
        "run-shell", "-b",
        f"sleep {duration}; tmux select-pane -t {qp} -T {restore_title} 2>/dev/null || true",
    ], check=False)


def notify(
    message: str,
    pane_id: str,
    seconds: int = 12,
    *,
    highlight: bool = False,
    window_status: bool = True,
    source: str = notify_state.SOURCE_AGENT_CLI,
    kind: str = "agent_attention",
) -> dict[str, object]:
    window_target = tmux.get_pane_window_target(pane_id) or ""
    window_name = tmux.get_pane_window_name(pane_id) or "target"
    agent_name = tmux.get_pane_option(pane_id, "hive-agent") or ""
    session_name = tmux.get_pane_session_name(pane_id) or ""
    client_mode = tmux.get_client_mode(pane_id)
    suppressed = _user_is_already_in_target_window(
        pane_id,
        session_name=session_name,
        window_target=window_target,
    )
    if suppressed:
        return {
            "agent": agent_name,
            "paneId": pane_id,
            "window": window_target,
            "tab": window_name,
            "message": message,
            "seconds": seconds,
            "clientMode": client_mode,
            "surface": "suppressed",
            "highlight": highlight,
            "windowStatus": window_status,
            "suppressed": True,
            "suppressionReason": "same_window",
        }

    notify_state.record_notification(pane_id, source=source, kind=kind, message=message)
    if highlight:
        tmux.flash_pane_border(pane_id, seconds=seconds)
    if window_status and window_target:
        tmux.flash_window_status(window_target, seconds=seconds)
    if window_target:
        show_window_flash(message, pane_id, window_target, window_name, seconds=seconds)
    return {
        "agent": agent_name,
        "paneId": pane_id,
        "window": window_target,
        "tab": window_name,
        "message": message,
        "seconds": seconds,
        "clientMode": client_mode,
        "surface": "window_flash",
        "highlight": highlight,
        "windowStatus": window_status,
        "suppressed": False,
    }
