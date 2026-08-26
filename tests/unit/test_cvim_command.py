from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
COMMAND = ROOT / "src" / "hive" / "core_assets" / "cvim" / "bin" / "cvim-command"
SHARED_DIR = ROOT / "src" / "hive" / "core_assets" / "cvim" / "bin"


def _import_shared():
    if str(SHARED_DIR) not in sys.path:
        sys.path.insert(0, str(SHARED_DIR))
    import _cvim_shared
    return _cvim_shared


def _make_session(tmp_path: Path, messages: list[dict]) -> Path:
    f = tmp_path / "session.jsonl"
    lines = []
    for msg in messages:
        lines.append(json.dumps({"type": "message", "message": msg}))
    f.write_text("\n".join(lines) + "\n")
    return f


def test_extract_includes_exit_spec_mode_plan_with_text(tmp_path):
    shared = _import_shared()
    session = _make_session(tmp_path, [
        {"role": "assistant", "content": [
            {"type": "text", "text": "Summary text here."},
            {"type": "tool_use", "name": "ExitSpecMode", "input": {"plan": "## Detailed plan", "title": "My Spec"}},
        ]},
    ])
    result = shared.extract_last_assistant_text(session)
    assert "Summary text here." in result
    assert 'Propose Specification title: "My Spec"' in result
    assert "Specification for approval:" in result
    assert "## Detailed plan" in result


def test_extract_plan_only_without_text(tmp_path):
    shared = _import_shared()
    session = _make_session(tmp_path, [
        {"role": "assistant", "content": [
            {"type": "tool_use", "name": "ExitSpecMode", "input": {"plan": "## Plan only"}},
        ]},
    ])
    result = shared.extract_last_assistant_text(session)
    assert "Specification for approval:" in result
    assert "## Plan only" in result


def _write_fake_tmux(tmp_path: Path) -> Path:
    tmp_path.mkdir(parents=True, exist_ok=True)
    script = tmp_path / "tmux"
    script.write_text(
        """#!/usr/bin/env python3
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
    sys.stdout.write(f"{value}\\n")


def _append(event):
    if actions_path is None:
        return
    event.setdefault("ts", time.monotonic())
    with actions_path.open("a") as fh:
        fh.write(json.dumps(event) + "\\n")


def _count_events(name):
    if actions_path is None or not actions_path.exists():
        return 0
    count = 0
    for line in actions_path.read_text().splitlines():
        if not line.strip():
            continue
        if json.loads(line).get("cmd") == name:
            count += 1
    return count


def _pane_is_ready(now):
    min_ready_delay = float(os.environ.get("FAKE_TMUX_MIN_READY_DELAY", "0") or "0")
    if actions_path is None or min_ready_delay <= 0 or not actions_path.exists():
        return True
    for line in reversed(actions_path.read_text().splitlines()):
        if not line.strip():
            continue
        event = json.loads(line)
        if event.get("cmd") == "run-shell":
            return now - event.get("ts", now) >= min_ready_delay
    return True


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

if cmd == "new-window":
    event = {"cmd": cmd, "args": args}
    log_path.write_text(json.dumps(event))
    _append(event)
    _print(state.get("new_window_id", "@2"))
    raise SystemExit(0)

if cmd == "capture-pane":
    capture_index = _count_events("capture-pane")
    event = {"cmd": cmd, "args": args}
    _append(event)
    if os.environ.get("FAKE_TMUX_CAPTURE_PANE_SEQUENCE"):
        sequence = json.loads(os.environ["FAKE_TMUX_CAPTURE_PANE_SEQUENCE"])
        if sequence:
            sys.stdout.write(sequence[min(capture_index, len(sequence) - 1)])
    else:
        sys.stdout.write(os.environ.get("FAKE_TMUX_CAPTURE_PANE_TEXT", ""))
    raise SystemExit(0)

if cmd in {"send-keys", "load-buffer", "paste-buffer"}:
    event = {"cmd": cmd, "args": args}
    if not _pane_is_ready(time.monotonic()):
        event["dropped"] = True
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
"""
    )
    script.chmod(0o755)
    return script


def _write_fake_ps(tmp_path: Path) -> Path:
    """Shadow the real ps so the pane's process probe never sees the
    developer's own machine.

    Default answer: a plain interactive claude on the pane tty, i.e. the one
    shape tmux keystrokes may go to. ``FAKE_PS_OUTPUT`` overrides it (empty =
    nothing running, ``claude attach <job>`` = an attach viewer).
    """
    script = tmp_path / "ps"
    script.write_text(
        """#!/usr/bin/env python3
import os
import sys

out = os.environ.get("FAKE_PS_OUTPUT", "456 claude claude")
if out:
    sys.stdout.write(out.rstrip("\\n") + "\\n")
"""
    )
    script.chmod(0o755)
    return script


def _isolated_env(tmp_path: Path) -> dict[str, str]:
    """Env bits that keep a cvim run off the developer's real agent trees.

    The sendback resolves a pane's engine address from those trees (claude's
    job records, codex's thread records, grok's session records), so all
    three are pinned at disposable paths: a stray record on the developer's
    machine must never make a fixture pane look hive-managed.
    """
    return {
        "HIVE_PYTHON": sys.executable,
        "PYTHONPATH": str(ROOT / "src"),
        "CLAUDE_HOME": str(tmp_path / "claude-home"),
        "CODEX_HOME": str(tmp_path / "codex-home"),
        "GROK_HOME": str(tmp_path / "grok-home"),
    }


