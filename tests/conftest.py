import pytest


@pytest.fixture(autouse=True)
def _isolate_host_agent_env(monkeypatch, tmp_path):
    """Prevent the host CLI tool env from leaking into tests.

    The test process may itself run inside a Claude session or a hive member
    engine; none of that identity may reach the hive binary under test.
    """
    monkeypatch.delenv("CODEX_THREAD_ID", raising=False)
    monkeypatch.delenv("CLAUDE_CODE_MESSAGING_SOCKET", raising=False)
    monkeypatch.delenv("CLAUDE_HOME", raising=False)
    monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(tmp_path / "claude-env-isolation"))
    monkeypatch.delenv("HIVE_TEAM", raising=False)
    monkeypatch.delenv("HIVE_MEMBER", raising=False)
