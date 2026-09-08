#!/usr/bin/env python3
"""Prepare an isolated executor run; does not launch an LLM or real Hive."""
import argparse
import hashlib
import json
from pathlib import Path
import shlex
import shutil
import subprocess
from harness.protocol import fixture_path

ROOT = Path(__file__).resolve().parent


def prepare(name, run, skill, engine='claude'):
    case = next(e for e in json.loads((ROOT / "evals.json").read_text())["evals"] if e["name"] == name)
    run, skill = Path(run).resolve(), Path(skill).resolve()
    if not (skill / "SKILL.md").is_file():
        raise ValueError("skill must be a directory containing SKILL.md")
    run.mkdir(parents=True, exist_ok=False)
    shutil.copytree(ROOT / "scenarios" / name / "files", run / "files")
    shutil.copytree(skill, run / "skill", ignore=shutil.ignore_patterns('__pycache__', '*.pyc'))
    shutil.copytree(ROOT / "harness", run / "bin", ignore=shutil.ignore_patterns('__pycache__', '*.pyc'))
    entry_prefix = {'claude': '/hive:hive', 'codex': '$hive', 'grok': '/hive'}[engine]
    entry_team = case.get('entry_team', 'wasp')
    entry = entry_prefix + (' ' + entry_team if entry_team else '')
    replacements = {"{{RUN}}": str(run), "{{FILES}}": str(run / "files"), "{{SKILL}}": str(run / "skill"), "{{WORKSPACE}}": str(run / "workspace")}
    replacements.update({'{{ENTRY}}': entry, '{{ENGINE}}': engine})

    def render(text):
        for old, new in replacements.items():
            text = text.replace(old, new)
        return text

    for path in (run / "files").rglob("*"):
        if path.is_file():
            path.write_text(render(path.read_text()))
    for directory in ("outputs", "workspace/artifacts/tasks", "shared", "isolated", "home", "hive-home"):
        (run / directory).mkdir(parents=True, exist_ok=True)
    # Minimal real git repositories permit pwd/rev-parse/status evidence without
    # touching the source checkout. The stub models worktree start, not Git internals.
    if name == "worktree-edit":
        for directory in ("shared", "isolated"):
            (run / directory / "value.txt").write_text("old\n")
            subprocess.run(["git", "init", "-q", str(run / directory)], check=True)
    def render_value(value):
        if isinstance(value, dict):
            return {k: render_value(v) for k, v in value.items()}
        if isinstance(value, list):
            return [render_value(v) for v in value]
        return render(value) if isinstance(value, str) else value

    fixture = render_value(json.loads((ROOT / 'scenarios' / name / 'fixture.json').read_text()))
    if fixture.get('entry_state') and engine != 'claude':
        fixture['team']['members'] = []
        for rule in fixture['rules']:
            if rule.get('argv', [])[:1] == ['create']:
                for response in rule['responses']:
                    response['stdout'] = '\n'.join(line for line in response['stdout'].splitlines()
                        if not line.startswith(('You are ', 'Rename this session now:'))) + '\n'
    control = fixture_path(run)
    control.parent.mkdir(parents=True, exist_ok=False)
    control.write_text(json.dumps(fixture, ensure_ascii=False, indent=2) + '\n')
    shutil.copy2(ROOT / 'support' / 'checkpoint.py', control.parent / 'checkpoint.py')
    shutil.copy2(ROOT / 'support' / 'help.json', control.parent / 'help.json')
    for roster in [fixture['team'], *fixture.get('teams', {}).values()]:
        Path(roster['runtimeWorkspace'], 'artifacts/tasks').mkdir(parents=True, exist_ok=True)
    for path, text in fixture.get('initial_files', {}).items():
        target = run / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text)
    (control.parent / "prompt.md").write_text(render(case["prompt"]))
    (control.parent / "hive-calls.jsonl").touch()
    (control.parent / "host-calls.jsonl").touch()
    hashes = {str(p.relative_to(run / "skill")): hashlib.sha256(p.read_bytes()).hexdigest()
              for p in (run / "skill").rglob("*") if p.is_file()}
    (run / "run.json").write_text(json.dumps({"eval_id": case["id"], "name": name, "engine": engine, "skill_sha256": hashes, "provenance": "executor"}, indent=2) + "\n")
    env = {"HIVE_EVAL_CONTROL": str(control.parent), "HIVE_EVAL_HOST_LOG": str(control.parent / "host-calls.jsonl"), "HIVE_EVAL_LOG": str(control.parent / "hive-calls.jsonl"), "HIVE_EVAL_RUN": str(run), 'HIVE_HOME': str(run / 'hive-home')}
    setup = "\n".join("export " + k + "=" + shlex.quote(v) for k, v in env.items())
    setup += '\nexport PATH=' + shlex.quote(str(run / "bin")) + ':"$PATH"\n'
    (run / "env.sh").write_text(setup)
    template = (ROOT / "executor-prompt.md").read_text()
    (control.parent / "executor-prompt.md").write_text(render(template) + "\n\n" + render(case["prompt"]))
    return run


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("scenario")
    p.add_argument("run", type=Path)
    p.add_argument("--skill", required=True, type=Path)
    p.add_argument('--engine', choices=('claude', 'codex', 'grok'), default='claude')
    a = p.parse_args()
    print(prepare(a.scenario, a.run, a.skill, a.engine))


if __name__ == "__main__":
    main()