def _write_fake_editor(tmp_path: Path) -> Path:
    script = tmp_path / "fake-editor"
    script.write_text(
        """#!/usr/bin/env python3
import os
import sys
from pathlib import Path

path = Path(sys.argv[1])
append_text = os.environ.get("FAKE_EDITOR_APPEND_TEXT")
if append_text:
    existing = path.read_text() if path.exists() else ""
    with path.open("a") as fh:
        if existing and not existing.endswith("\\n"):
            fh.write("\\n")
        fh.write(append_text)
"""
    )
    script.chmod(0o755)
    return script


def _materialize_cvim_bundle(tmp_path: Path) -> Path:
    bundle_root = tmp_path / "cvim-bundle"
    shutil.copytree(ROOT / "src" / "hive" / "core_assets" / "cvim", bundle_root)
    for file_path in (bundle_root / "bin").iterdir():
        if file_path.is_file():
            file_path.chmod(0o755)
    return bundle_root / "bin" / "cvim-command"


def _run_command(
    tmp_path: Path,
    *,
    current_pane: str,
    panes: list[dict[str, object]],
    extra_env: dict[str, str] | None = None,
) -> dict[str, object]:
    _write_fake_tmux(tmp_path)
    _write_fake_ps(tmp_path)
    command = _materialize_cvim_bundle(tmp_path)
    log_path = tmp_path / "tmux-log.json"
    state = {
        "current_pane": current_pane,
        "client_width": 200,
        "client_height": 100,
        "panes": panes,
    }
    env = os.environ.copy()
    env["PATH"] = f"{tmp_path}:{env.get('PATH', '')}"
    env["TMUX"] = "/tmp/tmux-test"
    env["TMUX_PANE"] = current_pane
    env["CVIM_SEED_MODE"] = "blank"
    env["CVIM_EDITOR"] = "sh"
    env["FAKE_TMUX_STATE"] = json.dumps(state)
    env["FAKE_TMUX_LOG"] = str(log_path)
    env.update(_isolated_env(tmp_path))
    if extra_env:
        env.update(extra_env)

    subprocess.run(["bash", str(command), "vim"], check=True, env=env, cwd=ROOT)
    return json.loads(log_path.read_text())


def _run_command_actions(
    tmp_path: Path,
    *,
    current_pane: str,
    panes: list[dict[str, object]],
    extra_env: dict[str, str] | None = None,
    use_default_delays: bool = False,
    workspace: Path | None = None,
) -> list[dict[str, object]]:
    _write_fake_tmux(tmp_path)
    _write_fake_ps(tmp_path)
    command = _materialize_cvim_bundle(tmp_path)
    editor = _write_fake_editor(tmp_path)
    log_path = tmp_path / "tmux-log.json"
    actions_path = tmp_path / "tmux-actions.jsonl"
    state = {
        "current_pane": current_pane,
        "client_width": 200,
        "client_height": 100,
        "panes": panes,
    }
    if workspace is not None:
        state["workspace"] = str(workspace)
    env = os.environ.copy()
    env["PATH"] = f"{tmp_path}:{env.get('PATH', '')}"
    env["TMUX"] = "/tmp/tmux-test"
    env["TMUX_PANE"] = current_pane
    env["CVIM_SEED_MODE"] = "blank"
    env["CVIM_OUTPUT_MODE"] = "text"
    env["CVIM_EDITOR"] = str(editor)
    if not use_default_delays:
        env["CVIM_PASTE_DELAY"] = "0"
        env["CVIM_INTERRUPT_SETTLE_DELAY"] = "0"
        env["CVIM_SUBMIT_DELAY"] = "0"
    env["FAKE_TMUX_STATE"] = json.dumps(state)
    env["FAKE_TMUX_LOG"] = str(log_path)
    env["FAKE_TMUX_ACTIONS"] = str(actions_path)
    env["FAKE_TMUX_EXEC_POPUP"] = "1"
    env.update(_isolated_env(tmp_path))
    if extra_env:
        env.update(extra_env)

    subprocess.run(["bash", str(command), "vim"], check=True, env=env, cwd=ROOT)
    return [json.loads(line) for line in actions_path.read_text().splitlines() if line.strip()]


def _read_latest_cvim_records(cache_dir: Path) -> list[dict[str, object]]:
    latest_file = cache_dir / "hive" / "cvim" / "latest"
    log_path = Path(latest_file.read_text().strip())
    return [json.loads(line) for line in log_path.read_text().splitlines() if line.strip()]


def _popup_geometry(log_record: dict[str, object]) -> tuple[int, int, int, int]:
    """Return the popup rectangle as (left, top, width, height).

    tmux's numeric `display-popup -y` is the popup *bottom* edge row, not the
    top. We convert back to the terminal-space top by subtracting height so
    assertions read like "popup covers pane %2" instead of leaking the tmux
    contract.
    """
    args = log_record["args"]
    assert log_record["cmd"] == "display-popup"
    x = int(args[args.index("-x") + 1])
    y_bottom = int(args[args.index("-y") + 1])
    width = int(args[args.index("-w") + 1])
    height = int(args[args.index("-h") + 1])
    return (x, y_bottom - height, width, height)


