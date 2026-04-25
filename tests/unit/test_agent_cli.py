import json
import os

from hive import agent_cli, tmux


def test_normalize_command_strips_path_and_aliases():
    assert agent_cli.normalize_command("droid") == "droid"
    assert agent_cli.normalize_command("/usr/local/bin/claude") == "claude"
    assert agent_cli.normalize_command("claude-code") == "claude"
    assert agent_cli.normalize_command("CODEX") == "codex"
    assert agent_cli.normalize_command("") == ""


def test_member_role_classifies_agents_and_shells():
    assert agent_cli.member_role("droid") == "agent"
    assert agent_cli.member_role("claude") == "agent"
    assert agent_cli.member_role("codex") == "agent"
    assert agent_cli.member_role("zsh") == "terminal"
    assert agent_cli.member_role("python3") == "terminal"


def test_profiles_use_expected_skill_commands():
    assert agent_cli.get_profile("droid").skill_cmd == "/{name}"
    assert agent_cli.get_profile("claude").skill_cmd == "/{name}"
    assert agent_cli.get_profile("codex").skill_cmd == "${name}"


def test_detect_profile_for_pane_uses_title_and_tty_processes(monkeypatch):
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "2.1.89")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "\u2733 Claude Code")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "/dev/ttys012")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [])

    profile = agent_cli.detect_profile_for_pane("%138")

    assert profile is not None
    assert profile.name == "claude"


def test_detect_profile_for_pane_falls_back_to_tty_processes(monkeypatch):
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "2.1.89")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "/dev/ttys012")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [
        tmux.TTYProcessInfo(pid="100", command="-zsh", argv="-zsh"),
        tmux.TTYProcessInfo(pid="200", command="codex", argv="codex"),
    ])

    profile = agent_cli.detect_profile_for_pane("%141")

    assert profile is not None
    assert profile.name == "codex"


def test_resolve_session_id_for_pane_dispatches_to_adapter(monkeypatch):
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "claude")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [])

    calls: list[str] = []

    class FakeAdapter:
        def resolve_current_session_id(self, pane_id: str) -> str | None:
            calls.append(pane_id)
            return "fake-sess"

    monkeypatch.setattr("hive.agent_cli.adapters.get", lambda name: FakeAdapter() if name == "claude" else None)

    assert agent_cli.resolve_session_id_for_pane("%138") == "fake-sess"
    assert calls == ["%138"]


def test_resolve_session_id_for_pane_resolves_newer_claude_project_session(monkeypatch, tmp_path):
    sessions_dir = tmp_path / "sessions"
    projects_dir = tmp_path / "projects" / "-repo"
    sessions_dir.mkdir(parents=True)
    projects_dir.mkdir(parents=True)
    (sessions_dir / "42424.json").write_text(json.dumps({"sessionId": "sess-old"}))

    stale = projects_dir / "sess-old.jsonl"
    stale.write_text(json.dumps({"sessionId": "sess-old", "cwd": "/repo"}) + "\n")
    fresh = projects_dir / "sess-new.jsonl"
    fresh.write_text(json.dumps({"sessionId": "sess-new", "cwd": "/repo"}) + "\n")
    stale_ns = 1_700_000_000_000_000_000
    fresh_ns = stale_ns + 5_000
    os.utime(stale, ns=(stale_ns, stale_ns))
    os.utime(fresh, ns=(fresh_ns, fresh_ns))

    monkeypatch.setenv("CLAUDE_HOME", str(tmp_path))
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "claude")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "/dev/ttys001")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [])
    monkeypatch.setattr("hive.adapters.claude.tmux.get_pane_tty", lambda _pane: "/dev/ttys001")
    monkeypatch.setattr("hive.adapters.claude.tmux.display_value", lambda _pane, _fmt: "/repo")
    monkeypatch.setattr("hive.adapters.claude.tmux.list_tty_processes", lambda _tty: [
        tmux.TTYProcessInfo(pid="42424", command="claude", argv="claude --verbose"),
    ])

    assert agent_cli.resolve_session_id_for_pane("%138") == "sess-new"


