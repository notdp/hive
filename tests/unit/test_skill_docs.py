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