def test_popup_uses_right_half_when_only_one_pane(tmp_path):
    record = _run_command(
        tmp_path,
        current_pane="%1",
        panes=[
            {"id": "%1", "left": 0, "top": 0, "width": 200, "height": 100},
        ],
    )

    assert _popup_geometry(record) == (100, 0, 100, 100)


def test_popup_prefers_all_panes_to_the_right_in_linear_layout(tmp_path):
    panes = [
        {"id": "%1", "left": 0, "top": 0, "width": 50, "height": 100},
        {"id": "%2", "left": 50, "top": 0, "width": 50, "height": 100},
        {"id": "%3", "left": 100, "top": 0, "width": 50, "height": 100},
        {"id": "%4", "left": 150, "top": 0, "width": 50, "height": 100},
    ]

    leftmost = _run_command(tmp_path / "leftmost", current_pane="%1", panes=panes)
    mid_right = _run_command(tmp_path / "mid_right", current_pane="%3", panes=panes)
    rightmost = _run_command(tmp_path / "rightmost", current_pane="%4", panes=panes)

    assert _popup_geometry(leftmost) == (50, 0, 150, 100)
    assert _popup_geometry(mid_right) == (150, 0, 50, 100)
    assert _popup_geometry(rightmost) == (0, 0, 150, 100)


def test_popup_uses_only_right_column_in_tiled_layout(tmp_path):
    panes = [
        {"id": "%1", "left": 0, "top": 0, "width": 100, "height": 50},
        {"id": "%2", "left": 100, "top": 0, "width": 100, "height": 50},
        {"id": "%3", "left": 0, "top": 50, "width": 100, "height": 50},
        {"id": "%4", "left": 100, "top": 50, "width": 100, "height": 50},
    ]

    record = _run_command(tmp_path, current_pane="%1", panes=panes)

    assert _popup_geometry(record) == (100, 0, 100, 100)


def test_popup_uses_lower_half_in_top_bottom_layout(tmp_path):
    panes = [
        {"id": "%1", "left": 0, "top": 0, "width": 200, "height": 50},
        {"id": "%2", "left": 0, "top": 50, "width": 200, "height": 50},
    ]

    top = _run_command(tmp_path / "top", current_pane="%1", panes=panes)
    bottom = _run_command(tmp_path / "bottom", current_pane="%2", panes=panes)

    # Source in the upper pane → popup lands on the lower pane.
    assert _popup_geometry(top) == (0, 50, 200, 50)
    # Source in the lower pane → popup lands on the upper pane.
    assert _popup_geometry(bottom) == (0, 0, 200, 50)


def test_popup_chooses_left_column_when_source_is_middle_right_of_staggered_layout(tmp_path):
    panes = [
        {"id": "%1", "left": 0, "top": 0, "width": 100, "height": 33},
        {"id": "%4", "left": 100, "top": 0, "width": 100, "height": 33},
        {"id": "%2", "left": 0, "top": 33, "width": 100, "height": 34},
        {"id": "%x", "left": 100, "top": 33, "width": 100, "height": 34},
        {"id": "%3", "left": 0, "top": 67, "width": 100, "height": 33},
        {"id": "%6", "left": 100, "top": 67, "width": 100, "height": 33},
    ]

    record = _run_command(tmp_path, current_pane="%x", panes=panes)

    assert _popup_geometry(record) == (0, 0, 100, 100)


def test_popup_chooses_right_block_in_three_by_three_left_middle_layout(tmp_path):
    panes = [
        {"id": "%1", "left": 0, "top": 0, "width": 50, "height": 33},
        {"id": "%2", "left": 50, "top": 0, "width": 50, "height": 33},
        {"id": "%3", "left": 100, "top": 0, "width": 100, "height": 33},
        {"id": "%x", "left": 0, "top": 33, "width": 50, "height": 34},
        {"id": "%5", "left": 50, "top": 33, "width": 50, "height": 34},
        {"id": "%6", "left": 100, "top": 33, "width": 100, "height": 34},
        {"id": "%7", "left": 0, "top": 67, "width": 50, "height": 33},
        {"id": "%8", "left": 50, "top": 67, "width": 50, "height": 33},
        {"id": "%9", "left": 100, "top": 67, "width": 100, "height": 33},
    ]

    record = _run_command(tmp_path, current_pane="%x", panes=panes)

    assert _popup_geometry(record) == (50, 0, 150, 100)


def test_popup_chooses_right_column_when_source_is_center_of_three_by_three(tmp_path):
    panes = [
        {"id": "%1", "left": 0, "top": 0, "width": 50, "height": 33},
        {"id": "%2", "left": 50, "top": 0, "width": 100, "height": 33},
        {"id": "%3", "left": 150, "top": 0, "width": 50, "height": 33},
        {"id": "%4", "left": 0, "top": 33, "width": 50, "height": 34},
        {"id": "%x", "left": 50, "top": 33, "width": 100, "height": 34},
        {"id": "%6", "left": 150, "top": 33, "width": 50, "height": 34},
        {"id": "%7", "left": 0, "top": 67, "width": 50, "height": 33},
        {"id": "%8", "left": 50, "top": 67, "width": 100, "height": 33},
        {"id": "%9", "left": 150, "top": 67, "width": 50, "height": 33},
    ]

    record = _run_command(tmp_path, current_pane="%x", panes=panes)

    assert _popup_geometry(record) == (150, 0, 50, 100)


