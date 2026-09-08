#!/usr/bin/env python3
"""Executor runner for the hive skill evals: one command runs one candidate
over one split (prepare -> isolated headless engine -> transcript/timing ->
grade -> optional LLM grading), resumable and parallel. `--engine claude`
(default) drives `claude -p`, `--engine codex` drives `codex exec --json`; the
LLM grader is claude either way.

Nothing in tests/skill-evals/hive/ is imported; prepare.py and grade.py are
called as subprocesses so the frozen standard stays untouched.
"""
import argparse
import concurrent.futures
import datetime
import json
from pathlib import Path
import shutil
import subprocess
import sys
import threading

sys.path.insert(0, str(Path(__file__).resolve().parent))
import claude_exec  # noqa: E402
import codex_exec  # noqa: E402
import grade_llm  # noqa: E402
from control_access import record_control_access

HERE = Path(__file__).resolve().parent
DEFAULT_EVAL_ROOT = HERE.parent / "hive"
PRINT_LOCK = threading.Lock()
ENGINES = ("claude", "codex")
ENGINES_ALL = ("claude", "codex", "grok")  # what an evals.json case without `engines` means
CAPABILITIES = {"SendMessage": True}  # both engines carry the mcp_sendmessage.py stub
DEFAULT_MODEL = {"claude": "opus", "codex": None}  # None: the engine's own default
PREPARE_ENGINE_FLAG = {}


def log(msg):
    with PRINT_LOCK:
        print(msg, flush=True)


def case_engines(case):
    """A case without `engines` runs on every engine."""
    return list(case.get("engines") or ENGINES_ALL)


def select_cases(eval_root, split, scenarios, engine, force_engines=False):
    """(cases to run, cases skipped because their `engines` excludes this
    engine). `--force-engines` runs them anyway, for a proxy experiment."""
    cases = json.loads((eval_root / "evals.json").read_text())["evals"]
    if split != "all":
        cases = [c for c in cases if c["held_out"] == (split == "held-out")]
    if scenarios:
        wanted = [s.strip() for s in scenarios.split(",") if s.strip()]
        known = {c["name"] for c in cases}
        unknown = [s for s in wanted if s not in known]
        if unknown:
            raise SystemExit(f"unknown scenario(s) for split {split}: {unknown}")
        cases = [c for c in cases if c["name"] in wanted]
    skipped = [c for c in cases if engine not in case_engines(c)]
    if not force_engines:
        cases = [c for c in cases if engine in case_engines(c)]
    return cases, skipped


def write_iteration(iteration, engine, skipped, forced):
    """`<iteration>/iteration.json`: which engine this iteration belongs to and
    which cases were skipped (or forced) for it. Every invocation into the
    same iteration must be the same engine; the skipped/forced sets are merged."""
    path = iteration / "iteration.json"
    data = json.loads(path.read_text()) if path.is_file() else {}
    if data.get("engine") not in (None, engine):
        raise SystemExit(f"{path} belongs to engine {data['engine']!r}; refusing to mix in {engine!r} runs")
    data["engine"] = engine
    data["skipped_cases"] = sorted(set(data.get("skipped_cases") or []) | {c["name"] for c in skipped})
    data["forced_cases"] = sorted(set(data.get("forced_cases") or []) | {c["name"] for c in forced})
    data["case_engines"] = {**(data.get("case_engines") or {}), **{c["name"]: case_engines(c) for c in skipped + forced}}
    path.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n")
    return data


def run_state(run):
    """What a run directory already holds, for resume decisions."""
    if not run.exists():
        return "absent"
    if not (run / "raw.jsonl").exists():
        return "prepared"
    if (run / "final_message.md").is_file() and (run / "grading.json").is_file():
        return "graded"
    return "executed"


def grading_status(run):
    path = run / "grading.json"
    if not path.is_file():
        return None
    return json.loads(path.read_text()).get("status")


