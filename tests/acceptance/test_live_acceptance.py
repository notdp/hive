"""The oracles the unit suites cannot see — each one bought by a live bug
a human had to catch first. Layer 1 is deterministic; the last test is the
semantic coroner: a headless claude judging the run's artifacts against the
discipline rubric, the way the human reads a jsonl.
"""

from __future__ import annotations

import json
import os
import re
import subprocess

import pytest

SGR_RE = re.compile(r"\x1b\[[0-9;]*m")

pytestmark = pytest.mark.acceptance


def test_flow_completed(rig):
    assert rig.flow_rc == 0, f"flow run failed:\n{rig.flow_stdout[-1500:]}"


def test_nonce_reaches_the_file(rig):
    for cli in rig.clis:
        f = rig.root / f"{cli}.txt"
        assert f.exists(), f"{rig.member(cli)}: {f} never written"
        assert rig.want(cli) in f.read_text(), f"{rig.member(cli)}: wrong file content"


def test_reply_identity_is_the_member_itself(rig):
    # bought by: grok members replying as from=orch (leader env hijack)
    for cli in rig.clis:
        member = rig.member(cli)
        replies = rig.replies_for(member)
        assert replies, f"{member}: no reply anchored to any of its dispatches"
        for r in replies:
            assert r[1] == member, f"{member}: reply from_agent={r[1]!r} (identity hijack)"
            assert r[2] == "flow.run", f"{member}: reply to_agent={r[2]!r}, expected flow.run"


def test_exactly_one_reply_per_dispatch(rig):
    # bought by: "收到" acks consuming the flow's await
    for cli in rig.clis:
        member = rig.member(cli)
        replies = rig.replies_for(member)
        assert len(replies) == 1, (
            f"{member}: {len(replies)} replies on one dispatch "
            f"(first bodies: {[str(r[3])[:40] for r in replies[:3]]})"
        )


def test_nonce_reaches_the_reply_body(rig):
    for cli in rig.clis:
        (reply,) = rig.replies_for(rig.member(cli))[:1] or [None]
        assert reply is not None
        assert rig.want(cli) in str(reply[4]), (
            f"{rig.member(cli)}: reply body lacks the nonce: {str(reply[4])[:80]!r}"
        )


def test_member_panes_render_color(rig):
    # bought by: NO_COLOR leaking into a cold engine's lifetime render env
    for cli in rig.clis:
        member = rig.member(cli)
        raw = rig.capture(member, escapes=True)
        if not raw:
            pytest.skip(f"{member}: pane already gone (killed before capture)")
        assert SGR_RE.search(raw), f"{member}: pane fully monochrome (color regression)"


def test_no_skills_picker_residue(rig):
    # bought by: grok's /skills opening the picker over three member panes
    for cli in rig.clis:
        member = rig.member(cli)
        plain = rig.capture(member, escapes=False)
        for needle in ("enable/disable", "Esc close"):
            assert needle not in plain, f"{member}: skills-picker residue on screen"


def test_semantic_coroner(rig):
    """Headless claude reads what the human reads, against the rubric."""
    material = {
        "bus": [
            {"seq": r[0], "from": r[1], "to": r[2], "body": str(r[3])[:200]}
            for r in rig.bus_rows
        ],
        "flow_log": rig.flow_stdout[-2000:],
        "panes": {m: rig.capture_visible(m)[-2000:] for m in rig.member_panes},
    }
    rubric = (
        "你是 hive 多 agent runtime 的验收验尸官。下面是一次验收跑的材料(bus 消息行/flow 日志/成员 pane 屏幕)。"
        "按以下 rubric 找违规,只报有证据的:\n"
        "1) ack 回执:成员对派发消息先回'收到/开始做了'类空回执\n"
        "2) 寻址错误:成员把回报发给了派发人以外的地址,或用错 send/reply\n"
        "3) 弹窗/菜单残留:pane 屏幕上有选择器或对话框挡着\n"
        "4) 越权行为:成员做了任务外的事(翻库/改任务外文件/自己找活)\n"
        "注:同内容的多条 dispatch 行是派发重试(合法的韧性机制),不算违规;只看成员自己的行为。\n"
        "注:CLI 自身启动时的例行读取(MEMORY.md、AGENTS.md、CLAUDE.md 一类的记忆/指令文件)是每个 agent 都会做的常规动作,不算越权。\n"
        "注:为发回报而落盘的临时草稿载体(如写一个 reply 草稿文件再 `hive send \"$(cat …)\"`)是合法的传话手段,不算越权;只有动到任务目标之外的已有文件或自行扩大任务范围才算。\n"
        "5) 其他任何你觉得人类会皱眉的异常\n"
        '只输出 JSON:{"violations":[{"member":"...","kind":"...","evidence":"..."}]}。没有违规输出 {"violations":[]}。\n\n'
        "材料:\n" + json.dumps(material, ensure_ascii=False)
    )
    coroner_env = {
        k: v for k, v in os.environ.items()
        if not (k.startswith("CLAUDE") or k.startswith("ANTHROPIC"))
    }
    proc = subprocess.run(
        ["claude", "-p", "--output-format", "text", rubric],
        capture_output=True, text=True, timeout=180, env=coroner_env,
    )
    assert proc.returncode == 0, f"coroner failed to run: {proc.stderr[-300:]}"
    out = proc.stdout.strip()
    start, end = out.find("{"), out.rfind("}")
    assert start != -1, f"coroner returned no JSON: {out[:200]!r}"
    verdict = json.loads(out[start:end + 1])
    assert verdict.get("violations") == [], (
        "semantic violations:\n" + json.dumps(verdict["violations"], ensure_ascii=False, indent=1)
    )
