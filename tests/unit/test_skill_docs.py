from pathlib import Path


def test_hive_skill_guides_multiline_send_via_artifact():
    skill_text = (Path(__file__).resolve().parents[2] / "skills" / "hive" / "SKILL.md").read_text()

    assert 'npx skills add https://github.com/notdp/hive -g --all' in skill_text
    assert 'npx skills add "$PWD" -g --all' in skill_text
    assert "pipx upgrade hive" in skill_text
    assert "npx skills update hive -g" in skill_text
    assert "--artifact -" in skill_text
    assert "heredoc + stdin artifact" in skill_text
    assert "cat <<'EOF' | hive send <name> " in skill_text
    assert "带引号的 `EOF` 标签不会做 shell 插值" in skill_text
    assert "不要把 `$(cat <<EOF ...)` 这类多行 command substitution 直接塞进 `hive send`" in skill_text


def test_hive_skill_guides_team_first_spawn_handoff_and_model_routing():
    skill_text = (Path(__file__).resolve().parents[2] / "skills" / "hive" / "SKILL.md").read_text()

    assert "不要先把用户当传话筒" in skill_text
    assert "优先用 `hive handoff`" in skill_text
    assert "hive handoff dodo --artifact /tmp/task.md" in skill_text
    assert "hive handoff dodo-2 --spawn --artifact /tmp/task.md" in skill_text
    assert "hive handoff orch-2 --fork --artifact /tmp/task.md" in skill_text
    assert "默认 anchor = 你最近一条**未回复**的入站消息" in skill_text
    assert '第一件事是用 `hive reply <sender> --reply-to <msgId> "<short takeover with reason>"` 通知原 sender' in skill_text
    assert "从 orch 手中接管了 X 任务，因为 orch 正在处理 Y" in skill_text
    assert "autoReply 推断不出来" in skill_text
    assert "后续直接 `hive reply <sender> \"...\"` 走 autoReply" in skill_text
    assert "Claude 偏前端体验、文案收敛和发散式讨论；GPT 偏后端 correctness、约束检查和严谨 review" in skill_text


def test_hive_install_docs_use_npx_skills_add_as_canonical_path():
    repo_root = Path(__file__).resolve().parents[2]
    readme_text = (repo_root / "README.md").read_text()
    agents_text = (repo_root / "AGENTS.md").read_text()

    assert "pipx upgrade hive" in readme_text
    assert "npx skills update hive -g" in readme_text
    assert 'npx skills add https://github.com/notdp/hive -g --all' in readme_text
    assert 'npx skills add "$PWD" -g --all' in readme_text
    assert "hive handoff dodo --artifact /tmp/task.md" in readme_text
    assert 'Plugin enable does not install or refresh the base `hive` skill' in readme_text
    assert 'npx skills add "$PWD" -g --all' in agents_text
    assert 'repo changes to `skills/hive/SKILL.md` do not reach agents unless you refresh it via `npx skills add`' in agents_text
