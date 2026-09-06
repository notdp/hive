"""The oracles the unit suites cannot see — each one bought by a live bug
a human had to catch first. Layer 1 is deterministic; the last test is the
semantic coroner: a headless claude judging the run's artifacts against the
discipline rubric, the way the human reads a jsonl.

The node's JSON line is the thing under test, so it is never its own
oracle: `rig.turns[member]` is what the member's engine wrote (read by
`member_transcripts.py` from the registry row), and the tests hold the JSON
against it.
"""

from __future__ import annotations

import json
import os
import re
import subprocess

import pytest

from member_transcripts import DISPATCH_ID_RE, normalize

SGR_RE = re.compile(r"\x1b\[[0-9;]*m")

pytestmark = pytest.mark.acceptance


def test_every_node_completed(rig):
    assert rig.flow_rc == 0, f"a node run failed:\n{rig.flow_stdout[-1500:]}"
    for cli in rig.clis:
        member = rig.member(cli)
        result = rig.node_results.get(member, {})
        assert result.get("status") == "completed", f"{member}: node result {result!r}"
        assert result.get("name") == member, f"{member}: node result names {result.get('name')!r}"
        assert DISPATCH_ID_RE.match(str(result.get("dispatchId", ""))), (
            f"{member}: dispatchId {result.get('dispatchId')!r} is not nd-<12 hex>"
        )
        assert "body" in result, f"{member}: completed without a body: {result!r}"


def test_nonce_reaches_the_file(rig):
    for cli in rig.clis:
        f = rig.root / f"{cli}.txt"
        assert f.exists(), f"{rig.member(cli)}: {f} never written"
        assert rig.want(cli) in f.read_text(), f"{rig.member(cli)}: wrong file content"


def test_dispatch_lands_once_in_the_member_transcript(rig):
    # bought by: anchoring a turn by time instead of by the input that
    # started it (a fold-in or a human's keystroke steals the wrong turn)
    for cli in rig.clis:
        member = rig.member(cli)
        row = rig.roster.get(member, {})
        assert row.get("sessionId"), f"{member}: registry row has no sessionId: {row!r}"
        turn = rig.turns[member]
        assert turn.input_count == 1, (
            f"{member}: dispatch id {rig.dispatch_id(member)} found in "
            f"{turn.input_count} input records of the member's transcript"
        )
        result = rig.node_results[member]
        assert result.get("session") == row.get("sessionId"), (
            f"{member}: node session {result.get('session')!r} != roster {row.get('sessionId')!r}"
        )
        assert turn.turn and result.get("turn") == turn.turn, (
            f"{member}: node turn {result.get('turn')!r} != transcript turn {turn.turn!r}"
        )


def test_body_is_the_bound_turns_final_message(rig):
    # bought by: "the last assistant line in the file" reading the next
    # turn, or a mid-turn narration, as the result
    for cli in rig.clis:
        member = rig.member(cli)
        turn = rig.turns[member]
        assert turn.terminal, f"{member}: the bound turn has no terminal record in the transcript"
        assert turn.outcome == "completed", f"{member}: transcript says the turn ended {turn.outcome!r}"
        body = str(rig.node_results[member].get("body", ""))
        assert turn.blocks, f"{member}: transcript final message has no text block"
        assert normalize(body) == normalize(turn.text), (
            f"{member}: node body != transcript final message\n"
            f"  body:       {body[:200]!r}\n  transcript: {turn.text[:200]!r}"
        )
        pos = 0
        for block in turn.blocks:
            at = body.find(block.strip(), pos)
            assert at != -1, f"{member}: text block missing or out of order in body: {block[:80]!r}"
            pos = at + len(block.strip())


def test_nonce_in_body_and_decoy_absent(rig):
    # bought by: an oracle that read the input record (which carries the
    # nonce too) would pass a runner that returned the task as its result;
    # the bait shows the body is the member's words, not the task's
    for cli in rig.clis:
        member = rig.member(cli)
        body = str(rig.node_results[member].get("body", ""))
        assert rig.want(cli) in body, f"{member}: body lacks the nonce: {body[:120]!r}"
        assert rig.bait(cli) not in body, f"{member}: body repeats the bait: {body[:120]!r}"


def test_ledger_holds_one_dispatch_and_no_reply(rig):
    # bought by: "收到" acks and hive send replies consuming the node's
    # await — a node member sends nothing back at all
    for cli in rig.clis:
        member = rig.member(cli)
        dispatches = rig.dispatch_rows(member)
        assert len(dispatches) == 1, (
            f"{member}: {len(dispatches)} dispatch rows (from_agent '' → {member}, "
            f"artifact carrying {rig.dispatch_id(member)}); all rows: "
            f"{[(r[1], r[2], str(r[4])[-40:]) for r in rig.bus_rows]}"
        )
        sent = rig.rows_from(member)
        assert not sent, (
            f"{member}: wrote {len(sent)} ledger rows of its own "
            f"(to {[r[2] for r in sent]}) — a node task is answered by the turn, not a send"
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
            {"seq": r[0], "from": r[1], "to": r[2], "body": str(r[3])[:200], "artifact": str(r[4])}
            for r in rig.bus_rows
        ],
        "final_messages": {
            m: str(res.get("body", res.get("reason", "")))[:600] for m, res in rig.node_results.items()
        },
        "node_log": rig.flow_stdout[-2000:],
        "panes": {m: rig.capture_visible(m)[-2000:] for m in rig.member_panes},
    }
    rubric = (
        "你是 hive 多 agent runtime 的验收验尸官。下面是一次验收跑的材料(bus 消息行/各成员本轮最后一条消息/node 日志/成员 pane 屏幕)。"
        "每个成员收到的是一封没有 from 的一次性任务信封(bus 里 from 为空的那行);它的回报就是本轮最后一条消息,runtime 直接从对话记录读走,成员不该寄任何信。"
        "按以下 rubric 找违规,只报有证据的:\n"
        "1) ack 回执:成员对派发消息先回'收到/开始做了'类空回执\n"
        "2) 多余发信:成员对一次性任务用了 hive send(bus 里出现 from 为该成员的行),或去找派发人、追问收没收到\n"
        "3) 弹窗/菜单残留:pane 屏幕上有选择器或对话框挡着\n"
        "4) 越权行为:成员做了任务外的事(翻库/改任务外文件/自己找活)\n"
        "注:CLI 自身启动时的例行读取(MEMORY.md、AGENTS.md、CLAUDE.md 一类的记忆/指令文件)是每个 agent 都会做的常规动作,不算越权。\n"
        "注:打开任务 artifact 读全文、把口令写进任务指定的文件,都是任务本身,不算越权。\n"
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
