//! Hand-maintained help text in click's layout: the one help surface hive
//! prints. `cli/mod.rs::help_path` intercepts every `-h`/`--help` before
//! clap parses, so clap's own help never reaches a user; keep each clap
//! command's `about` equal to its first line here. Adding a command or group
//! means adding its arm here and, for a group, listing it in
//! `cli/mod.rs::HELP_GROUPS`.

/// Exact click `--help` output for a command path (`[]` = root).
pub(crate) fn help_for(path: &[&str]) -> Option<&'static str> {
    Some(match path {
        [] => {
            r#"Usage: hive [OPTIONS] COMMAND [ARGS]...

  Hive - tmux-first multi-agent collaboration runtime.

Options:
  --version   Show the version and exit.
  -h, --help  Show this message and exit.

Daily:
  Core loop per turn: inspect context, talk to peers, pull the human in when
  blocked.

  ccd      Discover Claude Code sessions outside the team — the desktop app,
           another...
  compact  Trigger /compact on your own pane.
  notify   Notify the user for the current pane.
  send     Send a message to another agent — the only message verb.
  team     Show team overview.

Panes:
  Bring up another agent pane — a fresh spawn or a forked clone.

  fork   Fork the current agent session into a new split pane.
  spawn  Spawn an agent pane, optionally dispatching a task atomically.

Workflow:
  Higher-level flows on top of Hive: worktrees, PR anchors, team snapshots.

  attach    Jump to a team's tmux window, rebuilding it first when it is
            gone.
  ls        List hive teams from the registry, with their display state.
  node      One task on one live member, as a single blocking call.
  pr        Pin a PR number on the team window's status bar.
  view      Read-only viewer for a Claude session transcript (follows live).
  worktree  Per-feature worktree pool: start a feature, finish it, inspect
            state.

Team:
  Create, extend, and wire up the tmux team around the current window.

  create  Create a team.
  delete  Delete a team and clean up.
  join    Join a team.
  layout  Apply a tmux layout preset to the current team window.
  mirror  Show or hide the team's read-only orch mirror pane.

Human Helpers:
  Popup editor and split helpers for the human (not the model). In Claude Code
  / Codex, type `!hive cvim` via shell escape. Requires tmux >= 3.2.

  cvim   Human-only: edit the last assistant message in vim, send it back.
  hfork  Human-only: fork the current Hive session into a horizontal split.
  vfork  Human-only: fork the current Hive session into a vertical split.
  vim    Human-only: compose in a blank vim buffer, send it to the agent pane.

Debug:
  Troubleshoot delivery, runtime state, and low-level pane behavior. Not on
  the happy path.

  capture    Debug: capture raw pane output from a team member's pane.
  doctor     Diagnose agent connectivity and session state.
  inject     Debug: inject raw input into an agent pane.
  interrupt  Interrupt an agent's running turn.
  kill       Kill an agent pane and remove it from the team.

Extensions:
  Manage first-party Hive plugins (Claude Code, Codex).

  config  Read / write user-level settings (~/.hive/settings.json).
  plugin  Manage first-party Hive plugins.

Launchers:
  hive-managed launchers behind the `hcodex` / `hclaude` / `hgrok` shell
  functions from `hive shell-init`, rarely run by hand. All arguments are
  forwarded verbatim, so `hive claude --help` shows claude's own help, not
  this wrapper's.

  claude      Launch claude as a hive-managed background job (hclaude
              launcher).
  codex       Launch codex on the shared app-server daemon (hive-managed).
  grok        Launch grok attached to the pane's leader daemon (hive-managed):
              a team member's identity-keyed engine, else a pane-scoped one.
  shell-init  Print the `hcodex` / `hclaude` / `hgrok` launchers for your
              shell.

Examples:
  # Team lifecycle
  hive create                                  # make this pane the orch of a new team
  hive spawn explore --task /tmp/task.md       # spawn a member and dispatch its task atomically
  hive team                                    # members + runtime state (busy / inputState / turnPhase)

  # Messaging (root thread: body is a short summary, details go in --artifact)
  hive send dodo "review this diff" --artifact /tmp/diff.md
  hive send dodo "see report" --artifact - <<'EOF'
  # Findings
  - item
  EOF

  # Fork, spawn
  hive fork                                    # split the current pane into a clone
  hive spawn claude                            # bring up a new agent pane

  # Debug connectivity
  hive doctor dodo                             # probe a peer's connectivity
"#
        }
        ["attach"] => {
            r#"Usage: hive attach [OPTIONS] TEAM_NAME

  Jump to a team's tmux window, rebuilding it first when it is gone.

  The registry is the team's existence; this makes its display whole before
  jumping — a missing window is rebuilt (outside tmux: in a detached session
  named after the team; inside: in your session), and a member without a
  pane gets one riding its engine's own viewer (claude attach loop / codex
  thread resume / grok session resume; a joined interactive Claude session
  gets a read-only `hive view` mirror; `hive mirror` parks or restores
  it). Outside tmux this finishes by exec'ing `tmux attach`.

Options:
  -h, --help  Show this message and exit.
"#
        }
        ["capture"] => {
            r#"Usage: hive capture [OPTIONS] MEMBER_NAME

  Debug: capture raw pane output from a team member's pane.

  Prints the last N lines (default 30) of the member's tmux pane. Use to
  inspect what the agent actually sees when transcript parsing gives
  unexpected results.

  Example:
    hive capture dodo -n 80

Options:
  -n, --lines INTEGER
  -h, --help           Show this message and exit.
"#
        }
        ["ccd"] => {
            r#"Usage: hive ccd [OPTIONS] COMMAND [ARGS]...

  Discover Claude Code sessions outside the team — the desktop app, another
  terminal — by their cross-session inbox registry.

  `hive ccd ls` lists the reachable sessions; messaging one is plain `hive
  send ccd.<name>` (name, desktop title, or pid).

Options:
  -h, --help  Show this message and exit.

Commands:
  ls  List the Claude Code sessions `hive send ccd.<name>` can reach.
"#
        }
        ["compact"] => {
            r##"Usage: hive compact [OPTIONS]

  Trigger /compact on your own pane.

  Works on any agent pane, team-bound or not: a pane with no Hive team is
  compacted by its literal pane facts, and the response carries `member` = the
  pane id with `team: null`.

  When wired into a tmux key binding, pass `--pane "#{pane_id}"` so the
  triggering pane is captured by tmux at keypress time rather than read from
  the (potentially stale) TMUX_PANE env in a detached subprocess.

  Examples:
    hive compact
    hive compact --pane %21

Options:
  --pane TEXT  Target pane ID (default: current pane via TMUX_PANE)
  -h, --help   Show this message and exit.
"##
        }
        ["config"] => {
            r#"Usage: hive config [OPTIONS] COMMAND [ARGS]...

  Read / write user-level settings (~/.hive/settings.json).

Options:
  -h, --help  Show this message and exit.

Commands:
  get    Print the value at KEY (dot-path).
  set    Set KEY to VALUE (true/false/int/float/string).
  unset  Remove KEY.
"#
        }
        ["create"] => {
            r#"Usage: hive create [OPTIONS] [NAME]

  Create a team.

  NAME is optional everywhere (pool-picked by default). Outside tmux: a
  tmux session named after the team (created detached when missing) holds
  its window; a Claude session running the command becomes the orch,
  mirrored read-only in the first pane (`hive mirror`, the status bar's
  orch chip or `prefix+m` park and restore it; the team session gets
  hive's two-line status bar). Inside tmux on an agent pane: that pane
  becomes the orch. Inside tmux on a shell pane: the window binds the team
  without an orch.

  The workspace defaults to the team's own directory, $HIVE_HOME/teams/NAME/
  (beside its team.json; holds hive.db, run/, artifacts/), and is reset on
  every create — a pool name recycled after `hive delete` never inherits the
  old bus or event log. --workspace puts it elsewhere; an existing directory
  there is kept unless --reset-workspace wipes it.

Options:
  -d, --desc TEXT       Team description
  -w, --workspace TEXT  Workspace path to initialize (default: the team dir)
  --reset-workspace     Wipe an existing --workspace before initialization
  --state TEXT          Initial state KEY=VALUE (repeatable)
  -h, --help            Show this message and exit.
"#
        }
        ["cvim"] => {
            r#"Usage: hive cvim [OPTIONS] [ARGS]...

  Human-only: edit the last assistant message in vim, send it back.

  Opens a popup vim seeded with the previous assistant message and sends the
  edited result back to the agent pane. Intended to be typed by the human via
  the agent's shell escape (e.g. `!hive cvim`) in Claude Code or Codex. Not
  meant for the model to invoke on its own.

Options:
  --help  Show this message and exit.
"#
        }
        ["delete"] => {
            r#"Usage: hive delete [OPTIONS] NAME

  Delete a team and clean up.

  Removes the registry entry ($HIVE_HOME/teams/NAME/team.json), closes the
  window hive built, stops the hived. The team directory's bus, run/ and
  artifacts/ stay for reading until the name is recycled; --delete-workspace
  removes the whole team directory — or the external workspace the entry
  records, which is never removed without the flag.

  --down is the teardown of a workflow run (`hive create RUN`, `hive node
  run` nodes, `hive delete RUN --down`): every member is retired first, and
  the team's own tmux session — the one `hive create` built outside tmux,
  named after the team — is killed after, by its exact name, never a prefix
  match. Refuses when neither a team nor such a session exists.

Options:
  -w, --workspace TEXT  Workspace path to remove (default: the entry's)
  --delete-workspace    Also delete the workspace directory
  --down                Retire every member first and kill the team's tmux
                        session
  -h, --help            Show this message and exit.
"#
        }
        ["doctor"] => {
            r#"Usage: hive doctor [OPTIONS] [AGENT_NAME]

  Diagnose agent connectivity and session state.

  With no argument, probes yourself. With an agent name, probes that peer —
  pane liveness, transcript readability, hived heartbeat, runtime input state.

  Examples:
    hive doctor                  # probe self
    hive doctor dodo             # probe a peer

Options:
  -h, --help  Show this message and exit.
"#
        }
        ["node"] => {
            r#"Usage: hive node [OPTIONS] COMMAND [ARGS]...

  One task on one live member, as a single blocking call.

  The seam an external orchestrator — Claude Code's Workflow tool through
  the `hive-node` agent the hive plugin ships — runs in the background: a
  workflow run is `hive create RUN`, one `hive node run` per node, `hive
  delete RUN --down`. Every node is a visible pane on the team window.

Options:
  -h, --help  Show this message and exit.

Commands:
  run  Place the task on stdin onto member NAME and block for its reply.
"#
        }
        ["node", "run"] => {
            r#"Usage: hive node run [OPTIONS] --name <NAME> < task.md

  Place the task on stdin onto member NAME and block for its reply.

  Spawns the member (or reuses one of that name that is still alive),
  dispatches the task atomically, waits without timeout, and prints one JSON
  object on stdout: {"status":"replied","body":…,"artifact":…,"name":…,
  "pane":…,"reused":…}. Progress goes to stderr. A member that
  dies before replying ends the call with an error and exit 1; a spawn or
  dispatch that fails retires the member it created. The member replies
  with an ordinary `hive send flow.run`, the runner's mailbox address.

  This is the seam the `hive-node` agent (shipped by the hive plugin) runs in
  the background from a Claude Code Workflow script.

Options:
  --name <NAME>    Member name (stable; a live member of that name is reused)
  --cli <CLI>      claude | codex | grok (default: claude)
  --model <MODEL>  Model for the member's CLI
  --team <TEAM>    Team (default: the team in scope)
  -h, --help       Show this message and exit.
"#
        }
        ["fork"] => {
            r#"Usage: hive fork [OPTIONS]

  Fork the current agent session into a new split pane.

  Humans typically bind this to a keyboard shortcut (terminal + tmux). Agents
  also invoke it to create a clone that can pick up work without interrupting
  the current turn.

  Pass `--join-as <name>` to register the new pane as a team member;
  `--prompt` then sends an initial message after the fork is ready.

  On a pane not bound to any Hive team, fork still works: it produces a bare,
  independent clone (no team registration, no `@hive-*` tags) and returns
  `registered: null`, `team: null`. `--join-as` requires a team-bound pane.

  Examples:
    hive fork                                  # auto-detect split direction
    hive fork --split h                        # force horizontal split
    hive fork --join-as dodo-c1 --prompt "continue the thread"

Options:
  --pane TEXT             Source pane ID (default: auto-detect)
  -s, --split [auto|h|v]  Split direction (default: auto-detect from pane
                          dimensions)
  --join-as TEXT          Register the forked pane into the current team as
                          this agent name
  --prompt TEXT           Prompt to send to the forked agent after it is ready
  -h, --help              Show this message and exit.
"#
        }
        ["hfork"] => {
            r#"Usage: hive hfork [OPTIONS] [ARGS]...

  Human-only: fork the current Hive session into a horizontal split.

  Intended to be typed by the human via the agent's shell escape (e.g. `!hive
  hfork`) in Claude Code or Codex. Not meant for the model to invoke on its
  own.

Options:
  --help  Show this message and exit.
"#
        }
        ["inject"] => {
            r#"Usage: hive inject [OPTIONS] AGENT_NAME TEXT

  Debug: inject raw input into an agent pane.

  Writes text directly into the target pane without the `<HIVE>` envelope or
  delivery tracking. Use only when bypassing the message protocol for low-
  level debugging.

  Example:
    hive inject dodo "plain ping"

Options:
  -h, --help  Show this message and exit.
"#
        }
        ["interrupt"] => {
            r#"Usage: hive interrupt [OPTIONS] AGENT_NAME

  Interrupt an agent's running turn.

  Aborts the turn over the member's own transport — addressed to its engine,
  not typed at its pane. Use when a peer is stuck in a tool loop or you need
  to abort a runaway action.

  Example:
    hive interrupt dodo

Options:
  -h, --help  Show this message and exit.
"#
        }
        ["join"] => {
            r#"Usage: hive join [OPTIONS] [TEAM_ARG]

  Join a team.

  Outside tmux: the current Claude session enters TEAM's roster as a full
  member. Inside tmux: the current pane (or --pane) registers into the
  window's team.

Options:
  --as TEXT               Name for the new member (default: auto-derived)
  --pane TEXT             Register another pane instead of the current one
                          (tmux only)
  --notify / --no-notify  Deliver the join message over the native transport
                          (doubles as a reachability check; --no-notify
                          registers without proving the pane deliverable)
  --group TEXT            Cross-team group tag for display and namespace
                          reservation (optional; qualified-name routing works
                          without it).
  -h, --help              Show this message and exit.
"#
        }
        ["kill"] => {
            r#"Usage: hive kill [OPTIONS] AGENT_NAME

  Kill an agent pane and remove it from the team.

  Qualified names (`<group>.<name>`) resolve across teams so you can kill a
  peer-team agent from the main group pane. Bare names resolve against the
  caller's scoped team.

  Example:
    hive kill worker1

Options:
  -t, --team TEXT  Explicit team (default: the pane's binding)
  -h, --help       Show this message and exit.
"#
        }
        ["layout"] => {
            r#"Usage: hive layout [OPTIONS] {auto|main-vertical|main-horizontal|tiled|even-
                   horizontal|even-vertical}

  Plan the team window's layout, or apply a tmux preset over it.

  hive owns the layout: from the window's size and its panes' roles it
  plans a mirror column (landscape) or row (portrait) and a member grid
  sized toward 80x24 cells, and re-plans on every layout event — a
  resize, a spawn or kill, a mirror coming and going — through two window
  hooks. A dragged pane
  border holds until the plan changes. ``auto`` applies the plan now
  (the repair for a window dragged out of shape); an explicit preset
  applies as given and holds until the next event.

  The window records the applied plan's key as `@hive-layout`; ``auto``
  prints it as `layout`, with `applied` and a `reason` when it did not.

Options:
  --on-change      Hook form: apply only when the plan's key changed; prints
                   nothing.
  --window TARGET  The team window (default: the caller's)
  -h, --help       Show this message and exit.
"#
        }
        ["mirror"] => {
            r#"Usage: hive mirror [OPTIONS] [{on|off}]

  Show or hide the team's read-only orch mirror pane.

  The mirror is the `hive view` pane of a session member (a Claude session
  that created or joined the team). `off` moves it
  with break-pane into a hidden window of the team session (tagged
  `@hive-hidden`), the viewer keeps running, and the window records
  `@hive-mirror off` so `hive attach` and spawn leave it out when they heal
  the display; `on` joins the same pane back as the window's first pane —
  or rebuilds it when the hidden pane is gone — and records `on`. No
  argument toggles. The status bar's orch chip (▸ closed, ◂ open) and
  prefix+m run the same verb on the current window; --window names the
  window when the caller has no pane (a tmux run-shell job). prefix+m is
  bound server-wide when a team session is built, gated on a team window:
  elsewhere it runs whatever the key ran before (tmux's `select-pane -m`,
  or your own binding), remembered in the server option `@hive-prefix-m`.

  `off` refuses from the mirror pane itself and when the mirror is the
  window's only pane; a refusal records nothing. Prints one line: `mirror
  on (TEAM)` / `mirror off (TEAM)`, with `: already shown`, `: no mirror`
  or `: no session mirror to show` when nothing had to move — the last one
  records nothing either.

Options:
  --window <TARGET>  The team window (default: the caller's)
  -h, --help         Show this message and exit.
"#
        }
        ["ls"] => {
            r#"Usage: hive ls [OPTIONS]

  List hive teams from the registry, with their display state.

  Works outside tmux too — the registry is the truth layer; without a server
  every team simply shows as detached.

Options:
  --plain     Human-readable output instead of the default JSON
  -h, --help  Show this message and exit.
"#
        }
        ["notify"] => {
            r#"Usage: hive notify [OPTIONS] MESSAGE

  Notify the user for the current pane.

  Marks the pane and its window and rings the terminal bell: the team
  status bar draws the pane's chip as ✱ and the message on its second line,
  the pane border shows [!]. The marks persist until the user selects the
  window (no timeout); a fire on the window the user is already looking at
  is suppressed. From a parked mirror pane the marks land on the team
  window. Use this only when you are blocked and need the human back — not
  for progress updates. Message structure should cover: what happened, why
  you need them now, what to do on return.

  Examples:
    hive notify "press Space to come back and confirm migration"

Options:
  -h, --help  Show this message and exit.
"#
        }
        ["plugin"] => {
            r#"Usage: hive plugin [OPTIONS] COMMAND [ARGS]...

  Manage first-party Hive plugins.

Options:
  -h, --help  Show this message and exit.

Commands:
  disable  Disable a plugin and remove its commands.
  enable   Enable a plugin and materialize its commands.
  list     List available plugins and whether they are enabled.
  setup    One-time install: sync the marketplace, register + install for claude and codex.
  sync     Materialize the embedded plugin marketplace and print the payload directory.
"#
        }
        ["pr"] => {
            r#"Usage: hive pr [OPTIONS] COMMAND [ARGS]...

  Pin a PR number on the team window's status bar.

Options:
  -h, --help  Show this message and exit.

Commands:
  clear  Clear the current team window's PR number stamp.
  set    Label the current team window with its PR number.
"#
        }
        ["resume-hint"] => {
            r#"Usage: hive resume-hint [OPTIONS] {claude|codex|grok}

  Print a cd-ready resume command for the session this pane just ran.

  Called by the shell-init `hclaude`/`hcodex`/`hgrok` launchers after a
  managed launch exits: claude's own "Resume this session with" line omits the
  directory and codex/grok print none at all. Resolution rides hive's existing
  session truth only — codex reads the thread record its launch wrote (the
  record outlives the TUI), grok reads the session file its launch wrote,
  claude reads the pane's bg job record (the jobId outlives viewer and engine
  alike; `hive claude --resume <jobId>` reattaches and wakes it). A pane
  outside a hive team gets no hint; tracking arbitrary user panes is not this
  feature's job. Prints nothing and exits 0 on any failure: a hint must never
  break the wrapper.

Options:
  -h, --help  Show this message and exit.
"#
        }
        ["send"] => {
            r#"Usage: hive send [OPTIONS] TO_AGENT [BODY]

  Send a message to another agent — the only message verb.

  There is no threading to manage: the bus keeps every send in order, and a
  reply is simply the next message back. Senders and readers never see a
  message id.

  The recipient is an address, and every `from=` value on a received envelope
  is one — answer by copying it verbatim. A teammate is a bare name. A member
  of some team is `<team>.<member>` (how a Claude session outside tmux, e.g.
  the desktop app, reaches in; bare names work there too while unique across
  live teams — its message arrives as `from=ccd.<its name>`). A Claude session
  outside any team is `ccd.<name or title or pid>` (how a member reaches out).
  `flow.run` is the mailbox of a `hive node run` runner — an address kind,
  not a member; sends to it confirm with one `delivered to flow mailbox`
  line and never get a HIVE ack back.

  New-thread sends must keep `body` to a short summary and put details in
  `--artifact`; the body is rejected if longer than 500 chars, has 3+ lines,
  contains fenced code, or starts markdown heading/list lines. A send that
  continues a thread is exempt.

  Delivery is binary and fire-and-forget: the native transport (claude
  daemon / codex daemon) either accepted the message — its runtime owns
  it from there — or the command exits non-zero with the transport
  error. Success prints nothing; there is nothing to poll afterwards.

  Examples:
    hive send dodo "review this diff" --artifact /tmp/diff.md
    hive send "ccd.PR review" "build is green"    # session by desktop title
    hive send dodo "see report" --artifact - <<'EOF'
    # Findings
    - item
    EOF

Options:
  --artifact TEXT  Artifact path for large payloads
  -h, --help       Show this message and exit.
"#
        }
        ["shell-init"] => {
            r#"Usage: hive shell-init [OPTIONS] [SHELL]

  Print the `hcodex` / `hclaude` / `hgrok` launchers for your shell.

  Add to your shell rc; then `hcodex` / `hclaude` / `hgrok` start a hive-
  connected codex / claude / grok in the current tmux pane, while the plain
  `codex` / `claude` / `grok` stay untouched:

    # ~/.zshrc or ~/.bashrc
    eval "$(hive shell-init zsh)"
    # ~/.config/fish/config.fish
    hive shell-init fish | source

  Outside tmux, and for management subcommands and non-interactive flags, the
  launchers run the plain binary.

Options:
  -h, --help  Show this message and exit.
"#
        }
        ["spawn"] => {
            r#"Usage: hive spawn [OPTIONS] AGENT_NAME

  Spawn an agent pane, optionally dispatching a task atomically.

  Splits a new pane into the team's window and starts the chosen agent CLI
  there — from inside tmux, another session, or outside tmux alike (a window
  that is gone is rebuilt first). By default spawns the same CLI as the
  current pane (claude when there is none); use `--cli claude|codex|grok` to
  pick one.

  With `--task <artifact>`, the member boots straight into the member contract
  (`/hive:hive`) and the task artifact arrives as its first `<HIVE>` message —
  spawn and dispatch are one atomic step, so the member never wanders off
  exploring while waiting for work.

  Examples:
    hive spawn explore --task /tmp/tasks/explore.md
    hive spawn review --cli codex --task /tmp/tasks/review.md
    hive spawn dodo --cli codex
    hive spawn claude -m claude-opus-5 --skill none

Options:
  -m, --model TEXT           Model ID. claude: prefer aliases
                             (fable/opus/sonnet) — they always track the
                             latest; codex/grok: checked against the CLI's own
                             catalog
  -p, --prompt TEXT          Initial prompt (typed into TUI after startup)
  --cwd TEXT                 Working directory
  --skill TEXT               Base skill to load after startup ('none' to skip)
  -e, --env TEXT             Extra env vars (KEY=VALUE, repeatable)
  --cli [claude|codex|grok]  Agent CLI to spawn (default: same as current
                             pane)
  --task FILE                Task artifact to dispatch atomically once the
                             member is ready (member never boots into an empty
                             inbox)
  -t, --team TEXT            Explicit team (default: the pane's binding)
  -h, --help                 Show this message and exit.
"#
        }
        ["team"] => {
            r#"Usage: hive team [OPTIONS]

  Show team overview.

  Returns a JSON payload with `members[]`, `self` (your own name), the bound
  `tmuxSession` / `tmuxWindow`, `runtimeWorkspace`, and `cwd`.

  Each member row carries the runtime fields `busy`, `inputState`, and
  `turnPhase` — see docs/runtime-model.md for semantics. `self` is a string
  pointer: look yourself up in `members[]` for your own state.

  If the current tmux window has no team bound, returns a bootstrap payload
  instead: `team=null`, a pane list, and a `hint` telling you to run `hive
  create`.

  Examples:
    hive team                                # full payload when a team is bound
    hive team | jq '.members[] | select(.name=="dodo")'

Options:
  -t, --team TEXT  Explicit team (default: the pane's binding)
  -h, --help       Show this message and exit.
"#
        }
        ["vfork"] => {
            r#"Usage: hive vfork [OPTIONS] [ARGS]...

  Human-only: fork the current Hive session into a vertical split.

  Intended to be typed by the human via the agent's shell escape (e.g. `!hive
  vfork`) in Claude Code or Codex. Not meant for the model to invoke on its
  own.

Options:
  --help  Show this message and exit.
"#
        }
        ["view"] => {
            r#"Usage: hive view [OPTIONS] SESSION_ID

  Read-only viewer for a Claude session transcript (follows live).

Options:
  -h, --help  Show this message and exit.
"#
        }
        ["vim"] => {
            r#"Usage: hive vim [OPTIONS] [ARGS]...

  Human-only: compose in a blank vim buffer, send it to the agent pane.

  Intended to be typed by the human via the agent's shell escape (e.g. `!hive
  vim`) in Claude Code or Codex. Not meant for the model to invoke on its own.

Options:
  --help  Show this message and exit.
"#
        }
        ["worktree"] => {
            r#"Usage: hive worktree [OPTIONS] COMMAND [ARGS]...

  Per-feature worktree pool: start a feature, finish it, inspect state.

  Pool layout: <main checkout>/.claude/worktrees/<feature>, branch == feature.
  Hive creates/removes worktrees and records ownership in git config;
  entering/leaving the directory is the agent's own move (Claude:
  EnterWorktree path=<path> / ExitWorktree action=keep; Codex: cd).

  Examples:
    hive worktree start login-flow         # create worktree + branch, print JSON with path
    hive worktree status                   # pool state for this repo
    hive worktree done login-flow          # remove the worktree, keep the branch

Options:
  -h, --help  Show this message and exit.

Commands:
  done      Remove FEATURE's worktree.
  set-base  Declare the team's integration branch (the base of every...
  start     Create (or re-attach) the worktree for FEATURE and print its...
  status    Read-only lifecycle view of FEATURE (or every hive-labeled...
"#
        }
        ["ccd", "ls"] => {
            r#"Usage: hive ccd ls [OPTIONS]

  List the Claude Code sessions `hive send ccd.<name>` can reach.

  The same registry `/list-agents` reads: every live session that binds a
  cross-session inbox (Claude Code 2.1.224+). A session on an older CLI, or
  started in bare mode, has no inbox and is not listed. `title` is the desktop
  app's session title when one is set. A session that is really a live team
  member carries a `member` field with its `<team>.<agent>` address: message
  it over the bus, not here.

Options:
  -h, --help  Show this message and exit.
"#
        }
        ["config", "get"] => {
            r#"Usage: hive config get [OPTIONS] KEY

  Print the value at KEY (dot-path). Exit 1 when unset.

Options:
  -h, --help  Show this message and exit.
"#
        }
        ["config", "set"] => {
            r#"Usage: hive config set [OPTIONS] KEY VALUE

  Set KEY to VALUE (true/false/int/float/string).

Options:
  -h, --help  Show this message and exit.
"#
        }
        ["config", "unset"] => {
            r#"Usage: hive config unset [OPTIONS] KEY

  Remove KEY. Exit 1 when KEY was not set.

Options:
  -h, --help  Show this message and exit.
"#
        }
        ["plugin", "disable"] => {
            r#"Usage: hive plugin disable [OPTIONS] NAME

  Disable a plugin and remove its commands.

Options:
  --plain     Human-readable output instead of the default JSON
  -h, --help  Show this message and exit.
"#
        }
        ["plugin", "enable"] => {
            r#"Usage: hive plugin enable [OPTIONS] NAME

  Enable a plugin and materialize its commands.

Options:
  --plain     Human-readable output instead of the default JSON
  -h, --help  Show this message and exit.
"#
        }
        ["plugin", "list"] => {
            r#"Usage: hive plugin list [OPTIONS]

  List available plugins and whether they are enabled.

Options:
  --plain     Human-readable output instead of the default JSON
  -h, --help  Show this message and exit.
"#
        }
        ["plugin", "ls"] => {
            r#"Usage: hive plugin ls [OPTIONS]

  Hidden alias of `hive plugin list`.

Options:
  --plain     Human-readable output instead of the default JSON
  -h, --help  Show this message and exit.
"#
        }
        ["plugin", "setup"] => {
            r#"Usage: hive plugin setup [OPTIONS]

  One-time install: sync the marketplace, then register and install the hive
  plugin for claude and codex on PATH.

Options:
  -h, --help  Show this message and exit.
"#
        }
        ["plugin", "sync"] => {
            r#"Usage: hive plugin sync [OPTIONS]

  Materialize the embedded plugin marketplace and print the payload directory
  (the command source Claude re-runs each session).

Options:
  -h, --help  Show this message and exit.
"#
        }
        ["pr", "clear"] => {
            r#"Usage: hive pr clear [OPTIONS]

  Clear the current team window's PR number stamp.

Options:
  --plain     Human-readable output instead of the default JSON
  -h, --help  Show this message and exit.
"#
        }
        ["pr", "set"] => {
            r#"Usage: hive pr set [OPTIONS] NUMBER

  Label the current team window with its PR number.

  Run right after ``gh pr create --draft`` — writes ``@hive-pr`` on the
  current tmux window and installs a per-window status-bar display derived
  from the global ``window-status-format`` / ``window-status-current-format``
  (the index position renders ``PR<n>``; user styling and padding are
  preserved). A team session hive built shows no window tabs: there the
  stamp renders as a ``PR<n>`` field on the first line of hive's own status
  bar. Idempotent — re-running replaces the stamp and re-derives the
  display.

Options:
  --plain     Human-readable output instead of the default JSON
  -h, --help  Show this message and exit.
"#
        }
        ["worktree", "done"] => {
            r#"Usage: hive worktree done [OPTIONS] FEATURE

  Remove FEATURE's worktree. The branch is always kept (PRs live on it).

  Refuses while you are inside the worktree, while a git operation is in
  progress, or while there are uncommitted changes (unless --force).

Options:
  --force     Discard uncommitted work (destructive; prints a status summary
              first)
  --plain     Human-readable output instead of the default JSON
  -h, --help  Show this message and exit.
"#
        }
        ["worktree", "set-base"] => {
            r#"Usage: hive worktree set-base [OPTIONS] REF

  Declare the team's integration branch (the base of every sub-PR).

  Run from the team window after creating and pushing the branch; every `hive
  worktree start` in this window afterwards resolves its base from it. REF
  must already resolve to a commit.

Options:
  --plain     Human-readable output instead of the default JSON
  -h, --help  Show this message and exit.
"#
        }
        ["worktree", "start"] => {
            r#"Usage: hive worktree start [OPTIONS] FEATURE

  Create (or re-attach) the worktree for FEATURE and print its path as JSON.

  Exit 0 = ready (mode created/existing/attached/adopted-existing-branch).
  Exit 1 with mode=needs-rebase = branch exists but does not contain the
  resolved base: rebase inside the worktree, then rerun start.

Options:
  --base TEXT  Base ref override (default: the window's integration branch
               from `hive worktree set-base`, else detected default branch)
  --plain      Human-readable output instead of the default JSON
  -h, --help   Show this message and exit.
"#
        }
        ["worktree", "status"] => {
            r#"Usage: hive worktree status [OPTIONS] [FEATURE]

  Read-only lifecycle view of FEATURE (or every hive-labeled worktree).

Options:
  --plain     Human-readable output instead of the default JSON
  -h, --help  Show this message and exit.
"#
        }
        _ => return None,
    })
}