def test_edited_save_interrupts_before_paste_and_submits(tmp_path):
    actions = _run_command_actions(
        tmp_path,
        current_pane="%1",
        panes=[
            {"id": "%1", "left": 0, "top": 0, "width": 200, "height": 100},
        ],
        extra_env={
            "FAKE_EDITOR_APPEND_TEXT": "new line added",
            "FAKE_TMUX_CAPTURE_PANE_TEXT": "ready for input\n> [<comment on=\"previous_reply\"> pasted]",
        },
    )

    escape_indexes = [
        index for index, event in enumerate(actions)
        if event["cmd"] == "send-keys" and event["args"][-1] == "Escape"
    ]
    paste_index = next(
        index for index, event in enumerate(actions)
        if event["cmd"] == "paste-buffer"
    )
    enter_index = next(
        index for index, event in enumerate(actions)
        if event["cmd"] == "send-keys" and event["args"][-1] == "Enter"
    )

    assert len(escape_indexes) == 1
    assert any(event["cmd"] == "load-buffer" for event in actions)
    assert escape_indexes[0] < paste_index < enter_index


def test_edited_save_waits_for_pane_ready_before_paste(tmp_path):
    actions = _run_command_actions(
        tmp_path,
        current_pane="%1",
        panes=[
            {"id": "%1", "left": 0, "top": 0, "width": 200, "height": 100,
             "command": "claude"},
        ],
        extra_env={
            "FAKE_EDITOR_APPEND_TEXT": "new line added",
            "FAKE_TMUX_MIN_READY_DELAY": "0.30",
        },
        use_default_delays=True,
    )

    paste_event = next(event for event in actions if event["cmd"] == "paste-buffer")

    assert not any(
        event.get("dropped")
        for event in actions
        if event["cmd"] in {"send-keys", "load-buffer", "paste-buffer"}
    )
    assert not paste_event.get("dropped", False)


def test_claude_profile_clears_input_with_ctrl_u_before_paste(tmp_path):
    actions = _run_command_actions(
        tmp_path,
        current_pane="%1",
        panes=[
            {
                "id": "%1",
                "left": 0,
                "top": 0,
                "width": 200,
                "height": 100,
                "command": "claude",
                "title": "✳ Claude Code",
            },
        ],
        extra_env={
            "FAKE_EDITOR_APPEND_TEXT": "new line added",
            "FAKE_TMUX_CAPTURE_PANE_TEXT": "ready for input\n> [<comment on=\"previous_reply\"> pasted]",
        },
    )

    ctrl_u_index = next(
        index for index, event in enumerate(actions)
        if event["cmd"] == "send-keys" and event["args"][-1] == "C-u"
    )
    paste_index = next(
        index for index, event in enumerate(actions)
        if event["cmd"] == "paste-buffer"
    )

    assert ctrl_u_index < paste_index


def test_claude_profile_accepts_pasted_placeholder_before_submit(tmp_path):
    cache_dir = tmp_path / "cache"
    actions = _run_command_actions(
        tmp_path,
        current_pane="%1",
        panes=[
            {
                "id": "%1",
                "left": 0,
                "top": 0,
                "width": 200,
                "height": 100,
                "command": "claude",
                "title": "✳ Claude Code",
            },
        ],
        extra_env={
            "CVIM_OUTPUT_MODE": "diff",
            "FAKE_EDITOR_APPEND_TEXT": "new line added",
            "FAKE_TMUX_CAPTURE_PANE_TEXT": "❯ [Pasted text #1 +10 lines]",
            "XDG_CACHE_HOME": str(cache_dir),
        },
        use_default_delays=True,
    )

    records = _read_latest_cvim_records(cache_dir)

    assert any(event["cmd"] == "paste-buffer" for event in actions)
    assert any(
        event["cmd"] == "send-keys" and event["args"][-1] == "Enter"
        for event in actions
    )
    assert any("matcher=claude_pasted_placeholder" in str(record.get("message", "")) for record in records)


def test_claude_profile_clears_input_even_when_editor_content_is_unchanged(tmp_path):
    actions = _run_command_actions(
        tmp_path,
        current_pane="%1",
        panes=[
            {
                "id": "%1",
                "left": 0,
                "top": 0,
                "width": 200,
                "height": 100,
                "command": "claude",
                "title": "✳ Claude Code",
            },
        ],
    )

    assert any(
        event["cmd"] == "send-keys" and event["args"][-1] == "C-u"
        for event in actions
    )
    assert not any(event["cmd"] in {"load-buffer", "paste-buffer"} for event in actions)
    assert not any(
        event["cmd"] == "send-keys" and event["args"][-1] == "Enter"
        for event in actions
    )


_CLAUDE_PANE = {
    "id": "%1",
    "left": 0,
    "top": 0,
    "width": 200,
    "height": 100,
    "command": "claude",
    "title": "✳ Claude Code",
}


