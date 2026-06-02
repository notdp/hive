"""Gang layout now routes through the shared adaptive picker.

Covers bug 1 regression: gang-authored two-pane windows must pick
`even-vertical` on portrait and `main-vertical`+50% on landscape — and
keep the `orientation` string on the JSON contract (`horizontal`/`vertical`).
"""

from hive.cli import _apply_gang_layout


def _install_layout_mocks(monkeypatch, *, size: tuple[int, int], pane_count: int):
    calls: list[tuple] = []
    monkeypatch.setattr("hive.layout.tmux.window_size", lambda _t: size)
    monkeypatch.setattr("hive.layout.tmux.list_panes", lambda _t: [f"%{i}" for i in range(pane_count)])
    monkeypatch.setattr("hive.layout.tmux.set_window_option", lambda t, k, v: calls.append(("opt", t, k, v)))
    monkeypatch.setattr("hive.layout.tmux.select_layout", lambda t, p: calls.append(("layout", t, p)))
    return calls


def test_apply_gang_layout_portrait_picks_even_vertical(monkeypatch):
    calls = _install_layout_mocks(monkeypatch, size=(191, 171), pane_count=2)
    assert _apply_gang_layout("dev:2") == "vertical"
    assert ("layout", "dev:2", "even-vertical") in calls
    assert not any(call[0] == "opt" for call in calls)


def test_apply_gang_layout_landscape_picks_main_vertical_with_50pct(monkeypatch):
    calls = _install_layout_mocks(monkeypatch, size=(220, 60), pane_count=2)
    assert _apply_gang_layout("dev:2") == "horizontal"
    assert ("opt", "dev:2", "main-pane-width", "50%") in calls
    assert ("layout", "dev:2", "main-vertical") in calls


def test_apply_gang_layout_single_pane_is_noop(monkeypatch):
    calls = _install_layout_mocks(monkeypatch, size=(220, 60), pane_count=1)
    assert _apply_gang_layout("dev:2") == ""
    assert calls == []


def test_apply_gang_layout_empty_window_target_is_noop(monkeypatch):
    calls = _install_layout_mocks(monkeypatch, size=(220, 60), pane_count=2)
    assert _apply_gang_layout("") == ""
    assert calls == []
