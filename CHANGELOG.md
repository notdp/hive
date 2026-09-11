# Changelog

One section per released version, newest first. The bump step in
AGENTS.md writes the section; `release-notes.yml` puts it on the GitHub
release.

## 0.21.0

### Features

- Team, task, hived and window each get their own lifetime. A team's hived sleeps after ten minutes with no window, no request in flight and no obligation, and the next verb wakes it; `hive ps` inventories hiveds, control clients, engine daemons and claude jobs read-only; requests are accepted on their own worker so a `ping` or `send` never waits on display sampling; node dispatches are journaled under `run/operations/` so a result survives a hived restart and an ambiguous one is reported rather than re-sent; the binary check behind the automatic re-exec hashes only when the file's fingerprint changes (#202)
- Teams end in the trash: `hive delete` archives the team directory whole into `$HIVE_HOME/trash/<id>/` (`--keep-workspace` with no purge date, `--delete-workspace` removes it outright), the collector archives a team cold for 30 days at the tail of a mutating verb, an archive is purged 30 days after it was quarantined or last written into, `hive gc run|keep|restore` drive it, and the name is free the moment the entry leaves the store. Archiving is a closed transaction: a close intent the registry refuses work into, a graceful hived stop, a second look, then the archive under the store lock (#202)
- The window's drag and the `hive mirror` choice are remembered per team instance in `state/hive-arrangement/window.json` and restored when the window is rebuilt; `hive layout auto` forgets the drag (#202)
- A grok member hive spawns runs always-approve (`--always-approve` on its pane TUI, `_meta.yoloMode` at mint and load); a human's own `hgrok` keeps grok's prompts. Hive's observer client no longer answers a shared permission request `cancelled` (#202)
- The desktop session's mirror starts collapsed — the desktop already shows that session — and the window records `@hive-mirror off` so the orch chip appears closed; `hive mirror on`, the chip or prefix+m draw it. The bare pane a withheld mirror leaves goes to the first member that needs one instead of a split beside an empty shell, and a desktop session runs `hive mirror` on its own team's window without a pane or a tmux client (#213)
- `hive setup` ends with a short launcher hint (#199)

### Fixes

- The tmux server hive starts itself is started from `$HIVE_HOME`, not the caller's directory, so a deleted worktree cannot leave a server whose every `-c` is skipped (#201)
- Hive panes are spawned through a shell-level `cd`, so a dead tmux cwd cannot swallow the spawn (#200)
- A dev build under a shared cargo `target-dir` logs at `dev` verbosity like any source checkout: the rule is now the `target/debug` or `target/release` path segment, not a `Cargo.toml` beside `target/` (#202)
- The hive skill lets a closing message go unanswered (#197)

### Internal

- `uv.lock` dropped, caches ignored, the hived restart note and the zh/ja launcher docs aligned (#198)

## 0.20.1

### Fixes

- `hive create` from an `hclaude` pane inside tmux moves that pane into a session named after the team with hive's two-line status bar, the same display an outside-tmux create gets. The pane, viewer and engine binding stay; the source window keeps a shell in the pane's place; the one attached client is switched over, otherwise the result carries an attach hint. A same-name non-hive session or a linked source window is refused before anything moves; `hive attach` rebuilds a lost window in the team session from either side of tmux; `delete --down` kills only a session hive marked. The team session's first pane is a placeholder until the registry commit, so no freshly started user shell is ever killed mid-rc (#196)

## 0.20.0

### Features

- Terminal handoff: `hclaude`, `hcodex` and `hgrok` in a plain terminal look and behave like the bare CLI, and `/hive:hive` (create or join) is the moment the conversation moves into the team window. The launcher stays in the original terminal owning the TUI, create/join stop the local viewer over a local control socket, bind the pane to the same engine (Claude bg job, Codex shared daemon thread, Grok launch leader via a member alias) and commit the roster, then the terminal attaches to the team session. Resuming an enrolled session opens its team; bare `claude`/`codex`/`grok` are not enrolled, the desktop app stays the one read-only mirror member; `hive attach` is only offered from the desktop app (#194, #195)

## 0.19.6

### Features

- The hive skill drops the humanDirective/source authorization relay: a member acts on the task sender's task and asks the task sender before going outside it, never a human provenance; "派发人" becomes "任务发送者" throughout, and `references/worktree.md` no longer gates push/PR on human authorization. Verified against the previous text on claude and codex: same pass profile except the new `directive-relay` situation, which the old text fails by asking for provenance (#193)

### Fixes

- Team panes are told the attached human terminal's real theme (tmux `client_theme`, mode 2031) instead of hive's own `view.theme` default, so a plain codex started in a team session on a dark terminal no longer flips to a light theme; explicit `HIVE_VIEW_THEME` / `view.theme` still win, sampling backs off to 60s once every client's theme is known, and headless sessions keep a provisional light answer (#192)

### Internal

- `tests/skill-evals/`: `check_benchmark.py` filters expectations per engine like `grade.py`, the entry situations drop the rule-location criterion that penalised ending the turn, and `directive-sourced` is replaced by `directive-relay` (#193)

## 0.19.5

### Features

- The hive skill (`plugins/hive/skills/hive/`) is rewritten from behavioral evals: same protocol facts, shorter text (SKILL.md 153 → 94 lines), guest orchestration spawns into the new team with `-t`, `hive team` is re-run only after `join` or to check a member, send failures split by cause, authorization is its own rule, and the message after create/spawn hands the human a runnable ```bash `hive attach <team>` block. Verified on claude and codex: identical pass profile to the previous text at 23% (claude) / 5% (codex) fewer tokens (#191)

### Internal

- `tests/skill-evals/`: a behavioral eval standard for the skill (16 situations, 152 expectations graded from an offline `hive` stub's call log plus an LLM grader) and headless executors for `claude -p` and `codex exec` with isolation gates; not wired into cargo/pytest because every run is a real model call (#191)

## 0.19.4

### Features

- `hive update` narrates each step on stderr as it starts (release lookup, download, checksum, unpack, the candidate's own `--version`, the install) and, on a terminal, shows curl's progress bar for the archive download; a pipe still gets the silent download and stdout keeps its one-line outcome (#189)

## 0.19.3

### Features

- one-command install: `install.sh` (served from `raw.githubusercontent.com/notdp/hive/main/install.sh`) downloads the dist installer to a file, runs it, then runs `hive plugin setup` from the directory the installer chose (the same precedence as dist: `HIVE_INSTALL_DIR`, `CARGO_DIST_FORCE_INSTALL_DIR`, `HIVE_UNMANAGED_INSTALL`, `CARGO_HOME`), never falling back to a binary already on PATH; `hive plugin setup` now exits 1 when any registration step fails (a CLI missing from PATH is skipped, not a failure), and says so when Claude refused the command-source review because the command ran inside a Claude Code session (#188)
- `hive uninstall`: removes the running binary, the dist receipt and the user-scope plugin registrations in claude and codex, stops hive's shared codex app-server; refuses while teams are registered unless `--force` (which runs `hive delete --down` on each), keeps `$HIVE_HOME` unless `--purge`, leaves external workspaces and the shell rc alone (#188)

### Internal

- `hive plugin` is the skill install alone (`setup`, and the hidden `sync` Claude re-runs each session): the plugin lifecycle hive kept for itself (`list` / `enable` / `disable`, an install dir and a state file) and its one plugin `notify` are gone; the hived's idle watcher is the user setting `notify.idle` instead, on unless set to `false` (#187)

## 0.19.2

### Fixes

- the shared codex app-server daemon is replaced when `auth.json` moves to another account or workspace: codex reloads auth only for the same account id (`reload_if_account_id_matches`), so after a cross-account login every member's turn ended with "you have since logged out or signed in to another account". hive records the account the daemon was spawned with (`hive-shared.auth`), asks a daemon without a baseline for its own account over `account/rateLimits/read`, and the hived's tick and `spawn_daemon` replace a stale daemon under one flock per CODEX_HOME, signalling only a pid that is still this socket's app-server and clearing records only once the process is gone; attached TUIs reconnect on their own, and the hived keeps its daemon client (and the workflow turns it tracks) whenever the daemon is reused (#185)
- `hive join` runs the same claude pane↔job gate as `hive create` before any tag, context or roster write, so a bare interactive claude pane is refused up front instead of enrolled with an empty session id when `--no-notify` skipped the delivery check; the refusal points at the managed launcher for a fresh session (#186)

## 0.19.1

### Fixes

- claude bg engines are born and woken with the pane's terminal — `TERM` from tmux's `default-terminal`, `COLORTERM=truecolor`, inherited `NO_COLOR` dropped — so a member spawned from a desktop Claude session or a `TERM=dumb` tool shell renders in color; `ensure_hived`'s identity ping waits 5s through a hived tick instead of 0.1s, so a busy hived is no longer killed and restarted with the caller's env (#184)
- the hived's control-mode client attaches with the server's `default-terminal`, not the hived's own `TERM`, so codex draws when the team was created from an agent's tool shell (#182)
- grok session creation is separated from the replay timeout (#183)

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
