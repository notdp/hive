from pathlib import Path


def test_hive_skill_guides_multiline_send_via_artifact():
    """守 canonical heredoc + stdin artifact 片段(root 协议 happy path)。

    install 命令留在薄 discovery stub;消息协议(含 heredoc/artifact idiom)随 CLI
    下发,锚在 core spec(`hive skills get core`)。只断言低变更、高价值的 shell
    idioms,不守具体中文措辞。"""
    repo_root = Path(__file__).resolve().parents[2]
    stub_text = (repo_root / "skills" / "hive" / "SKILL.md").read_text()
    core_spec = (repo_root / "src" / "hive" / "core_assets" / "specs" / "core.md").read_text()

    # install / refresh canonical commands live in the discovery stub
    assert 'npx skills add https://github.com/notdp/hive -g --all' in stub_text
    assert 'npx skills add "$PWD" -g --all' in stub_text
    assert "pipx upgrade hive" in stub_text
    assert "npx skills update hive -g" in stub_text

    # stub is a discovery pointer at the CLI-shipped core spec
    assert "hive skills get core" in stub_text

    # heredoc + stdin artifact idiom now lives in the version-locked core spec
    assert "--artifact -" in core_spec
    assert "<<'EOF'" in core_spec
    assert "--artifact - <<'EOF'" in core_spec


def test_crew_role_skills_are_thin_stubs_pointing_at_specs():
    """守拓扑重构的架构不变量:crew 角色 skill 是薄 stub,只指向 CLI 下发的
    spec(orch/challenger → crew;worker/validator → cell);fat 角色内核
    (handoff schema)单一来源在 cell spec,不在 stub 里复刻。"""
    repo_root = Path(__file__).resolve().parents[2]
    skills = repo_root / "skills"

    orch = (skills / "crew-orch" / "SKILL.md").read_text()
    challenger = (skills / "crew-challenger" / "SKILL.md").read_text()
    worker = (skills / "crew-worker" / "SKILL.md").read_text()
    validator = (skills / "crew-validator" / "SKILL.md").read_text()

    # stubs point at the version-locked spec for their topology
    assert "hive skills get crew" in orch
    assert "hive skills get crew" in challenger
    assert "hive skills get cell" in worker
    assert "hive skills get cell" in validator

    # the fat role kernel (handoff schema) lives ONLY in the cell spec, never
    # re-inlined into a stub — guards against drift via duplication
    cell_spec = (repo_root / "src" / "hive" / "core_assets" / "specs" / "cell.md").read_text()
    assert "salientSummary" in cell_spec
    for stub in (orch, challenger, worker, validator):
        assert "salientSummary" not in stub

    # skeptic → challenger rename is complete in the skill surface
    assert not (skills / "crew-skeptic").exists()


def test_crew_skills_are_not_model_auto_invocable():
    """所有 crew 系 skill 都该是 deliberate-only:`crew` 入口靠用户显式 /crew
    (动作是 `hive crew init`,会破窗 + spawn challenger,误触发代价大),角色
    stub 靠 spawn 注入 `/crew-orch` 等加载。两者都不该被模型按描述自动调用,
    所以全部钉 `disable-model-invocation: true`。"""
    skills = Path(__file__).resolve().parents[2] / "skills"
    for name in ("crew", "crew-orch", "crew-challenger", "crew-worker", "crew-validator"):
        text = (skills / name / "SKILL.md").read_text()
        assert "disable-model-invocation: true" in text, f"{name} missing disable-model-invocation"


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