def test_resolve_session_id_for_pane_returns_none_when_no_profile(monkeypatch):
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "zsh")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [])

    assert agent_cli.resolve_session_id_for_pane("%2") is None


def test_member_role_for_pane_returns_agent_when_profile_detected(monkeypatch):
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "droid")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [])

    assert agent_cli.member_role_for_pane("%1") == "agent"


def test_member_role_for_pane_returns_terminal_for_shell(monkeypatch):
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_current_command", lambda _pane: "zsh")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_title", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.get_pane_tty", lambda _pane: "")
    monkeypatch.setattr("hive.agent_cli.tmux.list_tty_processes", lambda _tty: [])

    assert agent_cli.member_role_for_pane("%2") == "terminal"


_PEER_ENV_KEYS = (
    "HIVE_DROID_PEER_OPENAI_MODEL",
    "HIVE_DROID_PEER_OPENAI_EFFORT",
    "HIVE_DROID_PEER_ANTHROPIC_MODEL",
    "HIVE_DROID_PEER_ANTHROPIC_EFFORT",
)


def _clear_peer_env(monkeypatch):
    for key in _PEER_ENV_KEYS:
        monkeypatch.delenv(key, raising=False)


_PEER_SAMPLE_SETTINGS = {
    "sessionDefaultSettings": {"reasoningEffort": "high"},
    "customModels": [
        {
            "id": "custom:Claude-Opus-4.7-0",
            "model": "claude-opus-4-7",
            "reasoningEffort": "max",
            "maxOutputTokens": 128000,
            "index": 0,
            "provider": "anthropic",
        },
        {
            "id": "custom:GPT-5.5-1",
            "model": "gpt-5.5",
            "reasoningEffort": "xhigh",
            "maxOutputTokens": 100000,
            "index": 1,
            "provider": "openai",
        },
        {
            "id": "custom:GPT-5.3-CODEX-2",
            "model": "gpt-5.3-codex",
            "reasoningEffort": "xhigh",
            "maxOutputTokens": 200000,
            "index": 2,
            "provider": "openai",
        },
        {
            "id": "custom:Kimi-K2.5-3",
            "model": "kimi-k2.5",
            "reasoningEffort": "max",
            "maxOutputTokens": 128000,
            "index": 3,
            "provider": "generic-chat-completion-api",
        },
    ],
}


def test_droid_peer_plan_picks_top_opposite_family_from_settings(monkeypatch):
    _clear_peer_env(monkeypatch)

    # anthropic orch → openai peer: GPT-5.5 wins over GPT-5.3-codex purely
    # on version number (tier 5.5 > 5.3), even though both share the same
    # effort rank and codex has more output tokens.
    assert agent_cli.droid_peer_plan("anthropic", settings=_PEER_SAMPLE_SETTINGS) == (
        "custom:GPT-5.5-1",
        "xhigh",
    )
    # openai orch → anthropic peer: only Opus qualifies (Kimi is unknown).
    assert agent_cli.droid_peer_plan("openai", settings=_PEER_SAMPLE_SETTINGS) == (
        "custom:Claude-Opus-4.7-0",
        "max",
    )
    assert agent_cli.droid_peer_plan("unknown", settings=_PEER_SAMPLE_SETTINGS) is None
    assert agent_cli.droid_peer_plan("", settings=_PEER_SAMPLE_SETTINGS) is None


def test_droid_peer_plan_returns_none_when_opposite_family_missing(monkeypatch):
    _clear_peer_env(monkeypatch)
    only_claude = {
        "customModels": [
            {"id": "custom:Claude-A", "model": "claude-opus", "provider": "anthropic"},
        ],
    }
    assert agent_cli.droid_peer_plan("anthropic", settings=only_claude) is None
    assert agent_cli.droid_peer_plan("openai", settings=only_claude) == (
        "custom:Claude-A",
        "",
    )


