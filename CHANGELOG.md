# Changelog

One section per released version, newest first. The bump step in
AGENTS.md writes the section; `release-notes.yml` puts it on the GitHub
release.

## 0.19.0

### Features

- `hive update` replaces the running binary with the latest GitHub release: `--check` only looks (exit 1 when newer exists, 2 on a failed query), `--force` reinstalls the running version, never a downgrade. hive downloads the archive and its `.sha256` itself, verifies the digest and the archive entries, runs the candidate's `--version`, then renames it over `current_exe()`; the target is locked and fingerprinted before any network call (#181)

## 0.18.2

### Features

- The GitHub release body is the version's CHANGELOG.md section plus the README-style install one-liner, written by a cargo-dist post-announce job; the bump step writes the section (#179)

### Internal

- Skill text: commands a human runs go in a bash fence (#180)

## 0.18.1

### Features

- The team creator badges its session title `[<team>.orch]`, the same shape every member uses (#178)
- The hived follows a desktop conversation across the CLI sessions it restarts as: a rewind no longer drops the member from the roster (#177)
- Inbox delivery rides inside claude's own cross-session-message tag, so the receiver draws a clean card instead of the wrapper prose (#176)

## 0.18.0

### Features

- A node's result is the engine's own turn end; `hive workflow done` and the claude node are gone (#175)
- Eager tmux display, engine-first grok minting, one directory per team (#157)
- Team-session status bar with member chips, orch mirror chip and ticker; mirror open/close via break-pane/join-pane (#158)
- Auto layout: hive owns the team window layout and re-plans it through tmux window hooks (#159)

### Fixes

- Team panes are told their real colours; tmux 3.5+ required, tmux 3.7 followed (#174)
- The layout hook yields to an apply in flight instead of queueing on the window lock (#163)
- Status-bar mirror binding and layout hooks no longer pop run-shell view mode over a member pane (#160)
- A minted codex thread is flushed to disk before the member is trusted alive (#155)
- Grok reap kills every client of the socket before the leader; the session row outranks spawn env in the identity ladder (#153)
- Headless grok lifecycle: send gate, self identity, spawn ordering, leader reap; attach split into jump-only attach and render (#152)
- Eight review findings across delete, join, codex identity, registry verdicts, grok reap, the viewer (#151)
- Inbox frames name the author; the hived socket relocates for deep workspaces (#150)
- The acceptance coroner exempts the CLI's own memory and instruction reads (#167)

### Internal

- msgId dropped: the bus is an append-only ledger and a reply is the next message back (#172)
- The JS flow engine, board, rig and dock are cut; `hive node run` is the one node verb (#173)
- CLI split by domain; team, naming, send and identity own the logic (#170, #171)
- HIVE_TEAM/HIVE_MEMBER retired; grok identity is GROK_SESSION_ID against the roster (#154)
- Port-era residue removed: leading-underscore names, the Python byte-compat layer, the legacy-install cleanup paths (#161, #162, #164, #168)
- Leaf modules for paths, shell and clock so lower layers stop reaching up (#165)
- Comment and doc accuracy pass, dead-code cleanup, test architecture rebuild (#156, #166)

## 0.17.1

### Fixes

- The codex gate accepts headless members: the registry sessionId is identity, not just the pane record

## 0.17.0

### Features

- Flow v2: a JS dialect engine, hive members as Claude Code workflow nodes, board and rig (#149)
- The plugin is skill and manifests, nothing else: the last hook dies (#148) and the codex plugin refresh moves into the launch path (#147)
