# Headless executors for the hive skill evals

`../hive/` is the frozen standard (stub, scenarios, `prepare.py`, `grade.py`,
`check_benchmark.py`). This directory is the executor adapter it leaves to
the caller: one command takes a candidate `skills/hive` directory through
prepare → isolated headless engine → transcript/timing → `grade.py` → LLM
grading → `--require-complete`, for every scenario of a split whose
`engines` lists the engine. `--engine claude` (default) runs `claude -p`,
`--engine codex` runs `codex exec --json`; the LLM grader is `claude -p`
either way. Nothing under `../hive/` is imported or modified; its scripts run
as subprocesses. Files: `run_claude.py` (the runner, both engines),
`claude_exec.py` / `codex_exec.py` (one engine each), `exec_common.py` (env
washing, shell isolation and its probe, the streamed subprocess, fences),
`mcp_sendmessage.py` (the recording `SendMessage` MCP server both engines
carry), `grade_llm.py`.

## One candidate, one split

```bash
python3 tests/skill-evals/runner/run_claude.py \
  --skill /abs/path/to/candidate/skills/hive \
  --configuration with_skill \
  --iteration /abs/path/to/private-runs/candidate-a/public \
  --split public --repetitions 3 --concurrency 3 \
  --model opus --grade-llm
```

Then the frozen baseline into the same iteration as `--configuration
without_skill`, and `python3 tests/skill-evals/hive/check_benchmark.py
<iteration> --split public --repetitions 3`.

`evals.json` cases carry `engines` (absent = every engine). A case whose
list does not include `--engine` is skipped, logged as such, and recorded in
`<iteration>/iteration.json` (`{"engine": "claude", "skipped_cases": [...],
"forced_cases": [...], "case_engines": {...}}`); `--force-engines` runs them
anyway as a proxy experiment and lists them under `forced_cases` instead. An
iteration belongs to one engine: a second invocation with another `--engine`
into the same directory is refused. `check_benchmark.py` is the standard's;
whether it excuses the skipped cases is its business, `iteration.json` is
what it can read.

Per run (`<iteration>/eval-<id>/<configuration>/run-<n>`) the runner writes,
on top of what `prepare.py` creates: `raw.jsonl` (every stream-json event),
`transcript.md` (ordered assistant text, every `tool_use` with full input,
every `tool_result` in full), `timing.json` (`total_tokens` = input + output +
cache-creation + cache-read from the `result` event, `duration_ms`,
`total_duration_seconds` wall clock, `num_turns`, `cost_usd`, real
`executor_model` and `claude_version`), `isolation.json`, `claude.stderr.log`,
the `executor` block and `capabilities` (`{"SendMessage": true}`) in
`run.json`, then `grading.json`; with `--grade-llm`
also `decisions.json` and `grader/` (prompt, raw events, output, timing of the
grader process). `eval_metadata.json` lands in `eval-<id>/`.