def test_droid_peer_plan_returns_none_on_empty_settings(monkeypatch):
    _clear_peer_env(monkeypatch)
    assert agent_cli.droid_peer_plan("anthropic", settings=None) is None
    assert agent_cli.droid_peer_plan("openai", settings={}) is None
    assert agent_cli.droid_peer_plan("anthropic", settings={"customModels": []}) is None


def test_droid_peer_plan_honours_env_overrides(monkeypatch):
    _clear_peer_env(monkeypatch)
    monkeypatch.setenv("HIVE_DROID_PEER_OPENAI_MODEL", "custom:OtherGPT")
    monkeypatch.setenv("HIVE_DROID_PEER_OPENAI_EFFORT", "high")
    monkeypatch.setenv("HIVE_DROID_PEER_ANTHROPIC_MODEL", "custom:OtherClaude")
    monkeypatch.setenv("HIVE_DROID_PEER_ANTHROPIC_EFFORT", "max")

    # env wins over settings-driven auto-pick.
    assert agent_cli.droid_peer_plan("anthropic", settings=_PEER_SAMPLE_SETTINGS) == (
        "custom:OtherGPT",
        "high",
    )
    assert agent_cli.droid_peer_plan("openai", settings=_PEER_SAMPLE_SETTINGS) == (
        "custom:OtherClaude",
        "max",
    )


def test_droid_peer_plan_env_model_without_effort(monkeypatch):
    _clear_peer_env(monkeypatch)
    monkeypatch.setenv("HIVE_DROID_PEER_OPENAI_MODEL", "custom:BareGPT")
    assert agent_cli.droid_peer_plan("anthropic", settings=_PEER_SAMPLE_SETTINGS) == (
        "custom:BareGPT",
        "",
    )


def test_select_droid_peer_uses_effort_rank(monkeypatch):
    _clear_peer_env(monkeypatch)
    settings = {
        "customModels": [
            {"id": "custom:gpt-low", "model": "gpt-5", "reasoningEffort": "low", "provider": "openai"},
            {"id": "custom:gpt-xhigh", "model": "gpt-5", "reasoningEffort": "xhigh", "provider": "openai"},
            {"id": "custom:gpt-high", "model": "gpt-5", "reasoningEffort": "high", "provider": "openai"},
        ],
    }
    assert agent_cli.select_droid_peer_from_settings("anthropic", settings) == (
        "custom:gpt-xhigh",
        "xhigh",
    )


def test_select_droid_peer_uses_provider_fallback(monkeypatch):
    _clear_peer_env(monkeypatch)
    # When neither id/model/displayName reveals the family, fall back to
    # the provider tag.
    settings = {
        "customModels": [
            {"id": "custom:proxy-a", "model": "some-proxy-x", "provider": "anthropic", "reasoningEffort": "max"},
            {"id": "custom:proxy-b", "model": "another-proxy-y", "provider": "openai", "reasoningEffort": "xhigh"},
        ],
    }
    assert agent_cli.select_droid_peer_from_settings("openai", settings) == (
        "custom:proxy-a",
        "max",
    )
    assert agent_cli.select_droid_peer_from_settings("anthropic", settings) == (
        "custom:proxy-b",
        "xhigh",
    )


def test_select_droid_peer_tier_beats_effort(monkeypatch):
    _clear_peer_env(monkeypatch)
    # Opus + high MUST beat Sonnet + max because tier is the primary axis.
    settings = {
        "customModels": [
            {"id": "custom:Sonnet-max", "model": "Claude-Sonnet-4", "reasoningEffort": "max", "provider": "anthropic"},
            {"id": "custom:Opus-high", "model": "Claude-Opus-4", "reasoningEffort": "high", "provider": "anthropic"},
        ],
    }
    assert agent_cli.select_droid_peer_from_settings("openai", settings) == (
        "custom:Opus-high",
        "high",
    )


