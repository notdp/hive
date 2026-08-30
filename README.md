# Hive

> tmux-first multi-agent collaboration runtime — `claude`, `codex`, and `grok` members run as their own engines, exchange `<HIVE>` messages over each engine's native transport, and share one registry as the truth layer.

**English** · [简体中文](README.zh-CN.md) · [日本語](README.ja.md)

_This README is maintained in English. Translations may lag behind the canonical version._

## What is Hive

Hive is a runtime for agents, not a CLI you drive by hand. A team is a roster in the registry (one JSON file per team under `$HIVE_HOME/state/teams/`) plus one engine per member; the tmux window is an optional display that `hive attach` renders on top of it. Day-to-day work — dispatching tasks, sending messages, reading runtime state — happens inside the agent session, and your agent runs the commands.

The human entry point is the plugin skill `/hive:hive [team]`: no argument creates or joins by circumstance, a name joins that team and creates it if it does not exist.

A small set of commands stays yours: installing plugins, reading a session transcript (`hive view`), the popup editor (`hive cvim` / `hive vim`), split forks, and local dev setup.

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

Install the CLI yourself before the plugin's `SessionStart` hook runs. That hook (`plugins/hive/scripts/bootstrap.py`) does two things: converge the CLI, then enable Claude's marketplace auto-update. Its converge step still shells out to `pipx install git+https://github.com/notdp/hive` (`bootstrap.py:114`), which predates the Rust cutover — the repo ships no `pyproject.toml`, so that lane cannot produce a binary and phase two never runs. With a `hive` ≥ 0.10.1 already on PATH the check returns `already meets minimum` and installs nothing, which is the only path that converges.

Requires:

- `tmux` — 3.2+ for the `hive cvim` / `hive vim` popups, and for the bare OSC 11 background query `hive view` uses to pick a theme (`crates/hive/src/view_theme.rs:349`)
- a Rust toolchain, to build
- `python3` — `hive flow run` execs the interpreter against the embedded `hive.flow` client (`crates/hive/src/cli/rest.rs:1094`), and the notify popup is a python heredoc (`crates/hive/src/notify_ui.rs:242`)
- at least one agent CLI: `claude`, `codex`, or `grok`

## Start in your agent session

```bash
# one-time setup: eval "$(hive shell-init zsh)" in your shell rc
# Inside tmux, start your agent through hive's launcher
$ hclaude      # or: hcodex / hgrok

# In the agent session, type:
/hive:hive
```

The skill loads and the agent runs `hive create`, which makes the current pane the team's orch, then `hive spawn` for members as tasks call for them. From here on you talk to the agent; the agent runs the team.

None of this requires tmux. `hive create` outside tmux registers a headless team, `hive spawn` with no display brings up engine-only members that still receive, report, and get killed normally, and `hive attach <team>` materializes the window later — one pane per member.

## Operator commands

Commands commonly run by humans:

```bash
# Plugins
hive plugin enable notify --plain # hived idle watcher toggle (manual `hive notify` stays available either way)
hive plugin list --plain          # human-readable listing (default output is JSON)

# Read-only transcript mirror
hive view <session-id>            # follows a Claude session live; keystrokes go nowhere

# Popup editor (tmux 3.2+)
hive cvim                         # edit the last assistant message in vim, send it back
hive vim                          # compose in a blank vim buffer

# Fork the current agent session into a split pane
hive fork                         # auto-detect split direction
hive vfork                        # vertical split
hive hfork                        # horizontal split
```

`hive view` renders `~/.claude/projects/*/<session-id>.jsonl` (`crates/hive/src/transcript_view.rs:44`). An interactive Claude session has no attachable pty — `claude attach` is job-only — but its transcript is appended event by event as the turn unfolds, so a renderer over that file is a faithful live mirror that cannot type back. On a tty it is a ratatui pager: `↑↓` selects a block, `←→` folds it, `Enter` opens it full-screen, `Ctrl+o` cycles density, `/` opens the command palette (`/theme`, `/view`, `/find`, `/quit`), `q` quits. Piped or redirected, it degrades to a plain ANSI stream (`transcript_view.rs:1622`). Theme comes from `HIVE_VIEW_THEME=light|dark|auto`, else the `view.theme` setting, else detection that falls back to light (`view_theme.rs:281`).

