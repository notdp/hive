//! CLI entry point for hive — port of `src/hive/cli.py` (skeleton half).
//!
//! This module owns the clap command tree for the ENTIRE surface, `pub fn
//! main()`, and the shared helpers both command halves use. Core registry
//! verbs live in `core_cmds`; everything else routes to `rest` (ported
//! separately as `cli/rest.rs`).

pub mod core_cmds;
pub mod help_text;
pub mod rest;
mod team_ops;
mod util;

use std::path::Path;

use clap::{Arg, ArgAction, ArgMatches, Command};

use crate::tmux;

pub use team_ops::*;
pub use util::*;

// ---------------------------------------------------------------------------
// Command tree
// ---------------------------------------------------------------------------

fn passthrough_command(
    name: &'static str,
    about: &'static str,
    long_about: &'static str,
) -> Command {
    Command::new(name)
        .about(about)
        .long_about(long_about)
        .disable_help_flag(true)
        .arg(
            Arg::new("args")
                .num_args(0..)
                .allow_hyphen_values(true)
                .trailing_var_arg(true),
        )
}

fn json_default_options(cmd: Command) -> Command {
    cmd.arg(
        Arg::new("plain")
            .long("plain")
            .action(ArgAction::SetTrue)
            .help("Human-readable output instead of the default JSON"),
    )
    .arg(
        Arg::new("legacy_json")
            .long("json")
            .action(ArgAction::SetTrue)
            .hide(true)
            .help("Deprecated no-op (JSON is the default output)"),
    )
}

