import hive.layout as layout


def test_pick_portrait_two_panes():
    choice = layout.pick((191, 171), 2)
    assert choice is not None
    assert choice.orientation == "vertical"
    assert choice.preset == "even-vertical"
    assert choice.options == {}


def test_pick_portrait_three_panes():
    choice = layout.pick((100, 100), 3)
    assert choice is not None
    assert choice.orientation == "vertical"
    assert choice.preset == "even-vertical"


def test_pick_landscape_two_panes():
    choice = layout.pick((200, 50), 2)
    assert choice is not None
    assert choice.orientation == "horizontal"
    assert choice.preset == "main-vertical"
    assert choice.options == {"main-pane-width": "50%"}


def test_pick_landscape_exactly_two_x_threshold():
    choice = layout.pick((200, 100), 2)
    assert choice is not None
    assert choice.orientation == "horizontal"


def test_pick_just_below_landscape_threshold():
    choice = layout.pick((199, 100), 2)
    assert choice is not None
    assert choice.orientation == "vertical"


def test_pick_single_pane_returns_none():
    assert layout.pick((200, 50), 1) is None
    assert layout.pick((100, 100), 1) is None


def test_pick_zero_panes_returns_none():
    assert layout.pick((200, 50), 0) is None


def test_pick_unknown_window_size_falls_back_to_landscape():
    choice = layout.pick((0, 0), 2)
    assert choice is not None
    assert choice.orientation == "horizontal"
    assert choice.preset == "main-vertical"


def test_apply_adaptive_empty_window_target_is_noop(monkeypatch):
    calls: list[tuple] = []
    monkeypatch.setattr("hive.layout.tmux.window_size", lambda t: calls.append(("size", t)) or (0, 0))
    monkeypatch.setattr("hive.layout.tmux.list_panes", lambda t: calls.append(("list", t)) or [])
    monkeypatch.setattr("hive.layout.tmux.select_layout", lambda t, p: calls.append(("layout", t, p)))
    assert layout.apply_adaptive("") is None
    assert calls == []


def test_apply_adaptive_portrait_applies_even_vertical(monkeypatch):
    calls: list[tuple] = []
    monkeypatch.setattr("hive.layout.tmux.window_zoomed", lambda t: False)
    monkeypatch.setattr("hive.layout.tmux.window_size", lambda t: (191, 171))
    monkeypatch.setattr("hive.layout.tmux.list_panes", lambda t: ["%1", "%2"])
    monkeypatch.setattr("hive.layout.tmux.set_window_option", lambda t, k, v: calls.append(("opt", t, k, v)))
    monkeypatch.setattr("hive.layout.tmux.select_layout", lambda t, p: calls.append(("layout", t, p)))
    result = layout.apply_adaptive("dev:0")
    assert result is not None
    assert result.preset == "even-vertical"
    assert calls == [("layout", "dev:0", "even-vertical")]


def test_apply_adaptive_landscape_sets_main_pane_width_before_select(monkeypatch):
    calls: list[tuple] = []
    monkeypatch.setattr("hive.layout.tmux.window_zoomed", lambda t: False)
    monkeypatch.setattr("hive.layout.tmux.window_size", lambda t: (200, 50))
    monkeypatch.setattr("hive.layout.tmux.list_panes", lambda t: ["%1", "%2", "%3"])
    monkeypatch.setattr("hive.layout.tmux.set_window_option", lambda t, k, v: calls.append(("opt", t, k, v)))
    monkeypatch.setattr("hive.layout.tmux.select_layout", lambda t, p: calls.append(("layout", t, p)))
    result = layout.apply_adaptive("dev:0")
    assert result is not None
    assert result.preset == "main-vertical"
    assert calls == [
        ("opt", "dev:0", "main-pane-width", "50%"),
        ("layout", "dev:0", "main-vertical"),
    ]


def test_apply_adaptive_single_pane_skips_select_layout(monkeypatch):
    calls: list[tuple] = []
    monkeypatch.setattr("hive.layout.tmux.window_zoomed", lambda t: False)
    monkeypatch.setattr("hive.layout.tmux.window_size", lambda t: (200, 50))
    monkeypatch.setattr("hive.layout.tmux.list_panes", lambda t: ["%1"])
    monkeypatch.setattr("hive.layout.tmux.set_window_option", lambda t, k, v: calls.append(("opt", t, k, v)))
    monkeypatch.setattr("hive.layout.tmux.select_layout", lambda t, p: calls.append(("layout", t, p)))
    assert layout.apply_adaptive("dev:0") is None
    assert calls == []


def test_pick_five_panes_tiles_regardless_of_orientation():
    landscape = layout.pick((200, 50), 5)
    assert landscape is not None and landscape.preset == "tiled"
    assert landscape.orientation == "horizontal"
    portrait = layout.pick((100, 100), 6)
    assert portrait is not None and portrait.preset == "tiled"
    assert portrait.orientation == "vertical"


def test_pick_four_panes_keeps_main_layout():
    choice = layout.pick((200, 50), 4)
    assert choice is not None and choice.preset == "main-vertical"


def test_apply_adaptive_skips_while_zoomed(monkeypatch):
    calls: list[tuple] = []
    monkeypatch.setattr("hive.layout.tmux.window_zoomed", lambda t: True)
    monkeypatch.setattr("hive.layout.tmux.window_size", lambda t: (200, 50))
    monkeypatch.setattr("hive.layout.tmux.list_panes", lambda t: ["%1", "%2"])
    monkeypatch.setattr("hive.layout.tmux.set_window_option", lambda t, k, v: calls.append(("opt", t, k, v)))
    monkeypatch.setattr("hive.layout.tmux.select_layout", lambda t, p: calls.append(("layout", t, p)))
    assert layout.apply_adaptive("dev:0") is None
    assert calls == []  # a zoomed window is never re-tiled under the human
