#!/usr/bin/env python3
"""Headless Claude Code (`claude -p`) plumbing shared by run_claude.py and grade_llm.py.

Everything here is about launching one isolated `claude -p` process, streaming
its stream-json output to disk, gating it on the init event, and turning the
event list into transcript/timing material. Nothing here knows about hive
scenarios or grading rules. Environment washing, the streamed subprocess and
the markdown/JSON helpers live in exec_common.py and are re-exported here.
"""
import json
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from exec_common import (  # noqa: E402,F401  (re-exported for callers)
    WASHED_PREFIXES, StreamedProcess, control_dir, env_exports, env_from_script, fence as _fence, probe_hive_path,
    read_events, run_file, shell_isolation, update_json, washed_env, washed_names,
)

HERE = Path(__file__).resolve().parent
DEFAULT_TOOLS = "Bash,Read,Write,Edit,Glob,Grep"

# The one MCP server an executor may carry: mcp_sendmessage.py, exposing a
# recording `SendMessage` (see that file). Claude Code names its tool
# `mcp__<server>__SendMessage`; the server name must not contain "hive" (the
# init gate greps the tool list for that word).
MCP_SERVER = "host"
MCP_TOOL = "SendMessage"
MCP_SCRIPT = HERE / "mcp_sendmessage.py"

# Flags that empty the user's plugins/skills/MCP for this process. Verified on
# claude 2.1.263: `--setting-sources ""` alone drops plugins but keeps the
# claude.ai MCP server and the bundled skills; `--strict-mcp-config` with a
# table naming only our servers drops every other MCP; `--disable-slash-commands`
# drops skills/slash commands. `--safe-mode` and `--bare` still list installed
# plugins (and --bare cannot use OAuth); neither is used.
ISOLATION_FLAGS = [
    "--setting-sources", "",
    "--disable-slash-commands",
    "--no-session-persistence",
]


def mcp_tool_name(server=MCP_SERVER):
    return f"mcp__{server}__{MCP_TOOL}"


def mcp_server_env(run, env=None):
    """What the stub needs to find its log: HIVE_EVAL_RUN, and the v4
    HIVE_EVAL_HOST_LOG when the executor environment (env.sh) names one.
    Passed explicitly rather than inherited: codex hands an MCP server a
    trimmed environment, and claude's inheritance is not relied on either."""
    out = {"HIVE_EVAL_RUN": str(run)}
    host_log = (env or {}).get("HIVE_EVAL_HOST_LOG")
    if host_log:
        out["HIVE_EVAL_HOST_LOG"] = host_log
    return out


def mcp_config(run, env=None):
    """`--mcp-config` table registering the SendMessage stub for one run."""
    return {"mcpServers": {MCP_SERVER: {"command": sys.executable, "args": [str(MCP_SCRIPT)],
                                        "env": mcp_server_env(run, env)}}}


def mcp_flags(mcp):
    return ["--strict-mcp-config", "--mcp-config", json.dumps(mcp or {"mcpServers": {}}, ensure_ascii=False)]


def permission_flags(mode, tools, mcp_servers=()):
    """Flags that let every whitelisted tool (and the MCP stub) run unattended.

    acceptEdits + `--permission-prompts none` + allow rules for each tool
    never prompts and, unlike bypassPermissions, injects no "prefer Bash"
    guidance into the model's system prompt. bypassPermissions is kept as an
    explicit option.
    """
    if mode == "bypassPermissions":
        return ["--permission-mode", "bypassPermissions"]
    allowed = ",".join([tools] + [mcp_tool_name(s) for s in mcp_servers])
    return ["--permission-mode", mode, "--permission-prompts", "none", "--allowedTools", allowed]


def build_command(model, tools, max_turns, permission_mode="acceptEdits", add_dirs=(), extra=(), mcp=None):
    """`--tools` restricts the built-in set only: an MCP server's tools are
    added on top of it (verified on 2.1.263), so the MCP table is what decides
    whether the model has SendMessage."""
    servers = tuple((mcp or {}).get("mcpServers", {}))
    cmd = ["claude", "-p", "--output-format", "stream-json", "--verbose", "--model", model,
           "--max-turns", str(max_turns), *ISOLATION_FLAGS, *mcp_flags(mcp),
           *permission_flags(permission_mode, tools, servers), "--tools", tools]
    for d in add_dirs:
        cmd += ["--add-dir", str(d)]
    return cmd + list(extra)


