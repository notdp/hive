# Hive

> tmux-based collaboration runtime for CLI agents — `claude` and `codex` talk to each other via inline `<HIVE>` messages, tracked deliveries, and handoff threads.

**English** · [简体中文](README.zh-CN.md) · [日本語](README.ja.md)

_This README is maintained in English. Translations may lag behind the canonical version._

## What is Hive

Hive is a runtime for agents, not a CLI you drive by hand. Day-to-day work — sending messages, replying on threads, handing off tasks, tracking delivery — happens inside the agent session, and your agent runs the commands. The main day-to-day entry point for humans is `/hive`, which loads the Hive skill into your agent so it can bootstrap the team.

A small set of commands is still yours: installing plugins, checking skill drift, the popup editor (`hive cvim` / `hive vim`), and local dev setup.

## Install

```bash
# Hive CLI
pipx install git+https://github.com/notdp/hive.git

# Hive skill, for Claude Code / Codex
npx skills add https://github.com/notdp/hive -g --all
```

Requires:

- `tmux` (3.2+ is needed for the `hive cvim` / `hive vim` popup helpers)
- Python 3.11+
- At least one agent CLI: `claude` or `codex`

## Start in your agent session

```bash
# Inside tmux, start your agent of choice
$ claude       # or: codex

# In the agent session, type:
/hive
```

The skill loads, the agent runs `hive init` to bind the current tmux window as a team, and auto-pairs with an idle peer of a different model family — attaching an existing one if found, otherwise spawning a new pane. From here on you talk to the agent; the agent talks to its peer.

## Operator commands

Commands commonly run by humans:

```bash
# Plugins
hive plugin enable notify         # sidecar idle watcher toggle (manual `hive notify` stays available either way)
hive plugin list

# Diagnostics
hive doctor --skills              # check for hive skill drift after upgrades

# Popup editor (tmux 3.2+)
hive cvim                         # tmux popup editor
hive vim                          # single-pane variant

# Fork the current agent session into a split pane
hive fork                         # auto-detect split direction
hive vfork                        # vertical split
hive hfork                        # horizontal split
```

Inside Claude Code / Codex, invoke these via shell escape: `!hive cvim`, `!hive vfork`, `!hive fork`, etc.

Binding `hive fork` to a keyboard shortcut pairs well with tmux. Example (Ghostty + tmux on macOS) — Cmd+Shift+F forks the current pane; change the key to match your terminal:

```
# ~/.config/ghostty/config
keybind = cmd+shift+f=text:\x1bf

# ~/.tmux.conf
bind -n M-f run-shell -b 'hive fork --pane "#{pane_id}"'
```

Everything else — `hive send`, `hive reply`, `hive team`, `hive doctor <agent>`, `hive handoff`, etc. — is designed for the agent to invoke. Running them yourself works, but that is the debugging / advanced path, not the happy path.

## Upgrade

```bash
pipx upgrade hive           # upgrade the CLI
npx skills update hive -g   # upgrade the skill (GitHub-sourced installs only)
```

The CLI and the skill upgrade separately. Upgrading the CLI does not refresh the skill. When the skill is stale, `hive` commands run from an agent pane warn on stderr and continue, and `hive doctor --skills` shows the mismatch.

For local checkout development, keep source-under-test separate from the live install — see the contributor section below.

## For Contributors

```bash
PYTHONPATH=src python -m pytest tests/ -q
```

The global `hive` binary is live agent transport. Keep it on the stable install while developing Hive itself; tests should import the checkout explicitly with `PYTHONPATH=src`. Manual verification that needs plugin materialization or sidecar behavior should use disposable `HIVE_HOME`, `CLAUDE_HOME`, `CODEX_HOME`, and a temporary team/window rather than the live team. Repository conventions live in [AGENTS.md](AGENTS.md).

## Docs

- [`docs/runtime-model.md`](docs/runtime-model.md) — runtime field semantics (`busy`, `inputState`, `turnPhase`)
- [`docs/transcript-signals.md`](docs/transcript-signals.md) — Claude transcript parsing rules
- [`skills/hive/SKILL.md`](skills/hive/SKILL.md) — agent behavior / prompt contract loaded by the Hive skill at runtime

## License

[GPL-3.0-or-later](LICENSE) © 2026 notdp
