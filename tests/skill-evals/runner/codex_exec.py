#!/usr/bin/env python3
"""Headless Codex (`codex exec --json`) plumbing for run_claude.py --engine codex.

One isolated codex process per run: a throwaway CODEX_HOME under the run that
holds only a copy of auth.json and a config.toml written here (no user
config, memories, plugins; the SendMessage stub as the only MCP server; every
discovered skill disabled), the
prompt on stdin, the JSONL event stream to raw.jsonl, the engine's own rollout
file copied next to it, and the rollout rendered into the same transcript /
timing shape the claude executor produces. Nothing here knows about hive
scenarios or grading rules.

Isolation is gated twice: before the model runs, `codex debug prompt-input`
renders the exact developer/user preamble the model would see under the same
CODEX_HOME, cwd and environment; after the run the rollout's own preamble is
checked the same way.
"""
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from exec_common import (  # noqa: E402,F401  (probe_hive_path re-exported for run_claude.py)
    StreamedProcess, control_dir, env_from_script, fence, probe_hive_path, shell_isolation, washed_env,
)
from claude_exec import MCP_SCRIPT, MCP_SERVER, MCP_TOOL, mcp_server_env  # noqa: E402

# Skill roots codex 0.153.4 scans on top of $CODEX_HOME/skills: the shared
# ~/.agents/skills tree (HOME-based, so a fresh CODEX_HOME does not drop it)
# and the bundled system skills it materializes into $CODEX_HOME/skills/.system
# at every start. The disable list is not built from these paths but from what
# `codex debug prompt-input` actually lists; they are only documentation.
KNOWN_SKILL_ROOTS = ("~/.agents/skills", "$CODEX_HOME/skills/.system")

# Features switched off in the per-run config.toml. `plugins`/`remote_plugin`/
# `recommended_plugins` drop the marketplace plugins and the "available but not
# installed" catalogue, `memories` the ~/.codex/memories prompt, `shell_snapshot`
# the login-shell snapshot that would run the user's rc files under the stub
# PATH. `multi_agent` has no effect on codex 0.153.4 (the <multi_agent_role>
# developer message and its spawn_agent/send_message tools stay); it is left
# at its default and reported in isolation.json as a native section.
DISABLED_FEATURES = ("plugins", "remote_plugin", "recommended_plugins", "memories", "shell_snapshot")

PREAMBLE_FORBIDDEN = {
    "recommended_plugins": "<recommended_plugins>",
    "memory": "## Memory",
    "agents_md": "AGENTS.md instructions",
    "mcp_instructions": "<mcp_instructions>",
    "apps": "<apps_instructions>",
}


def codex_version():
    try:
        out = subprocess.run(["codex", "--version"], capture_output=True, text=True, check=True).stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        return None
    return out.split()[-1] if out else None


def executor_env(run, shell="/bin/bash"):
    """Washed environment + run/env.sh + shell/tmux isolation (ZDOTDIR,
    TMUX_TMPDIR) + CODEX_HOME pointed at the run's throwaway home. codex runs
    every command as `/bin/zsh -lc` (the passwd shell), which is why ZDOTDIR
    matters here: without it that login zsh reads ~/.zshenv."""
    env = shell_isolation(run, env_from_script(run / "env.sh", washed_env()))
    env["SHELL"] = shell
    env["CODEX_HOME"] = str(codex_home(run))
    env.setdefault("TERM", "dumb")
    env["NO_COLOR"] = "1"
    return env


def codex_home(run):
    return Path(run) / "codex-home"


def source_auth():
    return Path(os.environ.get("HIVE_EVAL_CODEX_AUTH", str(Path.home() / ".codex" / "auth.json")))


def toml_str(s):
    return json.dumps(str(s), ensure_ascii=False)


def mcp_config(run, env=None):
    """`[mcp_servers.*]` entries for the run: the SendMessage stub only.
    `default_tools_approval_mode = "approve"` is required: without it codex
    0.153.4 answers the call with "MCP tool call requires approval, but
    approval policy is never" and records nothing. The tool reaches the model
    as `mcp__host__SendMessage`, the same name Claude Code gives it."""
    return {MCP_SERVER: {"command": sys.executable, "args": [str(MCP_SCRIPT)],
                         "default_tools_approval_mode": "approve", "env": mcp_server_env(run, env)}}