def test_select_droid_peer_within_same_tier_uses_effort(monkeypatch):
    _clear_peer_env(monkeypatch)
    settings = {
        "customModels": [
            {"id": "custom:Opus-high", "model": "Claude-Opus-4", "reasoningEffort": "high", "provider": "anthropic"},
            {"id": "custom:Opus-max", "model": "Claude-Opus-4", "reasoningEffort": "max", "provider": "anthropic"},
        ],
    }
    assert agent_cli.select_droid_peer_from_settings("openai", settings) == (
        "custom:Opus-max",
        "max",
    )


def test_select_droid_peer_gpt_higher_version_wins(monkeypatch):
    _clear_peer_env(monkeypatch)
    # GPT-5.5 + high beats GPT-5.3 + max purely on version number.
    settings = {
        "customModels": [
            {"id": "custom:gpt-5.3-codex", "model": "gpt-5.3-codex", "reasoningEffort": "max", "provider": "openai"},
            {"id": "custom:gpt-5.5", "model": "gpt-5.5", "reasoningEffort": "high", "provider": "openai"},
        ],
    }
    assert agent_cli.select_droid_peer_from_settings("anthropic", settings) == (
        "custom:gpt-5.5",
        "high",
    )


def test_select_droid_peer_haiku_never_beats_sonnet(monkeypatch):
    _clear_peer_env(monkeypatch)
    # Even an absurdly large Haiku version stays below the Sonnet floor.
    settings = {
        "customModels": [
            {"id": "custom:Haiku-99", "model": "Claude-Haiku-99", "reasoningEffort": "max", "provider": "anthropic"},
            {"id": "custom:Sonnet-0", "model": "Claude-Sonnet-0", "reasoningEffort": "low", "provider": "anthropic"},
        ],
    }
    assert agent_cli.select_droid_peer_from_settings("openai", settings) == (
        "custom:Sonnet-0",
        "low",
    )


def test_select_droid_peer_is_case_insensitive(monkeypatch):
    _clear_peer_env(monkeypatch)
    # Mixed case for family tokens, effort, and provider should not change
    # the selection.
    settings = {
        "customModels": [
            {"id": "CUSTOM:Sonnet-MAX", "model": "CLAUDE-SONNET-4", "reasoningEffort": "MAX", "provider": "Anthropic"},
            {"id": "CUSTOM:Opus-Hi", "model": "Claude-Opus-4.7", "reasoningEffort": "xHIGH", "provider": "ANTHROPIC"},
        ],
    }
    # Opus tier still beats Sonnet tier regardless of upper/lower casing.
    assert agent_cli.select_droid_peer_from_settings("openai", settings) == (
        "CUSTOM:Opus-Hi",
        "xHIGH",
    )


def test_extract_leading_version_handles_multi_segment(monkeypatch):
    # Regression: three-segment versions used to make `float("5.1.3")` raise
    # ValueError, dropping the tier silently to 0.0 and misranking patch
    # releases. Now we keep major.minor and discard the patch tail.
    assert agent_cli._extract_leading_version("gpt-5.1.3") == 5.1
    assert agent_cli._extract_leading_version("gpt-5.5.2") == 5.5
    assert agent_cli._extract_leading_version("claude-opus-4.7.1") == 4.7


def test_extract_leading_version_treats_dash_as_minor_separator():
    # Factory's customModels stores Claude versions hyphen-separated in the
    # `model` field (e.g. `claude-opus-4-7`). Treat the dash as a minor
    # separator so 4-7 vs 4-8 don't tie at the same tier (they used to both
    # collapse to 4.0).
    assert agent_cli._extract_leading_version("claude-opus-4-7") == 4.7
    assert agent_cli._extract_leading_version("claude-opus-4-8") == 4.8
    # The trailing -0 in id strings must not pollute the captured version.
    assert agent_cli._extract_leading_version("custom:Claude-Opus-4.7-0") == 4.7