def check_isolation(init, allowed_tools, mcp_servers=()):
    """Hard gate on the stream-json init event: nothing hive-related, nothing
    beyond the whitelisted built-in tools plus the SendMessage stub of each
    expected MCP server, exactly those servers (connected) and no other, no
    plugins, no skills.

    Returns (ok, report). The report is written next to the run either way.
    """
    allowed = {t.strip() for t in allowed_tools.split(",") if t.strip()}
    expected_tools = {mcp_tool_name(s) for s in mcp_servers}
    tools = list(init.get("tools") or [])
    report = {
        "claude_code_version": init.get("claude_code_version"),
        "model": init.get("model"),
        "permission_mode": init.get("permissionMode"),
        "tools": tools,
        "mcp_servers": init.get("mcp_servers"),
        "mcp_servers_expected": sorted(mcp_servers),
        "plugins": init.get("plugins"),
        "skills": init.get("skills"),
        "slash_commands": init.get("slash_commands"),
        "agents": init.get("agents"),
        "problems": [],
    }
    extra = sorted(set(tools) - allowed - expected_tools)
    if extra:
        report["problems"].append(f"tools outside whitelist: {extra}")
    missing = sorted(expected_tools - set(tools))
    if missing:
        report["problems"].append(f"expected MCP tools absent: {missing}")
    servers = init.get("mcp_servers") or []
    names = sorted(s.get("name") for s in servers if isinstance(s, dict))
    if names != sorted(mcp_servers):
        report["problems"].append(f"mcp_servers {names} != expected {sorted(mcp_servers)}")
    for s in servers:
        if isinstance(s, dict) and s.get("name") in mcp_servers and s.get("status") != "connected":
            report["problems"].append(f"mcp server {s.get('name')} status {s.get('status')!r}, expected connected")
    for key in ("plugins", "skills", "slash_commands"):
        value = init.get(key)
        if value:
            report["problems"].append(f"{key} not empty: {json.dumps(value, ensure_ascii=False)[:500]}")
    blob = json.dumps({k: init.get(k) for k in ("tools", "mcp_servers", "plugins", "skills", "slash_commands", "agents")},
                      ensure_ascii=False).lower()
    if "hive" in blob:
        report["problems"].append("the word 'hive' appears in the init tool/plugin/skill/agent lists")
    report["ok"] = not report["problems"]
    return report["ok"], report


