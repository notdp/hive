from pathlib import Path


def test_hive_skill_guides_multiline_send_via_artifact():
    skill_text = (Path(__file__).resolve().parents[2] / "skills" / "hive" / "SKILL.md").read_text()

    assert "大内容或多行结构化内容先写 artifact" in skill_text
    assert "不要把 `$(cat <<EOF ...)` 这类多行 command substitution 直接塞进 `hive send`" in skill_text
    assert "有疑问时优先问团队里的其他 agent" in skill_text
    assert "只有当团队内没有 agent 能决策" in skill_text
    assert "用户是合作者，不是监工" in skill_text
