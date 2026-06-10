from pathlib import Path


def test_hive_skill_guides_multiline_send_via_artifact():
    """守 canonical heredoc + stdin artifact 片段(root 协议 happy path)。

    stub 学 agent-browser:只留 install happy-path 一行,upgrade / 本地刷新等变体
    指回 README(canonical 由 test_hive_install_docs_* 守)。消息协议(含
    heredoc/artifact idiom)随 CLI 下发,锚在 core spec。只断言低变更、高价值的
    shell idioms,不守具体中文措辞。"""
    repo_root = Path(__file__).resolve().parents[2]
    stub_text = (repo_root / "skills" / "hive" / "SKILL.md").read_text()
    core_spec = (repo_root / "src" / "hive" / "core_assets" / "specs" / "core.md").read_text()

    # stub keeps only the install happy-path; upgrade/refresh variants live in README
    assert 'npx skills add https://github.com/notdp/hive -g --all' in stub_text

    # stub is a discovery pointer at the CLI-shipped core spec
    assert "hive skills get core" in stub_text

    # heredoc + stdin artifact idiom now lives in the version-locked core spec
    assert "--artifact -" in core_spec
    assert "<<'EOF'" in core_spec
    assert "--artifact - <<'EOF'" in core_spec


def test_role_content_is_cli_served_specs_not_installed_skills():
    """拓扑重构终态:角色内容不再是装机 skill,而是 CLI 现取的 spec
    (`hive skills get <role>`)。每个角色 spec 都在;worker/validator 指 duo
    内核、orch/challenger 指 squad;fat handoff schema 单一来源在 duo spec。"""
    specs = Path(__file__).resolve().parents[2] / "src" / "hive" / "core_assets" / "specs"

    for role in ("squad-orch", "squad-challenger", "squad-worker", "squad-validator",
                 "duo-worker", "duo-validator"):
        assert (specs / f"{role}.md").exists(), f"missing role spec: {role}.md"

    assert "hive skills get squad" in (specs / "squad-orch.md").read_text()
    assert "hive skills get squad" in (specs / "squad-challenger.md").read_text()
    for role in ("squad-worker", "duo-worker", "duo-validator"):
        assert "hive skills get duo" in (specs / f"{role}.md").read_text()

    # fat kernel (handoff schema) single-sourced in the duo spec, never copied
    assert "salientSummary" in (specs / "duo.md").read_text()
    for role in ("squad-worker", "squad-validator", "duo-worker", "duo-validator"):
        assert "salientSummary" not in (specs / f"{role}.md").read_text()


def test_hive_is_the_only_installed_skill():
    """单一装机 skill = `/hive`;duo/squad + 所有角色都被拉进 CLI(`hive skills
    get`),不再是 SKILL.md —— 可 drift 面收到只剩 /hive。"""
    skills = Path(__file__).resolve().parents[2] / "skills"
    installed = sorted(p.name for p in skills.iterdir() if p.is_dir())
    assert installed == ["hive"], f"expected only the hive skill installed, got {installed}"


def test_hive_picker_dispatches_to_cli_init():
    """/hive 起拓扑时问 duo/squad(引用 core「问用户」),按答案跑 CLI 的
    `hive duo init` / `hive squad init`,不再 dispatch /duo · /squad skill。"""
    stub = (Path(__file__).resolve().parents[2] / "skills" / "hive" / "SKILL.md").read_text()
    assert "hive duo init" in stub
    assert "hive squad init" in stub
    assert "问用户" in stub


def _section(text: str, start: str, end: str) -> str:
    start_index = text.index(start)
    end_index = text.index(end, start_index + len(start))
    return text[start_index:end_index]


def test_hive_install_docs_keep_live_transport_out_of_dev_refresh():
    """守 live/dev lane 边界,只断言高风险命令和 source-test 入口。"""
    repo_root = Path(__file__).resolve().parents[2]
    readme_text = (repo_root / "README.md").read_text()
    agents_text = (repo_root / "AGENTS.md").read_text()
    agents_build = _section(agents_text, "## Build, Test, and Development Commands", "## Coding Style")
    readme_contributors = _section(readme_text, "## For Contributors", "## Docs")
    toxic = (
        "python3 -m pip install -e . --break-system-packages",
        'npx skills add "$PWD" -g --all',
    )

    assert "pipx upgrade hive" in readme_text
    assert "npx skills update hive -g" in readme_text
    assert 'npx skills add https://github.com/notdp/hive -g --all' in readme_text
    for command in toxic:
        assert command not in agents_build
        assert command not in readme_contributors
    assert "PYTHONPATH=src python -m pytest" in agents_build
    assert "PYTHONPATH=src python -m pytest" in readme_contributors


def test_specs_point_onward_only_via_reachable_skills_get():
    """A CLI-served spec may point onward only via `hive skills get <name>` — never
    at skill-home files (`references/...`) or a stub section (`../SKILL.md`) that an
    agent holding just the CLI output cannot follow. Guards the regression where
    slimming the stub left specs pointing at a `消息机制` section it no longer has."""
    specs = Path(__file__).resolve().parents[2] / "src" / "hive" / "core_assets" / "specs"
    for p in sorted(specs.glob("*.md")):
        text = p.read_text()
        assert "references/" not in text, f"{p.name}: unreachable references/ pointer"
        assert "../SKILL.md" not in text, f"{p.name}: points at ../SKILL.md (stub has no protocol sections)"


def test_stub_tells_init_runner_to_execute_next_itself():
    """init's role load reaches the current pane via the JSON `next` field the
    agent runs itself — the stub must say so and never regress to the old
    "自动到位" wording (which described the fake-user-message injection)."""
    repo_root = Path(__file__).resolve().parents[2]
    stub_text = (repo_root / "skills" / "hive" / "SKILL.md").read_text()
    assert "自动到位" not in stub_text
    assert "`next`" in stub_text
    assert "hive skills get duo-worker" in stub_text
