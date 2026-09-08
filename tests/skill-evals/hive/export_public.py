#!/usr/bin/env python3
"""Export public scenarios for candidate authors; do not expose the source suite."""
import argparse
import json
from pathlib import Path
import shutil

ROOT = Path(__file__).resolve().parent


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("destination", type=Path)
    a = p.parse_args()
    a.destination.mkdir(parents=True, exist_ok=False)
    data = json.loads((ROOT / "evals.json").read_text())
    data["evals"] = [e for e in data["evals"] if not e["held_out"]]
    (a.destination / "evals.json").write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n")
    for case in data["evals"]:
        shutil.copytree(ROOT / "scenarios" / case["name"], a.destination / "scenarios" / case["name"], ignore=shutil.ignore_patterns('__pycache__', '*.pyc'))
    for filename in ("prepare.py", "grade.py", "executor-prompt.md"):
        shutil.copy2(ROOT / filename, a.destination / filename)
    shutil.copytree(ROOT / "harness", a.destination / "harness", ignore=shutil.ignore_patterns('__pycache__', '*.pyc'))
    shutil.copytree(ROOT / 'support', a.destination / 'support', ignore=shutil.ignore_patterns('__pycache__', '*.pyc'))
    print(f"Exported {len(data['evals'])} public cases. Isolate authors from the private source checkout.")


if __name__ == "__main__":
    main()