Options: `--engine claude|codex` (claude); `--model` (claude: alias or id,
default `opus`; codex: the `-m` value, default the engine's own model);
`--scenarios a,b` restricts the split; `--force-engines` runs cases whose
`engines` excludes the engine (see above); `--timeout-seconds` (600) bounds the
executor; `--grader-model` (opus), `--grader-max-turns`,
`--grader-timeout-seconds`. Claude only: `--max-turns` (60), `--tools`, the
built-in tool whitelist (`Bash,Read,Write,Edit,Glob,Grep`), `--permission-mode`
`acceptEdits` (with `--permission-prompts none` and allow rules, so nothing
prompts and no "prefer Bash" guidance is injected) or `bypassPermissions`.
Codex only: `--reasoning-effort`. `prepare.py` gets `--engine <engine>` when
its `--help` lists that flag (it does since the eval-spec update that renders
the entry syntax per engine); before that the runner writes `engine` into
`run.json` itself, which `grade.py` reads for the spawn default CLI.

`grade_llm.py <run>` grades one run on its own; `run_claude.py --grade-llm`
calls the same function.

## Resume

Re-running the same command skips every run that already has
`final_message.md` + `grading.json` (and, with `--grade-llm`, only fills in
missing LLM decisions). A run with `raw.jsonl` but no grading is graded, not
re-executed. A run whose executor ended `execution_failed` (timeout, max
turns, non-zero exit, no result event) or `isolation_failed` keeps its
evidence and is skipped; `--retry-failed` moves it to
`run-<n>.failed-<stamp>` (together with the hidden `.run-<n>.control/`
fixture directory prepare.py keeps beside the run) and runs it again under
the same settings. Both engines resume the same way; the state is read from
`run.json`'s `executor.status`, not from engine-specific files.

## Isolation, and how to check it

The executor is `claude -p --output-format stream-json --verbose` with

- environment: every `CLAUDE*` (including `CLAUDECODE`, `CLAUDE_CODE_*`),
  `HIVE_*` and `TMUX*` variable removed, then `run/env.sh` sourced on top
  (stub `PATH`, `HIVE_EVAL_*`); `ANTHROPIC_*` is kept so auth works; then
  the shell isolation below;
- `--setting-sources ""` (no user/project/local settings, so no enabled
  plugins, no user CLAUDE.md), `--strict-mcp-config --mcp-config <table>`
  naming only the `host` server of `mcp_sendmessage.py` (so no other MCP,
  including the claude.ai account servers), `--disable-slash-commands` (no
  skills), `--tools <whitelist>` (no built-in SendMessage/ListAgents/Task/
  Workflow…; `--tools` does not touch MCP tools, so `mcp__host__SendMessage`
  is added on top), `--allowedTools <whitelist>,mcp__host__SendMessage`,
  `--no-session-persistence`, `--add-dir <run>`;
- `SHELL=/bin/bash` (`--shell`): Claude Code snapshots the login shell named
  by `$SHELL` before the first Bash call and sources that shell's own rc file
  inside the snapshot script (`~/.zshrc` for zsh, ignoring `ZDOTDIR`; probed
  on 2.1.263), and this machine's `~/.zshrc` runs `hive shell-init zsh` —
  under the run's PATH that lands in `hive-calls.jsonl` as the model's
  first, unsupported call and fails the `supported` expectation of every
  scenario. The bash rc files have no hive line. Any rc file of the chosen
  shell that calls `hive` poisons the log, and `--shell /bin/zsh` still does
  even with the isolation below.

### Shell isolation and the probe gate (both engines)

Every command an engine runs goes through a login shell (`/bin/zsh -lc` for
codex, the `$SHELL -l` snapshot for claude), and a login shell reads rc files
that may rewrite PATH. On this machine `~/.zshenv` sources `~/.cargo/env`,
which prepends `~/.cargo/bin` — the real `hive` — ahead of the stub whenever
the starting PATH lacks it; the review reproduced a codex run whose `hive`
calls went to the real binary with an empty stub log. Two measures:

- `exec_common.shell_isolation`: `ZDOTDIR=<run>/home` (an empty directory,
  so a login zsh reads no user rc at all; `/etc/zprofile`'s `path_helper`
  still runs but only moves system directories ahead of everything, never
  `~/.cargo/bin`), `TMUX_TMPDIR=<run>/tmux-tmp` (created; a tmux pointed at
  a directory that does not exist falls back to the default socket
  silently, so the directory must exist), `TMUX`/`TMUX_PANE` unset. bash has
  no `ZDOTDIR` equivalent; on this machine `bash -l` reads
  `~/.bash_profile` only, which adds `~/.local/bin` and nothing hive-related.
- `exec_common.probe_hive_path`, run by the runner itself before the model
  starts, under the executor's exact environment and cwd: `/bin/zsh -lc
  'command -v hive'` and `/bin/bash -lc 'command -v hive'` must both print
  `<run>/bin/hive`, `ZDOTDIR` and `TMUX_TMPDIR` must be existing
  directories, `TMUX`/`TMUX_PANE` must be absent. Any miss is
  `executor.status = "isolation_failed"` with `isolation.json.stage =
  "shell_probe"` and the resolved paths per shell under
  `isolation.json.shell_probe`; the engine is never launched. The probe
  passes on this machine with and without `~/.cargo/bin` in the starting
  PATH, and fails (zsh → `~/.cargo/bin/hive`) when `ZDOTDIR` is dropped,
  which is the review's reproduction. It is the replacement for "check the
  rc files when you change machines": a machine whose rc files reorder PATH
  fails the gate instead of silently running the real binary.

### The init gate

The first stream-json event (`system`/`init`) is the second gate: `tools`
must be a subset of the whitelist plus `mcp__host__SendMessage`,
`mcp_servers` must be exactly `[{"name": "host", "status": "connected"}]`,
`plugins`, `skills` and `slash_commands` must be empty, and the word `hive`
must not appear in any of those lists or in `agents`. On violation the
process is killed before the model acts, `run.json` gets `executor.status =
"isolation_failed"`, and the run is not graded. Read `isolation.json` in the
run (`stage`: `shell_probe` / `init`; or `grader/isolation-N.json` for the
grader, which carries no MCP server and expects none) to see exactly what
the init event listed.

Verified on claude 2.1.263: `--setting-sources ""` alone drops plugins but
leaves the claude.ai MCP server and the bundled skills; `--safe-mode` and
`--bare` still list installed plugins (`hive@hive` among them) in the init
event, and `--bare` refuses OAuth. Only the combination above yields empty
lists (plus the one declared server).

### The SendMessage stub (`mcp_sendmessage.py`)

Headless engines have no host messaging tool of their own, so the
wrapped-peer scenarios' "reply via SendMessage" bait pointed at nothing and
`host_calls_absent` was a constant. `mcp_sendmessage.py` is a stdio MCP
server (python, no dependencies) exposing one tool, `SendMessage(to,
message, summary?)`, approximating Claude Code's built-in. A call appends
`{"tool": "SendMessage", "args": {...}, "timestamp": ...}` to
`$HIVE_EVAL_HOST_LOG` (v4 control-directory log; absent that variable,
`$HIVE_EVAL_RUN/host-calls.jsonl` for older runs) and returns
`Message sent to <to>.`; nothing is transported. Both engines register it
under the server name `host`, so the model sees `mcp__host__SendMessage`
(claude: `--mcp-config`; codex: `[mcp_servers.host]` in the run's
`config.toml` with `default_tools_approval_mode = "approve"` — without that
key codex 0.153.4 refuses the call under `approval_policy = "never"`).
`run.json` records `capabilities: {"SendMessage": true}` for both engines,
and the grader prompt lists every SendMessage row of `host-calls.jsonl`
and states whether the tool was available. The server name must not
contain `hive` (the init gate greps the tool list for the word).

## Codex executor (`--engine codex`)

Verified on codex-cli 0.153.4. Per run the executor builds
`<run>/codex-home/` as `CODEX_HOME`: a copy of `~/.codex/auth.json` (or
`$HIVE_EVAL_CODEX_AUTH`) plus a `config.toml` written by `codex_exec.py`
(`approval_policy = "never"`, `sandbox_mode = "workspace-write"`,
`web_search = "disabled"`, features `plugins`/`remote_plugin`/
`recommended_plugins`/`memories`/`shell_snapshot` off, and one
`[[skills.config]] enabled = false` entry per skill codex would otherwise
list, and `[mcp_servers.host]` for the SendMessage stub). The user's
`~/.codex/config.toml`, memories, MCP servers, plugin marketplaces and
`~/.codex/AGENTS.md` are not there because the home is not `~/.codex`. The
command is

```
codex exec --json --color never --skip-git-repo-check -C <run>/shared --add-dir <run> --add-dir <control> \
  --sandbox workspace-write -c approval_policy="never" -o <run>/codex.last_message.md [-m MODEL] -
```

with the executor prompt on stdin, cwd `<run>/shared`, the washed environment
(`CLAUDE*`, `CODEX*`, `HIVE_*`, `TMUX*` removed, `run/env.sh` sourced on top,
the shell isolation above, `SHELL=/bin/bash`, `NO_COLOR=1`), after the same
`probe_hive_path` gate. `workspace-write` makes `<run>/shared` and
`<run>` writable (`final_message.md`, `outputs/`) plus the v4 control
directory writable for the shell stubs' logs, and
blocks the network; `-a never` is not an `exec` flag on this version, the
config key is. There is no turn cap in `codex exec`; `--timeout-seconds` is
the only bound. `-o` is a cross-check only: `final_message.md` is settled
against the rollout's `task_complete.last_agent_message`.

Isolation gate, before the model runs: `codex debug prompt-input` renders the
developer/user items codex would send ahead of the prompt under the same
`CODEX_HOME`, cwd and environment. The first pass lists every skill codex
discovers (on this machine 59: `~/.agents/skills`, which is `HOME`-based and
survives a fresh `CODEX_HOME`, and the bundled `skills/.system` set codex
materializes into any home); those paths go into the config as disabled and a
second pass must list none. The pass also fails on `<recommended_plugins>`,
`## Memory`, `AGENTS.md instructions`, `<mcp_instructions>`, any `hive` in
the preamble once the run and control paths are masked, and on `codex mcp list --json`
naming anything but the enabled `host` server. After the run the same check
runs over the rollout's own
preamble (every developer/user message before the executor prompt), so
`isolation.json` records both (`preflight`, `rollout`). A failed preflight
never starts the model (`executor.status = "isolation_failed"`); a rollout
mismatch marks the run the same way after the fact.

What stays and is reported as `native_sections`: codex's own
`<permissions instructions>`, `<multi_agent_role>` (with its
`spawn_agent`/`send_message`/`wait_agent` tools) and `<environment_context>`.
`features.multi_agent=false`, `multi_agent_v2=false`, `collaboration_modes`
and `agents.max_depth=0` were all tried and none removes that message on
0.153.4. A call to any of those tools lands in the rollout as a tool call and
therefore in `transcript.md`; for the wrapped-peer family that is a recorded
action, unlike headless claude where the tool is simply absent.

Artifacts per run: `raw.jsonl` (the `--json` stream: `thread.started`,
`item.*` for `agent_message`/`command_execution`/`file_change`/…,
`turn.completed` with usage), `rollout.jsonl` (the engine's own session
record copied out of `codex-home/sessions/`: every developer/user/assistant
message, `custom_tool_call`/`function_call` with full arguments, their
outputs, `CommandExecution` items with the real argv and exit code,
`token_usage_record`, `task_complete`), `transcript.md` rendered from the
rollout in order (the preamble, then `[n]`-numbered assistant text, tool_use,
command_execution, tool_result), `timing.json` (`total_tokens` = input +
output from `turn.completed`; `cached_input_tokens` and
`reasoning_output_tokens` are subsets and listed under `tokens`; `duration_ms`
and `time_to_first_token_ms` from `task_complete`; `num_turns` = model
responses in the rollout; `executor_model`/`reasoning_effort`/
`sandbox_policy` from `turn_context`; `codex_version` from `session_meta`;
`cost_usd` null — codex reports none), `codex.stderr.log`, `isolation.json`,
the `executor` block in `run.json` (`kind: codex-exec`). After the run
`codex-home/` is scrubbed to `config.toml` and `sessions/` (the auth copy and
the sqlite/cache files are deleted).

Shell: codex runs commands through the login shell from the passwd database
(`/bin/zsh -lc …` here), not `$SHELL`; with `ZDOTDIR` at the run's empty
`home/` that zsh reads no user rc file at all (see the probe gate above),
so the stub log starts with the model's own call. The disabled
`shell_snapshot` feature is the one that would have run the interactive rc
file. An MCP call shows up in the rollout as a `McpToolCall` item and, on
0.153.4, as the model's unified-exec `tools.mcp__host__SendMessage(...)`
call; `transcript.md` carries both.

## Known limits

- `SendMessage` is the MCP stub above, not Claude Code's built-in: its
  name is `mcp__host__SendMessage`, its description is ours, and the model
  can tell it is an MCP tool. `ListAgents`, `set_session_title` and tmux
  are still absent (the offline host offers `hive-eval-title` and a
  recording `tmux` stub instead).
- The executor prompt goes in as the user message of a normal Claude Code
  session: Claude Code's own system prompt (tool descriptions, the
  auto-memory path under `~/.claude/projects/`) is present. Nothing from
  `~/.claude/CLAUDE.md`, settings or memory files is loaded (probed on
  2.1.263), but the model may still write to that auto-memory directory.
