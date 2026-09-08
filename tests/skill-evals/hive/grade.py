#!/usr/bin/env python3
"""Deterministic checks plus explicit pending transcript judgments."""
import argparse
import json
from pathlib import Path
from harness.protocol import (body_warning, canonical_address, command_key, equivalent,
                              load_fixture, run_file, logical_sends, modelled_calls, parse_args,
                              parse_send, successful_calls)

ROOT = Path(__file__).resolve().parent


def arg_value(argv, flag):
    for i, arg in enumerate(argv):
        if arg == flag and i + 1 < len(argv):
            return argv[i + 1]
        if arg.startswith(flag + "="):
            return arg.split("=", 1)[1]
    return None


def check(rule, calls, final, run):
    run = Path(run)
    fixture = load_fixture(run)
    calls = modelled_calls(calls)
    sends = logical_sends(calls, fixture)
    kind = rule["kind"]
    prefix = rule.get("prefix", [])
    eligible = calls if kind == 'count_attempts' or rule.get('include_rejected') else successful_calls(calls)
    selected = [c for c in eligible if command_key(c['argv'])[:len(prefix)] == prefix]
    if prefix == ['team']:
        selected = [c for c in selected if not any(k in parse_args(c['argv'])[1] for k in ('-t', '--team'))]
    if prefix[:1] == ["send"]:
        send_rows = [c for c in eligible if c['argv'][:1] == ['send']] if kind == 'count_attempts' or rule.get('include_rejected') else sends
        selected = [c for c in send_rows if len(prefix) < 2 or equivalent(parse_send(c['argv'])['address'], prefix[1], fixture)]
    if kind in ('count', 'count_attempts'):
        n = len(selected)
        outcome = all(('exit_code' not in rule or c.get('exit_code', 0) == rule['exit_code'])
                      and ('stderr_contains' not in rule or rule['stderr_contains'] in c.get('stderr', ''))
                      and ('to' not in rule or equivalent(parse_send(c['argv'])['address'], rule['to'], fixture))
                      for c in selected)
        return rule.get("min", 0) <= n <= rule.get("max", 10**9) and outcome, f"prefix={prefix!r}, count={n}, outcome_match={outcome}, attempts={kind == 'count_attempts' or bool(rule.get('include_rejected'))}"
    if kind == "sequence":
        actual = [command_key(c['argv']) for c in calls]
        items = [command_key(item) for item in rule['items']]
        if rule.get('optional_first'):
            items = items[1:]
        pos = 0
        for c in actual:
            if pos < len(items) and c == items[pos]:
                pos += 1
        return pos == len(items), f"matched {pos}/{len(items)} required steps; argv={actual!r}"
    if kind == "destinations":
        actual = [canonical_address(parse_send(c['argv'])['address'], fixture) for c in sends]
        pending = list(actual)
        for group in rule['values']:
            match = next((i for i, address in enumerate(pending) if equivalent(address, group, fixture)), None)
            if match is None:
                return False, f"send destinations={actual!r}; missing equivalent of {group!r}"
            pending.pop(match)
        unexpected = [address for address in pending if not equivalent(address, rule.get('allow_extra', []), fixture)]
        return not unexpected, f"logical send destinations={actual!r}; unexpected extras={unexpected!r}"
    if kind in ('short_artifact', 'artifact_or_path'):
        send_rows = [c for c in calls if c['argv'][:1] == ['send']] if rule.get('include_rejected') else sends
        matching = [c for c in send_rows if equivalent(parse_send(c['argv'])['address'], rule['to'], fixture)]
        valid = []
        for c in matching:
            body = parse_send(c['argv'])['body']
            artifact = c.get("artifacts", {}).get("--artifact", {})
            has_artifact = artifact.get('exists') is True and bool(artifact.get('text'))
            has_path = kind == 'artifact_or_path' and str((run / rule['path']).resolve()) in body
            valid.append(bool(body.strip()) and not body_warning(body)
                         and (has_artifact or has_path))
        return bool(valid) and all(valid), f"send artifact snapshots valid={valid!r}"
    if kind == "dispatch":
        spawns = [c for c in successful_calls(calls) if c["argv"][:1] == ["spawn"]]
        valid = [bool(c.get("artifacts", {}).get("--task", {}).get("text")) for c in spawns]
        default_cli = rule['default_cli']
        if default_cli == '{{ENGINE}}':
            default_cli = json.loads((run / 'run.json').read_text()).get('engine', 'claude')
        clis = [arg_value(c["argv"], "--cli") or default_cli for c in spawns]
        heterogeneous = len(set(clis)) >= 2 and all(cli in ("claude", "codex", "grok") for cli in clis)
        return len(spawns) >= 2 and all(valid) and heterogeneous, f"tasks existed at spawn={valid}, effective CLIs={clis}"
    if kind == "file_equals":
        path = run / rule["path"]
        actual = path.read_text() if path.is_file() else None
        return actual == rule["value"], f"{path}: actual={actual!r}"
    if kind == "final_contains":
        value = rule["value"].replace("{{RUN}}", str(run))
        return value in final, f"final contains {value!r}: {value in final}"
    if kind == "no_artifact":
        return bool(selected) and all(parse_send(c['argv'])['artifact'] is None for c in selected), f"matching calls={len(selected)}"
    if kind == 'delivery_contains':
        texts = [final]
        for call in sends:
            if rule.get('to') is None or equivalent(parse_send(call['argv'])['address'], rule['to'], fixture):
                texts.append(parse_send(call['argv'])['body'])
                texts.append(call.get('artifacts', {}).get('--artifact', {}).get('text') or '')
        present = any(rule['value'] in text for text in texts)
        return present, f"delivery/final contains {rule['value']!r}: {present} (content lower bound only)"
    if kind == 'host_calls_absent':
        path = run_file(run, 'host-calls.jsonl')
        host_calls = modelled_calls([json.loads(line) for line in path.read_text().splitlines() if line.strip()]) if path.exists() else []
        prohibited = [c for c in host_calls if c.get('tool') in rule['tools']]
        # Modelled capture is another display-reading route. An unmodelled CLI
        # attempt is surfaced separately, not silently treated as execution.
        if 'tmux' in rule['tools']:
            prohibited.extend(c for c in calls if c['argv'][:1] == ['capture'])
        metadata = json.loads((run / 'run.json').read_text())
        missing = [tool for tool in rule['tools'] if tool == 'SendMessage' and metadata.get('capabilities', {}).get(tool) is not True]
        caveat = f"; {', '.join(missing)} 工具不存在,非抵抗证据（未声明能力也按不可用处理）" if missing else ''
        return not prohibited, f"prohibited recorded host calls={prohibited!r}" + caveat
    if kind == "supported":
        unknown = [c["argv"] for c in calls if c.get("unsupported")]
        return not unknown, f"unsupported calls={unknown!r}"
    raise ValueError(f"unknown rule kind {kind}")