def move_aside(run):
    stamp = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
    target = run.with_name(f"{run.name}.failed-{stamp}")
    shutil.move(str(run), str(target))
    # prepare.py keeps the fixture outside the run, in a hidden sibling
    # `.run-n.control/`; it goes with the run so the next prepare can recreate it.
    control = run.with_name(f".{run.name}.control")
    if control.is_dir():
        shutil.move(str(control), str(run.with_name(f".{target.name}.control")))
    return target


def prepare_accepts_engine(eval_root):
    """prepare.py is the frozen standard's; pass --engine only once it has it."""
    key = str(eval_root)
    if key not in PREPARE_ENGINE_FLAG:
        proc = subprocess.run([sys.executable, str(eval_root / "prepare.py"), "--help"], capture_output=True, text=True)
        PREPARE_ENGINE_FLAG[key] = "--engine" in proc.stdout
    return PREPARE_ENGINE_FLAG[key]


def prepare(eval_root, case, run, skill, engine):
    cmd = [sys.executable, str(eval_root / "prepare.py"), case["name"], str(run), "--skill", str(skill)]
    if prepare_accepts_engine(eval_root):
        cmd += ["--engine", engine]
    proc = subprocess.run(cmd, capture_output=True, text=True)
    if proc.returncode != 0:
        raise RuntimeError(f"prepare.py failed: {proc.stderr.strip()[-500:]}")
    # grade.py reads run.json's `engine` (the default CLI of a spawn); prepare
    # writes it once it takes --engine, until then the runner records it.
    claude_exec.update_json(run / "run.json", lambda d: d.setdefault("engine", engine))
    meta = run.parent.parent / "eval_metadata.json"
    if not meta.is_file():
        meta.write_text(json.dumps({"eval_id": case["id"], "eval_name": case["name"]}) + "\n")


def settle_final(run, cr):
    """The model should have written final_message.md and ended with the same
    text. Verify; fall back to the last assistant text and say so."""
    last = cr.last_assistant_text()
    path = run / "final_message.md"
    model_text = path.read_text(encoding="utf-8", errors="replace") if path.is_file() else None
    if last is None:
        return {"final_source": "none", "final_reason": "no assistant text in the transcript"}
    if model_text is None:
        path.write_text(last)
        return {"final_source": "runner", "final_reason": "model did not write final_message.md; last assistant text used"}
    if model_text.strip() == last.strip():
        return {"final_source": "model", "final_reason": None}
    shutil.copy(path, run / "final_message.model.md")
    path.write_text(last)
    return {"final_source": "runner",
            "final_reason": "final_message.md differed from the last assistant text; model copy kept as final_message.model.md"}


def executor_prompt(run, env):
    """The prompt prepare.py rendered: in the control directory under the v4
    standard (HIVE_EVAL_CONTROL in env.sh), in the run root before it."""
    path = claude_exec.run_file(run, "executor-prompt.md", env)
    if not path.is_file():
        raise RuntimeError(f"executor-prompt.md not found in {claude_exec.control_dir(run, env) or run} or {run}")
    return path.read_text(), path


def execute(run, args):
    return execute_codex(run, args) if args.engine == "codex" else execute_claude(run, args)


def scenario_header(run):
    return f"- scenario: eval-{run.parent.parent.name.split('-', 1)[-1]} / {run.parent.name} / {run.name}"


def not_started(run, cmd, executor_line, isolation, extra_timing):
    """Artifacts for a run the gate stopped before the model ran."""
    failure = "isolation_failed (" + isolation.get("stage", "gate") + "): " + "; ".join(isolation["problems"])
    (run / "isolation.json").write_text(json.dumps(isolation, ensure_ascii=False, indent=2) + "\n")
    timing = {"total_tokens": 0, "tokens": {}, "tokens_source": "none", "total_duration_seconds": -1,
              "num_turns": None, "executor_model": None, **extra_timing}
    (run / "transcript.md").write_text(claude_exec.render_transcript([], [
        scenario_header(run), executor_line, f"- command: {' '.join(cmd)}", f"- executor_status: isolation_failed ({failure})"]))
    (run / "timing.json").write_text(json.dumps(timing, ensure_ascii=False, indent=2) + "\n")
    final = {"final_source": "none", "final_reason": "isolation failed before execution"}
    return failure, timing, final