def _write_fake_claude(tmp_path: Path) -> Path:
    """Stand-in for the claude binary, for the member sendback path.

    `attach` records its argv, registers a bg engine entry for the job (so
    the pipe gets past engine resolution) and exits at once — which is how
    the primitive learns the client is gone. That is enough to prove the
    member branch went down the pipe: what matters here is that it never
    reaches for tmux.
    """
    script = tmp_path / "claude"
    script.write_text(
        """#!/usr/bin/env python3
import json
import os
import sys
from pathlib import Path

Path(os.environ["FAKE_CLAUDE_LOG"]).open("a").write(" ".join(sys.argv[1:]) + "\\n")
if sys.argv[1:2] == ["attach"]:
    # hive washes CLAUDE_HOME out of the pipe env and re-exports the config
    # tree the way claude itself takes it.
    home = Path(os.environ["CLAUDE_CONFIG_DIR"])
    sock = home / "inbox.sock"
    sock.parent.mkdir(parents=True, exist_ok=True)
    sock.touch()
    registry = home / "sessions"
    registry.mkdir(parents=True, exist_ok=True)
    (registry / f"{os.getppid()}.json").write_text(json.dumps({
        "kind": "bg",
        "pid": os.getppid(),
        "jobId": sys.argv[2],
        "sessionId": "sid-fake",
        "messagingSocketPath": str(sock),
        "cwd": os.getcwd(),
        "status": "idle",
    }))
"""
    )
    script.chmod(0o755)
    return script


def _record_member_job(tmp_path: Path, pane: str, job_id: str) -> None:
    control = tmp_path / "claude-home" / "hive-control"
    control.mkdir(parents=True, exist_ok=True)
    slug = pane.replace("%", "")
    (control / f"hive-pane-{slug}.job").write_text(
        json.dumps({"jobId": job_id, "sessionId": "sid-fake", "cwd": str(tmp_path)})
    )


def test_claude_member_sendback_goes_through_the_pipe_not_tmux(tmp_path):
    # The pane records a bg job, so its keyboard is the job: the sendback is
    # piped into `claude attach <jobId>` and no keystroke goes near the pane.
    _write_fake_claude(tmp_path)
    _record_member_job(tmp_path, "%1", "cafe1234")
    claude_log = tmp_path / "fake-claude.log"
    actions = _run_command_actions(
        tmp_path,
        current_pane="%1",
        panes=[dict(_CLAUDE_PANE)],
        extra_env={
            "FAKE_EDITOR_APPEND_TEXT": "new line added",
            "FAKE_CLAUDE_LOG": str(claude_log),
        },
    )

    assert "attach cafe1234" in claude_log.read_text()
    assert not any(
        event["cmd"] in {"send-keys", "load-buffer", "paste-buffer"}
        for event in actions
    )


def test_claude_member_unedited_save_interrupts_only_when_asked(tmp_path):
    # Nothing was edited, so the sendback is just the interrupt — and the pipe
    # honours the same two switches the tmux path does instead of always
    # interrupting.
    def run(where: Path, env: dict[str, str]) -> str:
        where.mkdir(parents=True, exist_ok=True)
        _write_fake_claude(where)
        _record_member_job(where, "%1", "cafe1234")
        claude_log = where / "fake-claude.log"
        claude_log.touch()
        actions = _run_command_actions(
            where,
            current_pane="%1",
            panes=[dict(_CLAUDE_PANE)],
            extra_env={"FAKE_CLAUDE_LOG": str(claude_log), **env},
        )
        # Whatever it decided, it decided over the pipe: no keys near the pane.
        assert not any(event["cmd"] == "send-keys" for event in actions)
        return claude_log.read_text()

    assert "attach cafe1234" in run(tmp_path / "default", {})
    assert "attach" not in run(
        tmp_path / "no-interrupt",
        {"CVIM_INTERRUPT_BEFORE_PASTE": "0", "CVIM_INTERRUPT_AFTER_PASTE": "0"},
    )


def test_claude_pane_without_a_job_record_still_types_through_tmux(tmp_path):
    # A plain interactive claude on the pane tty (fake ps), no job record:
    # nothing to pipe into, so the sendback is typed like any other CLI's.
    actions = _run_command_actions(
        tmp_path,
        current_pane="%1",
        panes=[dict(_CLAUDE_PANE)],
        extra_env={"FAKE_EDITOR_APPEND_TEXT": "new line added"},
    )

    assert any(event["cmd"] == "paste-buffer" for event in actions)
    assert any(
        event["cmd"] == "send-keys" and event["args"][-1] == "Enter"
        for event in actions
    )


def test_claude_post_refuses_when_the_pane_only_holds_an_attach_viewer(tmp_path):
    # No job record, and the claude on the tty is an attach viewer: its
    # composer belongs to whatever session it displays, so typing there would
    # land the sendback in a stranger's turn.
    actions = _run_command_actions(
        tmp_path,
        current_pane="%1",
        panes=[dict(_CLAUDE_PANE)],
        extra_env={
            "FAKE_EDITOR_APPEND_TEXT": "new line added",
            "FAKE_PS_OUTPUT": "456 claude claude attach beef5678",
        },
    )

    assert not any(
        event["cmd"] in {"send-keys", "load-buffer", "paste-buffer"}
        for event in actions
    )


