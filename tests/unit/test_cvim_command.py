from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
COMMAND = ROOT / "src" / "hive" / "plugins" / "cvim" / "bin" / "cvim-command"
SHARED_DIR = ROOT / "src" / "hive" / "plugins" / "cvim" / "bin"


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
        "#{pane_current_command}": pane.get("command", "droid"),
        "#{pane_title}": pane.get("title", ""),
        "#{session_id}": state.get("session_id", "$1"),
        "#{window_id}": state.get("window_id", "@1"),
        "#{pane_pid}": pane.get("pid", 1234),
        "#{pane_tty}": pane.get("tty", "/dev/ttys001"),
        "#{client_tty}": pane.get("client_tty", "/dev/ttys010"),
        "#{client_pid}": pane.get("client_pid", 4321),
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
    shutil.copytree(ROOT / "src" / "hive" / "plugins" / "cvim", bundle_root)
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
    env["DROID_VIM_TRANSPORT"] = "popup"
    env["DROID_VIM_SEED_MODE"] = "blank"
    env["DROID_VIM_EDITOR"] = "sh"
    env["FAKE_TMUX_STATE"] = json.dumps(state)
    env["FAKE_TMUX_LOG"] = str(log_path)
    if extra_env:
        env.update(extra_env)

    subprocess.run([str(command)], check=True, env=env, cwd=ROOT)
    return json.loads(log_path.read_text())


def _run_command_actions(
    tmp_path: Path,
    *,
    current_pane: str,
    panes: list[dict[str, object]],
    extra_env: dict[str, str] | None = None,
    use_default_delays: bool = False,
) -> list[dict[str, object]]:
    _write_fake_tmux(tmp_path)
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
    env = os.environ.copy()
    env["PATH"] = f"{tmp_path}:{env.get('PATH', '')}"
    env["TMUX"] = "/tmp/tmux-test"
    env["TMUX_PANE"] = current_pane
    env["DROID_VIM_TRANSPORT"] = "popup"
    env["DROID_VIM_SEED_MODE"] = "blank"
    env["DROID_VIM_OUTPUT_MODE"] = "text"
    env["DROID_VIM_EDITOR"] = str(editor)
    if not use_default_delays:
        env["DROID_VIM_PASTE_DELAY"] = "0"
        env["DROID_VIM_INTERRUPT_SETTLE_DELAY"] = "0"
        env["DROID_VIM_SUBMIT_DELAY"] = "0"
    env["FAKE_TMUX_STATE"] = json.dumps(state)
    env["FAKE_TMUX_LOG"] = str(log_path)
    env["FAKE_TMUX_ACTIONS"] = str(actions_path)
    env["FAKE_TMUX_EXEC_POPUP"] = "1"
    if extra_env:
        env.update(extra_env)

    subprocess.run([str(command)], check=True, env=env, cwd=ROOT)
    return [json.loads(line) for line in actions_path.read_text().splitlines() if line.strip()]


def _popup_geometry(log_record: dict[str, object]) -> tuple[str, str, str, str]:
    args = log_record["args"]
    assert log_record["cmd"] == "display-popup"
    return (
        args[args.index("-x") + 1],
        args[args.index("-y") + 1],
        args[args.index("-w") + 1],
        args[args.index("-h") + 1],
    )


def test_popup_uses_right_half_when_only_one_pane(tmp_path):
    record = _run_command(
        tmp_path,
        current_pane="%1",
        panes=[
            {"id": "%1", "left": 0, "top": 0, "width": 200, "height": 100},
        ],
    )

    assert _popup_geometry(record) == ("100", "0", "100", "100")


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

    assert _popup_geometry(leftmost) == ("50", "0", "150", "100")
    assert _popup_geometry(mid_right) == ("150", "0", "50", "100")
    assert _popup_geometry(rightmost) == ("0", "0", "150", "100")


def test_popup_uses_only_right_column_in_tiled_layout(tmp_path):
    panes = [
        {"id": "%1", "left": 0, "top": 0, "width": 100, "height": 50},
        {"id": "%2", "left": 100, "top": 0, "width": 100, "height": 50},
        {"id": "%3", "left": 0, "top": 50, "width": 100, "height": 50},
        {"id": "%4", "left": 100, "top": 50, "width": 100, "height": 50},
    ]

    record = _run_command(tmp_path, current_pane="%1", panes=panes)

    assert _popup_geometry(record) == ("100", "0", "100", "100")


def test_popup_uses_lower_half_in_top_bottom_layout(tmp_path):
    panes = [
        {"id": "%1", "left": 0, "top": 0, "width": 200, "height": 50},
        {"id": "%2", "left": 0, "top": 50, "width": 200, "height": 50},
    ]

    top = _run_command(tmp_path / "top", current_pane="%1", panes=panes)
    bottom = _run_command(tmp_path / "bottom", current_pane="%2", panes=panes)

    assert _popup_geometry(top) == ("0", "50", "200", "50")
    assert _popup_geometry(bottom) == ("0", "0", "200", "50")


