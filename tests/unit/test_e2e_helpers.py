from pathlib import Path

from tests.e2e._helpers import base_env


def test_base_env_isolates_hive_and_cache_paths(tmp_path: Path):
    env = base_env(tmp_path)

    assert env["HIVE_HOME"] == str(tmp_path / ".hive")
    assert env["XDG_CACHE_HOME"] == str(tmp_path / ".cache")