pub(crate) fn build_cli() -> Command {
    Command::new("hive")
        .about("Hive - tmux-first multi-agent collaboration runtime.")
        .version(_hive_version())
        .disable_version_flag(true)
        .disable_help_subcommand(true)
        .subcommand(
            Command::new("fork")
                .about("Fork the current agent session into a new split pane.")
                .long_about(
                    "Fork the current agent session into a new split pane.\n\n\
                     Humans typically bind this to a keyboard shortcut (terminal + tmux).\n\
                     Agents also invoke it to create a clone that can pick up work without\n\
                     interrupting the current turn.\n\n\
                     Pass `--join-as <name>` to register the new pane as a team member;\n\
                     `--prompt` then sends an initial message after the fork is ready.\n\n\
                     On a pane not bound to any Hive team, fork still works: it produces a bare,\n\
                     independent clone (no team registration, no `@hive-*` tags) and returns\n\
                     `registered: null`, `team: null`. `--join-as` requires a team-bound pane.\n\n\
                     Examples:\n  \
                     hive fork                                  # auto-detect split direction\n  \
                     hive fork --split h                        # force horizontal split\n  \
                     hive fork --join-as dodo-c1 --prompt \"continue the thread\"",
                )
                .arg(
                    Arg::new("pane_id")
                        .long("pane")
                        .default_value("")
                        .help("Source pane ID (default: auto-detect)"),
                )
                .arg(
                    Arg::new("split")
                        .long("split")
                        .short('s')
                        .value_parser(["auto", "h", "v"])
                        .default_value("auto")
                        .help("Split direction (default: auto-detect from pane dimensions)"),
                )
                .arg(
                    Arg::new("join_as")
                        .long("join-as")
                        .default_value("")
                        .help("Register the forked pane into the current team as this agent name"),
                )
                .arg(
                    Arg::new("prompt")
                        .long("prompt")
                        .default_value("")
                        .help("Prompt to send to the forked agent after it is ready"),
                ),
        )
        .subcommand(
            Command::new("join")
                .about("Join a team.")
                .long_about(
                    "Join a team.\n\n\
                     Outside tmux: the current Claude session enters TEAM's roster as a\n\
                     full member. Inside tmux: the current pane (or --pane) registers into\n\
                     the window's team.",
                )
                .arg(Arg::new("team_arg").default_value(""))
                .arg(
                    Arg::new("name_override")
                        .long("as")
                        .default_value("")
                        .help("Name for the new member (default: auto-derived)"),
                )
                .arg(
                    Arg::new("pane_override")
                        .long("pane")
                        .default_value("")
                        .help("Register another pane instead of the current one (tmux only)"),
                )
                .arg(
                    Arg::new("notify")
                        .long("notify")
                        .action(ArgAction::SetTrue)
                        .overrides_with("no_notify")
                        .help("Deliver the join message over the native transport (doubles as a reachability check; --no-notify registers without proving the pane deliverable)"),
                )
                .arg(
                    Arg::new("no_notify")
                        .long("no-notify")
                        .action(ArgAction::SetTrue)
                        .overrides_with("notify"),
                )
                .arg(
                    Arg::new("group_name")
                        .long("group")
                        .default_value("")
                        .help("Cross-team group tag for display and namespace reservation (optional; qualified-name routing works without it)."),
                ),
        )
        .subcommand(
            Command::new("create")
                .about("Create a team.")
                .long_about(
                    "Create a team.\n\n\
                     NAME is optional everywhere (pool-picked by default). Outside tmux:\n\
                     a headless team — `hive attach` renders it. Inside tmux on an agent\n\
                     pane: that pane becomes the orch. Inside tmux on a shell pane: the\n\
                     window binds the team without an orch.",
                )
                .arg(Arg::new("name").default_value(""))
                .arg(
                    Arg::new("desc")
                        .long("desc")
                        .short('d')
                        .default_value("")
                        .help("Team description"),
                )
                .arg(
                    Arg::new("workspace")
                        .long("workspace")
                        .short('w')
                        .default_value("")
                        .help("Workspace path to initialize"),
                )
                .arg(
                    Arg::new("reset_workspace")
                        .long("reset-workspace")
                        .action(ArgAction::SetTrue)
                        .help("Remove existing workspace before initialization"),
                )
                .arg(
                    Arg::new("state_entries")
                        .long("state")
                        .action(ArgAction::Append)
                        .help("Initial state KEY=VALUE (repeatable)"),
                ),
        )
        .subcommand(
            Command::new("delete")
                .about("Delete a team and clean up.")
                .arg(Arg::new("name").required(true))
                .arg(
                    Arg::new("workspace")
                        .long("workspace")
                        .short('w')
                        .default_value("")
                        .help("Workspace path to remove"),
                )
                .arg(
                    Arg::new("keep_workspace")
                        .long("keep-workspace")
                        .action(ArgAction::SetTrue)
                        .hide(true)
                        .help("Deprecated no-op (workspace is now kept by default)"),
                )
                .arg(
                    Arg::new("delete_workspace")
                        .long("delete-workspace")
                        .action(ArgAction::SetTrue)
                        .help("Also delete the workspace directory"),
                ),
        )
        .subcommand(
            Command::new("spawn")
                .about("Spawn an agent pane, optionally dispatching a task atomically.")
                .long_about(
                    "Spawn an agent pane, optionally dispatching a task atomically.\n\n\
                     Creates a new tmux pane in the current window and starts the chosen\n\
                     agent CLI. By default spawns the same CLI as the current pane; use\n\
                     `--cli claude|codex|grok` to pick a specific one.\n\n\
                     With `--task <artifact>`, the member boots straight into the member\n\
                     contract (`/hive:hive`) and the task artifact arrives as its first\n\
                     `<HIVE>` message — spawn and dispatch are one atomic step, so the\n\
                     member never wanders off exploring while waiting for work.\n\n\
                     Examples:\n  \
                     hive spawn explore --task /tmp/tasks/explore.md\n  \
                     hive spawn review --cli codex --task /tmp/tasks/review.md\n  \
                     hive spawn dodo --cli codex\n  \
                     hive spawn claude -m claude-opus-5 --skill none",
                )
                .arg(Arg::new("agent_name").required(true))
                .arg(
                    Arg::new("model")
                        .long("model")
                        .short('m')
                        .default_value("")
                        .help("Model ID. claude: prefer aliases (fable/opus/sonnet) — they always track the latest; codex/grok: checked against the CLI's own catalog"),
                )
                .arg(
                    Arg::new("prompt")
                        .long("prompt")
                        .short('p')
                        .default_value("")
                        .help("Initial prompt (typed into TUI after startup)"),
                )
                .arg(
                    Arg::new("cwd")
                        .long("cwd")
                        .default_value("")
                        .help("Working directory"),
                )
                .arg(
                    Arg::new("skill")
                        .long("skill")
                        .default_value("hive:hive")
                        .help("Base skill to load after startup ('none' to skip)"),
                )
                .arg(
                    Arg::new("env")
                        .long("env")
                        .short('e')
                        .action(ArgAction::Append)
                        .help("Extra env vars (KEY=VALUE, repeatable)"),
                )
                .arg(
                    Arg::new("cli_name")
                        .long("cli")
                        .value_parser(["claude", "codex", "grok"])
                        .help("Agent CLI to spawn (default: same as current pane)"),
                )
                .arg(
                    Arg::new("task_artifact")
                        .long("task")
                        .help("Task artifact to dispatch atomically once the member is ready (member never boots into an empty inbox)"),
                )
                .arg(
                    Arg::new("team_arg")
                        .long("team")
                        .short('t')
                        .default_value("")
                        .help("Explicit team (default: the pane's binding)"),
                ),
        )
        .subcommand(
            Command::new("config")
                .about("Read / write user-level settings (~/.hive/settings.json).")
                .subcommand_required(true)
                .arg_required_else_help(true)
                .subcommand(
                    Command::new("get")
                        .about("Print the value at KEY (dot-path). Exit 1 when unset.")
                        .arg(Arg::new("key").required(true)),
                )
                .subcommand(
                    Command::new("set")
                        .about("Set KEY to VALUE (true/false/int/float/string).")
                        .arg(Arg::new("key").required(true))
                        .arg(Arg::new("value").required(true)),
                )
                .subcommand(
                    Command::new("unset")
                        .about("Remove KEY. Exit 1 when KEY was not set.")
                        .arg(Arg::new("key").required(true)),
                ),
        )
        .subcommand(
            Command::new("inject")
                .about("Debug: inject raw input into an agent pane.")
                .long_about(
                    "Debug: inject raw input into an agent pane.\n\n\
                     Writes text directly into the target pane without the `<HIVE>`\n\
                     envelope or delivery tracking. Use only when bypassing the message\n\
                     protocol for low-level debugging.\n\n\
                     Example:\n  hive inject dodo \"plain ping\"",
                )
                .arg(Arg::new("agent_name").required(true))
                .arg(Arg::new("text").required(true).allow_hyphen_values(true)),
        )
        .subcommand(
            Command::new("compact")
                .about("Trigger /compact on your own pane.")
                .long_about(
                    "Trigger /compact on your own pane.\n\n\
                     Works on any agent pane, team-bound or not: a pane with no Hive team is\n\
                     compacted by its literal pane facts, and the response carries `member` =\n\
                     the pane id with `team: null`.\n\n\
                     When wired into a tmux key binding, pass `--pane \"#{pane_id}\"` so the\n\
                     triggering pane is captured by tmux at keypress time rather than read\n\
                     from the (potentially stale) TMUX_PANE env in a detached subprocess.\n\n\
                     Examples:\n  hive compact\n  hive compact --pane %21",
                )
                .arg(
                    Arg::new("pane_id")
                        .long("pane")
                        .default_value("")
                        .help("Target pane ID (default: current pane via TMUX_PANE)"),
                ),
        )
        .subcommand(
            Command::new("team")
                .about("Show team overview.")
                .long_about(
                    "Show team overview.\n\n\
                     Returns a JSON payload with `members[]`, `self` (your own name), the\n\
                     bound `tmuxSession` / `tmuxWindow`, `runtimeWorkspace`, and `cwd`.\n\n\
                     Each member row carries the runtime fields `busy`, `inputState`, and\n\
                     `turnPhase` — see docs/runtime-model.md for semantics. `self` is a\n\
                     string pointer: look yourself up in `members[]` for your own state.\n\n\
                     If the current tmux window has no team bound, returns a bootstrap\n\
                     payload instead: `team=null`, a pane list, and a `hint` telling you\n\
                     to run `hive create`.\n\n\
                     Examples:\n  \
                     hive team                                # full payload when a team is bound\n  \
                     hive team | jq '.members[] | select(.name==\"dodo\")'",
                )
                .arg(
                    Arg::new("team_arg")
                        .long("team")
                        .short('t')
                        .default_value("")
                        .help("Explicit team (default: the pane's binding)"),
                ),
        )
        .subcommand(
            Command::new("layout")
                .about("Apply a tmux layout preset to the current team window.")
                .long_about(
                    "Apply a tmux layout preset to the current team window.\n\n\
                     Use ``auto`` to pick a preset adaptively from the window's aspect ratio.",
                )
                .arg(
                    Arg::new("preset")
                        .required(true)
                        .ignore_case(true)
                        .value_parser([
                            "auto",
                            "main-vertical",
                            "main-horizontal",
                            "tiled",
                            "even-horizontal",
                            "even-vertical",
                        ]),
                ),
        )
        .subcommand(
            Command::new("flow")
                .about("Deterministic member orchestration from a Python script.")
                .long_about(
                    "Deterministic member orchestration from a Python script.\n\n\
                     A flow script uses the `hive.flow` library: `agent()` spawns a live\n\
                     member pane, dispatches a task atomically, and blocks for the reply;\n\
                     `parallel()` fans out. Every node is a visible pane — watch, type\n\
                     into, or interrupt any of them while the flow runs.",
                )
                .subcommand_required(true)
                .arg_required_else_help(true)
                .subcommand(
                    Command::new("run")
                        .about("Run SCRIPT against the current team.")
                        .long_about(
                            "Run SCRIPT against the current team.\n\n\
                             The script is trusted Python (you or your orch wrote it). Members it\n\
                             spawns reply to the reserved `flow` mailbox; the runner blocks until\n\
                             the script finishes. Typical use from an orch: run it in a background\n\
                             shell and read the output when it completes.",
                        )
                        .arg(Arg::new("script").required(true)),
                ),
        )
        .subcommand(
            Command::new("pr")
                .about("Pin a PR number on the team window's status bar.")
                .subcommand_required(true)
                .arg_required_else_help(true)
                .subcommand(json_default_options(
                    Command::new("set")
                        .about("Label the current team window with its PR number.")
                        .long_about(
                            "Label the current team window with its PR number.\n\n\
                             Run right after ``gh pr create --draft`` — writes ``@hive-pr`` on the\n\
                             current tmux window and installs a per-window status-bar display derived\n\
                             from the global ``window-status-format`` / ``window-status-current-format``\n\
                             (the index position renders ``PR<n>``; user styling and padding are\n\
                             preserved). Idempotent — re-running replaces the stamp and re-derives\n\
                             the display.",
                        )
                        .arg(Arg::new("number").required(true).value_parser(clap::value_parser!(i64))),
                ))
                .subcommand(json_default_options(
                    Command::new("clear")
                        .about("Clear the current team window's PR number stamp."),
                )),
        )
        .subcommand(
            Command::new("view")
                .about("Read-only viewer for a Claude session transcript (follows live).")
                .arg(Arg::new("session_id").required(true)),
        )
        .subcommand(
            Command::new("attach")
                .about("Render a team's display: jump to its window, or build one.")
                .long_about(
                    "Render a team's display: jump to its window, or build one.\n\n\
                     The registry is the team's existence; this materializes (or finds) its\n\
                     tmux window — one attach pane per member, each riding its engine's own\n\
                     viewer (claude attach loop / codex thread resume / grok session resume).\n\
                     Run from outside tmux it finishes by exec'ing `tmux attach`.",
                )
                .arg(Arg::new("team_name").required(true)),
        )
        .subcommand(json_default_options(
            Command::new("ls")
                .about("List hive teams from the registry, with their display state.")
                .long_about(
                    "List hive teams from the registry, with their display state.\n\n\
                     Works outside tmux too — the registry is the truth layer; without a\n\
                     server every team simply shows as detached.",
                ),
        ))
        .subcommand(
            Command::new("send")
                .about("Send a message to another agent — the only message verb.")
                .long_about(
                    "Send a message to another agent — the only message verb.\n\n\
                     Threading is automatic: when the latest inbound message from the\n\
                     recipient is still unanswered, this send is recorded as its reply;\n\
                     otherwise it opens a new thread. Senders never handle msgIds.\n\n\
                     The recipient is an address, and every `from=` value on a received\n\
                     envelope is one — answer by copying it verbatim. A teammate is a bare\n\
                     name. A member of some team is `<team>.<member>` (how a Claude session\n\
                     outside tmux, e.g. the desktop app, reaches in; bare names work there\n\
                     too while unique across live teams — its message arrives as\n\
                     `from=ccd.<its name>`). A Claude session outside any team is\n\
                     `ccd.<name or title or pid>` (how a member reaches out). `flow.run`\n\
                     is the flow runner's mailbox — an address kind, not a member; sends\n\
                     to it confirm with one `delivered to flow mailbox` line and never\n\
                     get a HIVE ack back.\n\n\
                     New-thread sends must keep `body` to a short summary and put details\n\
                     in `--artifact`; the body is rejected if longer than 500 chars, has\n\
                     3+ lines, contains fenced code, or starts markdown heading/list\n\
                     lines. A send that continues a thread is exempt.\n\n\
                     Delivery is binary and fire-and-forget: the native transport (claude\n\
                     daemon / codex daemon) either accepted the message — its runtime owns\n\
                     it from there — or the command exits non-zero with the transport\n\
                     error. Success prints nothing; there is nothing to poll afterwards.\n\n\
                     Examples:\n  \
                     hive send dodo \"review this diff\" --artifact /tmp/diff.md\n  \
                     hive send \"ccd.PR review\" \"build is green\"    # session by desktop title\n  \
                     hive send dodo \"see report\" --artifact - <<'EOF'\n  \
                     # Findings\n  - item\n  EOF",
                )
                // click passes hyphen-leading values ("- bullet reply…")
                // through positional arguments; clap must too.
                .arg(Arg::new("to_agent").required(true))
                .arg(
                    Arg::new("body")
                        .default_value("")
                        .allow_hyphen_values(true),
                )
                .arg(
                    Arg::new("artifact")
                        .long("artifact")
                        .default_value("")
                        .help("Artifact path for large payloads"),
                ),
        )
        .subcommand(
            Command::new("thread")
                .about("Show a reply thread rooted at a msgId.")
                .long_about(
                    "Show a reply thread rooted at a msgId.\n\n\
                     Returns the chain of send/reply events linked to this msgId. Useful\n\
                     to audit conversation flow or resolve \"who replied to what\".\n\n\
                     Example:\n  hive thread aBc1",
                )
                .arg(Arg::new("message_id").required(true)),
        )
        .subcommand(
            Command::new("doctor")
                .about("Diagnose agent connectivity and session state.")
                .long_about(
                    "Diagnose agent connectivity and session state.\n\n\
                     With no argument, probes yourself. With an agent name, probes that\n\
                     peer — pane liveness, transcript readability, hived heartbeat,\n\
                     runtime input state.\n\n\
                     Examples:\n  \
                     hive doctor                  # probe self\n  \
                     hive doctor dodo             # probe a peer",
                )
                .arg(Arg::new("agent_name").default_value("")),
        )
        .subcommand(
            Command::new("capture")
                .about("Debug: capture raw pane output from a team member's pane.")
                .long_about(
                    "Debug: capture raw pane output from a team member's pane.\n\n\
                     Prints the last N lines (default 30) of the member's tmux pane.\n\
                     Use to inspect what the agent actually sees when transcript parsing\n\
                     gives unexpected results.\n\n\
                     Example:\n  hive capture dodo -n 80",
                )
                .arg(Arg::new("member_name").required(true))
                .arg(
                    Arg::new("lines")
                        .long("lines")
                        .short('n')
                        .default_value("30")
                        .value_parser(clap::value_parser!(i64)),
                ),
        )
        .subcommand(
            Command::new("interrupt")
                .about("Interrupt an agent's running turn.")
                .long_about(
                    "Interrupt an agent's running turn.\n\n\
                     Aborts the turn over the member's own transport — addressed to its\n\
                     engine, not typed at its pane. Use when a peer is stuck in a tool\n\
                     loop or you need to abort a runaway action.\n\n\
                     Example:\n  hive interrupt dodo",
                )
                .arg(Arg::new("agent_name").required(true)),
        )
        .subcommand(
            Command::new("kill")
                .about("Kill an agent pane and remove it from the team.")
                .long_about(
                    "Kill an agent pane and remove it from the team.\n\n\
                     Qualified names (`<group>.<name>`) resolve across teams so you can\n\
                     kill a peer-team agent from the main group pane. Bare names resolve\n\
                     against the caller's scoped team.\n\n\
                     Example:\n  hive kill worker1",
                )
                .arg(Arg::new("agent_name").required(true)),
        )
        .subcommand(passthrough_command(
            "cvim",
            "Human-only: edit the last assistant message in vim, send it back.",
            "Human-only: edit the last assistant message in vim, send it back.\n\n\
             Opens a popup vim seeded with the previous assistant message and sends the\n\
             edited result back to the agent pane. Intended to be typed by the human via\n\
             the agent's shell escape (e.g. `!hive cvim`) in Claude Code or Codex. Not\n\
             meant for the model to invoke on its own.",
        ))
        .subcommand(passthrough_command(
            "vim",
            "Human-only: compose in a blank vim buffer, send it to the agent pane.",
            "Human-only: compose in a blank vim buffer, send it to the agent pane.\n\n\
             Intended to be typed by the human via the agent's shell escape (e.g. `!hive vim`)\n\
             in Claude Code or Codex. Not meant for the model to invoke on its own.",
        ))
        .subcommand(passthrough_command(
            "vfork",
            "Human-only: fork the current Hive session into a vertical split.",
            "Human-only: fork the current Hive session into a vertical split.\n\n\
             Intended to be typed by the human via the agent's shell escape (e.g. `!hive vfork`)\n\
             in Claude Code or Codex. Not meant for the model to invoke on its own.",
        ))
        .subcommand(passthrough_command(
            "hfork",
            "Human-only: fork the current Hive session into a horizontal split.",
            "Human-only: fork the current Hive session into a horizontal split.\n\n\
             Intended to be typed by the human via the agent's shell escape (e.g. `!hive hfork`)\n\
             in Claude Code or Codex. Not meant for the model to invoke on its own.",
        ))
        .subcommand(
            Command::new("notify")
                .about("Notify the user for the current pane.")
                .long_about(
                    "Notify the user for the current pane.\n\n\
                     Flashes the tmux window status line, renames the tab, and rings the\n\
                     terminal bell so the user can spot the pending pane at a glance. The\n\
                     flash persists until the user focuses the target window (no\n\
                     timeout). Use this only when you are blocked and need the human\n\
                     back — not for progress updates. Message structure should cover:\n\
                     what happened, why you need them now, what to do on return.\n\n\
                     Examples:\n  hive notify \"press Space to come back and confirm migration\"",
                )
                .arg(Arg::new("message").required(true).allow_hyphen_values(true)),
        )
        .subcommand(
            Command::new("plugin")
                .about("Manage first-party Hive plugins.")
                .subcommand_required(true)
                .arg_required_else_help(true)
                .subcommand(json_default_options(
                    Command::new("list").about("List available plugins and whether they are enabled."),
                ))
                .subcommand(json_default_options(
                    Command::new("ls")
                        .about("Hidden alias of `hive plugin list`.")
                        .hide(true),
                ))
                .subcommand(json_default_options(
                    Command::new("enable")
                        .about("Enable a plugin and materialize its commands.")
                        .arg(Arg::new("name").required(true)),
                ))
                .subcommand(json_default_options(
                    Command::new("disable")
                        .about("Disable a plugin and remove its commands.")
                        .arg(Arg::new("name").required(true)),
                ))
                .subcommand(Command::new("path").about(
                    "Materialize the embedded plugin marketplace and print the payload \
                     directory (the command source Claude re-runs each session).",
                )),
        )
        .subcommand(passthrough_command(
            "codex",
            "Launch codex on the shared app-server daemon (hive-managed).",
            "Launch codex on the shared app-server daemon (hive-managed).\n\n\
             Usually invoked through the `hcodex` launcher from `hive shell-init` rather\n\
             than by hand; all arguments are forwarded to codex. Replaces the current process\n\
             with codex and never returns on success.",
        ))
        .subcommand(passthrough_command(
            "claude",
            "Launch claude as a hive-managed background job (hclaude launcher).",
            "Launch claude as a hive-managed background job (hclaude launcher).\n\n\
             Interactive launches run as `claude --bg` jobs with the pane attached as\n\
             a viewer; management subcommands and non-interactive shapes pass through\n\
             to plain claude. Does not return on the raw path; on the managed path it\n\
             exits with the viewer loop's status.",
        ))
        .subcommand(passthrough_command(
            "grok",
            "Launch grok attached to a per-pane leader daemon (hive-managed).",
            "Launch grok attached to a per-pane leader daemon (hive-managed).\n\n\
             Usually invoked through the `hgrok` launcher from `hive shell-init` rather\n\
             than by hand; all arguments are forwarded to grok. Replaces the current\n\
             process with grok and never returns on success.",
        ))
        .subcommand(
            Command::new("ccd")
                .about("Discover Claude Code sessions outside the team — the desktop app, another terminal — by their cross-session inbox registry.")
                .long_about(
                    "Discover Claude Code sessions outside the team — the desktop app,\n\
                     another terminal — by their cross-session inbox registry.\n\n\
                     `hive ccd ls` lists the reachable sessions; messaging one is plain\n\
                     `hive send ccd.<name>` (name, desktop title, or pid).",
                )
                .subcommand_required(true)
                .arg_required_else_help(true)
                .subcommand(
                    Command::new("ls")
                        .about("List the Claude Code sessions `hive send ccd.<name>` can reach.")
                        .long_about(
                            "List the Claude Code sessions `hive send ccd.<name>` can reach.\n\n\
                             The same registry `/list-agents` reads: every live session that binds a\n\
                             cross-session inbox (Claude Code 2.1.224+). A session on an older CLI, or\n\
                             started in bare mode, has no inbox and is not listed. `title` is the\n\
                             desktop app's session title when one is set. A session that is really a\n\
                             live team member carries a `member` field with its `<team>.<agent>`\n\
                             address: message it over the bus, not here.",
                        ),
                ),
        )
        .subcommand(
            Command::new("resume-hint")
                .about("Print a cd-ready resume command for the session this pane just ran.")
                .hide(true)
                .arg(
                    Arg::new("cli_name")
                        .required(true)
                        .value_parser(["claude", "codex", "grok"]),
                ),
        )
        .subcommand(
            Command::new("shell-init")
                .about("Print the `hcodex` / `hclaude` / `hgrok` launchers for your shell.")
                .long_about(
                    "Print the `hcodex` / `hclaude` / `hgrok` launchers for your shell.\n\n\
                     Add to your shell rc; then `hcodex` / `hclaude` / `hgrok` start a\n\
                     hive-connected codex / claude / grok in the current tmux pane, while the\n\
                     plain `codex` / `claude` / `grok` stay untouched:\n\n  \
                     # ~/.zshrc or ~/.bashrc\n  \
                     eval \"$(hive shell-init zsh)\"\n  \
                     # ~/.config/fish/config.fish\n  \
                     hive shell-init fish | source\n\n\
                     Outside tmux, and for management subcommands and non-interactive flags,\n\
                     the launchers run the plain binary.",
                )
                .arg(Arg::new("shell").default_value("")),
        )
        .subcommand(
            Command::new("worktree")
                .about("Per-feature worktree pool: start a feature, finish it, inspect state.")
                .long_about(
                    "Per-feature worktree pool: start a feature, finish it, inspect state.\n\n\
                     Pool layout: <main checkout>/.claude/worktrees/<feature>, branch == feature.\n\
                     Hive creates/removes worktrees and records ownership in git config;\n\
                     entering/leaving the directory is the agent's own move (Claude:\n\
                     EnterWorktree path=<path> / ExitWorktree action=keep; Codex: cd).\n\n\
                     Examples:\n  \
                     hive worktree start login-flow         # create worktree + branch, print JSON with path\n  \
                     hive worktree status                   # pool state for this repo\n  \
                     hive worktree done login-flow          # remove the worktree, keep the branch",
                )
                .subcommand_required(true)
                .arg_required_else_help(true)
                .subcommand(json_default_options(
                    Command::new("set-base")
                        .about("Declare the team's integration branch (the base of every sub-PR).")
                        .long_about(
                            "Declare the team's integration branch (the base of every sub-PR).\n\n\
                             Run from the team window after creating and pushing the branch; every\n\
                             `hive worktree start` in this window afterwards resolves its base from\n\
                             it. REF must already resolve to a commit.",
                        )
                        .arg(Arg::new("ref").required(true)),
                ))
                .subcommand(json_default_options(
                    Command::new("start")
                        .about("Create (or re-attach) the worktree for FEATURE and print its path as JSON.")
                        .long_about(
                            "Create (or re-attach) the worktree for FEATURE and print its path as JSON.\n\n\
                             Exit 0 = ready (mode created/existing/attached/adopted-existing-branch).\n\
                             Exit 1 with mode=needs-rebase = branch exists but does not contain the\n\
                             resolved base: rebase inside the worktree, then rerun start.",
                        )
                        .arg(Arg::new("feature").required(true))
                        .arg(
                            Arg::new("base_ref")
                                .long("base")
                                .help("Base ref override (default: the window's integration branch from `hive worktree set-base`, else detected default branch)"),
                        ),
                ))
                .subcommand(json_default_options(
                    Command::new("done")
                        .about("Remove FEATURE's worktree. The branch is always kept (PRs live on it).")
                        .long_about(
                            "Remove FEATURE's worktree. The branch is always kept (PRs live on it).\n\n\
                             Refuses while you are inside the worktree, while a git operation is in\n\
                             progress, or while there are uncommitted changes (unless --force).",
                        )
                        .arg(Arg::new("feature").required(true))
                        .arg(
                            Arg::new("force")
                                .long("force")
                                .action(ArgAction::SetTrue)
                                .help("Discard uncommitted work (destructive; prints a status summary first)"),
                        ),
                ))
                .subcommand(json_default_options(
                    Command::new("status")
                        .about("Read-only lifecycle view of FEATURE (or every hive-labeled worktree).")
                        .arg(Arg::new("feature")),
                )),
        )
}

