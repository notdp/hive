from pathlib import Path

import pytest
from click.testing import CliRunner

from hive.tmux import PaneInfo


@pytest.fixture(autouse=True)
def _isolate_notify_debug_global_log(tmp_path, monkeypatch):
    """Prevent notify_debug.emit from writing to the real ~/.cache/hive log."""
    from hive import notify_debug

    monkeypatch.setattr(notify_debug, "_GLOBAL_LOG", tmp_path / "notify-debug-isolation.jsonl")


@pytest.fixture(autouse=True)
def _isolate_codex_tool_env(monkeypatch):
    """Prevent the host Codex tool env from leaking into CLI tests."""
    monkeypatch.delenv("CODEX_THREAD_ID", raising=False)
    monkeypatch.delenv("HIVE_CODEX_PANE", raising=False)


@pytest.fixture
def runner(monkeypatch) -> CliRunner:
    # CLI tests assume an in-tmux caller unless a test says otherwise
    # (configure_hive_home(tmux_inside=False) or a local patch overrides this
    # later). Without the pin, the verdict of every bare-runner test depends
    # on where pytest itself runs: from a shell outside tmux the root gate
    # fails every non-optional command.
    monkeypatch.setattr("hive.tmux.is_inside_tmux", lambda: True)
    return CliRunner()


class FakeTmuxState:
    """In-memory tmux state for testing. Tracks window and pane options."""

    def __init__(self):
        self.window_options: dict[str, dict[str, str]] = {}
        self.pane_options: dict[str, dict[str, str]] = {}
        self.pane_alive: dict[str, bool] = {}

    def set_window_option(self, target: str, option: str, value: str) -> None:
        key = option.removeprefix("@")
        self.window_options.setdefault(target, {})[key] = value

    def get_window_option(self, target: str, key: str) -> str | None:
        return self.window_options.get(target, {}).get(key)

    def clear_window_option(self, target: str, option: str) -> None:
        key = option.removeprefix("@")
        self.window_options.get(target, {}).pop(key, None)

    def tag_pane(self, pane_id: str, role: str, agent: str, team: str, *, cli: str = "", group: str = "") -> None:
        opts = {"hive-role": role, "hive-agent": agent, "hive-team": team}
        if cli:
            opts["hive-cli"] = cli
        if group:
            opts["hive-group"] = group
        self.pane_options[pane_id] = {**self.pane_options.get(pane_id, {}), **opts}

    def get_pane_option(self, pane_id: str, key: str) -> str | None:
        return self.pane_options.get(pane_id, {}).get(key)

    def clear_pane_tags(self, pane_id: str) -> None:
        self.pane_options.pop(pane_id, None)

    def window_id_for_target(self, target: str) -> str:
        suffix = target.split(":")[-1] if ":" in target else "0"
        return f"@{suffix}"

    def find_team_window(self, name: str, *, prefer_pane: str = "") -> tuple[str, dict[str, str]]:
        for target, opts in self.window_options.items():
            if opts.get("hive-team") == name:
                return target, {
                    "window_id": self.window_id_for_target(target),
                    "workspace": opts.get("hive-workspace", ""),
                    "desc": opts.get("hive-desc", ""),
                    "created": opts.get("hive-created", "0"),
                }
        return "", {}

    def list_teams(self) -> list[dict[str, str]]:
        teams = []
        for target, opts in self.window_options.items():
            team_name = opts.get("hive-team")
            if team_name:
                teams.append({
                    "name": team_name,
                    "tmuxWindow": target,
                    "tmuxSession": target.split(":")[0] if ":" in target else "",
                    "workspace": opts.get("hive-workspace", ""),
                })
        return teams

    def list_panes_full(self, target: str) -> list[PaneInfo]:
        result = []
        for pane_id, opts in self.pane_options.items():
            if opts.get("hive-team") and target:
                result.append(PaneInfo(
                    pane_id=pane_id,
                    title="",
                    command="claude",
                    role=opts.get("hive-role", ""),
                    agent=opts.get("hive-agent", ""),
                    team=opts.get("hive-team", ""),
                    cli=opts.get("hive-cli", ""),
                    group=opts.get("hive-group", ""),
                ))
        return result

    def list_panes_all(self) -> list[PaneInfo]:
        result = []
        for pane_id, opts in self.pane_options.items():
            result.append(PaneInfo(
                pane_id=pane_id,
                title="",
                command="claude",
                role=opts.get("hive-role", ""),
                agent=opts.get("hive-agent", ""),
                team=opts.get("hive-team", ""),
                cli=opts.get("hive-cli", ""),
                group=opts.get("hive-group", ""),
            ))
        return result