def test_extract_leading_version_keeps_openai_dash_as_registration_suffix():
    # Factory handle ids append a registration slot index (`custom:GPT-5.5-1`,
    # `custom:GPT-5-9`). For non-Claude strings the trailing `-<digits>` must
    # NOT be read as a minor version — otherwise `custom:GPT-5-9` would tier
    # at 5.9 and falsely beat a real `gpt-5.5` candidate.
    assert agent_cli._extract_leading_version("gpt-5-9") == 5.0
    assert agent_cli._extract_leading_version("custom:GPT-5-9") == 5.0
    # The dot-separated minor still works for real OpenAI versions.
    assert agent_cli._extract_leading_version("gpt-5.5") == 5.5
    assert agent_cli._extract_leading_version("custom:GPT-5.5-1") == 5.5


def test_select_droid_peer_gpt_patch_release_outranks_lower_minor(monkeypatch):
    _clear_peer_env(monkeypatch)
    # Regression: gpt-5.5.1 used to drop tier to 0.0 and lose to gpt-5.0.
    # Now major.minor is preserved so gpt-5.5.1 (5.5) beats gpt-5 (5.0)
    # even though gpt-5 has higher effort.
    settings = {
        "customModels": [
            {"id": "custom:gpt-5.5.1", "model": "gpt-5.5.1", "reasoningEffort": "low", "provider": "openai"},
            {"id": "custom:gpt-5", "model": "gpt-5", "reasoningEffort": "max", "provider": "openai"},
        ],
    }
    assert agent_cli.select_droid_peer_from_settings("anthropic", settings) == (
        "custom:gpt-5.5.1",
        "low",
    )


def test_select_droid_peer_ignores_openai_registration_suffix(monkeypatch):
    _clear_peer_env(monkeypatch)
    # Regression: when dash-minor parsing was enabled for every family, the
    # Factory registration suffix in `custom:GPT-5-9` was read as a minor
    # version (5.9) and beat a real `gpt-5.5` entry. Dash-minor must be
    # Claude-only so this case selects the higher real version.
    settings = {
        "customModels": [
            {"id": "custom:GPT-5-9", "model": "gpt-5", "reasoningEffort": "low", "provider": "openai"},
            {"id": "custom:GPT-5.5-1", "model": "gpt-5.5", "reasoningEffort": "max", "provider": "openai"},
        ],
    }
    assert agent_cli.select_droid_peer_from_settings("anthropic", settings) == (
        "custom:GPT-5.5-1",
        "max",
    )


def test_select_droid_peer_hyphen_minor_distinguishes_anthropic_versions(monkeypatch):
    _clear_peer_env(monkeypatch)
    # claude-opus-4-7 and claude-opus-4-8 must rank by minor version when no
    # other tie-breaker (effort/tokens) separates them.
    settings = {
        "customModels": [
            {"id": "custom:opus-4-7", "model": "claude-opus-4-7", "reasoningEffort": "high", "provider": "anthropic"},
            {"id": "custom:opus-4-8", "model": "claude-opus-4-8", "reasoningEffort": "high", "provider": "anthropic"},
        ],
    }
    assert agent_cli.select_droid_peer_from_settings("openai", settings) == (
        "custom:opus-4-8",
        "high",
    )


def test_classify_model_family_drops_o_codenames():
    # Old o1/o3/o4 OpenAI codenames have been retired; we no longer want to
    # classify bare "o1" as openai because it triggers false positives on
    # unrelated names.
    assert agent_cli.classify_model_family("o1-preview") == "unknown"
    assert agent_cli.classify_model_family("o3") == "unknown"
    assert agent_cli.classify_model_family("o4-mini") == "unknown"
    # gpt-* still works.
    assert agent_cli.classify_model_family("gpt-4") == "openai"
    assert agent_cli.classify_model_family("GPT-5.5") == "openai"