def test_popup_respects_optional_size_overrides_within_target_region(tmp_path):
    panes = [
        {"id": "%1", "left": 0, "top": 0, "width": 100, "height": 100},
        {"id": "%2", "left": 100, "top": 0, "width": 100, "height": 100},
    ]

    record = _run_command(
        tmp_path,
        current_pane="%1",
        panes=panes,
        extra_env={
            "DROID_VIM_POPUP_WIDTH": "80",
            "DROID_VIM_POPUP_HEIGHT": "40",
        },
    )

    assert _popup_geometry(record) == ("120", "30", "80", "40")


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

    assert _popup_geometry(record) == ("0", "0", "100", "100")


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

    assert _popup_geometry(record) == ("50", "0", "150", "100")


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

    assert _popup_geometry(record) == ("150", "0", "50", "100")


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
            "DROID_VIM_OUTPUT_MODE": "diff",
            "FAKE_EDITOR_APPEND_TEXT": "new line added",
            "FAKE_TMUX_CAPTURE_PANE_TEXT": "❯ [Pasted text #1 +10 lines]",
            "XDG_CACHE_HOME": str(cache_dir),
        },
        use_default_delays=True,
    )

    latest_file = cache_dir / "cvim" / "debug" / "latest"
    log_path = Path(latest_file.read_text().strip())
    log_text = log_path.read_text()

    assert any(event["cmd"] == "paste-buffer" for event in actions)
    assert any(
        event["cmd"] == "send-keys" and event["args"][-1] == "Enter"
        for event in actions
    )
    assert "matcher=claude_pasted_placeholder" in log_text


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
            "DROID_VIM_OUTPUT_MODE": "diff",
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
            "DROID_VIM_OUTPUT_MODE": "diff",
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

    latest_file = cache_dir / "cvim" / "debug" / "latest"
    log_path = Path(latest_file.read_text().strip())
    log_text = log_path.read_text()

    assert any(event["cmd"] == "paste-buffer" for event in actions)
    assert len([event for event in actions if event["cmd"] == "capture-pane"]) == 5
    assert not any(
        event["cmd"] == "send-keys" and event["args"][-1] == "Enter"
        for event in actions
    )
    assert "post.probe.failed label=after_paste attempts=4" in log_text
    assert "post.submit_skipped reason=missing_submit_ready_input_after_paste" in log_text
    assert "post.capture.after_paste_failed > still empty" in log_text


def test_popup_debug_log_records_sendback_stages(tmp_path):
    cache_dir = tmp_path / "cache"
    actions = _run_command_actions(
        tmp_path,
        current_pane="%1",
        panes=[
            {"id": "%1", "left": 0, "top": 0, "width": 200, "height": 100},
        ],
        extra_env={
            "DROID_VIM_OUTPUT_MODE": "diff",
            "FAKE_EDITOR_APPEND_TEXT": "new line added",
            "FAKE_TMUX_CAPTURE_PANE_TEXT": "ready for input\n> [<comment on=\"previous_reply\"> pasted diff]",
            "XDG_CACHE_HOME": str(cache_dir),
        },
        use_default_delays=True,
    )

    latest_file = cache_dir / "cvim" / "debug" / "latest"
    log_path = Path(latest_file.read_text().strip())
    log_text = log_path.read_text()

    assert any(event["cmd"] == "paste-buffer" for event in actions)
    assert len([event for event in actions if event["cmd"] == "capture-pane"]) == 1
    assert "command.start" in log_text
    assert "popup.open" in log_text
    assert "helper.payload_ready" in log_text
    assert "queue_post_run.enqueue" in log_text
    assert "post.probe.ready label=after_paste attempt=1" in log_text
    assert "post.submit" in log_text


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
record = {{
    "argv": sys.argv,
    "env": {{k: os.environ.get(k, "") for k in (
        "CVIM_MENU_JSON", "CVIM_SEEDS_DIR", "CVIM_MSG_FILE",
        "CVIM_ORIG_FILE", "CVIM_OFFSET_FILE", "CVIM_MENU_SELECTED_FILE",
    )}},
    "menu": menu,
    "seeds": sorted(os.listdir(os.environ.get("CVIM_SEEDS_DIR", "") or "."))
        if os.environ.get("CVIM_SEEDS_DIR") and os.path.isdir(os.environ["CVIM_SEEDS_DIR"]) else [],
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
    shutil.copytree(ROOT / "src" / "hive" / "plugins" / "cvim", bundle_root)
    for path in (bundle_root / "bin").iterdir():
        if path.is_file():
            path.chmod(0o755)
    session_helper = bundle_root / "bin" / "cvim-session"
    session_helper.write_text(f"#!/usr/bin/env bash\nprintf '%s\\n' {subprocess.list2cmdline([str(transcript)])}\n")
    session_helper.chmod(0o755)
    command = bundle_root / "bin" / "cvim-command"

    _write_fake_tmux(tmp_path)
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
    env["DROID_VIM_TRANSPORT"] = "popup"
    env["DROID_VIM_SEED_MODE"] = seed_mode
    env["DROID_VIM_OUTPUT_MODE"] = "text"
    env["DROID_VIM_EDITOR"] = str(fake_vim)
    env["DROID_VIM_PASTE_DELAY"] = "0"
    env["DROID_VIM_INTERRUPT_SETTLE_DELAY"] = "0"
    env["DROID_VIM_SUBMIT_DELAY"] = "0"
    env["FAKE_TMUX_STATE"] = json.dumps(state)
    env["FAKE_TMUX_LOG"] = str(log_path)
    env["FAKE_TMUX_ACTIONS"] = str(actions_path)
    env["FAKE_TMUX_EXEC_POPUP"] = "1"
    if extra_env:
        env.update(extra_env)

    cmd_args = [str(command)]
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
