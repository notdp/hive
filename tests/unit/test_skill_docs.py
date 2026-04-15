from pathlib import Path


def test_hive_skill_guides_multiline_send_via_artifact():
    skill_text = (Path(__file__).resolve().parents[2] / "skills" / "hive" / "SKILL.md").read_text()

    assert "--artifact -" in skill_text
    assert "stdin artifact" in skill_text
    assert "不要把 `$(cat <<EOF ...)` 这类多行 command substitution 直接塞进 `hive send`" in skill_text


def test_hive_skill_guides_team_first_fork_handoff_and_model_routing():
    skill_text = (Path(__file__).resolve().parents[2] / "skills" / "hive" / "SKILL.md").read_text()

    assert "不要先把用户当传话筒" in skill_text
    assert 'hive fork --join-as orch-2 --prompt "先跑 hive thread Veh9 看原始内容' in skill_text
    assert '要直接写清 initial prompt：先跑 `hive thread <msgId>` 拿原始上下文' in skill_text
    assert '第一件事是用 `hive send <sender> "<short takeover with reason>" --reply-to <msgId>` 通知原 sender' in skill_text
    assert "从 orch 手中接管了 X 任务，因为 orch 正在处理 Y" in skill_text
    assert '自己用 `hive send <sender> "<done summary>" --reply-to <msgId> [--artifact <path>]` 沿同一条 thread 回结果' in skill_text
    assert "Claude 偏前端体验、文案收敛和发散式讨论；GPT 偏后端 correctness、约束检查和严谨 review" in skill_text