def write_config(home, model=None, disabled_skills=(), reasoning_effort=None, mcp=None):
    lines = [
        "# written by tests/skill-evals/runner/codex_exec.py for one eval run; not the user's config",
        'approval_policy = "never"',
        'sandbox_mode = "workspace-write"',
        'web_search = "disabled"',
        "suppress_unstable_features_warning = true",
    ]
    if model:
        lines.append(f"model = {toml_str(model)}")
    if reasoning_effort:
        lines.append(f"model_reasoning_effort = {toml_str(reasoning_effort)}")
    lines += ["", "[features]"] + [f"{name} = false" for name in DISABLED_FEATURES]
    for name, server in (mcp or {}).items():
        lines += ["", f"[mcp_servers.{name}]", f"command = {toml_str(server['command'])}",
                  "args = [" + ", ".join(toml_str(a) for a in server.get("args", ())) + "]"]
        if server.get("default_tools_approval_mode"):
            lines.append(f"default_tools_approval_mode = {toml_str(server['default_tools_approval_mode'])}")
        if server.get("env"):
            lines += [f"[mcp_servers.{name}.env]"] + [f"{k} = {toml_str(v)}" for k, v in server["env"].items()]
    for path in disabled_skills:
        lines += ["", "[[skills.config]]", f"path = {toml_str(path)}", "enabled = false"]
    (home / "config.toml").write_text("\n".join(lines) + "\n")


def make_home(run, model=None, reasoning_effort=None, disabled_skills=(), mcp=None):
    """A CODEX_HOME holding auth.json and our config.toml, nothing of the user's."""
    home = codex_home(run)
    home.mkdir(parents=True, exist_ok=True)
    auth = source_auth()
    if not auth.is_file():
        raise RuntimeError(f"codex auth file not found: {auth} (set HIVE_EVAL_CODEX_AUTH)")
    shutil.copy(auth, home / "auth.json")
    os.chmod(home / "auth.json", 0o600)
    write_config(home, model, disabled_skills, reasoning_effort, mcp)
    return home


def scrub_home(run):
    """Drop the credential copy and the engine's caches once the run is over;
    keep config.toml (what the model ran under) and sessions/ (the rollout)."""
    home = codex_home(run)
    if not home.is_dir():
        return
    (home / "auth.json").unlink(missing_ok=True)
    for child in home.iterdir():
        if child.name in ("config.toml", "sessions"):
            continue
        if child.is_dir() and not child.is_symlink():
            shutil.rmtree(child, ignore_errors=True)
        else:
            child.unlink(missing_ok=True)


def message_text(item):
    content = item.get("content")
    if isinstance(content, str):
        return content
    parts = []
    for block in content or []:
        if isinstance(block, dict) and isinstance(block.get("text"), str):
            parts.append(block["text"])
    return "".join(parts)


def prompt_input(run, env, prompt="hello"):
    """The developer/user items `codex exec` would send before the prompt, under
    this run's CODEX_HOME/cwd/env. Returns (items, error)."""
    proc = subprocess.run(["codex", "debug", "prompt-input", prompt], cwd=str(run / "shared"), env=env,
                          capture_output=True, text=True)
    if proc.returncode != 0:
        return None, f"codex debug prompt-input exited {proc.returncode}: {proc.stderr.strip()[-500:]}"
    try:
        items = json.loads(proc.stdout)
    except ValueError:
        return None, "codex debug prompt-input did not print JSON: " + proc.stdout[:300]
    if not isinstance(items, list):
        return None, "codex debug prompt-input printed a non-list"
    return items, None


def listed_skills(items):
    """(root_alias -> root_path, [absolute SKILL.md paths]) from a <skills_instructions> block."""
    roots, files = {}, []
    for item in items:
        text = message_text(item)
        if "<skills_instructions>" not in text:
            continue
        for alias, path in re.findall(r"^- `(r\d+)` = `([^`]+)`$", text, flags=re.M):
            roots[alias] = path
        for alias, rel in re.findall(r"\(file: (r\d+)/([^)]+)\)", text):
            if alias in roots:
                files.append(str(Path(roots[alias]) / rel))
    return roots, files