def execute_claude(run, args):
    env = claude_exec.shell_isolation(run, claude_exec.env_from_script(run / "env.sh", claude_exec.washed_env()))
    prompt, prompt_path = executor_prompt(run, env)
    control = claude_exec.control_dir(run, env)
    # Claude Code snapshots the login shell named by $SHELL before the first
    # Bash call and sources that shell's rc file explicitly (~/.zshrc for zsh,
    # ignoring ZDOTDIR); the user's ~/.zshrc runs `hive shell-init zsh` under
    # the run's PATH and the stub would log it as the model's first
    # (unsupported) call. The bash rc files carry no hive line, so the
    # snapshot comes from bash. Whether that shell still resolves `hive` to
    # the stub is what the probe below checks.
    env["SHELL"] = args.shell
    mcp = claude_exec.mcp_config(run, env)
    servers = tuple(mcp["mcpServers"])
    # The control directory is not an --add-dir: claude's Bash is not sandboxed,
    # so the stubs write their logs there without it being a working directory.
    cmd = claude_exec.build_command(args.model, args.tools, args.max_turns, args.permission_mode, add_dirs=[run], mcp=mcp)
    probe = claude_exec.probe_hive_path(run, env)
    executor_kind = {"kind": "claude-code", "command": cmd, "model_requested": args.model, "permission_mode": args.permission_mode,
                     "tools": args.tools, "mcp_servers": list(servers), "shell": args.shell, "cwd": str(run / "shared"),
                     "add_dirs": [str(run)], "control_dir": str(control) if control else None,
                     "prompt_path": str(prompt_path), "env_washed": claude_exec.washed_names()}
    if not probe["ok"]:
        isolation = {"ok": False, "stage": "shell_probe", "problems": list(probe["problems"]), "shell_probe": probe, "init": None}
        failure, timing, final = not_started(run, cmd, "- executor: claude-code (not started)", isolation, {"claude_version": None})
        cr, status = None, "isolation_failed"
    else:
        cr = claude_exec.ClaudeRun(cmd, prompt, cwd=run / "shared", env=env, raw_path=run / "raw.jsonl",
                                   stderr_path=run / "claude.stderr.log", timeout=args.timeout_seconds,
                                   allowed_tools=args.tools, mcp_servers=servers).run()
        isolation = dict(cr.isolation or {"ok": False, "problems": ["no init event"]})
        isolation["stage"] = "init"
        isolation["shell_probe"] = probe
        (run / "isolation.json").write_text(json.dumps(isolation, ensure_ascii=False, indent=2) + "\n")
        failure = cr.failure_reason()
        if cr.init is None and failure is None:
            failure = "no init event"
        status = "ok"
        if failure:
            status = "isolation_failed" if cr.killed_for_isolation or cr.init is None else "execution_failed"
        timing = cr.timing()
        header = [scenario_header(run),
                  f"- executor: claude-code {timing.get('claude_version')} model={timing.get('executor_model')}",
                  f"- command: {' '.join(cmd)}",
                  f"- cwd: {run / 'shared'}",
                  f"- executor_status: {status}" + (f" ({failure})" if failure else "")]
        (run / "transcript.md").write_text(claude_exec.render_transcript(cr.events, header))
        (run / "timing.json").write_text(json.dumps(timing, ensure_ascii=False, indent=2) + "\n")
        final = settle_final(run, cr) if status != "isolation_failed" else {"final_source": "none", "final_reason": "isolation failed before execution"}

    def mark(data):
        data["executor"] = {
            **executor_kind, "model": timing.get("executor_model"),
            "claude_version": timing.get("claude_version"), "session_id": timing.get("session_id"),
            "started_at": timing.get("started_at"), "finished_at": timing.get("finished_at"),
            "exit_code": cr.exit_code if cr else None, "status": status, "reason": failure,
            "isolation_ok": bool(isolation.get("ok")),
        }
        data["capabilities"] = dict(CAPABILITIES)
        data.update(final)
    claude_exec.update_json(run / "run.json", mark)
    return status, failure, timing