@pytest.fixture
def configure_hive_home(monkeypatch, tmp_path):
    def _configure(*, tmux_inside: bool = True, current_pane: str = "%0", session_name: str = "dev"):
        hive_home = tmp_path / ".hive"
        codex_home = tmp_path / ".codex"
        claude_home = tmp_path / ".claude"
        monkeypatch.setenv("HIVE_HOME", str(hive_home))
        monkeypatch.setenv("CODEX_HOME", str(codex_home))
        monkeypatch.setenv("CLAUDE_HOME", str(claude_home))
        monkeypatch.setenv("XDG_CACHE_HOME", str(tmp_path / ".cache"))
        # The test process may itself run inside a desktop Claude session; its
        # inbox socket must never make the guest-send gate treat a test as
        # that session.
        monkeypatch.delenv("CLAUDE_CODE_MESSAGING_SOCKET", raising=False)
        monkeypatch.setattr("hive.team.HIVE_HOME", hive_home)
        monkeypatch.setattr("hive.agent.detect_current_session_id", lambda _cwd, model="", pane_id="": None)
        monkeypatch.setattr("hive.cli.HIVE_HOME", hive_home)
        monkeypatch.setattr("hive.context.HIVE_HOME", hive_home)
        monkeypatch.setattr("hive.context.CONTEXT_DIR", hive_home / "contexts")
        monkeypatch.setattr("hive.context.CURRENT_CONTEXT_FILE", hive_home / "current.json")

        state = FakeTmuxState()

        # team.tmux mocks
        monkeypatch.setattr("hive.team.tmux.is_inside_tmux", lambda: tmux_inside)
        monkeypatch.setattr("hive.team.tmux.get_current_pane_id", lambda: current_pane)
        monkeypatch.setattr("hive.team.tmux.get_current_session_name", lambda: session_name)
        monkeypatch.setattr("hive.team.tmux.get_current_window_target", lambda: f"{session_name}:0")
        monkeypatch.setattr("hive.team.tmux.get_current_window_id", lambda: state.window_id_for_target(f"{session_name}:0"))
        monkeypatch.setattr("hive.team.tmux.get_window_id", lambda target: state.window_id_for_target(target))
        monkeypatch.setattr("hive.team.tmux.get_pane_current_command", lambda _pane: "claude")
        monkeypatch.setattr("hive.team.tmux.has_session", lambda _name: True)
        monkeypatch.setattr("hive.team.tmux.is_pane_alive", lambda _pane: True)
        monkeypatch.setattr("hive.team.tmux.tag_pane", state.tag_pane)
        monkeypatch.setattr("hive.team.tmux.clear_pane_tags", state.clear_pane_tags)
        monkeypatch.setattr("hive.team.tmux.set_window_option", state.set_window_option)
        monkeypatch.setattr("hive.team.tmux.get_window_option", state.get_window_option)
        monkeypatch.setattr("hive.team.tmux.clear_window_option", state.clear_window_option)
        monkeypatch.setattr("hive.team.tmux.list_panes_full", state.list_panes_full)

        # Status-aware variant: the fake tmux always answers, so a state-backed
        # listing counts as successful (never None/unknown). Duo placement
        # derives pane count + neighbors from this snapshot and aborts when the
        # current pane is missing, so the default snapshot always contains the
        # current pane (as an untagged single pane → no break-out), mirroring
        # the old get_pane_count=1 safety default.
        def _fake_list_panes_full_or_none(target):
            from hive import tmux as tmux_mod

            panes = list(state.list_panes_full(target))
            # Resolve at call time: tests override get_current_pane_id per-case.
            cur = tmux_mod.get_current_pane_id() or current_pane
            if not any(p.pane_id == cur for p in panes):
                panes.append(PaneInfo(pane_id=cur, title=""))
            return panes

        monkeypatch.setattr("hive.team.tmux.list_panes_full_or_none", _fake_list_panes_full_or_none)
        monkeypatch.setattr("hive.team._find_team_window", state.find_team_window)
        monkeypatch.setattr("hive.team.list_teams", state.list_teams)

        # cli.tmux mocks
        monkeypatch.setattr("hive.cli.tmux.is_inside_tmux", lambda: tmux_inside)
        monkeypatch.setattr("hive.cli.tmux.get_current_pane_id", lambda: current_pane)
        monkeypatch.setattr("hive.cli.tmux.get_current_session_name", lambda: session_name)
        monkeypatch.setattr("hive.cli.tmux.get_current_window_target", lambda: f"{session_name}:0")
        monkeypatch.setattr("hive.cli.tmux.get_current_window_id", lambda: state.window_id_for_target(f"{session_name}:0"))
        monkeypatch.setattr("hive.cli.tmux.get_window_id", lambda target: state.window_id_for_target(target))
        monkeypatch.setattr("hive.cli.tmux.get_pane_current_command", lambda _pane: "claude")
        monkeypatch.setattr("hive.cli.tmux.get_pane_window_target", lambda _pane: f"{session_name}:0")
        monkeypatch.setattr("hive.cli.tmux.get_pane_option", state.get_pane_option)
        monkeypatch.setattr("hive.cli.tmux.get_window_option", state.get_window_option)
        monkeypatch.setattr("hive.cli.tmux.set_window_option", state.set_window_option)
        # Deterministic global window-status formats (tmux defaults) so
        # `duo set-pr` display derivation never shells to the real server.
        global_window_formats = {
            "window-status-format": "#I:#W#{?window_flags,#{window_flags}, }",
            "window-status-current-format": "#I:#W#{?window_flags,#{window_flags}, }",
        }
        monkeypatch.setattr(
            "hive.cli.tmux.get_global_window_option",
            lambda option: global_window_formats.get(option),
        )
        monkeypatch.setattr("hive.cli.tmux.clear_window_option", state.clear_window_option)
        monkeypatch.setattr("hive.cli.tmux.is_pane_alive", lambda _pane: True)
        monkeypatch.setattr("hive.cli.tmux.tag_pane", state.tag_pane)
        monkeypatch.setattr("hive.cli.tmux.clear_pane_tags", state.clear_pane_tags)
        monkeypatch.setattr("hive.cli.tmux.list_panes_all", state.list_panes_all)
        # Safety: cli tests must never touch the real tmux server. Duo placement
        # decides break-out from the *real* pane count when unmocked, and the
        # placeholder pane ids used in tests (%5, %10, ...) can collide with live
        # panes — so a `hive init` could fire a real `break_pane` against a
        # running session and rip a teammate's pane into a new window. Default to
        # a single-pane window (no break-out) and hard-fail if any test reaches
        # break_pane without mocking it; break-out tests override both.
        monkeypatch.setattr("hive.cli.tmux.get_pane_count", lambda _pane: 1)

        def _guard_break_pane(*_args, **_kwargs):
            raise AssertionError(
                "test reached real tmux.break_pane — mock hive.cli.tmux.break_pane "
                "(and get_pane_count) to exercise break-out without touching live tmux"
            )

        monkeypatch.setattr("hive.cli.tmux.break_pane", _guard_break_pane)
        # Duo windows are renamed after the worker's git branch; keep the suite
        # hermetic — no real tmux rename (would hit a live window on pane-id
        # collision) and no git subprocess against the test cwd.
        monkeypatch.setattr("hive.cli.tmux.rename_window", lambda *_a, **_k: None)
        monkeypatch.setattr("hive.cli._git_branch_for_cwd", lambda _cwd: "")
        # No other live windows by default → duo window names don't collide.
        monkeypatch.setattr("hive.cli.tmux.list_window_names", lambda: [])
        # Duo formation during `hive init` spawns or adopts a validator pane.
        # Tests that want to exercise that flow must override this mock; by
        # default we return a representative descriptor so plain `hive init`
        # tests stay focused on the init scaffolding itself.
        monkeypatch.setattr(
            "hive.cli._attach_duo_to_team",
            lambda t, **_kw: {
                "team": t.name,
                "window": t.tmux_window,
                "group": "duo",
                "worker": {"pane": "%self", "name": "worker", "cli": "claude"},
                "validator": {"pane": "%peer", "name": "validator", "cli": "codex", "mode": "spawned"},
                "dispatched": ["validator"],
                "next": "hive skills get duo-worker",
            },
        )
        monkeypatch.delenv("TMUX_PANE", raising=False)
        # Default: skip the real sidecar fork + 2s socket-ready wait. Tests
        # that want to observe sidecar startup patch this themselves.
        monkeypatch.setattr("hive.sidecar.ensure_sidecar", lambda *args, **kwargs: None, raising=False)
        return hive_home

    return _configure


