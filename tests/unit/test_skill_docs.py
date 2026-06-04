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
    (`hive skills get <role>`)。每个角色 spec 都在;worker/validator 指 cell
    内核、orch/challenger 指 crew;fat handoff schema 单一来源在 cell spec。"""
    specs = Path(__file__).resolve().parents[2] / "src" / "hive" / "core_assets" / "specs"

    for role in ("crew-orch", "crew-challenger", "crew-worker", "crew-validator",
                 "cell-worker", "cell-validator"):
        assert (specs / f"{role}.md").exists(), f"missing role spec: {role}.md"

    assert "hive skills get crew" in (specs / "crew-orch.md").read_text()
    assert "hive skills get crew" in (specs / "crew-challenger.md").read_text()
    for role in ("crew-worker", "cell-worker", "cell-validator"):
        assert "hive skills get cell" in (specs / f"{role}.md").read_text()

    # fat kernel (handoff schema) single-sourced in the cell spec, never copied
    assert "salientSummary" in (specs / "cell.md").read_text()
    for role in ("crew-worker", "crew-validator", "cell-worker", "cell-validator"):
        assert "salientSummary" not in (specs / f"{role}.md").read_text()


def test_hive_is_the_only_installed_skill():
    """单一装机 skill = `/hive`;cell/crew + 所有角色都被拉进 CLI(`hive skills
    get`),不再是 SKILL.md —— 可 drift 面收到只剩 /hive。"""
    skills = Path(__file__).resolve().parents[2] / "skills"
    installed = sorted(p.name for p in skills.iterdir() if p.is_dir())
    assert installed == ["hive"], f"expected only the hive skill installed, got {installed}"


def test_hive_picker_dispatches_to_cli_init():
    """/hive 起拓扑时问 cell/crew(引用 core「问用户」),按答案跑 CLI 的
    `hive cell init` / `hive crew init`,不再 dispatch /cell · /crew skill。"""
    stub = (Path(__file__).resolve().parents[2] / "skills" / "hive" / "SKILL.md").read_text()
    assert "hive cell init" in stub
    assert "hive crew init" in stub
    assert "问用户" in stub


def test_hive_install_docs_use_npx_skills_add_as_canonical_path():
    """守 install/refresh 命令在 README / AGENTS 跨文件一致 — contract test"""
    repo_root = Path(__file__).resolve().parents[2]
    readme_text = (repo_root / "README.md").read_text()
    agents_text = (repo_root / "AGENTS.md").read_text()

    assert "pipx upgrade hive" in readme_text
    assert "npx skills update hive -g" in readme_text
    assert 'npx skills add https://github.com/notdp/hive -g --all' in readme_text
    assert 'npx skills add "$PWD" -g --all' in readme_text
    assert 'npx skills add "$PWD" -g --all' in agents_text
    assert 'repo changes to `skills/hive/SKILL.md` do not reach agents unless you refresh it via `npx skills add`' in agents_text
