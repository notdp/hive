# Hive

> tmux-first multi-agent collaboration runtime — `claude`, `codex`, and `grok` members run as their own engines, exchange `<HIVE>` messages over each engine's native transport, and share one registry as the truth layer.

**English** · [简体中文](README.zh-CN.md) · [日本語](README.ja.md)

_This README is maintained in English. Translations may lag behind the canonical version._

## What is Hive

Hive is a runtime for agents, not a hand-driven CLI. A team is a roster in the registry (one directory per team, `$HIVE_HOME/teams/<team>/`, holding its `team.json` and, by default, its workspace) plus one engine per member. The tmux window is a display drawn on top of that, eagerly: every team has one from creation.

The split is enforced in the implementation. `hive create` outside tmux builds a detached tmux session named after the team to hold its window; `hive spawn` splits a pane into the team's window from anywhere; `hive attach` jumps there and, when the window was closed or the tmux server restarted, rebuilds it from the registry first — the engines were never in the window. Nothing tmux holds is truth, so closing the window discards no state.

Dispatching tasks, sending messages, and reading runtime state happen inside the agent session, and the agent runs the commands. The human entry point is the plugin skill `/hive:hive [team]`: no argument creates or joins by circumstance, a name joins that team and creates it if it does not exist. A small set of commands is run by hand: installing plugins, reading a session transcript (`hive view`), the popup editor (`hive cvim` / `hive vim`), split forks, and local dev setup.

## Install

Hive is one Rust binary. Prebuilt binaries ship on [GitHub Releases](https://github.com/notdp/hive/releases) for macOS and Linux (aarch64 and x86_64):

```bash
curl -fsSL https://github.com/notdp/hive/releases/latest/download/hive-installer.sh | sh
```

With a Rust toolchain there are two more routes: [`cargo binstall`](https://github.com/cargo-bins/cargo-binstall) fetches the same prebuilt release (no compile), `cargo install` builds from source:

```bash
cargo binstall --git https://github.com/notdp/hive hive
# or
cargo install --git https://github.com/notdp/hive hive
```

The plugin — the skill that teaches an agent the protocol — ships inside the binary and is served from a local marketplace that `hive` materializes under `$HIVE_HOME`. One command registers and installs it for every agent CLI on PATH (re-running it repairs an install):

```bash
hive plugin setup
```

Under the hood that materializes the marketplace and runs `plugin marketplace add` + install for claude (2.1.229+) and codex. On claude the marketplace entry is a command source — Claude re-runs `hive plugin sync` once per session, so skill updates ride the binary; on codex the plugin ships no hooks (hooks would sit behind codex's hook-review dialog) — hive's own codex launch path re-adds the plugin when the binary version changes, before the engine starts. Nothing is fetched from a remote and no settings are touched.

Requires:

- `tmux` 3.2+ — for the `hive cvim` / `hive vim` popups, and because a pane only answers the bare OSC 11 background query that `hive view` uses to pick a theme from 3.2 on
- a Rust toolchain — only for the build-from-source route; the installer ships prebuilt binaries
- at least one agent CLI: `claude`, `codex`, or `grok`

## Start in your agent session

```bash
# one-time setup: add eval "$(hive shell-init zsh)" to your shell rc,
# after the PATH line the installer already wrote there
# Inside tmux, start your agent through hive's launcher
$ hclaude      # or: hcodex / hgrok

# In the agent session, type:
/hive:hive
```

The agent makes the current pane the team's orch and spawns members as tasks call for them. From that point the conversation is with the agent, and the agent runs the team.

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

Without `-b` the tmux server blocks while the fork runs. `--pane` is required
too: the binding fires from outside the pane, so auto-detection would pick the
wrong source.

## Why the transcript viewer is read-only

An interactive Claude session has no attachable pty (`claude attach` is job-only), but its transcript is appended event by event as the turn unfolds, so a renderer over that file is a live mirror that cannot type back. `hive view` is that renderer.

The display layer binds it automatically for a claude member whose sessionId has no bg-job row: an interactive session, a desktop `ccd` or one that was joined. Resuming such a session would mint a forked job that steals the member's deliveries, so that pane gets the mirror instead of a resume, and the pane is read-only. Delivery is unaffected: the same missing job row routes `hive send` to the live interactive session instead of the pane. Nobody can type at that member except the app that owns the session.

The mirror is an ordinary pane, the first of the team window, and it is there by default: nothing withholds it until you park it. Parking is `hive mirror off` (`on` restores, no argument toggles), the orch chip on the status bar, or `prefix+m`. All three move the pane between the window and a hidden window of the team session with `break-pane` / `join-pane`, so the viewer process never restarts; the choice lands on the window as `@hive-mirror`, and `hive attach` respects it when it heals the display.

A team session hive builds — `hive create` outside tmux, `hive attach` rebuilding a lost window — carries its own two-line status bar, set through session options only, so your global tmux status is untouched. Line one: the team chip; the orch chip, ` ◂ orch ` while the mirror pane is in the window and ` ▸ orch ` while it is parked (a click toggles it); one chip per pane — the member name after ● busy, ○ idle or ✱ unread (a message delivered while the member has not started on it) or awaiting attention after a notify, the active pane bold, a click selects that pane; then `PR<n>` once `hive pr set` stamped one, the session name and the clock. Line two is a ticker: the two newest bus messages as `from → to · age · "first words"`, with a pending notify's text ahead of them. Everything on the bar is a tmux option the CLI or the hived wrote, so it never runs a shell command; a click anywhere else on a status line does what tmux always did.

## Upgrade

Re-run the installer one-liner from [Install](#install); it always fetches the latest release. Releases are cut by pushing a `v*` tag matching the crate version; CI (cargo-dist) builds the platform binaries and publishes the GitHub Release.

Skill updates ride the binary: on claude the marketplace's command source re-runs `hive plugin sync` each session and picks up changed content automatically; on codex hive's launch path re-adds the plugin when the cache has no entry for the running binary's version. Plugin manifest versions stay locked to the CLI version — that lock is what keys the codex cache.

## Development

The installed `hive` binary is the transport for live agents. Keep it on a committed checkout while developing Hive itself, and do not `cargo install` from a dirty worktree a team is using. Manual verification that needs plugin materialization or hived behavior uses a disposable `HIVE_HOME`, `CLAUDE_HOME`, `CODEX_HOME` and a throwaway team, not the live team's hived. Test lanes and repository conventions live in [AGENTS.md](AGENTS.md).

## Docs

- [`docs/runtime-model.md`](docs/runtime-model.md) — registry-vs-display identity, the per-CLI native runtime sources, and `busy` / `inputState` / `turnPhase`
- [`docs/transcript-view.md`](docs/transcript-view.md) — what `hive view` draws: the JSONL → `DisplayBlock` parse model, the viewer's chrome, theme resolution
- [`docs/daemon-control-socket.md`](docs/daemon-control-socket.md) — the Claude supervisor daemon's control protocol, whose `op:"reply"` is hive's delivery lane
- [`plugins/hive/skills/hive/SKILL.md`](plugins/hive/skills/hive/SKILL.md) — the collaboration protocol `/hive:hive` loads into an agent

## License

[GPL-3.0-or-later](LICENSE) © 2026 notdp