def test_claude_post_refuses_when_nothing_is_running_on_the_pane(tmp_path):
    # No job record and no claude process on the pane tty: no keystroke may
    # reach the pane shell.
    actions = _run_command_actions(
        tmp_path,
        current_pane="%1",
        panes=[dict(_CLAUDE_PANE)],
        extra_env={
            "FAKE_EDITOR_APPEND_TEXT": "new line added",
            "FAKE_PS_OUTPUT": "",
        },
    )

    assert not any(
        event["cmd"] in {"send-keys", "load-buffer", "paste-buffer"}
        for event in actions
    )


def _record_pane_thread(tmp_path: Path, pane: str, thread_id: str) -> Path:
    control = tmp_path / "codex-home" / "app-server-control"
    control.mkdir(parents=True, exist_ok=True)
    slug = pane.replace("%", "")
    (control / f"hive-pane-{slug}.thread").write_text(
        json.dumps({"threadId": thread_id, "cwd": str(tmp_path)})
    )
    return tmp_path / "codex-home"


def test_codex_pane_with_a_recorded_thread_never_reaches_for_tmux(tmp_path):
    # The thread record is the pane's engine address, so the sendback is a
    # turn/interrupt + turn/start on it. Here the daemon is absent (scratch
    # CODEX_HOME, no socket), which is a failed delivery — and a failed
    # native delivery still never degrades into keystrokes.
    cache_dir = tmp_path / "cache"
    codex_home = _record_pane_thread(tmp_path, "%1", "th-cafe")
    actions = _run_command_actions(
        tmp_path,
        current_pane="%1",
        panes=[
            {"id": "%1", "left": 0, "top": 0, "width": 200, "height": 100,
             "command": "codex"},
        ],
        extra_env={
            "FAKE_EDITOR_APPEND_TEXT": "new line added",
            "CODEX_HOME": str(codex_home),
            "XDG_CACHE_HOME": str(cache_dir),
        },
    )

    records = _read_latest_cvim_records(cache_dir)

    assert not any(
        event["cmd"] in {"send-keys", "load-buffer", "paste-buffer", "capture-pane"}
        for event in actions
    )
    assert any(
        record.get("event") == "post.sendback"
        and "route=codexThread" in str(record.get("message", ""))
        for record in records
    )
    # The user's edit outlived the refusal instead of being deleted with the
    # popup's tmpdir.
    retained = [
        record for record in records if record.get("event") == "post.sendback_retained"
    ]
    assert len(retained) == 1
    send_file = str(retained[0]["message"]).split("send_file=", 1)[1]
    assert Path(send_file).read_text().strip() == "new line added"


def test_popup_schedules_post_after_popup_exits(tmp_path):
    actions = _run_command_actions(
        tmp_path,
        current_pane="%1",
        panes=[
            {"id": "%1", "left": 0, "top": 0, "width": 200, "height": 100},
        ],
        extra_env={
            "FAKE_EDITOR_APPEND_TEXT": "new line added",
            "FAKE_TMUX_CAPTURE_PANE_TEXT": "ready for input\n> [<comment on=\"previous_reply\"> pasted]",
            "FAKE_TMUX_MARK_POPUP_CONTEXT": "1",
            "FAKE_TMUX_DROP_RUN_SHELL_IN_POPUP": "1",
        },
        use_default_delays=True,
    )

    run_shell_events = [event for event in actions if event["cmd"] == "run-shell"]

    assert len(run_shell_events) == 1
    assert not run_shell_events[0].get("dropped", False)
    assert any(event["cmd"] == "paste-buffer" for event in actions)
    assert any(
        event["cmd"] == "send-keys" and event["args"][-1] == "Enter"
        for event in actions
    )


def test_edited_save_waits_for_structured_input_before_submit(tmp_path):
    actions = _run_command_actions(
        tmp_path,
        current_pane="%1",
        panes=[
            {"id": "%1", "left": 0, "top": 0, "width": 200, "height": 100},
        ],
        extra_env={
            "CVIM_OUTPUT_MODE": "diff",
            "FAKE_EDITOR_APPEND_TEXT": "new line added",
            "FAKE_TMUX_CAPTURE_PANE_SEQUENCE": json.dumps([
                "ready for input\n> still empty",
                "ready for input\n> [<comment on=\"previous_reply\"> pasted]",
            ]),
        },
        use_default_delays=True,
    )

    capture_events = [event for event in actions if event["cmd"] == "capture-pane"]
    enter_events = [
        event for event in actions
        if event["cmd"] == "send-keys" and event["args"][-1] == "Enter"
    ]

    assert len(capture_events) == 2
    assert len(enter_events) == 1