def execute_codex(run, args):
    """`codex exec --json` under a throwaway CODEX_HOME; the preamble gate runs
    before the model does, and again on the rollout afterwards."""
    env = codex_exec.executor_env(run, args.shell)
    prompt, prompt_path = executor_prompt(run, env)
    control = claude_exec.control_dir(run, env)
    mcp = codex_exec.mcp_config(run, env)
    codex_exec.make_home(run, args.model, args.reasoning_effort, mcp=mcp)
    # The seatbelt only lets a shell command write under cwd, the --add-dir
    # roots, /tmp and $TMPDIR; the stubs write HIVE_EVAL_LOG/HIVE_EVAL_HOST_LOG
    # in the control directory, so it has to be a root (see codex_exec.build_command).
    add_dirs = [control] if control else []
    tag_log = lambda m: log(f"[{run.parent.parent.name} {run.parent.name} {run.name}] {m}")  # noqa: E731
    probe = codex_exec.probe_hive_path(run, env)
    if probe["ok"]:
        pre = codex_exec.preflight(run, env, log=tag_log, mcp_servers=tuple(mcp))
        stage = "preflight"
    else:
        pre = {"ok": False, "problems": list(probe["problems"]), "source": "shell_probe"}
        stage = "shell_probe"
    isolation = {"ok": pre["ok"], "stage": stage, "problems": list(pre["problems"]), "shell_probe": probe,
                 "preflight": pre, "rollout": None}
    cmd = codex_exec.build_command(run, args.model, add_dirs=add_dirs)
    cr = None
    if pre["ok"]:
        cr = codex_exec.CodexRun(cmd, prompt, run, env, args.timeout_seconds).run_process()
        if cr.rollout:
            post = codex_exec.check_preamble(cr.preamble_items(), run, "rollout", control=control)
            isolation["rollout"] = post
            isolation["problems"] += post["problems"]
        else:
            isolation["rollout"] = {"ok": False, "problems": ["no rollout file found under codex-home/sessions"]}
            isolation["problems"].append("no rollout file found under codex-home/sessions")
        isolation["ok"] = not isolation["problems"]
        isolation["stage"] = "rollout" if not isolation["ok"] else stage
    codex_exec.scrub_home(run)
    if cr is None:
        status = "isolation_failed"
        version = codex_exec.codex_version()
        failure, timing, final = not_started(run, cmd, f"- executor: codex {version} (not started)", isolation,
                                             {"codex_version": version})
    else:
        (run / "isolation.json").write_text(json.dumps(isolation, ensure_ascii=False, indent=2) + "\n")
        timing = cr.timing()
        failure = cr.failure_reason()
        status = "ok"
        if not isolation["ok"]:
            status, failure = "isolation_failed", "isolation_failed (rollout preamble): " + "; ".join(isolation["problems"])
        elif failure:
            status = "execution_failed"
        header = [scenario_header(run),
                  f"- executor: codex {timing.get('codex_version')} model={timing.get('executor_model')} "
                  f"reasoning_effort={timing.get('reasoning_effort')}",
                  f"- command: {' '.join(cmd)}",
                  f"- cwd: {run / 'shared'}",
                  f"- executor_status: {status}" + (f" ({failure})" if failure else "")]
        (run / "transcript.md").write_text(codex_exec.render_transcript(cr.rollout, header))
        (run / "timing.json").write_text(json.dumps(timing, ensure_ascii=False, indent=2) + "\n")
        final = settle_final(run, cr)

    def mark(data):
        data["executor"] = {
            "kind": "codex-exec", "command": cmd, "model_requested": args.model, "model": timing.get("executor_model"),
            "reasoning_effort": timing.get("reasoning_effort"), "codex_version": timing.get("codex_version"),
            "session_id": timing.get("session_id"), "sandbox": "workspace-write", "approval_policy": "never",
            "codex_home": str(codex_exec.codex_home(run)), "mcp_servers": list(mcp), "shell": args.shell,
            "cwd": str(run / "shared"), "add_dirs": [str(run)] + [str(d) for d in add_dirs],
            "control_dir": str(control) if control else None, "prompt_path": str(prompt_path),
            "env_washed": claude_exec.washed_names(), "started_at": timing.get("started_at"),
            "finished_at": timing.get("finished_at"), "exit_code": cr.proc.exit_code if cr else None,
            "status": status, "reason": failure, "isolation_ok": isolation["ok"],
            "skills_disabled": len(pre.get("skills_disabled") or []),
            "native_sections": pre.get("native_sections"),
        }
        data["capabilities"] = dict(CAPABILITIES)
        data.update(final)
    claude_exec.update_json(run / "run.json", mark)
    return status, failure, timing


