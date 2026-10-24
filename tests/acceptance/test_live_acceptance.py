"""The oracles the unit suites cannot see — each one bought by a live bug
a human had to catch first. Layer 1 is deterministic; the last test is the
semantic coroner: a headless claude judging the run's artifacts against the
discipline rubric, the way the human reads a jsonl.

The member's report is the thing under test, so it is never its own
oracle: the tests hold it against the ledger, the nonce file the task
names, and the one transcript fact that matters — the task landed in the
member's conversation exactly once.
"""

from __future__ import annotations

import json
import os
import re
import subprocess

import pytest

from conftest import _bare

SGR_RE = re.compile(r"\x1b\[[0-9;]*m")

pytestmark = pytest.mark.acceptance


def test_every_member_spawned_with_its_task(rig):
    for cli in rig.clis:
        member = rig.member(cli)
        assert rig.spawn_rcs.get(member) == 0, f"{member}: spawn failed:\n{rig.spawn_log[-1500:]}"
        assert rig.spawn_results[member].get("dispatched") is True, (
            f"{member}: spawn answered {rig.spawn_results[member]!r}"
        )


def test_nonce_reaches_the_file(rig):
    for cli in rig.clis:
        f = rig.root / f"{cli}.txt"
        assert f.exists(), f"{rig.member(cli)}: {f} never written"
        assert rig.want(cli) in f.read_text(), f"{rig.member(cli)}: wrong file content"


def test_dispatch_lands_once_in_the_member_transcript(rig):
    # bought by: a lost hived answer retried into a second delivery, and
    # an ok answer that injected nothing — the task must be in the member's
    # own conversation exactly once
    for cli in rig.clis:
        member = rig.member(cli)
        assert rig.dispatch_inputs[member] == 1, (
            f"{member}: task {rig.task_path(member)} found in "
            f"{rig.dispatch_inputs[member]} input records of the member's transcript"
        )


def test_ledger_holds_one_dispatch_and_one_report(rig):
    # bought by: "收到" acks and repeated sends — the member's first send is
    # its report, to the orch that dispatched it, and it sends nothing else
    for cli in rig.clis:
        member = rig.member(cli)
        dispatches = rig.dispatch_rows(member)
        assert len(dispatches) == 1, (
            f"{member}: {len(dispatches)} dispatch rows (→ {member}, artifact "
            f"{rig.task_path(member)}); all rows: "
            f"{[(r[1], r[2], str(r[4])[-40:]) for r in rig.bus_rows]}"
        )
        sent = rig.rows_from(member)
        assert len(sent) == 1, (
            f"{member}: wrote {len(sent)} ledger rows (to {[r[2] for r in sent]}) — "
            "one report is expected, no ack before it and nothing after"
        )
        assert _bare(sent[0][2]) == "orch", (
            f"{member}: reported to {sent[0][2]!r}, not the orch that dispatched it"
        )


def test_report_carries_the_nonce(rig):
    # bought by: a report that answers some other task — the nonce ties the
    # report to this dispatch, in its body or its artifact
    for cli in rig.clis:
        member = rig.member(cli)
        assert rig.want(cli) in rig.report_text(member), (
            f"{member}: report lacks the nonce: {rig.report_text(member)[:200]!r}"
        )


def test_report_lands_in_the_orch_inbox(rig):
    # bought by: a report the ledger records but nobody receives — an orch
    # with no inbox, or a member writing the ledger itself; the delivery is
    # the frame the orch engine's own inbox took, carrying the nonce
    for cli in rig.clis:
        member = rig.member(cli)
        frames = rig.inbox_frames(member)
        assert len(frames) == 1, (
            f"{member}: the orch inbox took {len(frames)} frames from it, one report expected"
        )
        assert rig.want(cli) in frames[0]["message"]["content"], (
            f"{member}: the delivered report lacks the nonce: {frames[0]['message']['content'][:200]!r}"
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
        "reports": {rig.member(c): rig.report_text(rig.member(c))[:600] for c in rig.clis},
        "spawn_log": rig.spawn_log[-2000:],
        "panes": {m: rig.capture_visible(m)[-2000:] for m in rig.member_panes},
    }
    rubric = (
        "你是 hive 多 agent runtime 的验收验尸官。下面是一次验收跑的材料(bus 消息行/各成员的回报/spawn 日志/成员 pane 屏幕)。"
        "每个成员由 orch 用 hive spawn --task 派了一项任务(bus 里发给该成员、artifact 是任务文件的那行);它该做完任务,再用 hive send 向 orch 回报一次。"
        "按以下 rubric 找违规,只报有证据的:\n"
        "1) ack 回执:成员对派发消息先回'收到/开始做了'类空回执\n"
        "2) 多余发信:成员回报不止一次,或发给 orch 以外的人,或追问收没收到\n"
        "3) 弹窗/菜单残留:pane 屏幕上有选择器或对话框挡着\n"
        "4) 越权行为:成员做了任务外的事(翻库/改任务外文件/自己找活)\n"
        "注:CLI 自身启动时的例行读取(MEMORY.md、AGENTS.md、CLAUDE.md 一类的记忆/指令文件)是每个 agent 都会做的常规动作,不算越权。\n"
        "注:打开任务 artifact 读全文、把口令写进任务指定的文件、把回报写成文件走 --artifact,都是任务本身,不算越权。\n"
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