// ---------------------------------------------------------------------------
// main + dispatch
// ---------------------------------------------------------------------------

const _KNOWN_COMMANDS: &[&str] = &[
    "fork",
    "join",
    "create",
    "delete",
    "spawn",
    "config",
    "inject",
    "compact",
    "team",
    "layout",
    "flow",
    "pr",
    "view",
    "attach",
    "ls",
    "send",
    "thread",
    "doctor",
    "capture",
    "interrupt",
    "kill",
    "cvim",
    "vim",
    "vfork",
    "hfork",
    "notify",
    "plugin",
    "codex",
    "claude",
    "grok",
    "ccd",
    "resume-hint",
    "shell-init",
    "worktree",
];

/// Click groups and their subcommands (help lookup + bare-group help).
const _HELP_GROUPS: &[(&str, &[&str])] = &[
    ("ccd", &["ls"]),
    ("config", &["get", "set", "unset"]),
    ("flow", &["run"]),
    ("plugin", &["disable", "enable", "list", "ls", "path"]),
    ("pr", &["clear", "set"]),
    ("worktree", &["done", "set-base", "start", "status"]),
];

/// Click help interception: the command path whose help click would print.
///
/// Click's `--help` (and `-h` outside the cvim family, whose
/// `help_option_names` is `["--help"]`) is an eager option: it prints help
/// and exits 0 wherever it appears among the parsed args (never past `--`).
/// A group prints its own help unless a known subcommand appears first.
fn help_path<'a>(invoked: &'a str, tail: &'a [String]) -> Option<Vec<&'a str>> {
    if matches!(invoked, "claude" | "codex" | "grok") {
        return None; // launchers forward all args to the wrapped CLI
    }
    let help_opts: &[&str] = if matches!(invoked, "cvim" | "vim" | "vfork" | "hfork") {
        &["--help"]
    } else {
        &["-h", "--help"]
    };
    let subs = _HELP_GROUPS
        .iter()
        .find(|(group, _)| *group == invoked)
        .map(|(_, subs)| *subs);
    for (i, tok) in tail.iter().enumerate() {
        if tok == "--" {
            return None;
        }
        if help_opts.contains(&tok.as_str()) {
            return Some(vec![invoked]);
        }
        if let Some(subs) = subs {
            if subs.contains(&tok.as_str()) {
                for tok2 in &tail[i + 1..] {
                    if tok2 == "--" {
                        break;
                    }
                    if help_opts.contains(&tok2.as_str()) {
                        return Some(vec![invoked, tok.as_str()]);
                    }
                }
            }
            // ponytail: a non-sub token stops the scan; click's own parse
            // error for it (out of the equivalence corpus) falls to clap.
            return None;
        }
    }
    None
}