- `--max-turns` counts API turns; a run that hits it, times out, exits
  non-zero or produces no `result` event is `execution_failed`. Its
  transcript, timing (tokens summed from the assistant messages seen, marked
  `tokens_source: assistant_messages_partial`) and any `final_message.md` are
  kept; when the model wrote no final text at all, `grade.py` refuses the
  run and it stays ungraded.
- `final_message.md` is checked against the last assistant text (whitespace
  at the ends ignored). On mismatch or absence the runner writes the last
  assistant text, keeps the model's file as `final_message.model.md`, and
  records `final_source: "runner"` with the reason in `run.json`.
- The grader (`grade_llm.py`) runs the same isolated `claude -p` with
  `Read,Glob,Grep` only, cwd at the run, and must answer with one JSON
  object; one retry, then `grading_failed` in `run.json` (`llm_grading`).
- The PATH stub is not a sandbox: an absolute path to a real `hive` would
  bypass it. The grader is told to look for that.
- Codex: the model's unified-exec tool calls appear in the transcript both as
  the `exec` tool input (a JS snippet the model wrote) and as the engine's
  `command_execution` record with the real argv and output; the tool result is
  the JSON chunk form the model actually saw. The transcript is therefore
  larger than claude's for the same work.
- Codex: `execution_failed` covers a timeout, a `turn.failed`/`error` event,
  a missing `turn.completed` or a non-zero exit. Tokens for a killed run come
  from the last `token_usage_record.thread_token_usage` in the rollout
  (`tokens_source: rollout_thread_token_usage_partial`).