class ClaudeRun:
    """One `claude -p` process: prompt on stdin, events streamed to raw_path,
    killed on the init event when the isolation gate fails."""

    def __init__(self, cmd, prompt, cwd, env, raw_path, stderr_path, timeout, allowed_tools,
                 on_init=None, mcp_servers=()):
        self.cmd, self.prompt, self.cwd, self.env = cmd, prompt, Path(cwd), env
        self.raw_path, self.stderr_path = Path(raw_path), Path(stderr_path)
        self.timeout, self.allowed_tools, self.on_init = timeout, allowed_tools, on_init
        self.mcp_servers = tuple(mcp_servers)
        self.init = None
        self.isolation = None
        self.proc = None

    def _gate(self, ev):
        if self.init is None and ev.get("type") == "system" and ev.get("subtype") == "init":
            self.init = ev
            ok, self.isolation = check_isolation(ev, self.allowed_tools, self.mcp_servers)
            if self.on_init:
                self.on_init(self.isolation)
            return not ok
        return False

    def run(self):
        self.proc = StreamedProcess(self.cmd, self.prompt, self.cwd, self.env, self.raw_path, self.stderr_path,
                                    self.timeout, on_event=self._gate).run()
        return self

    @property
    def events(self):
        return self.proc.events if self.proc else []

    @property
    def killed_for_isolation(self):
        return bool(self.proc and self.proc.stopped_early)

    @property
    def timed_out(self):
        return bool(self.proc and self.proc.timed_out)

    @property
    def exit_code(self):
        return self.proc.exit_code if self.proc else None

    @property
    def result(self):
        return next((e for e in self.events if e.get("type") == "result"), None)

    def assistant_texts(self):
        """Text of every assistant message that carries a text block, in order."""
        out = []
        for ev in self.events:
            if ev.get("type") != "assistant":
                continue
            texts = [b.get("text", "") for b in ev["message"].get("content", []) if b.get("type") == "text"]
            if any(t.strip() for t in texts):
                out.append("\n".join(texts))
        return out

    def last_assistant_text(self):
        texts = self.assistant_texts()
        return texts[-1] if texts else None

    def usage_summary(self):
        """Token totals: the result event's usage, else assistant messages
        (deduplicated by API message id) for a run that died before result."""
        res = self.result
        if res and isinstance(res.get("usage"), dict):
            u = res["usage"]
            source = "result_event"
        else:
            seen, u = set(), {"input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}
            for ev in self.events:
                if ev.get("type") != "assistant":
                    continue
                m = ev["message"]
                mid = m.get("id")
                if mid in seen or not isinstance(m.get("usage"), dict):
                    continue
                seen.add(mid)
                for k in u:
                    u[k] += int(m["usage"].get(k) or 0)
            source = "assistant_messages_partial"
        parts = {k: int(u.get(k) or 0) for k in ("input_tokens", "output_tokens", "cache_creation_input_tokens", "cache_read_input_tokens")}
        return {"total_tokens": sum(parts.values()), "tokens": parts, "tokens_source": source}

    def timing(self):
        res = self.result or {}
        init = self.init or {}
        summary = self.usage_summary()
        stamps = self.proc.stamps() if self.proc else {"started_at": None, "finished_at": None, "total_duration_seconds": -1}
        models = sorted((res.get("modelUsage") or {}).keys())
        return {
            **summary,
            "duration_ms": res.get("duration_ms"),
            "duration_api_ms": res.get("duration_api_ms"),
            "total_duration_seconds": stamps["total_duration_seconds"],
            "num_turns": res.get("num_turns"),
            "cost_usd": res.get("total_cost_usd"),
            "executor_model": init.get("model"),
            "models_billed": models,
            "claude_version": init.get("claude_code_version"),
            "session_id": res.get("session_id") or init.get("session_id"),
            "result_subtype": res.get("subtype"),
            "started_at": stamps["started_at"],
            "finished_at": stamps["finished_at"],
        }

    def failure_reason(self):
        """None when the process ended with a successful result event."""
        if self.killed_for_isolation:
            return "isolation_failed: " + "; ".join(self.isolation["problems"])
        if self.timed_out:
            return f"timeout after {self.timeout}s"
        res = self.result
        if res is None:
            return f"no result event (exit code {self.exit_code})"
        if res.get("subtype") != "success" or res.get("is_error"):
            return f"result {res.get('subtype')}: {str(res.get('result', ''))[:300]}"
        if self.exit_code not in (0, None):
            return f"exit code {self.exit_code}"
        return None


def _result_text(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = []
        for block in content:
            if isinstance(block, dict) and block.get("type") == "text":
                parts.append(block.get("text", ""))
            elif isinstance(block, dict):
                parts.append(f"[{block.get('type')} block]")
            else:
                parts.append(str(block))
        return "\n".join(parts)
    return json.dumps(content, ensure_ascii=False)


def render_transcript(events, header_lines=()):
    """Ordered, untruncated markdown: assistant text, every tool_use with its
    full input, every tool_result with its full content, then the result event."""
    lines = ["# Executor transcript", ""]
    lines += list(header_lines)
    lines.append("")
    step = 0
    for ev in events:
        t = ev.get("type")
        if t == "assistant":
            for block in ev["message"].get("content", []):
                kind = block.get("type")
                if kind == "text":
                    if not block.get("text", "").strip():
                        continue
                    step += 1
                    lines += [f"## [{step}] assistant", "", block["text"], ""]
                elif kind == "thinking":
                    if not block.get("thinking", "").strip():
                        continue
                    step += 1
                    lines += [f"## [{step}] assistant thinking", "", block["thinking"], ""]
                elif kind == "tool_use":
                    step += 1
                    lines += [f"## [{step}] tool_use {block.get('name')} id={block.get('id')}", "",
                              _fence(json.dumps(block.get("input", {}), ensure_ascii=False, indent=2), "json"), ""]
        elif t == "user":
            content = ev["message"].get("content")
            if isinstance(content, str):
                step += 1
                lines += [f"## [{step}] user", "", content, ""]
                continue
            for block in content or []:
                kind = block.get("type")
                if kind == "tool_result":
                    step += 1
                    flag = " is_error=true" if block.get("is_error") else ""
                    lines += [f"## [{step}] tool_result id={block.get('tool_use_id')}{flag}", "",
                              _fence(_result_text(block.get("content")))]
                    tur = ev.get("tool_use_result")
                    if isinstance(tur, dict) and tur.get("stderr"):
                        lines += ["", "stderr:", _fence(tur["stderr"])]
                    if isinstance(tur, dict) and tur.get("interrupted"):
                        lines += ["", "(tool call interrupted)"]
                    lines.append("")
                elif kind == "text":
                    step += 1
                    lines += [f"## [{step}] user", "", block.get("text", ""), ""]
        elif t == "result":
            lines += ["## result", "",
                      f"subtype={ev.get('subtype')} is_error={ev.get('is_error')} num_turns={ev.get('num_turns')} "
                      f"duration_ms={ev.get('duration_ms')} stop_reason={ev.get('stop_reason')}", ""]
    return "\n".join(lines).rstrip("\n") + "\n"