fn arg_str<'a>(m: &'a ArgMatches, key: &str) -> &'a str {
    m.get_one::<String>(key).map(String::as_str).unwrap_or("")
}

fn arg_vec(m: &ArgMatches, key: &str) -> Vec<String> {
    m.get_many::<String>(key)
        .map(|values| values.cloned().collect())
        .unwrap_or_default()
}

/// Root-group gates from the Python `cli()` callback.
fn run_root_gates(invoked: &str) {
    _require_codex_native(Some(invoked));
    if !_TMUX_OPTIONAL_ROOT_COMMANDS.contains(&invoked) && !tmux::is_inside_tmux() {
        if invoked == "send" && crate::adapters::claude_sessions::self_session().is_some() {
            return; // a Claude session sending into hive as a guest
        }
        fail(_TMUX_REQUIRED_MESSAGE);
    }
}

pub fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let args: Vec<String> = argv.iter().skip(1).cloned().collect();
    let root_help = help_text::help_for(&[]).expect("root help");

    if args.is_empty() {
        // Click's `no_args_is_help`: help goes to stderr, exit code 2.
        eprint!("{root_help}");
        std::process::exit(2);
    }
    match args[0].as_str() {
        "-h" | "--help" => {
            print!("{root_help}");
            std::process::exit(0);
        }
        "--version" => {
            println!("hive, version {}", _hive_version());
            std::process::exit(0);
        }
        _ => {}
    }

    let invoked = args[0].clone();
    let tail: Vec<String> = args.iter().skip(1).cloned().collect();

    // Hidden helper subcommands, never listed in help: the cvim toolkit
    // (called back into by the materialized `cvim-command` bash asset) and
    // the flow-op bridge (called by the materialized pylib flow client),
    // both via $HIVE_BIN. They replace standalone Python beside the old
    // asset trees, so they dispatch before the known-command gate and skip
    // the root gates.
    match invoked.as_str() {
        "cvim-sendback" => std::process::exit(crate::cvim::sendback_main(&tail)),
        "cvim-payload" => std::process::exit(crate::cvim::payload_main(&tail)),
        "cvim-list" => std::process::exit(crate::cvim::list_main(&tail)),
        "cvim-seed" => std::process::exit(crate::cvim::seed_main(&tail)),
        "cvim-session" => std::process::exit(crate::cvim::session_main(&tail)),
        "cvim-profile" => std::process::exit(crate::cvim::profile_main(&tail)),
        "flow-op" => std::process::exit(crate::flow::op_main(&tail)),
        // notify's tmux hook / flash-script callbacks (Python's
        // `-m hive.notify_ui` and the pane-attention middle layer).
        "notify-hook" => std::process::exit(crate::notify_ui::main(&tail)),
        "notify-attention" => std::process::exit(crate::notify_ui::attention_main()),
        _ => {}
    }

    // Click resolves the subcommand before the group callback runs, so an
    // unknown command errors before any tmux/codex gate fires.
    if !_KNOWN_COMMANDS.contains(&invoked.as_str()) && !invoked.starts_with('-') {
        eprint!(
            "Usage: hive [OPTIONS] COMMAND [ARGS]...\n\
             Try 'hive -h' for help.\n\n\
             Error: No such command '{invoked}'.\n"
        );
        std::process::exit(2);
    }

    // Click's eager help option prints before the subcommand body runs (and
    // the root callback skips its gates whenever -h/--help is in argv).
    if let Some(path) = help_path(&invoked, &tail) {
        print!("{}", help_text::help_for(&path).expect("known help path"));
        std::process::exit(0);
    }

    let help_requested = args.iter().any(|a| a == "-h" || a == "--help");
    if _KNOWN_COMMANDS.contains(&invoked.as_str()) && !help_requested {
        run_root_gates(&invoked);
    }

    // Click group with no subcommand: `no_args_is_help` — stderr, exit 2.
    if tail.is_empty() && _HELP_GROUPS.iter().any(|(group, _)| *group == invoked) {
        eprint!(
            "{}",
            help_text::help_for(&[invoked.as_str()]).expect("group help")
        );
        std::process::exit(2);
    }

    // Launcher / human-helper passthrough: everything after the subcommand is
    // forwarded verbatim (Click's ignore_unknown_options + UNPROCESSED args).
    match invoked.as_str() {
        "codex" => {
            rest::codex_cmd(&tail);
            return;
        }
        "claude" => {
            rest::claude_cmd(&tail);
            return;
        }
        "grok" => {
            rest::grok_cmd(&tail);
            return;
        }
        "cvim" => {
            rest::cvim_cmd(&tail);
            return;
        }
        "vim" => {
            rest::vim_cmd(&tail);
            return;
        }
        "vfork" => {
            rest::vfork_cmd(&tail);
            return;
        }
        "hfork" => {
            rest::hfork_cmd(&tail);
            return;
        }
        _ => {}
    }

    let matches = match build_cli().try_get_matches_from(&argv) {
        Ok(matches) => matches,
        Err(err) => err.exit(),
    };
    dispatch(&matches);
}

