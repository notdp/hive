#!/usr/bin/env python3
"""Reject missing, pending, synthetic, or mismatched runs before aggregation."""
import argparse
import json
from pathlib import Path
from grade import summarize
from harness.protocol import run_file

ROOT = Path(__file__).resolve().parent


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("iteration", type=Path)
    p.add_argument("--split", choices=("public", "held-out", "all"), default="public")
    p.add_argument("--repetitions", type=int, default=3)
    a = p.parse_args()
    if a.repetitions < 1:
        p.error("repetitions must be positive")
    iteration_meta = a.iteration / 'iteration.json'
    if not iteration_meta.is_file():
        p.error('missing iteration.json with engine')
    engine = json.loads(iteration_meta.read_text()).get('engine')
    if engine not in ('claude', 'codex', 'grok'):
        p.error('iteration.json engine must be claude, codex or grok')
    cases = json.loads((ROOT / "evals.json").read_text())["evals"]
    selected = [e for e in cases if engine in e.get('engines', ['claude','codex','grok'])
                and (a.split == "all" or e["held_out"] == (a.split == "held-out"))]
    expected = set()
    frozen_skills = {}
    for case in selected:
        for config in ("with_skill", "without_skill"):
            for n in range(1, a.repetitions + 1):
                run = a.iteration / f"eval-{case['id']}" / config / f"run-{n}"
                expected.add((run / "grading.json").resolve())
                for filename in ('grading.json', 'run.json', 'timing.json'):
                    if not (run / filename).is_file():
                        p.error(f'missing {filename}: {run}')
                data = json.loads((run / "grading.json").read_text())
                metadata = json.loads((run / "run.json").read_text())
                if metadata["eval_id"] != case["id"] or data.get("provenance") != "executor":
                    p.error(f"mismatched or synthetic run: {run}")
                hashes = metadata.get('skill_sha256')
                if not isinstance(hashes, dict) or not hashes or 'SKILL.md' not in hashes:
                    p.error(f'missing skill snapshot hashes: {run}')
                if config in frozen_skills and frozen_skills[config] != hashes:
                    p.error(f'skill changed within configuration {config}: {run}')
                frozen_skills[config] = hashes
                rows = data["expectations"]
                inherited = case.get('engines', ['claude', 'codex', 'grok'])
                applicable = [e for e in case["expectations"] if engine in e.get('engines', inherited)]
                if [r["text"] for r in rows] != [e["text"] for e in applicable]:
                    p.error(f"expectations differ from frozen suite: {run}")
                if data.get("status") != "complete" or any(type(r["passed"]) is not bool or not r["evidence"].strip() for r in rows):
                    p.error(f"pending/incomplete grading: {run}")
                summary = data["summary"]
                call_file = run_file(run, 'hive-calls.jsonl')
                calls = [json.loads(line) for line in call_file.read_text().splitlines() if line.strip()] if call_file.exists() else []
                expected_summary = summarize(case['expectations'], rows, calls)
                if any(summary.get(key) != value for key, value in expected_summary.items()):
                    p.error(f"stale summary: {run}")
                timing = json.loads((run / "timing.json").read_text())
                if timing.get("total_tokens", -1) < 0 or timing.get("total_duration_seconds", -1) < 0:
                    p.error(f"missing actual timing/tokens: {run}")
    if frozen_skills.get('with_skill') == frozen_skills.get('without_skill'):
        p.error('with_skill and without_skill have identical skill snapshots')
    actual = {p.resolve() for p in a.iteration.glob("eval-*/*/run-*/grading.json")}
    if actual != expected:
        p.error(f"extra or missing runs: {actual ^ expected}")
    print(f"Ready: {len(expected)} complete runs; split={a.split}; engine={engine}")


if __name__ == "__main__":
    main()
