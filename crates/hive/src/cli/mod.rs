//! CLI entry point for hive: the clap command tree for the whole surface,
//! `pub fn main()`, the root gates every subcommand passes (tmux, codex
//! native), the help interception, and the dispatch into one module per
//! domain — `team`, `member`, `attach`, `fork`, `flow`, `launch`, `setup`,
//! `worktree`. The handlers print and exit; the logic they call lives in
//! the crate (`team`, `naming`, `send`, `identity`, `team_display`).

mod attach;
mod flow;
mod fork;
pub mod help_text;
mod launch;
mod member;
mod setup;
mod team;
mod util;
mod worktree;

use std::path::Path;

use clap::{Arg, ArgAction, ArgMatches, Command};

use crate::identity;
use util::fail;

const TMUX_REQUIRED_MESSAGE: &str = "Hive requires tmux. Start or attach to a tmux session first.";

/// Refusal for an engine whose own session id names no roster row. Told
/// apart from `TMUX_REQUIRED_MESSAGE` because the caller has no terminal to
/// go find: it is an engine subprocess, and its identity is the broken part.
const UNROSTERED_ENGINE_MESSAGE: &str = "this engine's session names nobody on any team's roster \
     (the member was killed, or the team deleted)";

// Verbs that never need a tmux context — plus the team verbs, which read the
// registry (the truth layer) and address the team's window by id, so a
// caller outside tmux or in another session reaches it the same way. `flow`
// rides the same doctrine, and `flow node --team` exists for callers without
// a pane identity (a workflow proxy subagent, a desktop session).
const TMUX_OPTIONAL_ROOT_COMMANDS: &[&str] = &[
    "plugin",
    "config",
    "shell-init",
    "codex",
    "claude",
    "grok",
    "resume-hint",
    "worktree",
    "ls",
    "ccd",
    "create",
    "join",
    "spawn",
    "team",
    "kill",
    "delete",
    "attach",
    "view",
    "flow",
];

const CODEX_NATIVE_REQUIRED_BYPASS_COMMANDS: &[&str] = &[
    "claude",
    "codex",
    "config",
    "doctor",
    "grok",
    "inject",
    "plugin",
    "resume-hint",
    "shell-init",
];

// ---------------------------------------------------------------------------
// Command tree
// ---------------------------------------------------------------------------