fn dispatch(matches: &ArgMatches) {
    match matches.subcommand() {
        Some(("fork", m)) => rest::fork_cmd(
            arg_str(m, "pane_id"),
            arg_str(m, "split"),
            arg_str(m, "join_as"),
            arg_str(m, "prompt"),
        ),
        Some(("join", m)) => core_cmds::join_cmd(
            arg_str(m, "team_arg"),
            arg_str(m, "name_override"),
            arg_str(m, "pane_override"),
            !m.get_flag("no_notify"),
            arg_str(m, "group_name"),
        ),
        Some(("create", m)) => core_cmds::create(
            arg_str(m, "name"),
            arg_str(m, "desc"),
            arg_str(m, "workspace"),
            m.get_flag("reset_workspace"),
            &arg_vec(m, "state_entries"),
        ),
        Some(("delete", m)) => core_cmds::delete(
            arg_str(m, "name"),
            arg_str(m, "workspace"),
            m.get_flag("keep_workspace"),
            m.get_flag("delete_workspace"),
        ),
        Some(("spawn", m)) => {
            // Click declares --task as `type=click.Path(exists=True,
            // dir_okay=False)` — validated at parse time, before the handler.
            let task = m.get_one::<String>("task_artifact").cloned();
            if let Some(task) = &task {
                let p = Path::new(task);
                if !p.exists() {
                    eprintln!("Error: Invalid value for '--task': Path '{task}' does not exist.");
                    std::process::exit(2);
                }
                if p.is_dir() {
                    eprintln!("Error: Invalid value for '--task': Path '{task}' is a directory.");
                    std::process::exit(2);
                }
            }
            rest::spawn(
                arg_str(m, "agent_name"),
                arg_str(m, "model"),
                arg_str(m, "prompt"),
                arg_str(m, "cwd"),
                arg_str(m, "skill"),
                &arg_vec(m, "env"),
                m.get_one::<String>("cli_name").map(String::as_str),
                task.as_deref(),
                arg_str(m, "team_arg"),
            )
        }
        Some(("config", m)) => match m.subcommand() {
            Some(("get", m)) => rest::config_get(arg_str(m, "key")),
            Some(("set", m)) => rest::config_set(arg_str(m, "key"), arg_str(m, "value")),
            Some(("unset", m)) => rest::config_unset(arg_str(m, "key")),
            _ => unreachable!("subcommand required"),
        },
        Some(("inject", m)) => rest::inject_cmd(arg_str(m, "agent_name"), arg_str(m, "text")),
        Some(("compact", m)) => rest::compact_cmd(arg_str(m, "pane_id")),
        Some(("team", m)) => core_cmds::team_cmd(arg_str(m, "team_arg")),
        Some(("layout", m)) => rest::layout_cmd(&arg_str(m, "preset").to_lowercase()),
        Some(("flow", m)) => match m.subcommand() {
            Some(("run", m)) => {
                let script = arg_str(m, "script");
                if !Path::new(script).exists() {
                    eprintln!("Error: Invalid value for 'SCRIPT': Path '{script}' does not exist.");
                    std::process::exit(2);
                }
                rest::flow_run_cmd(script)
            }
            _ => unreachable!("subcommand required"),
        },
        Some(("pr", m)) => match m.subcommand() {
            Some(("set", m)) => rest::pr_set_cmd(
                *m.get_one::<i64>("number").expect("required"),
                m.get_flag("plain"),
            ),
            Some(("clear", m)) => rest::pr_clear_cmd(m.get_flag("plain")),
            _ => unreachable!("subcommand required"),
        },
        Some(("view", m)) => core_cmds::view_cmd(arg_str(m, "session_id")),
        Some(("attach", m)) => rest::attach_cmd(arg_str(m, "team_name")),
        Some(("ls", m)) => core_cmds::ls_cmd(m.get_flag("plain")),
        Some(("send", m)) => core_cmds::send(
            arg_str(m, "to_agent"),
            arg_str(m, "body"),
            arg_str(m, "artifact"),
        ),
        Some(("thread", m)) => rest::thread(arg_str(m, "message_id")),
        Some(("doctor", m)) => core_cmds::doctor(arg_str(m, "agent_name")),
        Some(("capture", m)) => rest::capture(
            arg_str(m, "member_name"),
            *m.get_one::<i64>("lines").unwrap_or(&30),
        ),
        Some(("interrupt", m)) => core_cmds::interrupt(arg_str(m, "agent_name")),
        Some(("kill", m)) => core_cmds::kill(arg_str(m, "agent_name")),
        Some(("notify", m)) => rest::notify_cmd(arg_str(m, "message")),
        Some(("plugin", m)) => match m.subcommand() {
            Some(("list", m)) => rest::plugin_list(m.get_flag("plain")),
            Some(("ls", m)) => rest::plugin_ls(m.get_flag("plain")),
            Some(("enable", m)) => rest::plugin_enable(arg_str(m, "name"), m.get_flag("plain")),
            Some(("disable", m)) => rest::plugin_disable(arg_str(m, "name"), m.get_flag("plain")),
            Some(("path", _)) => rest::plugin_path(),
            _ => unreachable!("subcommand required"),
        },
        Some(("ccd", m)) => match m.subcommand() {
            Some(("ls", _)) => rest::ccd_ls_cmd(),
            _ => unreachable!("subcommand required"),
        },
        Some(("resume-hint", m)) => rest::resume_hint_cmd(arg_str(m, "cli_name")),
        Some(("shell-init", m)) => rest::shell_init_cmd(arg_str(m, "shell")),
        Some(("worktree", m)) => match m.subcommand() {
            Some(("set-base", m)) => {
                rest::worktree_set_base_cmd(arg_str(m, "ref"), m.get_flag("plain"))
            }
            Some(("start", m)) => rest::worktree_start_cmd(
                arg_str(m, "feature"),
                m.get_one::<String>("base_ref").map(String::as_str),
                m.get_flag("plain"),
            ),
            Some(("done", m)) => rest::worktree_done_cmd(
                arg_str(m, "feature"),
                m.get_flag("force"),
                m.get_flag("plain"),
            ),
            Some(("status", m)) => rest::worktree_status_cmd(
                m.get_one::<String>("feature").map(String::as_str),
                m.get_flag("plain"),
            ),
            _ => unreachable!("subcommand required"),
        },
        _ => {
            print!("{}", help_text::help_for(&[]).expect("root help"));
            std::process::exit(0);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (ported from tests/unit — logic-level only)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_free_text_positionals_accept_hyphen_leading_values() {
        // A member's reply often starts with a markdown bullet ("- point");
        // click passed those through, clap needs allow_hyphen_values.
        for argv in [
            vec!["hive", "send", "orch", "- bullet reply"],
            vec!["hive", "inject", "orch", "- bullet text"],
            vec!["hive", "notify", "- check the pane"],
        ] {
            build_cli()
                .try_get_matches_from(argv.clone())
                .unwrap_or_else(|e| panic!("{argv:?} rejected: {e}"));
        }
    }

    #[test]
    fn test_command_tree_declares_every_python_command() {
        let cli = build_cli();
        for name in _KNOWN_COMMANDS {
            assert!(
                cli.find_subcommand(name).is_some(),
                "missing command {name}"
            );
        }
    }

    #[test]
    fn test_render_root_help_sections_present() {
        let help = help_text::help_for(&[]).unwrap();
        for section in [
            "Daily:",
            "Panes:",
            "Workflow:",
            "Team:",
            "Human Helpers:",
            "Debug:",
            "Extensions:",
            "Launchers:",
            "Examples:",
        ] {
            assert!(help.contains(section), "missing section {section}");
        }
        assert!(!help.contains("resume-hint"), "hidden command leaked");
    }
}