`hive attach` binds it on its own. A claude member whose sessionId has no bg-job row is an interactive session — a desktop `ccd`, a joined session — and resuming one would mint a forked job that steals the member's deliveries, so that member's pane gets `hive view` instead of a resume (`crates/hive/src/cli/rest.rs:1268`). The cost is that the pane is read-only. Delivery is unaffected — the same missing job row routes `hive send` to the live interactive session instead of the pane (`crates/hive/src/agent.rs:759`) — but nobody can type at that member except the app that owns the session.

Inside Claude Code / Codex, invoke these via shell escape: `!hive cvim`, `!hive vfork`, `!hive fork`, etc.

Binding `hive fork` to a keyboard shortcut pairs well with tmux. Example (Ghostty + tmux on macOS) — Cmd+Shift+F forks the current pane; change the key to match your terminal:

```
# ~/.config/ghostty/config
keybind = cmd+shift+f=text:\x1bf

# ~/.tmux.conf
bind -n M-f run-shell -b 'hive fork --pane "#{pane_id}"'
```

Everything else — `hive send`, `hive team`, `hive spawn`, `hive doctor <agent>`, etc. — is designed for the agent to invoke. Running them yourself works, but that is the debugging / advanced path, not the happy path.

## Upgrade

The CLI upgrades by rebuilding a committed checkout:

```bash
git pull && cargo install --path crates/hive
```

Plugin manifest versions are locked to the CLI version, so a release ships plugin updates with it. Claude Code auto-updates the marketplace once the bootstrap hook has written `extraKnownMarketplaces.hive` with `autoUpdate: true`; it skips that write when `DISABLE_AUTOUPDATER` is set without `FORCE_AUTOUPDATE_PLUGINS`, and then `claude plugin update hive@hive` is manual. Codex snapshots the marketplace when you add it and never refreshes on its own — run `codex plugin marketplace upgrade hive`.

## For Contributors

```bash
cargo nextest run                 # the whole Rust suite
python -m pytest tests/e2e -q     # black-box tmux flows against target/debug/hive
```

nextest is required rather than preferred: the tests mutate env vars freely, and plain `cargo test` shares one process across them, so they cross-contaminate.

After every live install, run the post-install acceptance suite — it spawns one real member per CLI and asserts the oracles the unit suites cannot see (reply identity, pane color via `capture-pane -e`, nonce causality, a headless-claude semantic coroner):

```bash
HIVE_ACCEPTANCE=1 HIVE_ACCEPTANCE_CLIS=claude,codex,grok python -m pytest tests/acceptance -q
```

The installed `hive` binary is live agent transport. Keep it on a committed checkout while developing Hive itself; never `cargo install` from a dirty worktree a team is using. Manual verification that needs plugin materialization or hived behavior gets a disposable `HIVE_HOME`, `CLAUDE_HOME`, `CODEX_HOME` and a throwaway team/window, not the live team's hived. Repository conventions live in [AGENTS.md](AGENTS.md).

## Docs

- [`docs/runtime-model.md`](docs/runtime-model.md) — registry-vs-display identity, the per-CLI native runtime sources, and `busy` / `inputState` / `turnPhase`
- [`docs/daemon-control-socket.md`](docs/daemon-control-socket.md) — the Claude supervisor daemon's control protocol, whose `op:"reply"` is hive's delivery lane
- [`plugins/hive/skills/hive/SKILL.md`](plugins/hive/skills/hive/SKILL.md) — the collaboration protocol `/hive:hive` loads into an agent

## License

[GPL-3.0-or-later](LICENSE) © 2026 notdp