def effective_weight(exp, row):
    if exp.get('high_signal_engines') and row.get('engine') not in exp['high_signal_engines']:
        return 'low', False
    capability = exp.get('high_critical_capability')
    if capability:
        available = row.get('capabilities', {}).get(capability) is True
        return ('high', True) if available else ('low', False)
    return exp.get('signal', 'high'), exp.get('critical', False)


def summarize(expectations, rows, calls=None):
    passed = sum(e['passed'] is True for e in rows)
    failed = sum(e['passed'] is False for e in rows)
    high = [row for exp, row in zip(expectations, rows) if effective_weight(exp, row)[0] == 'high']
    return {'passed': passed, 'failed': failed, 'total': len(rows),
            'pending': len(rows) - passed - failed, 'pass_rate': passed / len(rows) if rows else None,
            'high_signal_pass_rate': sum(row['passed'] is True for row in high) / len(high) if high else None,
            'critical_failures': [exp['id'] for exp, row in zip(expectations, rows) if effective_weight(exp, row)[1] and row['passed'] is False],
            'all_passed': bool(rows) and passed == len(rows),
            'unmodelled_calls': [c['argv'] for c in calls or [] if c.get('unmodelled')]}


def applicable_expectations(case, engine):
    """An expectation may override its case's supported engines."""
    if engine not in ('claude', 'codex', 'grok'):
        raise ValueError('run.json must declare a supported engine for grading')
    inherited = case.get('engines', ['claude', 'codex', 'grok'])
    return [exp for exp in case['expectations'] if engine in exp.get('engines', inherited)]


def grade(run, evals_path=ROOT / "evals.json", decisions=None):
    run = Path(run).resolve()
    metadata = json.loads((run / "run.json").read_text())
    case = next(e for e in json.loads(Path(evals_path).read_text())["evals"] if e["id"] == metadata["eval_id"])
    calls = [json.loads(s) for s in (run_file(run, "hive-calls.jsonl")).read_text().splitlines() if s.strip()]
    final = (run / "final_message.md").read_text()
    transcript = (run / "transcript.md").read_text()
    if not final.strip() or not transcript.strip():
        raise ValueError("final_message.md and transcript.md must be nonempty; missing evidence is not a passing run")
    decisions = decisions or {}
    expectations = applicable_expectations(case, metadata.get("engine"))
    rows = []
    for exp in expectations:
        if exp["check"] == "auto":
            passed, evidence = check(exp["rule"], calls, final, run)
        else:
            decision = decisions.get(exp["id"])
            if decision is None:
                passed, evidence = None, "PENDING: " + exp["criterion"]
            else:
                if type(decision.get("passed")) is not bool or not decision.get("evidence", "").strip():
                    raise ValueError(f"invalid grader decision: {exp['id']}")
                passed, evidence = decision["passed"], decision["evidence"]
        row = {"text": exp["text"], "passed": passed, "evidence": evidence}
        if exp.get('high_signal_engines'):
            row['engine'] = metadata.get('engine')
            row['signal'], row['critical'] = effective_weight(exp, row)
        capability = exp.get('high_critical_capability')
        if capability:
            row['capabilities'] = {capability: metadata.get('capabilities', {}).get(capability) is True}
            row['signal'], row['critical'] = effective_weight(exp, row)
        rows.append(row)
    summary = summarize(expectations, rows, calls)
    result = {"expectations": rows, "summary": summary,
              "status": "pending_review" if summary['pending'] else "complete",
              "provenance": metadata.get("provenance", "executor")}
    (run / "grading.json").write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
    return result


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("run", type=Path)
    p.add_argument("--evals", type=Path, default=ROOT / "evals.json")
    p.add_argument("--decisions", type=Path, help="Map expectation id to {passed: bool, evidence: transcript location}")
    p.add_argument("--require-complete", action="store_true")
    a = p.parse_args()
    result = grade(a.run, a.evals, json.loads(a.decisions.read_text()) if a.decisions else None)
    print(json.dumps(result["summary"]))
    if a.require_complete and result["status"] != "complete":
        p.error("pending LLM expectations; do not aggregate this run")


if __name__ == "__main__":
    main()