def check_preamble(items, run, label, control=None):
    """Isolation report over the model-visible preamble (everything before the
    executor prompt): no skills, no plugin catalogue, no memory, no AGENTS.md,
    no MCP, and the word hive nowhere once the run's own path (and the control
    directory's, which the sandbox lists as a writable root) is masked."""
    control = str(control) if control else None
    run = str(run)
    report = {"source": label, "items": [], "skills": [], "native_sections": [], "problems": []}
    blob = []
    for item in items:
        text = message_text(item)
        tags = re.findall(r"<([a-z_]+)>", text[:120])
        report["items"].append({"type": item.get("type"), "role": item.get("role"), "chars": len(text),
                                "section": tags[0] if tags else None})
        if tags and tags[0] in ("multi_agent_role", "multi_agent_mode", "environment_context", "collaboration_mode"):
            report["native_sections"].append(tags[0])
        blob.append(text)
    _, skills = listed_skills(items)
    report["skills"] = skills
    if skills:
        report["problems"].append(f"{len(skills)} skill(s) listed in <skills_instructions>: {skills[:5]}")
    joined = "\n".join(blob)
    for key, needle in PREAMBLE_FORBIDDEN.items():
        if needle in joined:
            report["problems"].append(f"{key}: preamble contains {needle!r}")
    masked = joined
    if control:
        masked = masked.replace(control, "<CONTROL>").replace(os.path.realpath(control), "<CONTROL>")
    masked = masked.replace(run, "<RUN>").replace(os.path.realpath(run), "<RUN>").lower()
    if "hive" in masked:
        idx = masked.index("hive")
        report["problems"].append("the word 'hive' appears in the preamble: ..." + masked[max(0, idx - 80):idx + 80].replace("\n", " "))
    report["ok"] = not report["problems"]
    return report


def list_mcp_servers(env):
    proc = subprocess.run(["codex", "mcp", "list", "--json"], env=env, capture_output=True, text=True)
    if proc.returncode != 0:
        return None, f"codex mcp list exited {proc.returncode}: {proc.stderr.strip()[-300:]}"
    try:
        return json.loads(proc.stdout), None
    except ValueError:
        return None, "codex mcp list printed no JSON: " + proc.stdout[:200]


def preflight(run, env, log=None, mcp_servers=()):
    """Two prompt-input passes: the first discovers every skill codex would
    list (and materializes its bundled ones), the config disables them all,
    the second must list none; `codex mcp list` must name exactly the
    expected servers, enabled. Returns the isolation report (ok/problems)."""
    home = codex_home(run)
    items, err = prompt_input(run, env)
    if err:
        return {"ok": False, "problems": [err], "source": "preflight"}
    roots, skills = listed_skills(items)
    if skills:
        cfg = (home / "config.toml").read_text()
        extra = "".join(f'\n[[skills.config]]\npath = {toml_str(p)}\nenabled = false\n' for p in skills)
        (home / "config.toml").write_text(cfg + extra)
        if log:
            log(f"  disabled {len(skills)} skill(s) from roots {sorted(roots.values())}")
        items, err = prompt_input(run, env)
        if err:
            return {"ok": False, "problems": [err], "source": "preflight"}
    report = check_preamble(items, run, "preflight", control=control_dir(run, env))
    report["skill_roots_seen"] = roots
    report["skills_disabled"] = skills
    servers, err = list_mcp_servers(env)
    report["mcp_servers"] = servers
    report["mcp_servers_expected"] = sorted(mcp_servers)
    if err:
        report["problems"].append(err)
    else:
        names = sorted(s.get("name") for s in servers if isinstance(s, dict))
        if names != sorted(mcp_servers):
            report["problems"].append(f"mcp servers {names} != expected {sorted(mcp_servers)}")
        for s in servers:
            if isinstance(s, dict) and s.get("name") in mcp_servers and not s.get("enabled", True):
                report["problems"].append(f"mcp server {s.get('name')} disabled: {s.get('disabled_reason')}")
    report["ok"] = not report["problems"]
    return report


def build_command(run, model=None, extra=(), add_dirs=()):
    """`add_dirs` beyond the run root: the v4 control directory has to be one.
    Under `workspace-write` the seatbelt denies a shell command every write
    outside cwd, the added directories, /tmp and $TMPDIR (probed on 0.153.4:
    a sibling `.run-N.control/` is "Operation not permitted", through a
    symlink inside the run too), and the hive/tmux/host stubs the model runs
    are shell commands writing HIVE_EVAL_LOG / HIVE_EVAL_HOST_LOG there. The
    price is that codex lists every writable root in its
    `<environment_context>`, so the model sees the control path; the prompt's
    do-not-read list is what keeps it out, not the sandbox."""
    cmd = ["codex", "exec", "--json", "--color", "never", "--skip-git-repo-check",
           "-C", str(run / "shared"), "--add-dir", str(run)]
    for d in add_dirs:
        cmd += ["--add-dir", str(d)]
    cmd += ["--sandbox", "workspace-write", "-c", "approval_policy=\"never\"",
            "-o", str(run / "codex.last_message.md")]
    if model:
        cmd += ["-m", model]
    return cmd + list(extra) + ["-"]