@pytest.fixture
def mock_tmux_send(monkeypatch):
    sent: list[tuple[str, str]] = []

    def _send_keys(pane, text, enter=True):
        sent.append((pane, text))
        if enter:
            sent.append((pane, "<Enter>"))

    monkeypatch.setattr("hive.agent.tmux.send_keys", _send_keys)
    monkeypatch.setattr("hive.agent.tmux.send_key", lambda pane, key: sent.append((pane, f"<{key}>")))
    monkeypatch.setattr("hive.agent.time.sleep", lambda _s: None)
    return sent


@pytest.fixture(autouse=True)
def _guard_global_zdotdir(request):
    """Fail fast if tmux global env has ZDOTDIR — a leak from manual debugging."""
    if "e2e" not in {m.name for m in request.node.iter_markers()}:
        return
    import shutil
    import subprocess
    if shutil.which("tmux") is None:
        return
    result = subprocess.run(
        ["tmux", "show-environment", "-g", "ZDOTDIR"],
        capture_output=True, text=True,
    )
    if result.returncode == 0 and result.stdout.strip().startswith("ZDOTDIR="):
        value = result.stdout.strip()
        pytest.fail(
            f"tmux global environment is polluted: {value}\n"
            f"This is likely a leak from manual debugging (set-environment without -t).\n"
            f"Fix: tmux set-environment -gr ZDOTDIR"
        )


def pytest_collection_modifyitems(items):
    tests_root = Path(__file__).resolve().parent
    for item in items:
        rel = Path(str(item.path)).resolve().relative_to(tests_root)
        if not rel.parts:
            continue
        top_level = rel.parts[0]
        if top_level in {"unit", "cli", "e2e"}:
            item.add_marker(getattr(pytest.mark, top_level))
