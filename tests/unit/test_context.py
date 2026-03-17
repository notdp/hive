import json

from hive import context


def test_context_file_uses_tmux_pane_slug(monkeypatch, tmp_path):
    monkeypatch.setattr("hive.context.CONTEXT_DIR", tmp_path / "contexts")
    monkeypatch.setenv("TMUX_PANE", "%12")

    assert context._context_file() == tmp_path / "contexts" / "pane-12.json"


def test_context_file_falls_back_to_default_without_tmux(monkeypatch, tmp_path):
    monkeypatch.setattr("hive.context.CONTEXT_DIR", tmp_path / "contexts")
    monkeypatch.delenv("TMUX_PANE", raising=False)

    assert context._context_file() == tmp_path / "contexts" / "default.json"


def test_save_and_load_current_context_round_trip(monkeypatch, tmp_path):
    monkeypatch.setattr("hive.context.CONTEXT_DIR", tmp_path / "contexts")
    monkeypatch.delenv("TMUX_PANE", raising=False)

    path = context.save_current_context(team="team-a", workspace="/tmp/ws", agent="claude")

    assert path == tmp_path / "contexts" / "default.json"
    assert context.load_current_context() == {
        "team": "team-a",
        "workspace": "/tmp/ws",
        "agent": "claude",
    }


def test_load_current_context_filters_empty_values(monkeypatch, tmp_path):
    path = tmp_path / "contexts" / "default.json"
    path.parent.mkdir(parents=True)
    path.write_text(json.dumps({"team": "team-a", "workspace": "", "agent": None}))
    monkeypatch.setattr("hive.context.CONTEXT_DIR", tmp_path / "contexts")
    monkeypatch.delenv("TMUX_PANE", raising=False)

    assert context.load_current_context() == {"team": "team-a"}


def test_load_current_context_uses_legacy_file_when_pane_file_missing(monkeypatch, tmp_path):
    monkeypatch.setattr("hive.context.CONTEXT_DIR", tmp_path / "contexts")
    monkeypatch.setattr("hive.context.CURRENT_CONTEXT_FILE", tmp_path / "current.json")
    monkeypatch.delenv("TMUX_PANE", raising=False)
    (tmp_path / "current.json").write_text(json.dumps({"team": "team-a", "workspace": "/tmp/ws", "agent": "gpt"}))

    assert context.load_current_context() == {
        "team": "team-a",
        "workspace": "/tmp/ws",
        "agent": "gpt",
    }


def test_load_current_context_returns_empty_on_invalid_json(monkeypatch, tmp_path):
    monkeypatch.setattr("hive.context.CONTEXT_DIR", tmp_path / "contexts")
    monkeypatch.setattr("hive.context.CURRENT_CONTEXT_FILE", tmp_path / "current.json")
    monkeypatch.delenv("TMUX_PANE", raising=False)
    (tmp_path / "contexts").mkdir(parents=True)
    (tmp_path / "contexts" / "default.json").write_text("not-json")

    assert context.load_current_context() == {}


def test_save_context_for_pane_writes_named_file(monkeypatch, tmp_path):
    monkeypatch.setattr("hive.context.CONTEXT_DIR", tmp_path / "contexts")

    path = context.save_context_for_pane("%77", team="team-a", workspace="/tmp/ws", agent="alpha")

    assert path == tmp_path / "contexts" / "pane-77.json"
    assert json.loads(path.read_text()) == {
        "team": "team-a",
        "workspace": "/tmp/ws",
        "agent": "alpha",
    }


def test_clear_current_context_removes_pane_and_legacy_files(monkeypatch, tmp_path):
    monkeypatch.setattr("hive.context.CONTEXT_DIR", tmp_path / "contexts")
    monkeypatch.setattr("hive.context.CURRENT_CONTEXT_FILE", tmp_path / "current.json")
    monkeypatch.setenv("TMUX_PANE", "%9")
    pane_file = tmp_path / "contexts" / "pane-9.json"
    pane_file.parent.mkdir(parents=True)
    pane_file.write_text("{}")
    (tmp_path / "current.json").write_text("{}")

    context.clear_current_context()

    assert not pane_file.exists()
    assert not (tmp_path / "current.json").exists()
