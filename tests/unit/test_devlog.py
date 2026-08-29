from hive import devlog


def test_default_verbosity_is_normal_from_site_packages(monkeypatch):
    monkeypatch.delenv("HIVE_LOG_VERBOSITY", raising=False)
    monkeypatch.setattr(devlog, "__file__", "/venv/lib/python3.11/site-packages/hive/devlog.py")

    assert devlog.default_verbosity() == "normal"


def test_default_verbosity_is_dev_from_source_checkout(monkeypatch):
    monkeypatch.delenv("HIVE_LOG_VERBOSITY", raising=False)
    monkeypatch.setattr(devlog, "__file__", "/repo/src/hive/devlog.py")

    assert devlog.default_verbosity() == "dev"


def test_env_overrides_default_verbosity(monkeypatch):
    monkeypatch.setenv("HIVE_LOG_VERBOSITY", "dev")
    monkeypatch.setattr(devlog, "__file__", "/venv/lib/python3.11/site-packages/hive/devlog.py")

    assert devlog.default_verbosity() == "dev"


def test_log_paths_are_workspace_run_paths(tmp_path):
    workspace = tmp_path / "ws"

    assert devlog.run_dir(workspace) == workspace / "run"
    assert devlog.log_paths(workspace) == {
        "notify": str(workspace / "run" / "notify.jsonl"),
        "hived_stderr": str(workspace / "run" / "hived.stderr"),
        "cvim_dir": str(workspace / "run" / "cvim"),
    }