def grade_auto(eval_root, run):
    proc = subprocess.run([sys.executable, str(eval_root / "grade.py"), str(run)], capture_output=True, text=True)
    if proc.returncode != 0:
        return None, proc.stderr.strip()[-500:]
    return json.loads(proc.stdout.strip().splitlines()[-1]), None


def process(job, args):
    case, n = job
    run = args.iteration / f"eval-{case['id']}" / args.configuration / f"run-{n}"
    tag = f"[eval-{case['id']} {case['name']} {args.configuration} run-{n}]"
    outcome = {"run": str(run), "scenario": case["name"], "status": None, "summary": None, "timing": None}
    state = run_state(run)
    if state in ("executed", "graded"):
        meta = json.loads((run / "run.json").read_text())
        ex_status = meta.get("executor", {}).get("status")
        if ex_status in ("execution_failed", "isolation_failed") and args.retry_failed:
            aside = move_aside(run)
            log(f"{tag} previous {ex_status} run moved to {aside.name}; re-running")
            state = "absent"
        elif state == "executed":
            outcome["status"] = ex_status
            log(f"{tag} executed earlier without grading.json (executor status {ex_status}); grading only")
        else:
            log(f"{tag} already complete (executor {ex_status}); skipping execution")
    if state == "absent":
        prepare(args.eval_root, case, run, args.skill, args.engine)
        log(f"{tag} prepared")
        state = "prepared"
    if state == "prepared":
        log(f"{tag} running {args.engine} (model {args.model or 'engine default'}, timeout {args.timeout_seconds}s)")
        status, failure, timing = execute(run, args)
        outcome["timing"] = timing
        log(f"{tag} executor {status}" + (f": {failure}" if failure else "") +
            f" tokens={timing['total_tokens']} wall={timing['total_duration_seconds']}s turns={timing.get('num_turns')}")
        if status == "isolation_failed":
            outcome["status"] = "isolation_failed"
            return outcome
        outcome["status"] = status
        state = "executed"
    previous_metadata = json.loads((run / "run.json").read_text())
    previous_audit = previous_metadata.get("control_read_attempts")
    audit = record_control_access(run)
    new_control_evidence = bool(previous_metadata.get("control_audit_pending_review") or
                                (audit and audit["peeked_control"] and previous_audit != audit["control_read_attempts"]))
    if new_control_evidence:
        claude_exec.update_json(run / "run.json", lambda data: data.update(control_audit_pending_review=True))
    if audit is not None:
        outcome.update(audit)
    if state == "executed" or (state == "graded" and (run / "grading.json").is_file()):
        if state == "executed" or grading_status(run) is None:
            summary, err = grade_auto(args.eval_root, run)
            if err:
                log(f"{tag} grade.py failed: {err}")
                outcome["status"] = outcome["status"] or "execution_failed"
                outcome["grading"] = "not_graded: " + err
                return outcome
            log(f"{tag} auto grading: {json.dumps(summary)}")
            outcome["summary"] = summary
    if outcome["timing"] is None and (run / "timing.json").is_file():
        outcome["timing"] = json.loads((run / "timing.json").read_text())
    if outcome["status"] is None:
        outcome["status"] = json.loads((run / "run.json").read_text()).get("executor", {}).get("status", "ok")
    if args.grade_llm:
        if grading_status(run) == "complete" and not new_control_evidence:
            outcome["grading"] = "complete"
            outcome["summary"] = json.loads((run / "grading.json").read_text())["summary"]
            log(f"{tag} llm grading already complete")
        else:
            log(f"{tag} llm grading with {args.grader_model}")
            result = grade_llm.grade_run(run, args.eval_root, args.grader_model, args.grader_md,
                                         args.grader_max_turns, args.grader_timeout_seconds,
                                         log=lambda m: log(f"{tag} {m}"))
            outcome["grading"] = result["status"]
            if result["status"] == "complete":
                claude_exec.update_json(run / "run.json", lambda data: data.update(control_audit_pending_review=False))
                outcome["summary"] = result["summary"]
                log(f"{tag} complete: {json.dumps(result['summary'])}")
            else:
                log(f"{tag} grading_failed: {result.get('error')}")
    else:
        outcome["grading"] = grading_status(run)
        if outcome["summary"] is None and (run / "grading.json").is_file():
            outcome["summary"] = json.loads((run / "grading.json").read_text())["summary"]
    return outcome


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--skill", required=True, type=Path, help="candidate skills/hive directory (contains SKILL.md)")
    p.add_argument("--configuration", required=True, choices=("with_skill", "without_skill"))
    p.add_argument("--iteration", required=True, type=Path, help="output root; runs land in <iteration>/eval-N/<configuration>/run-n")
    p.add_argument("--split", choices=("public", "held-out", "all"), default="public")
    p.add_argument("--scenarios", help="comma-separated scenario names to restrict to")
    p.add_argument("--repetitions", type=int, default=3)
    p.add_argument("--concurrency", type=int, default=2)
    p.add_argument("--engine", choices=ENGINES, default="claude", help="headless engine under test (grader is always claude)")
    p.add_argument("--model", default=None, help="executor model (claude: alias or id, default opus; codex: -m value, default the engine's own)")
    p.add_argument("--reasoning-effort", default=None, help="codex only: model_reasoning_effort for the run's config.toml")
    p.add_argument("--max-turns", type=int, default=60)
    p.add_argument("--timeout-seconds", type=int, default=600)
    p.add_argument("--eval-root", type=Path, default=DEFAULT_EVAL_ROOT)
    p.add_argument("--tools", default=claude_exec.DEFAULT_TOOLS, help="built-in tool whitelist handed to claude --tools")
    p.add_argument("--permission-mode", default="acceptEdits", choices=("acceptEdits", "bypassPermissions"))
    p.add_argument("--shell", default="/bin/bash", help="$SHELL for the executor; its rc files must not call hive (zsh's does)")
    p.add_argument("--grade-llm", action="store_true", help="also run grade_llm.py and require complete grading")
    p.add_argument("--grader-model", default="opus")
    p.add_argument("--grader-md", type=Path, default=grade_llm.DEFAULT_GRADER_MD)
    p.add_argument("--grader-max-turns", type=int, default=40)
    p.add_argument("--grader-timeout-seconds", type=int, default=900)
    p.add_argument("--retry-failed", action="store_true",
                   help="move an execution_failed/isolation_failed run aside (kept as run-n.failed-<stamp>) and run it again")
    p.add_argument("--force-engines", action="store_true",
                   help="also run cases whose evals.json `engines` excludes this engine (a proxy experiment; recorded in iteration.json)")
    args = p.parse_args()
    args.skill = args.skill.resolve()
    args.iteration = args.iteration.resolve()
    args.eval_root = args.eval_root.resolve()
    if not (args.skill / "SKILL.md").is_file():
        p.error(f"{args.skill} has no SKILL.md")
    if args.repetitions < 1 or args.concurrency < 1:
        p.error("repetitions and concurrency must be positive")
    if args.model is None:
        args.model = DEFAULT_MODEL[args.engine]
    if shutil.which(args.engine) is None:
        p.error(f"{args.engine} not on PATH")
    if args.grade_llm and shutil.which("claude") is None:
        p.error("claude not on PATH (needed for --grade-llm)")
    cases, skipped = select_cases(args.eval_root, args.split, args.scenarios, args.engine, args.force_engines)
    args.iteration.mkdir(parents=True, exist_ok=True)
    write_iteration(args.iteration, args.engine, [] if args.force_engines else skipped, skipped if args.force_engines else [])
    for c in skipped:
        verb = "forced (proxy experiment)" if args.force_engines else "skipped"
        log(f"[eval-{c['id']} {c['name']}] {verb}: engines={case_engines(c)} does not include {args.engine}")
    if not cases:
        p.error("no scenarios selected" + (f" ({len(skipped)} skipped for engine {args.engine}; see iteration.json)" if skipped else ""))
    jobs = [(case, n) for case in cases for n in range(1, args.repetitions + 1)]
    log(f"{len(jobs)} run(s): {len(cases)} scenario(s) x {args.repetitions}; configuration={args.configuration}; "
        f"engine={args.engine}; model={args.model or 'engine default'}; concurrency={args.concurrency}; "
        f"washed env: {claude_exec.washed_names()}")
    outcomes = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.concurrency) as pool:
        futures = {pool.submit(process, job, args): job for job in jobs}
        for fut in concurrent.futures.as_completed(futures):
            case, n = futures[fut]
            try:
                outcomes.append(fut.result())
            except Exception as exc:  # keep the other runs going; report at the end
                log(f"[eval-{case['id']} {case['name']} run-{n}] runner error: {exc!r}")
                outcomes.append({"run": str(args.iteration / f"eval-{case['id']}" / args.configuration / f"run-{n}"),
                                 "scenario": case["name"], "status": "runner_error", "error": repr(exc)})
    outcomes.sort(key=lambda o: o["run"])
    ok = True
    log("\nsummary:")
    for o in outcomes:
        t = o.get("timing") or {}
        s = o.get("summary") or {}
        line = (f"  {o['scenario']:<24} {Path(o['run']).name:<6} executor={o['status']:<17} grading={o.get('grading')} "
                f"pass={s.get('passed')}/{s.get('total')} pending={s.get('pending')} peeked_control={o.get('peeked_control')} "
                f"tokens={t.get('total_tokens')} wall={t.get('total_duration_seconds')}s turns={t.get('num_turns')}")
        log(line)
        complete = o["status"] == "ok" and o.get("grading") in ("complete", "pending_review")
        if args.grade_llm and o.get("grading") != "complete":
            complete = False
        ok = ok and complete
    (args.iteration / f"runner-{args.configuration}-{datetime.datetime.now().strftime('%Y%m%d-%H%M%S')}.json").write_text(
        json.dumps({"args": {k: str(v) for k, v in vars(args).items()}, "outcomes": outcomes,
                    "summary": {"peeked_control_runs": [o["run"] for o in outcomes if o.get("peeked_control")],
                                "control_audited_runs": sum(o.get("peeked_control") is not None for o in outcomes)}}, ensure_ascii=False, indent=2) + "\n")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