def test_edited_save_skips_submit_when_probe_never_finds_structured_input(tmp_path):
    cache_dir = tmp_path / "cache"
    actions = _run_command_actions(
        tmp_path,
        current_pane="%1",
        panes=[
            {"id": "%1", "left": 0, "top": 0, "width": 200, "height": 100},
        ],
        extra_env={
            "CVIM_OUTPUT_MODE": "diff",
            "FAKE_EDITOR_APPEND_TEXT": "new line added",
            "FAKE_TMUX_CAPTURE_PANE_SEQUENCE": json.dumps([
                "ready for input\n> still empty",
                "ready for input\n> still empty",
                "ready for input\n> still empty",
                "ready for input\n> still empty",
                "ready for input\n> still empty",
            ]),
            "XDG_CACHE_HOME": str(cache_dir),
        },
        use_default_delays=True,
    )

    records = _read_latest_cvim_records(cache_dir)
    events = [record["event"] for record in records]

    assert any(event["cmd"] == "paste-buffer" for event in actions)
    assert len([event for event in actions if event["cmd"] == "capture-pane"]) == 5
    assert not any(
        event["cmd"] == "send-keys" and event["args"][-1] == "Enter"
        for event in actions
    )
    assert "post.probe.failed" in events
    assert any(
        record.get("event") == "post.probe.failed"
        and "attempts=4" in str(record.get("message", ""))
        for record in records
    )
    assert any(
        record.get("event") == "post.submit_skipped"
        and "reason=missing_submit_ready_input_after_paste" in str(record.get("message", ""))
        for record in records
    )
    assert any(
        record.get("event") == "post.capture.after_paste_failed"
        and record.get("message") == "> still empty"
        for record in records
    )


def test_popup_debug_log_records_sendback_stages(tmp_path):
    cache_dir = tmp_path / "cache"
    actions = _run_command_actions(
        tmp_path,
        current_pane="%1",
        panes=[
            {"id": "%1", "left": 0, "top": 0, "width": 200, "height": 100},
        ],
        extra_env={
            "CVIM_OUTPUT_MODE": "diff",
            "FAKE_EDITOR_APPEND_TEXT": "new line added",
            "FAKE_TMUX_CAPTURE_PANE_TEXT": "ready for input\n> [<comment on=\"previous_reply\"> pasted diff]",
            "XDG_CACHE_HOME": str(cache_dir),
        },
        use_default_delays=True,
    )

    records = _read_latest_cvim_records(cache_dir)
    events = [record["event"] for record in records]

    assert any(event["cmd"] == "paste-buffer" for event in actions)
    assert len([event for event in actions if event["cmd"] == "capture-pane"]) == 1
    assert "command.start" in events
    assert "popup.open" in events
    assert "helper.payload_ready" in events
    assert "queue_post_run.enqueue" in events
    assert any(
        record.get("event") == "post.probe.ready"
        and "attempt=1" in str(record.get("message", ""))
        for record in records
    )
    assert "post.submit" in events


def test_popup_debug_log_uses_workspace_run_dir(tmp_path):
    workspace = tmp_path / "ws"
    actions = _run_command_actions(
        tmp_path,
        current_pane="%1",
        panes=[
            {"id": "%1", "left": 0, "top": 0, "width": 200, "height": 100},
        ],
        extra_env={
            "FAKE_EDITOR_APPEND_TEXT": "new line added",
            "FAKE_TMUX_CAPTURE_PANE_TEXT": "ready for input\n> pasted",
        },
        workspace=workspace,
    )

    latest_file = workspace / "run" / "cvim" / "latest"
    log_path = Path(latest_file.read_text().strip())
    records = [json.loads(line) for line in log_path.read_text().splitlines() if line.strip()]

    assert any(event["cmd"] == "paste-buffer" for event in actions)
    assert log_path.parent == workspace / "run" / "cvim"
    assert records[0]["workspace"] == str(workspace)
    assert records[0]["component"] == "cvim"
    assert records[0]["ts"].endswith("Z")
    if "." in records[0]["ts"]:
        assert len(records[0]["ts"].rsplit(".", 1)[1]) == 4


def _write_capturing_fake_vim(tmp_path: Path, log_path: Path) -> Path:
    script = tmp_path / "fake-vim"
    script.write_text(
        f"""#!/usr/bin/env python3
import json
import os
import sys

log_path = {str(log_path)!r}
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
record = {{
    "argv": sys.argv,
    "env": {{k: os.environ.get(k, "") for k in (
        "CVIM_MENU_JSON", "CVIM_SEEDS_DIR", "CVIM_MSG_FILE",
        "CVIM_ORIG_FILE", "CVIM_OFFSET_FILE", "CVIM_MENU_SELECTED_FILE",
    )}},
    "menu": menu,
    "seeds": sorted(os.listdir(os.environ.get("CVIM_SEEDS_DIR", "") or "."))
        if os.environ.get("CVIM_SEEDS_DIR") and os.path.isdir(os.environ["CVIM_SEEDS_DIR"]) else [],
    "msg_content": msg_content,
}}
with open(log_path, "w") as fh:
    json.dump(record, fh)
"""
    )
    script.chmod(0o755)
    return script