class CodexRun:
    """One `codex exec --json` process plus its rollout file."""

    def __init__(self, cmd, prompt, run, env, timeout):
        self.cmd, self.prompt, self.run, self.env, self.timeout = cmd, prompt, Path(run), env, timeout
        self.proc = None
        self.rollout = []
        self.rollout_path = None

    def run_process(self):
        self.proc = StreamedProcess(self.cmd, self.prompt, self.run / "shared", self.env, self.run / "raw.jsonl",
                                    self.run / "codex.stderr.log", self.timeout).run()
        self._collect_rollout()
        return self

    @property
    def events(self):
        return self.proc.events if self.proc else []

    @property
    def thread_id(self):
        ev = next((e for e in self.events if e.get("type") == "thread.started"), None)
        return ev.get("thread_id") if ev else None

    def _collect_rollout(self):
        sessions = codex_home(self.run) / "sessions"
        candidates = sorted(sessions.rglob("rollout-*.jsonl")) if sessions.is_dir() else []
        if self.thread_id:
            candidates = [p for p in candidates if self.thread_id in p.name] or candidates
        if not candidates:
            return
        src = candidates[-1]
        self.rollout_path = self.run / "rollout.jsonl"
        shutil.copy(src, self.rollout_path)
        for line in self.rollout_path.read_text(encoding="utf-8").splitlines():
            try:
                self.rollout.append(json.loads(line))
            except ValueError:
                continue

    # --- stream-derived facts -------------------------------------------------

    @property
    def turn_completed(self):
        return next((e for e in self.events if e.get("type") == "turn.completed"), None)

    @property
    def turn_failed(self):
        return next((e for e in self.events if e.get("type") in ("turn.failed", "error")), None)

    def agent_messages(self):
        out = []
        for ev in self.events:
            item = ev.get("item") or {}
            if ev.get("type") == "item.completed" and item.get("type") == "agent_message" and item.get("text", "").strip():
                out.append(item["text"])
        return out

    def last_assistant_text(self):
        for rec in self.rollout:
            p = rec.get("payload") or {}
            if rec.get("type") == "event_msg" and p.get("type") == "task_complete" and p.get("last_agent_message"):
                return p["last_agent_message"]
        texts = self.agent_messages()
        return texts[-1] if texts else None

    # --- rollout-derived facts ------------------------------------------------

    def session_meta(self):
        return next((r.get("payload") or {} for r in self.rollout if r.get("type") == "session_meta"), {})

    def turn_context(self):
        return next((r.get("payload") or {} for r in self.rollout if r.get("type") == "turn_context"), {})

    def task_complete(self):
        return next((r.get("payload") or {} for r in self.rollout
                     if r.get("type") == "event_msg" and (r.get("payload") or {}).get("type") == "task_complete"), {})

    def preamble_items(self):
        """Developer/user messages the engine sent before the executor prompt
        (the first user message whose text equals the prompt)."""
        items = []
        for rec in self.rollout:
            p = rec.get("payload") or {}
            if rec.get("type") != "response_item" or p.get("type") != "message":
                continue
            if p.get("role") == "user" and message_text(p).strip() == self.prompt.strip():
                break
            if p.get("role") in ("developer", "user", "system"):
                items.append(p)
        return items

    def usage_summary(self):
        tc = self.turn_completed
        if tc and isinstance(tc.get("usage"), dict):
            u, source = tc["usage"], "turn_completed_event"
        else:
            u, source = {}, "none"
            for rec in self.rollout:
                if rec.get("type") == "token_usage_record":
                    u = (rec.get("payload") or {}).get("thread_token_usage") or u
                    source = "rollout_thread_token_usage_partial"
        parts = {k: int(u.get(k) or 0) for k in ("input_tokens", "cached_input_tokens", "cache_write_input_tokens",
                                                  "output_tokens", "reasoning_output_tokens")}
        # codex counts cached and reasoning tokens inside input/output; the
        # total is input + output, matching its own token_count.total_tokens.
        return {"total_tokens": parts["input_tokens"] + parts["output_tokens"], "tokens": parts, "tokens_source": source}

    def timing(self):
        meta, ctx, done = self.session_meta(), self.turn_context(), self.task_complete()
        stamps = self.proc.stamps() if self.proc else {"started_at": None, "finished_at": None, "total_duration_seconds": -1}
        responses = sum(1 for r in self.rollout if r.get("type") == "token_usage_record")
        return {
            **self.usage_summary(),
            "duration_ms": done.get("duration_ms"),
            "time_to_first_token_ms": done.get("time_to_first_token_ms"),
            "total_duration_seconds": stamps["total_duration_seconds"],
            "num_turns": responses or None,
            "cost_usd": None,
            "executor_model": ctx.get("model"),
            "reasoning_effort": ctx.get("effort"),
            "sandbox_policy": ctx.get("sandbox_policy"),
            "approval_policy": ctx.get("approval_policy"),
            "codex_version": meta.get("cli_version") or codex_version(),
            "session_id": self.thread_id or meta.get("id"),
            "result_subtype": "turn.completed" if self.turn_completed else (self.turn_failed or {}).get("type"),
            "started_at": stamps["started_at"],
            "finished_at": stamps["finished_at"],
        }

    def failure_reason(self):
        if self.proc and self.proc.timed_out:
            return f"timeout after {self.timeout}s"
        if self.turn_failed:
            return f"{self.turn_failed.get('type')}: {json.dumps(self.turn_failed.get('error') or self.turn_failed.get('message') or self.turn_failed, ensure_ascii=False)[:300]}"
        if self.turn_completed is None:
            return f"no turn.completed event (exit code {self.proc.exit_code if self.proc else None})"
        if self.proc and self.proc.exit_code not in (0, None):
            return f"exit code {self.proc.exit_code}"
        return None