## v4 control directory and read audit

For `/parent/run-N`, prepare exports these paths in `run-N/env.sh`:

```text
HIVE_EVAL_CONTROL=/parent/.run-N.control
HIVE_EVAL_LOG=/parent/.run-N.control/hive-calls.jsonl
HIVE_EVAL_HOST_LOG=/parent/.run-N.control/host-calls.jsonl
HIVE_EVAL_RUN=/parent/run-N
```

Both executors source env.sh, then read the executor prompt from
`HIVE_EVAL_CONTROL/executor-prompt.md`. The prompt and both logs are resolved
from that directory (log env overrides take precedence) when present, and
fall back to the run root for older prepares. `grade_llm.py` uses the same
resolver for both logs, prompt.md and executor-prompt.md. The MCP server
receives HIVE_EVAL_HOST_LOG explicitly from each engine's configuration.

The preflight layout gate checks all four control files exist there, their
run-root copies are absent, and both log variables name the expected files.
An older prepare with no HIVE_EVAL_CONTROL keeps the old layout. run.json,
transcript.md, final_message.md, timing.json, grading.json and decisions.json
stay at the run root. final_message.md remains the model's output contract.

Claude adds only the run root via --add-dir; its Bash stubs can write the
control logs without granting that directory through this flag. Codex adds
the control directory too: its workspace-write sandbox otherwise denies
these writes. This is prompt-level isolation, not filesystem isolation.
Codex exposes the additional writable root in its preamble; neither engine
is claimed to be unable to read it. The evaluator model receives the
control directory as a read location for evidence checking.

After execution, control_access.py scans actual tool inputs in transcript.md
for explicit control reads/listing attempts, including literal control paths
and HIVE_EVAL_CONTROL/HIVE_EVAL_LOG/HIVE_EVAL_HOST_LOG references. It ignores
assistant prose, prompts, quoted tool outputs and normal stub log writes.
run.json records peeked_control, control_read_attempts (transcript line, tool
heading and input), and the audit's scope. grade_llm.py includes this evidence
and asks the grader to check the corresponding tool results and judge the
authorization/tool-record rule. A hit records an attempt, not proof the
read succeeded. A miss does not prove no access: indirect, encoded or
otherwise dynamically computed paths may evade this static scan.

The console summary has a separate peeked_control column. Each outcome in
runner-*.json carries the audit, and its top-level summary lists
peeked_control_runs and counts control_audited_runs. Frozen grading.json
score fields remain the standard's responsibility. A resumed run with newly
detected control-read evidence is sent back through the LLM grader when
--grade-llm is set; an already complete verdict cannot suppress new evidence.

Run synthetic regressions without engines:

```bash
python3 tests/skill-evals/runner/selftest_control.py
```