def _run_menu_command(
    tmp_path: Path,
    *,
    transcript_rows: list[dict[str, object]],
    extra_env: dict[str, str] | None = None,
    seed_mode: str = "session",
    vim_args: list[str] | None = None,
) -> dict[str, object]:
    transcript = tmp_path / "session.jsonl"
    transcript.write_text("".join(json.dumps(row) + "\n" for row in transcript_rows))

    bundle_root = tmp_path / "cvim-bundle"
    shutil.copytree(ROOT / "src" / "hive" / "core_assets" / "cvim", bundle_root)
    for path in (bundle_root / "bin").iterdir():
        if path.is_file():
            path.chmod(0o755)
    session_helper = bundle_root / "bin" / "cvim-session"
    session_helper.write_text(f"#!/usr/bin/env python3\nprint({json.dumps(str(transcript))})\n")
    session_helper.chmod(0o755)
    command = bundle_root / "bin" / "cvim-command"

    _write_fake_tmux(tmp_path)
    _write_fake_ps(tmp_path)
    editor_log = tmp_path / "editor.json"
    fake_vim = _write_capturing_fake_vim(tmp_path, editor_log)

    log_path = tmp_path / "tmux-log.json"
    actions_path = tmp_path / "tmux-actions.jsonl"
    state = {
        "current_pane": "%1",
        "client_width": 200,
        "client_height": 100,
        "panes": [{"id": "%1", "left": 0, "top": 0, "width": 200, "height": 100}],
    }
    env = os.environ.copy()
    env["PATH"] = f"{tmp_path}:{env.get('PATH', '')}"
    env["TMUX"] = "/tmp/tmux-test"
    env["TMUX_PANE"] = "%1"
    env["CVIM_SEED_MODE"] = seed_mode
    env["CVIM_OUTPUT_MODE"] = "text"
    env["CVIM_EDITOR"] = str(fake_vim)
    env["CVIM_PASTE_DELAY"] = "0"
    env["CVIM_INTERRUPT_SETTLE_DELAY"] = "0"
    env["CVIM_SUBMIT_DELAY"] = "0"
    env["FAKE_TMUX_STATE"] = json.dumps(state)
    env["FAKE_TMUX_LOG"] = str(log_path)
    env["FAKE_TMUX_ACTIONS"] = str(actions_path)
    env["FAKE_TMUX_EXEC_POPUP"] = "1"
    env.update(_isolated_env(tmp_path))
    if extra_env:
        env.update(extra_env)

    cmd_args = ["bash", str(command), "vim"]
    if vim_args:
        cmd_args.extend(vim_args)
    subprocess.run(cmd_args, check=True, env=env, cwd=ROOT)
    return json.loads(editor_log.read_text())


def test_cvim_menu_mode_activates_with_session_seed_and_no_offset(tmp_path):
    record = _run_menu_command(
        tmp_path,
        transcript_rows=[
            {"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "answer A"}]}},
            {"type": "message", "message": {"role": "user", "content": [{"type": "text", "text": "u"}]}},
            {"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "answer B"}]}},
        ],
    )
    assert "-S" in record["argv"]
    assert any(arg.endswith("menu.vim") for arg in record["argv"])
    assert record["env"]["CVIM_MENU_JSON"].endswith("/menu.json")
    assert record["env"]["CVIM_SEEDS_DIR"].endswith("/seeds")
    assert record["env"]["CVIM_MSG_FILE"].endswith("/message.md")
    assert record["env"]["CVIM_ORIG_FILE"].endswith("/original.md")
    assert record["env"]["CVIM_OFFSET_FILE"].endswith("/offset")
    assert record["env"]["CVIM_MENU_SELECTED_FILE"].endswith("/menu_selected")
    assert record["menu"] is not None
    assert [entry["offset"] for entry in record["menu"]] == [0, 1]
    assert "answer B" in record["menu"][0]["label"]
    assert "answer A" in record["menu"][1]["label"]
    assert set(record["seeds"]) == {"0.md", "1.md"}
    # menu mode pre-seeds msg_file with offset=0 (newest assistant message)
    # so popup_menu rendering failures (e.g. nested tmux popup + claude
    # alt-screen) leave a usable buffer instead of an empty one.
    assert "answer B" in record["msg_content"]


def test_cvim_menu_mode_skipped_when_offset_flag_present(tmp_path):
    record = _run_menu_command(
        tmp_path,
        transcript_rows=[
            {"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "a"}]}},
            {"type": "message", "message": {"role": "user", "content": [{"type": "text", "text": "u"}]}},
            {"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "b"}]}},
        ],
        vim_args=["-1"],
    )
    assert "-S" not in record["argv"]
    assert record["env"]["CVIM_MENU_JSON"] == ""
    assert record["menu"] is None


def test_cvim_menu_mode_skipped_in_blank_seed_mode(tmp_path):
    record = _run_menu_command(
        tmp_path,
        transcript_rows=[
            {"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "a"}]}},
        ],
        seed_mode="blank",
    )
    assert "-S" not in record["argv"]
    assert record["env"]["CVIM_MENU_JSON"] == ""


def test_cvim_menu_mode_falls_back_when_transcript_has_no_assistant(tmp_path):
    record = _run_menu_command(
        tmp_path,
        transcript_rows=[
            {"type": "message", "message": {"role": "user", "content": [{"type": "text", "text": "u"}]}},
        ],
    )
    assert "-S" not in record["argv"]
    assert record["env"]["CVIM_MENU_JSON"] == ""


def test_unedited_save_interrupts_without_paste_or_submit(tmp_path):
    actions = _run_command_actions(
        tmp_path,
        current_pane="%1",
        panes=[
            {"id": "%1", "left": 0, "top": 0, "width": 200, "height": 100},
        ],
    )

    escape_events = [
        event for event in actions
        if event["cmd"] == "send-keys" and event["args"][-1] == "Escape"
    ]

    assert len(escape_events) == 1
    assert not any(event["cmd"] in {"load-buffer", "paste-buffer"} for event in actions)
    assert not any(
        event["cmd"] == "send-keys" and event["args"][-1] == "Enter"
        for event in actions
    )
