"""Launcher option parsing: a following flag is never read as a value.

`hive grok --resume -m grok-4` used to record `-m` as the pane's session id,
and `_codex_opt_value` had the same shape. A token starting with `-` is the
next flag; the option is read as bare instead.
"""
import pytest

from hive import cli


pytestmark = pytest.mark.unit

_OPT_VALUE = pytest.mark.parametrize(
    "opt_value", [cli._grok_opt_value, cli._codex_opt_value], ids=["grok", "codex"]
)


@_OPT_VALUE
def test_a_following_flag_is_not_the_value(opt_value):
    assert opt_value(["--resume", "-m", "grok-4"], ("--resume",)) is None


@_OPT_VALUE
def test_a_trailing_bare_option_has_no_value(opt_value):
    assert opt_value(["--resume"], ("--resume",)) is None


@_OPT_VALUE
def test_a_real_value_still_reads(opt_value):
    assert opt_value(["--resume", "old-sid", "-m", "grok-4"], ("--resume",)) == "old-sid"


@_OPT_VALUE
def test_the_equals_form_still_reads(opt_value):
    assert opt_value(["--resume=old-sid"], ("--resume",)) == "old-sid"


def test_codex_cwd_does_not_swallow_the_next_flag():
    assert cli._codex_opt_value(["--cd", "--model", "x"], ("--cd", "-C")) is None
    assert cli._codex_opt_value(["--cd", "/tmp/w", "--model", "x"], ("--cd", "-C")) == "/tmp/w"


def test_grok_resume_before_a_flag_leaves_the_pane_unrecorded():
    # a bare --resume opens grok's picker: hive cannot know the session id, so
    # it records nothing rather than recording the next flag
    assert cli._grok_launch_session(["--resume", "-m", "grok-4"]) == (None, False)


def test_grok_resume_with_an_id_records_that_session():
    assert cli._grok_launch_session(["--resume", "old-sid"]) == ("old-sid", False)