fn passthrough_command(name: &'static str, about: &'static str) -> Command {
    Command::new(name).about(about).disable_help_flag(true).arg(
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
}

pub(crate) fn build_cli() -> Command {
    Command::new("hive")
        .about("Hive - tmux-first multi-agent collaboration runtime.")
        .version(env!("CARGO_PKG_VERSION"))
        .disable_version_flag(true)
        .disable_help_subcommand(true)
        .subcommand(
            Command::new("fork")
                .about("Fork the current agent session into a new split pane.")
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
                        .help("Workspace path to initialize (default: the team dir)"),
                )
                .arg(
                    Arg::new("reset_workspace")
                        .long("reset-workspace")
                        .action(ArgAction::SetTrue)
                        .help("Wipe an existing --workspace before initialization"),
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
                    Arg::new("delete_workspace")
                        .long("delete-workspace")
                        .action(ArgAction::SetTrue)
                        .help("Also delete the workspace directory"),
                ),
        )
        .subcommand(
            Command::new("spawn")
                .about("Spawn an agent pane, optionally dispatching a task atomically.")
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
                .arg(Arg::new("agent_name").required(true))
                .arg(Arg::new("text").required(true).allow_hyphen_values(true)),
        )
        .subcommand(
            Command::new("compact")
                .about("Trigger /compact on your own pane.")
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
                .about("Plan the team window's layout, or apply a tmux preset over it.")
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
                )
                .arg(
                    Arg::new("on_change")
                        .long("on-change")
                        .action(ArgAction::SetTrue)
                        .help("Hook form: apply only when the plan changed (silent)"),
                )
                .arg(
                    Arg::new("window")
                        .long("window")
                        .value_name("TARGET")
                        .default_value("")
                        .help("The team window (default: the caller's)"),
                ),
        )
        .subcommand(
            Command::new("mirror")
                .about("Show or hide the team's read-only orch mirror pane.")
                .arg(
                    Arg::new("mode")
                        .num_args(0..=1)
                        .value_parser(["on", "off"]),
                )
                .arg(
                    Arg::new("window")
                        .long("window")
                        .value_name("TARGET")
                        .help("The team window (default: the caller's)"),
                ),
        )
        .subcommand(
            Command::new("flow")
                .about("Deterministic member orchestration over live panes.")
                .subcommand_required(true)
                .arg_required_else_help(true)
                .subcommand(
                    Command::new("run")
                        .about("Run SCRIPT against the current team.")
                        .arg(Arg::new("script").required(true))
                        .arg(
                            Arg::new("resume")
                                .long("resume")
                                .value_name("RUN_ID")
                                .help("Resume a previous run from its journal"),
                        ),
                )
                .subcommand(
                    Command::new("board")
                        .about("Live progress board for the team's flow nodes (run it in a pane).")
                        .arg(Arg::new("team").long("team")),
                )
                .subcommand(
                    Command::new("node")
                        .about("One task on one live member, as a single blocking call.")
                        .subcommand_required(true)
                        .arg_required_else_help(true)
                        .subcommand(
                            Command::new("run")
                                .about("Place the task on stdin onto member NAME and block for its reply.")
                                .arg(Arg::new("name").long("name").required(true))
                                .arg(Arg::new("cli").long("cli"))
                                .arg(Arg::new("model").long("model"))
                                .arg(
                                    Arg::new("phase")
                                        .long("phase")
                                        .help("Phase label; lands on the pane group for `hive flow board`"),
                                )
                                .arg(Arg::new("team").long("team")),
                        ),
                )
                .subcommand(
                    Command::new("rig")
                        .about("Create (or tear down) a workflow team: tmux session, team, board.")
                        .arg(Arg::new("run").required(true))
                        .arg(Arg::new("orch").long("orch").value_name("SESSION_ID"))
                        .arg(Arg::new("workspace").long("workspace").value_name("DIR"))
                        .arg(
                            Arg::new("down")
                                .long("down")
                                .action(clap::ArgAction::SetTrue),
                        ),
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
                .about("Jump to a team's tmux window, rebuilding it first when it is gone.")
                .arg(Arg::new("team_name").required(true)),
        )
        .subcommand(json_default_options(
            Command::new("ls")
                .about("List hive teams from the registry, with their display state."),
        ))
        .subcommand(
            Command::new("send")
                .about("Send a message to another agent — the only message verb.")
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
                .arg(Arg::new("message_id").required(true)),
        )
        .subcommand(
            Command::new("doctor")
                .about("Diagnose agent connectivity and session state.")
                .arg(Arg::new("agent_name").default_value("")),
        )
        .subcommand(
            Command::new("capture")
                .about("Debug: capture raw pane output from a team member's pane.")
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
                .arg(Arg::new("agent_name").required(true)),
        )
        .subcommand(
            Command::new("kill")
                .about("Kill an agent pane and remove it from the team.")
                .arg(Arg::new("agent_name").required(true))
                .arg(
                    Arg::new("team_arg")
                        .long("team")
                        .short('t')
                        .default_value("")
                        .help("Explicit team (default: the pane's binding)"),
                ),
        )
        .subcommand(passthrough_command(
            "cvim",
            "Human-only: edit the last assistant message in vim, send it back.",
        ))
        .subcommand(passthrough_command(
            "vim",
            "Human-only: compose in a blank vim buffer, send it to the agent pane.",
        ))
        .subcommand(passthrough_command(
            "vfork",
            "Human-only: fork the current Hive session into a vertical split.",
        ))
        .subcommand(passthrough_command(
            "hfork",
            "Human-only: fork the current Hive session into a horizontal split.",
        ))
        .subcommand(
            Command::new("notify")
                .about("Notify the user for the current pane.")
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
                .subcommand(Command::new("sync").about(
                    "Materialize the embedded plugin marketplace and print the payload \
                     directory (the command source Claude re-runs each session).",
                ))
                .subcommand(Command::new("setup").about(
                    "One-time install: sync the marketplace, then register and install \
                     the hive plugin for claude and codex on PATH.",
                )),
        )
        .subcommand(passthrough_command(
            "codex",
            "Launch codex on the shared app-server daemon (hive-managed).",
        ))
        .subcommand(passthrough_command(
            "claude",
            "Launch claude as a hive-managed background job (hclaude launcher).",
        ))
        .subcommand(passthrough_command(
            "grok",
            "Launch grok attached to the pane's leader daemon (hive-managed).",
        ))
        .subcommand(
            Command::new("ccd")
                .about("Discover Claude Code sessions outside the team — the desktop app, another terminal — by their cross-session inbox registry.")
                .subcommand_required(true)
                .arg_required_else_help(true)
                .subcommand(
                    Command::new("ls")
                        .about("List the Claude Code sessions `hive send ccd.<name>` can reach."),
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
                .arg(Arg::new("shell").default_value("")),
        )
        .subcommand(
            Command::new("worktree")
                .about("Per-feature worktree pool: start a feature, finish it, inspect state.")
                .subcommand_required(true)
                .arg_required_else_help(true)
                .subcommand(json_default_options(
                    Command::new("set-base")
                        .about("Declare the team's integration branch (the base of every sub-PR).")
                        .arg(Arg::new("ref").required(true)),
                ))
                .subcommand(json_default_options(
                    Command::new("start")
                        .about("Create (or re-attach) the worktree for FEATURE and print its path as JSON.")
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

const KNOWN_COMMANDS: &[&str] = &[
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
    "mirror",
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

/// Click groups, by command path, and their subcommands (help lookup +
/// bare-group help). Every path here and every path + sub has a `help_text`
/// entry; `test_every_help_path_has_help_text` holds that line.
const HELP_GROUPS: &[(&[&str], &[&str])] = &[
    (&["ccd"], &["ls"]),
    (&["config"], &["get", "set", "unset"]),
    (&["flow"], &["board", "node", "rig", "run"]),
    (&["flow", "node"], &["run"]),
    (
        &["plugin"],
        &["disable", "enable", "list", "ls", "setup", "sync"],
    ),
    (&["pr"], &["clear", "set"]),
    (&["worktree"], &["done", "set-base", "start", "status"]),
];

fn group_subs(path: &[&str]) -> Option<&'static [&'static str]> {
    HELP_GROUPS
        .iter()
        .find(|(group, _)| *group == path)
        .map(|(_, subs)| *subs)
}

/// Click help interception: the command path whose help click would print.
///
/// Click's `--help` (and `-h` outside the cvim family, whose
/// `help_option_names` is `["--help"]`) is an eager option: it prints help
/// and exits 0 wherever it appears among the parsed args (never past `--`).
/// A group prints its own help unless a known subcommand appears first.
/// Only a known command has a help arm. An unknown token never reaches this
/// function (the known-command gate in `main_with_argv` rejects it first); a
/// dash-first one (`hive --bogus -h`, `hive -- flow -h`) is left to clap,
/// which rejects it with exit 2 like any other unexpected argument.
fn help_path<'a>(invoked: &'a str, tail: &'a [String]) -> Option<Vec<&'a str>> {
    if !KNOWN_COMMANDS.contains(&invoked) {
        return None;
    }
    if matches!(invoked, "claude" | "codex" | "grok") {
        return None; // launchers forward all args to the wrapped CLI
    }
    let help_opts: &[&str] = if matches!(invoked, "cvim" | "vim" | "vfork" | "hfork") {
        &["--help"]
    } else {
        &["-h", "--help"]
    };
    let mut path = vec![invoked];
    for tok in tail {
        if tok == "--" {
            return None;
        }
        if help_opts.contains(&tok.as_str()) {
            return Some(path);
        }
        match group_subs(&path) {
            Some(subs) if subs.contains(&tok.as_str()) => path.push(tok.as_str()),
            // a non-sub token on a group stops the scan; click's own parse
            // error for it (out of the equivalence corpus) falls to clap.
            Some(_) => return None,
            None => {}
        }
    }
    None
}

/// Click's `no_args_is_help` on a group: the group path *tail* stops on
/// (`hive flow`, `hive flow node`), or None when it reaches a leaf command.
fn bare_group_path<'a>(invoked: &'a str, tail: &'a [String]) -> Option<Vec<&'a str>> {
    let mut path = vec![invoked];
    for tok in tail {
        if !group_subs(&path)?.contains(&tok.as_str()) {
            return None;
        }
        path.push(tok.as_str());
    }
    group_subs(&path).map(|_| path)
}

fn arg_str<'a>(m: &'a ArgMatches, key: &str) -> &'a str {
    m.get_one::<String>(key).map(String::as_str).unwrap_or("")
}

fn arg_vec(m: &ArgMatches, key: &str) -> Vec<String> {
    m.get_many::<String>(key)
        .map(|values| values.cloned().collect())
        .unwrap_or_default()
}

/// Why a no-tmux call is refused, or None when it is admitted.
///
/// Outside tmux only `send` has identity lanes, and every one of them is the
/// engine's own minted session: a Claude session sending into hive as a
/// guest (its messaging socket), a codex member's tool (its thread keys its
/// roster row) or a grok member's tool (its leader's session id keys one).
/// An engine whose session names nobody on the roster is told so by
/// name — that is the shape a killed member's leftover subprocess arrives
/// in, and the tmux line would send it hunting for a terminal it is never
/// going to have.
fn no_tmux_refusal(invoked: &str) -> Option<&'static str> {
    if TMUX_OPTIONAL_ROOT_COMMANDS.contains(&invoked) || identity::is_inside_tmux() {
        return None;
    }
    if invoked != "send" {
        return Some(TMUX_REQUIRED_MESSAGE);
    }
    if crate::adapters::claude_sessions::self_session().is_some()
        || !identity::session_member_binding().is_empty()
    {
        return None;
    }
    if identity::engine_marker_env() {
        Some(UNROSTERED_ENGINE_MESSAGE)
    } else {
        Some(TMUX_REQUIRED_MESSAGE)
    }
}

// ---------------------------------------------------------------------------
// Codex-native gate
// ---------------------------------------------------------------------------

fn codex_relaunch_message() -> String {
    "this codex isn't hive-managed — hive runtime is degraded.\n\
     for future launches use hcodex (one-time setup, any shell):\n  \
     grep -q 'hive shell-init' ~/.zshrc || \
     echo 'eval \"$(hive shell-init zsh)\"' >> ~/.zshrc\n\
     then exit this codex (Ctrl-C twice) and run: hive codex resume"
        .to_string()
}

fn require_codex_native(invoked: Option<&str>) {
    if let Some(invoked) = invoked {
        if CODEX_NATIVE_REQUIRED_BYPASS_COMMANDS.contains(&invoked) {
            return;
        }
    }
    if !identity::is_codex_tool_env() || identity::current_codex_thread_is_hive_managed() {
        return;
    }
    fail(&codex_relaunch_message());
}

/// Root-group gates, run before any subcommand.
fn run_root_gates(invoked: &str) {
    require_codex_native(Some(invoked));
    if let Some(message) = no_tmux_refusal(invoked) {
        fail(message);
    }
}

pub fn main() {
    main_with_argv(std::env::args().collect());
}

/// The whole command-line path on an explicit argv (`argv[0]` is the
/// program name, skipped). Every early exit calls `std::process::exit`;
/// only the launcher passthroughs and a dispatched handler return.
fn main_with_argv(argv: Vec<String>) {
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
            println!("hive, version {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }
        _ => {}
    }

    let invoked = args[0].clone();
    let tail: Vec<String> = args.iter().skip(1).cloned().collect();

    // Hidden helper subcommands, never listed in help: the cvim toolkit
    // (called back into by the materialized `cvim-command` bash asset) via
    // $HIVE_BIN. They dispatch before the known-command gate and skip the
    // root gates.
    match invoked.as_str() {
        "cvim-sendback" => std::process::exit(crate::cvim::sendback_main(&tail)),
        "cvim-payload" => std::process::exit(crate::cvim::payload_main(&tail)),
        "cvim-list" => std::process::exit(crate::cvim::list_main(&tail)),
        "cvim-seed" => std::process::exit(crate::cvim::seed_main(&tail)),
        "cvim-session" => std::process::exit(crate::cvim::session_main(&tail)),
        "cvim-profile" => std::process::exit(crate::cvim::profile_main(&tail)),
        // the after-select-window hook's callback
        "notify-hook" => std::process::exit(crate::notify_ui::main(&tail)),
        _ => {}
    }

    // Click resolves the subcommand before the group callback runs, so an
    // unknown command errors before any tmux/codex gate fires.
    if !KNOWN_COMMANDS.contains(&invoked.as_str()) && !invoked.starts_with('-') {
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
    if KNOWN_COMMANDS.contains(&invoked.as_str()) && !help_requested {
        run_root_gates(&invoked);
    }

    // Click group with no subcommand: `no_args_is_help` — stderr, exit 2.
    if let Some(path) = bare_group_path(&invoked, &tail) {
        eprint!("{}", help_text::help_for(&path).expect("group help"));
        std::process::exit(2);
    }

    // Launcher / human-helper passthrough: everything after the subcommand is
    // forwarded verbatim (Click's ignore_unknown_options + UNPROCESSED args).
    match invoked.as_str() {
        "codex" => {
            launch::codex_cmd(&tail);
            return;
        }
        "claude" => {
            launch::claude_cmd(&tail);
            return;
        }
        "grok" => {
            launch::grok_cmd(&tail);
            return;
        }
        "cvim" => {
            fork::cvim_cmd(&tail);
            return;
        }
        "vim" => {
            fork::vim_cmd(&tail);
            return;
        }
        "vfork" => {
            fork::vfork_cmd(&tail);
            return;
        }
        "hfork" => {
            fork::hfork_cmd(&tail);
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
        Some(("fork", m)) => fork::fork_cmd(
            arg_str(m, "pane_id"),
            arg_str(m, "split"),
            arg_str(m, "join_as"),
            arg_str(m, "prompt"),
        ),
        Some(("join", m)) => team::join_cmd(
            arg_str(m, "team_arg"),
            arg_str(m, "name_override"),
            arg_str(m, "pane_override"),
            !m.get_flag("no_notify"),
            arg_str(m, "group_name"),
        ),
        Some(("create", m)) => team::create(
            arg_str(m, "name"),
            arg_str(m, "desc"),
            arg_str(m, "workspace"),
            m.get_flag("reset_workspace"),
            &arg_vec(m, "state_entries"),
        ),
        Some(("delete", m)) => team::delete(
            arg_str(m, "name"),
            arg_str(m, "workspace"),
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
            member::spawn(
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
            Some(("get", m)) => setup::config_get(arg_str(m, "key")),
            Some(("set", m)) => setup::config_set(arg_str(m, "key"), arg_str(m, "value")),
            Some(("unset", m)) => setup::config_unset(arg_str(m, "key")),
            _ => unreachable!("subcommand required"),
        },
        Some(("inject", m)) => member::inject_cmd(arg_str(m, "agent_name"), arg_str(m, "text")),
        Some(("compact", m)) => member::compact_cmd(arg_str(m, "pane_id")),
        Some(("team", m)) => team::team_cmd(arg_str(m, "team_arg")),
        Some(("layout", m)) => attach::layout_cmd(
            &arg_str(m, "preset").to_lowercase(),
            m.get_flag("on_change"),
            arg_str(m, "window"),
        ),
        Some(("mirror", m)) => attach::mirror_cmd(arg_str(m, "mode"), arg_str(m, "window")),
        Some(("flow", m)) => match m.subcommand() {
            Some(("run", m)) => {
                let script = arg_str(m, "script");
                if !Path::new(script).exists() {
                    eprintln!("Error: Invalid value for 'SCRIPT': Path '{script}' does not exist.");
                    std::process::exit(2);
                }
                flow::flow_run_cmd(script, m.get_one::<String>("resume").map(String::as_str))
            }
            Some(("board", m)) => std::process::exit(crate::flow_board::board_cmd(
                m.get_one::<String>("team").map(String::as_str),
            )),
            Some(("node", m)) => match m.subcommand() {
                Some(("run", m)) => flow::flow_node_run_cmd(
                    arg_str(m, "name"),
                    m.get_one::<String>("cli").map(String::as_str),
                    m.get_one::<String>("model")
                        .map(String::as_str)
                        .unwrap_or(""),
                    m.get_one::<String>("phase")
                        .map(String::as_str)
                        .unwrap_or(""),
                    m.get_one::<String>("team").map(String::as_str),
                ),
                _ => unreachable!("subcommand required"),
            },
            Some(("rig", m)) => std::process::exit(crate::flow_rig::rig_cmd(
                arg_str(m, "run"),
                m.get_one::<String>("orch").map(String::as_str),
                m.get_one::<String>("workspace").map(String::as_str),
                m.get_flag("down"),
            )),
            _ => unreachable!("subcommand required"),
        },
        Some(("pr", m)) => match m.subcommand() {
            Some(("set", m)) => worktree::pr_set_cmd(
                *m.get_one::<i64>("number").expect("required"),
                m.get_flag("plain"),
            ),
            Some(("clear", m)) => worktree::pr_clear_cmd(m.get_flag("plain")),
            _ => unreachable!("subcommand required"),
        },
        Some(("view", m)) => member::view_cmd(arg_str(m, "session_id")),
        Some(("attach", m)) => attach::attach_cmd(arg_str(m, "team_name")),
        Some(("ls", m)) => team::ls_cmd(m.get_flag("plain")),
        Some(("send", m)) => member::send(
            arg_str(m, "to_agent"),
            arg_str(m, "body"),
            arg_str(m, "artifact"),
        ),
        Some(("thread", m)) => member::thread(arg_str(m, "message_id")),
        Some(("doctor", m)) => team::doctor(arg_str(m, "agent_name")),
        Some(("capture", m)) => member::capture(
            arg_str(m, "member_name"),
            *m.get_one::<i64>("lines").unwrap_or(&30),
        ),
        Some(("interrupt", m)) => member::interrupt(arg_str(m, "agent_name")),
        Some(("kill", m)) => member::kill(arg_str(m, "agent_name"), arg_str(m, "team_arg")),
        Some(("notify", m)) => setup::notify_cmd(arg_str(m, "message")),
        Some(("plugin", m)) => match m.subcommand() {
            Some(("list", m)) => setup::plugin_list(m.get_flag("plain")),
            Some(("ls", m)) => setup::plugin_ls(m.get_flag("plain")),
            Some(("enable", m)) => setup::plugin_enable(arg_str(m, "name"), m.get_flag("plain")),
            Some(("disable", m)) => setup::plugin_disable(arg_str(m, "name"), m.get_flag("plain")),
            Some(("sync", _)) => setup::plugin_sync(),
            Some(("setup", _)) => setup::plugin_setup(),
            _ => unreachable!("subcommand required"),
        },
        Some(("ccd", m)) => match m.subcommand() {
            Some(("ls", _)) => launch::ccd_ls_cmd(),
            _ => unreachable!("subcommand required"),
        },
        Some(("resume-hint", m)) => launch::resume_hint_cmd(arg_str(m, "cli_name")),
        Some(("shell-init", m)) => setup::shell_init_cmd(arg_str(m, "shell")),
        Some(("worktree", m)) => match m.subcommand() {
            Some(("set-base", m)) => {
                worktree::worktree_set_base_cmd(arg_str(m, "ref"), m.get_flag("plain"))
            }
            Some(("start", m)) => worktree::worktree_start_cmd(
                arg_str(m, "feature"),
                m.get_one::<String>("base_ref").map(String::as_str),
                m.get_flag("plain"),
            ),
            Some(("done", m)) => worktree::worktree_done_cmd(
                arg_str(m, "feature"),
                m.get_flag("force"),
                m.get_flag("plain"),
            ),
            Some(("status", m)) => worktree::worktree_status_cmd(
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
    use crate::testenv::EnvGuard;

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
    fn test_command_tree_declares_every_known_command() {
        let cli = build_cli();
        for name in KNOWN_COMMANDS {
            assert!(
                cli.find_subcommand(name).is_some(),
                "missing command {name}"
            );
        }
    }

    /// Every path `help_path`/`bare_group_path` can produce — a
    /// `KNOWN_COMMANDS` entry, a `HELP_GROUPS` group, or a group plus one
    /// of its subs — must resolve, or `-h` on it panics instead of
    /// printing. Tokens outside that table never become a path.
    #[test]
    fn test_every_help_path_has_help_text() {
        let cli = build_cli();
        let tail = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(help_path("--bogus", &tail(&["-h"])), None);
        assert_eq!(help_path("--", &tail(&["flow", "-h"])), None);
        assert_eq!(help_path("bogus", &tail(&["--help"])), None);
        assert_eq!(bare_group_path("--bogus", &tail(&[])), None);
        for name in KNOWN_COMMANDS {
            if matches!(*name, "claude" | "codex" | "grok") {
                continue; // launchers forward -h to the wrapped CLI
            }
            assert!(help_text::help_for(&[name]).is_some(), "no help for {name}");
        }
        for (group, subs) in HELP_GROUPS {
            assert!(
                help_text::help_for(group).is_some(),
                "no help for {group:?}"
            );
            let mut cmd = &cli;
            for seg in *group {
                cmd = cmd
                    .find_subcommand(seg)
                    .unwrap_or_else(|| panic!("{group:?} not in tree"));
            }
            for sub in *subs {
                let path: Vec<&str> = group.iter().copied().chain([*sub]).collect();
                assert!(help_text::help_for(&path).is_some(), "no help for {path:?}");
                assert!(cmd.find_subcommand(sub).is_some(), "{path:?} not in tree");
            }
        }
    }

    #[test]
    fn test_help_path_walks_nested_groups() {
        let tail = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            help_path("flow", &tail(&["node", "run", "--name", "x", "--help"])),
            Some(vec!["flow", "node", "run"])
        );
        assert_eq!(
            help_path("flow", &tail(&["node", "-h"])),
            Some(vec!["flow", "node"])
        );
        assert_eq!(
            help_path("flow", &tail(&["-h", "node"])),
            Some(vec!["flow"])
        );
        assert_eq!(help_path("flow", &tail(&["bogus", "-h"])), None);
        assert_eq!(help_path("flow", &tail(&["run", "--", "-h"])), None);
        assert_eq!(
            help_path("plugin", &tail(&["sync", "--help"])),
            Some(vec!["plugin", "sync"])
        );
        assert_eq!(help_path("codex", &tail(&["--help"])), None);
        assert_eq!(
            bare_group_path("flow", &tail(&["node"])),
            Some(vec!["flow", "node"])
        );
        assert_eq!(bare_group_path("flow", &tail(&[])), Some(vec!["flow"]));
        assert_eq!(bare_group_path("flow", &tail(&["run"])), None);
        assert_eq!(bare_group_path("send", &tail(&[])), None);
    }

    /// A headless engine subprocess: no tmux, no socket, no engine session.
    fn headless_gate_env(tmp: &std::path::Path) -> EnvGuard {
        let mut env = EnvGuard::cleared(&crate::testenv::IDENTITY_VARS);
        env.set("HIVE_HOME", tmp.join(".hive"));
        env.set("CLAUDE_CONFIG_DIR", tmp.join(".claude"));
        env
    }

    #[test]
    fn test_no_tmux_refusal_admits_a_rostered_grok_session_and_names_a_stale_one() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut env = headless_gate_env(tmp.path());
        let mut member = serde_json::Map::new();
        member.insert("name".to_string(), serde_json::Value::String("bee".into()));
        member.insert("cli".to_string(), serde_json::Value::String("grok".into()));
        member.insert(
            "sessionId".to_string(),
            serde_json::Value::String("s-bee".into()),
        );
        crate::registry::record_team("hornet", "/tmp/ws-hn", "1.0", &[member], "").unwrap();

        // no identity at all: the generic tmux refusal
        assert_eq!(no_tmux_refusal("send"), Some(TMUX_REQUIRED_MESSAGE));

        env.set("GROK_SESSION_ID", "s-bee");
        assert_eq!(no_tmux_refusal("send"), None);

        // the leader's env outlived the member it names
        env.set("GROK_SESSION_ID", "s-ant");
        assert_eq!(no_tmux_refusal("send"), Some(UNROSTERED_ENGINE_MESSAGE));

        // the session lane is a send lane only; other verbs still need tmux
        env.set("GROK_SESSION_ID", "s-bee");
        assert_eq!(no_tmux_refusal("interrupt"), Some(TMUX_REQUIRED_MESSAGE));
        // ... and the tmux-optional verbs never reach the gate
        assert_eq!(no_tmux_refusal("config"), None);
        // `mirror` moves panes on the server: tmux-only (a run-shell job
        // carries TMUX)
        assert_eq!(no_tmux_refusal("mirror"), Some(TMUX_REQUIRED_MESSAGE));
    }

    /// Root help lists a command exactly when its clap node is not hidden:
    /// every `KNOWN_COMMANDS` entry has a `  <name>  <about>` line unless
    /// `.hide(true)` marks it (resume-hint today; `plugin ls` is hidden too
    /// but is a subcommand, outside the root table), and no hidden one leaks.
    #[test]
    fn test_root_help_lists_every_visible_command_and_no_hidden_one() {
        let help = help_text::help_for(&[]).unwrap();
        let listed: std::collections::HashSet<&str> = help
            .lines()
            .filter_map(|line| {
                let body = line.strip_prefix("  ")?;
                if body.starts_with(' ') || body.starts_with('-') {
                    return None; // wrapped description / an option row
                }
                let (name, rest) = body.split_once(' ')?;
                rest.starts_with(' ').then_some(name)
            })
            .collect();
        let cli = build_cli();
        for name in KNOWN_COMMANDS {
            let hidden = cli.find_subcommand(name).unwrap().is_hide_set();
            assert_eq!(
                listed.contains(name),
                !hidden,
                "{name}: hidden={hidden}, listed={}",
                listed.contains(name)
            );
        }
    }

    // --- exit-code lane: the binary's own argv path in a child process ---

    const CHILD_ENTRY: &str = "cli::tests::test_child_entry_runs_hive_argv_from_env";

    /// Child half of `run_hive_child`: a no-op in the normal run. When the
    /// parent re-executes this test binary with `HIVE_TEST_CHILD_ARGV`
    /// (JSON argv), it runs `main_with_argv` on it and exits the way `hive`
    /// would; stdout/stderr are redirected to the files named in
    /// `HIVE_TEST_CHILD_STDOUT` / `HIVE_TEST_CHILD_STDERR` first, so the
    /// harness's own preamble never reaches the oracle.
    #[test]
    fn test_child_entry_runs_hive_argv_from_env() {
        let Ok(argv) = std::env::var("HIVE_TEST_CHILD_ARGV") else {
            return;
        };
        let argv: Vec<String> = serde_json::from_str(&argv).expect("child argv is a JSON array");
        use std::io::Write;
        use std::os::unix::io::AsRawFd;
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        for (key, fd) in [("HIVE_TEST_CHILD_STDOUT", 1), ("HIVE_TEST_CHILD_STDERR", 2)] {
            let file = std::fs::File::create(std::env::var(key).expect(key)).expect(key);
            assert_eq!(unsafe { libc::dup2(file.as_raw_fd(), fd) }, fd);
            std::mem::forget(file);
        }
        main_with_argv(argv);
        std::process::exit(0);
    }

    /// `hive <args>` as the binary runs it, in a child process: (exit code,
    /// stdout, stderr). The child sees a throwaway `HIVE_HOME`, no engine
    /// identity and no pane; `inside_tmux` sets `TMUX` so the root gates
    /// (env checks only) admit tmux-only commands. No tmux double runs in
    /// the child, so drive only invocations that exit before a tmux call.
    fn run_hive_child(args: &[&str], inside_tmux: bool) -> (i32, String, String) {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("stdout");
        let err = tmp.path().join("stderr");
        let mut argv = vec!["hive".to_string()];
        argv.extend(args.iter().map(|s| s.to_string()));
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args([CHILD_ENTRY, "--exact", "--nocapture", "--test-threads=1"])
            .env(
                "HIVE_TEST_CHILD_ARGV",
                serde_json::to_string(&argv).unwrap(),
            )
            .env("HIVE_TEST_CHILD_STDOUT", &out)
            .env("HIVE_TEST_CHILD_STDERR", &err)
            .env("HIVE_HOME", tmp.path().join(".hive"))
            .env("CLAUDE_CONFIG_DIR", tmp.path().join(".claude"))
            .env_remove("TMUX_PANE")
            .env_remove("CODEX_THREAD_ID")
            .env_remove("GROK_SESSION_ID")
            .env_remove("CLAUDE_CODE_MESSAGING_SOCKET")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if inside_tmux {
            cmd.env("TMUX", "/tmp/hive-test-tmux,1,0");
        } else {
            cmd.env_remove("TMUX");
        }
        let status = cmd.status().expect("child test binary runs");
        let read = |p: &std::path::Path| std::fs::read_to_string(p).unwrap_or_default();
        (status.code().unwrap_or(-1), read(&out), read(&err))
    }

    #[test]
    fn test_help_on_an_unknown_or_dash_first_token_is_a_clap_usage_error() {
        // Reviewer case: these used to reach `help_for(...).expect(...)` and
        // panic (exit 101); now clap rejects the token like `hive -x` does.
        for (args, token) in [
            (&["--bogus", "-h"][..], "--bogus"),
            (&["--", "flow", "-h"][..], "flow"),
        ] {
            let (code, out, err) = run_hive_child(args, false);
            assert_eq!(code, 2, "{args:?}: stderr {err}");
            assert_eq!(out, "", "{args:?} printed help");
            assert!(
                err.contains("unexpected argument") && err.contains(token),
                "{args:?}: {err}"
            );
        }
        let (code, out, err) = run_hive_child(&["bogus", "-h"], false);
        assert_eq!(code, 2);
        assert_eq!(out, "");
        assert!(err.contains("No such command 'bogus'"), "{err}");
    }

    #[test]
    fn test_help_on_a_known_path_prints_its_text_and_exits_0() {
        let (code, out, err) = run_hive_child(&["plugin", "sync", "--help"], false);
        assert_eq!(code, 0, "{err}");
        assert_eq!(out, help_text::help_for(&["plugin", "sync"]).unwrap());
        assert_eq!(err, "");
    }

    #[test]
    fn test_spawn_task_path_is_validated_before_the_handler_runs() {
        // No team exists under the child's HIVE_HOME, so a handler that ran
        // would fail its team resolve (exit 1); exit 2 with click's message
        // proves the parse-time check fired first.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("task-dir");
        std::fs::create_dir(&dir).unwrap();
        let dir = dir.to_string_lossy().into_owned();
        let (code, out, err) = run_hive_child(&["spawn", "--task", &dir, "bee"], true);
        assert_eq!(code, 2, "{err}");
        assert_eq!(out, "");
        assert_eq!(
            err,
            format!("Error: Invalid value for '--task': Path '{dir}' is a directory.\n")
        );
        let missing = tmp.path().join("absent.md").to_string_lossy().into_owned();
        let (code, _, err) = run_hive_child(&["spawn", "--task", &missing, "bee"], true);
        assert_eq!(code, 2, "{err}");
        assert_eq!(
            err,
            format!("Error: Invalid value for '--task': Path '{missing}' does not exist.\n")
        );
    }

    #[test]
    fn test_resume_hint_prints_nothing_and_exits_0_without_a_pane_identity() {
        // A wrapper calls this after every launch; with no TMUX_PANE there is
        // no member identity to hint for, and the wrapper must see silence.
        let (code, out, err) = run_hive_child(&["resume-hint", "claude"], true);
        assert_eq!(code, 0, "{err}");
        assert_eq!(out, "");
        assert_eq!(err, "");
    }
}