def _output_text(output):
    if isinstance(output, str):
        return output
    if isinstance(output, list):
        return "\n".join(b.get("text", "") if isinstance(b, dict) and "text" in b else json.dumps(b, ensure_ascii=False)
                         for b in output)
    return json.dumps(output, ensure_ascii=False)


def render_transcript(rollout, header_lines=()):
    """Ordered, untruncated markdown from the rollout: every developer/user/
    assistant message, every tool call with its full arguments, every tool
    output in full, the engine's own command-execution records, then the
    task_complete line."""
    lines = ["# Executor transcript", ""]
    lines += list(header_lines)
    lines.append("")
    step = 0
    for rec in rollout:
        t = rec.get("type")
        p = rec.get("payload") or {}
        pt = p.get("type")
        if t == "response_item":
            if pt == "message":
                text = message_text(p)
                if not text.strip():
                    continue
                step += 1
                phase = f" phase={p['phase']}" if p.get("phase") else ""
                lines += [f"## [{step}] {p.get('role')}{phase}", "", text, ""]
            elif pt == "reasoning":
                summary = "\n".join(s.get("text", "") for s in p.get("summary") or [] if isinstance(s, dict))
                if summary.strip():
                    step += 1
                    lines += [f"## [{step}] assistant reasoning summary", "", summary, ""]
            elif pt and pt.endswith("_call_output"):
                step += 1
                lines += [f"## [{step}] tool_result call_id={p.get('call_id')}", "", fence(_output_text(p.get("output"))), ""]
            elif pt and pt.endswith("_call"):
                step += 1
                args = p.get("arguments", p.get("input"))
                if isinstance(args, str):
                    try:
                        args = json.dumps(json.loads(args), ensure_ascii=False, indent=2)
                        lang = "json"
                    except ValueError:
                        lang = ""
                else:
                    args, lang = json.dumps(args, ensure_ascii=False, indent=2), "json"
                lines += [f"## [{step}] tool_use {p.get('name')} call_id={p.get('call_id')}", "", fence(args, lang), ""]
            else:
                step += 1
                lines += [f"## [{step}] {pt}", "", fence(json.dumps(p, ensure_ascii=False, indent=2), "json"), ""]
        elif t == "event_msg" and pt == "item_completed":
            item = p.get("item") or {}
            kind = item.get("type")
            if kind == "CommandExecution":
                step += 1
                cmd = item.get("command")
                cmd = " ".join(cmd) if isinstance(cmd, list) else str(cmd)
                lines += [f"## [{step}] command_execution exit_code={item.get('exit_code')} status={item.get('status')}", "",
                          fence(cmd, "bash"), ""]
                if item.get("aggregated_output"):
                    lines += ["output:", fence(item["aggregated_output"]), ""]
            elif kind in ("FileChange", "PatchApply", "McpToolCall", "WebSearch", "CollabAgentToolCall"):
                step += 1
                lines += [f"## [{step}] {kind}", "", fence(json.dumps(item, ensure_ascii=False, indent=2), "json"), ""]
        elif t == "event_msg" and pt == "task_complete":
            lines += ["## result", "",
                      f"task_complete duration_ms={p.get('duration_ms')} time_to_first_token_ms={p.get('time_to_first_token_ms')}", ""]
        elif t == "event_msg" and pt in ("error", "turn_aborted", "stream_error"):
            step += 1
            lines += [f"## [{step}] engine {pt}", "", fence(json.dumps(p, ensure_ascii=False, indent=2), "json"), ""]
    return "\n".join(lines).rstrip("\n") + "\n"
