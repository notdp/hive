"""Python CLI vs Rust binary: identical exit code and output.

The corpus is deliberately restricted to invocations with no tmux, spawn, or
network side effects — help text, resolution/validation errors, and
registry-backed reads. Those are exactly the surfaces peers and tests parse,
so drift there is a real regression; the spawn/delivery lanes are covered by
the e2e suite run twice (HIVE_E2E_BIN).
"""
import pytest

pytestmark = pytest.mark.equivalence

ROOT_COMMANDS = [
    "attach", "capture", "ccd", "compact", "config", "create", "cvim", "delete",
    "doctor", "flow", "fork", "hfork", "inject", "interrupt", "join", "kill",
    "layout", "ls", "notify", "plugin", "pr", "resume-hint", "send",
    "shell-init", "spawn", "team", "thread", "vfork", "view", "vim", "worktree",
]

GROUPS = {
    "ccd": ["ls"],
    "config": ["get", "set", "unset"],
    "flow": ["run"],
    "plugin": ["disable", "enable", "list", "ls"],
    "pr": ["clear", "set"],
    "worktree": ["done", "set-base", "start", "status"],
}


def assert_same(sides, args, seed=None):
    py, rs = sides
    if seed:
        seed(py)
        seed(rs)
    py_code, py_out = py.run(args)
    rs_code, rs_out = rs.run(args)
    assert (py_code, py_out) == (rs_code, rs_out), (
        f"$ hive {' '.join(args)}\n"
        f"--- python (exit {py_code}) ---\n{py_out}\n"
        f"--- rust (exit {rs_code}) ---\n{rs_out}"
    )


def test_root_help(sides):
    assert_same(sides, ["--help"])


@pytest.mark.parametrize("command", ROOT_COMMANDS)
def test_command_help(sides, command):
    assert_same(sides, [command, "--help"])


@pytest.mark.parametrize(
    "group,sub",
    [(g, s) for g, subs in GROUPS.items() for s in subs],
)
def test_subcommand_help(sides, group, sub):
    assert_same(sides, [group, sub, "--help"])


def test_unknown_command(sides):
    assert_same(sides, ["definitely-not-a-command"])


def test_ls_on_empty_state(sides):
    assert_same(sides, ["ls"])


def test_ls_with_registered_teams(sides):
    def seed(side):
        side.seed_team("honey", [{"name": "orch", "cli": "claude", "sessionId": "sid-1", "cwd": "/repo"}])
        side.seed_team("comb")

    assert_same(sides, ["ls"], seed)


def test_team_without_a_team_context(sides):
    assert_same(sides, ["team"])


def test_team_by_name(sides):
    def seed(side):
        side.seed_team("honey", [{"name": "orch", "cli": "claude", "sessionId": "sid-1", "cwd": "/repo"}])

    assert_same(sides, ["team", "-t", "honey"], seed)


def test_team_unknown_name(sides):
    assert_same(sides, ["team", "-t", "nope"])


def test_send_without_a_team_context(sides):
    assert_same(sides, ["send", "dodo", "hello"])


def test_send_to_unknown_member(sides):
    def seed(side):
        side.seed_team("honey", [{"name": "orch", "cli": "claude", "sessionId": "sid-1", "cwd": "/repo"}])

    assert_same(sides, ["send", "honey.ghost", "hello"], seed)


def test_send_missing_body(sides):
    assert_same(sides, ["send", "dodo"])


def test_view_unknown_session(sides):
    assert_same(sides, ["view", "no-such-session-id"])


def test_delete_unknown_team(sides):
    assert_same(sides, ["delete", "nope"])


def test_config_get_unset_key(sides):
    assert_same(sides, ["config", "get", "nothing.here"])


def test_config_set_then_get_roundtrip(sides):
    py, rs = sides
    for side in (py, rs):
        set_code, _ = side.run(["config", "set", "ui.theme", "dark"])
        assert set_code == 0, "config set must succeed before the read is meaningful"
    assert_same(sides, ["config", "get", "ui.theme"])


def test_config_unset_missing_key(sides):
    assert_same(sides, ["config", "unset", "nothing.here"])


def test_worktree_status_outside_a_worktree(sides):
    assert_same(sides, ["worktree", "status"])


def test_plugin_list(sides):
    assert_same(sides, ["plugin", "list"])


def test_ccd_ls(sides):
    assert_same(sides, ["ccd", "ls"])


def test_create_rejects_reserved_name(sides):
    assert_same(sides, ["create", "flow"])


def test_join_unknown_team_outside_tmux(sides):
    assert_same(sides, ["join", "ghost-team"])
