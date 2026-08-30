# Hive

> tmux-first multi-agent collaboration runtime — `claude`, `codex`, and `grok` members run as their own engines, exchange `<HIVE>` messages over each engine's native transport, and share one registry as the truth layer.

**English** · [简体中文](README.zh-CN.md) · [日本語](README.ja.md)

_This README is maintained in English. Translations may lag behind the canonical version._

## What is Hive

Hive is a runtime for agents, not a CLI you drive by hand. A team is a roster in the registry — one JSON file per team under `$HIVE_HOME/state/teams/` — plus one engine per member. The tmux window is a display that `hive attach` renders on top of that, and nothing more.

The split is enforced. `hive create` outside tmux registers a headless team, `hive spawn` with no display brings up engine-only members that still receive, report, and get killed normally, and `hive attach <team>` materializes the window later, one pane per member. Nothing tmux holds is truth, so closing the window costs nothing.

Dispatching tasks, sending messages, reading runtime state: all of it happens inside the agent session, and your agent runs the commands. The human entry point is the plugin skill `/hive:hive [team]`: no argument creates or joins by circumstance, a name joins that team and creates it if it does not exist. A small set of commands stays yours — installing plugins, reading a session transcript (`hive view`), the popup editor (`hive cvim` / `hive vim`), split forks, and local dev setup.

## Install

Hive is one Rust binary, built from a checkout:

```bash
git clone https://github.com/notdp/hive.git
cd hive
cargo install --path crates/hive
```

The repo is also a plugin marketplace for both CLIs. The plugin ships the skill that teaches an agent the protocol:

```bash
# Claude Code
claude plugin marketplace add notdp/hive
claude plugin install hive@hive

# Codex
codex plugin marketplace add https://github.com/notdp/hive.git
codex plugin add hive@hive
```

Install the CLI yourself first. The plugin's `SessionStart` hook looks like it will do that for you and cannot: its converge step still shells out to `pipx install` against this repo, which predates the Rust cutover and has shipped no `pyproject.toml` since, so that lane cannot produce a binary and the hook's second phase — enabling Claude's marketplace auto-update — is never reached. With a new enough `hive` already on PATH the check installs nothing and falls through, which is the only path that converges.

Requires:

- `tmux` 3.2+ — for the `hive cvim` / `hive vim` popups, and because a pane only answers the bare OSC 11 background query that `hive view` uses to pick a theme from 3.2 on
- a Rust toolchain, to build
- `python3` — `hive flow run` execs the interpreter against the embedded `hive.flow` client, and the notify popup is a python heredoc
- at least one agent CLI: `claude`, `codex`, or `grok`

## Start in your agent session

```bash
# one-time setup: eval "$(hive shell-init zsh)" in your shell rc
# Inside tmux, start your agent through hive's launcher
$ hclaude      # or: hcodex / hgrok

# In the agent session, type:
/hive:hive
```

The agent makes the current pane the team's orch and spawns members as tasks call for them. From here on you talk to the agent; the agent runs the team.

## Binding `hive fork` to a key

A terminal keybinding cannot run a shell command, so the binding emits a raw
escape byte and tmux catches it. On macOS with Ghostty, Cmd+Shift+F sends ESC f
and tmux runs the fork:

```
# ~/.config/ghostty/config
keybind = cmd+shift+f=text:\x1bf

# ~/.tmux.conf
bind -n M-f run-shell -b 'hive fork --pane "#{pane_id}"'
```

`-b` is load-bearing: without it the tmux server blocks while the fork runs. So
is `--pane` — the binding fires from outside the pane, so auto-detection would
pick the wrong source.

## Why the transcript viewer is read-only

An interactive Claude session has no attachable pty — `claude attach` is job-only — but its transcript is appended event by event as the turn unfolds, so a renderer over that file is a faithful live mirror that cannot type back. That is what `hive view` is.

`hive attach` binds it on its own for a claude member whose sessionId has no bg-job row: an interactive session, a desktop `ccd` or one that was joined. Resuming such a session would mint a forked job that steals the member's deliveries, so that pane gets the mirror instead of a resume. The accepted cost is a read-only pane. Delivery is unaffected — the same missing job row routes `hive send` to the live interactive session instead of the pane — but nobody can type at that member except the app that owns the session.

## Upgrade

```bash
git pull && cargo install --path crates/hive
```

Plugin manifest versions are locked to the CLI version, so a release ships plugin updates with it. Claude Code auto-updates the marketplace once the bootstrap hook has written its `extraKnownMarketplaces` entry; it skips that write when `DISABLE_AUTOUPDATER` is set without `FORCE_AUTOUPDATE_PLUGINS`, and then `claude plugin update hive@hive` is manual. Codex snapshots the marketplace when you add it and never refreshes on its own — run `codex plugin marketplace upgrade hive`.

## Development

The installed `hive` binary is live agent transport. Keep it on a committed checkout while developing Hive itself; never `cargo install` from a dirty worktree a team is using. Manual verification that needs plugin materialization or hived behavior gets a disposable `HIVE_HOME`, `CLAUDE_HOME`, `CODEX_HOME` and a throwaway team, not the live team's hived. Test lanes and repository conventions live in [AGENTS.md](AGENTS.md).

## Docs

- [`docs/runtime-model.md`](docs/runtime-model.md) — registry-vs-display identity, the per-CLI native runtime sources, and `busy` / `inputState` / `turnPhase`
- [`docs/daemon-control-socket.md`](docs/daemon-control-socket.md) — the Claude supervisor daemon's control protocol, whose `op:"reply"` is hive's delivery lane
- [`plugins/hive/skills/hive/SKILL.md`](plugins/hive/skills/hive/SKILL.md) — the collaboration protocol `/hive:hive` loads into an agent

## License

[GPL-3.0-or-later](LICENSE) © 2026 notdp
