# Notify Effect: Hacker Lock

The pane-attention animation Hive plays when the human returns to a window it
flashed. It ships: `POPUP_CODE` (`crates/hive/src/notify_ui.rs:36`) is a
pure-stdlib Python program delivered as a heredoc into a borderless
`tmux display-popup` sized to the target pane, and it is the only Python left
in the notify path.

The effect ends on a locked target card:

```text
TARGET LOCKED: BOBO
window=613:6 pane=%4143
```

## Where it sits in the notify path

`notify()` (`notify_ui.rs:733`) raises the durable surface and *arms* the
effect; it never plays it.

1. `notify()` suppresses everything when the target window is already the most
   recent client window (`_target_window_is_focused`, `notify_ui.rs:265`) —
   payload `surface: "suppressed"`, `suppressionReason: "focused_window"`.
2. Otherwise `show_window_flash` (`notify_ui.rs:639`) renames the window to
   `<original> · 🤖 <agent>`, sets `window-status-style` and
   `window-status-current-style` to `reverse,bold`, writes a per-flash bash
   script into `$TMPDIR` (mode 0755, `notify_ui.rs:275`), and records the path
   in `@hive-notify-attention` plus the flash token in `@hive-notify-token` on
   the window. `_ring_terminal_bell` (`notify_ui.rs:590`) writes one `\x07` to
   the pane tty.
3. The session hook `after-select-window[900001]` (`SELECT_HOOK_NAME`,
   `notify_ui.rs:32`), re-installed on every flash, is gated
   `if-shell -F '#{?@hive-notify-token,1,0}'` and calls back
   `hive notify-hook --cleanup-selected '#{session_name}:#{window_index}'
   --client '#{client_tty}'` (`notify_ui.rs:336`).
4. `cleanup_selected_window` (`notify_ui.rs:563`) clears the durable flash
   state, then runs the saved script: it sets `@hive-notify-active` on the
   target pane, execs `hive notify-attention`, sleeps 0.18s, and on its EXIT
   trap unsets the pane option and deletes itself.
5. `attention_main` (`notify_ui.rs:203`) resolves the pane geometry and opens
   the popup.

The animation therefore plays **on arrival** — once per flash, on the client
that selected the window. Fire time gets the rename, the status style and the
bell, and draws nothing.

Two producers reach `notify()`: the hived idle watcher (`hived.rs:2341`, after
`IDLE_NOTIFY_THRESHOLD_SECONDS = 5.0`, `hived.rs:34`, and only while the
`notify` plugin is enabled — `hived.rs:2105`), and the manual
`hive notify <message>` (`cli/rest.rs:1644`), which is not plugin-gated.
`show_window_flash` takes an `animate_on_arrival` flag; `notify()` always
passes `true` (`notify_ui.rs:769`) and nothing else in the binary calls it, so
the un-animated branch exists only for tests.

`notify-hook` and `notify-attention` are hidden subcommands dispatched at
`cli/mod.rs:2357-2358`. The hook names the binary by absolute path
(`self_exe()`, overridable with `HIVE_BIN`) because `run-shell` executes with
the tmux server's environment, not the caller's.

## The popup command

```text
tmux display-popup [-c <client_tty>] -t <pane> -B \
  -x '#{popup_pane_left}' -y '#{popup_pane_top}' \
  -w <pane_width> -h <pane_height> -E \
  "HIVE_NOTIFY_AGENT=… HIVE_NOTIFY_WINDOW=… HIVE_NOTIFY_PANE_ID=… \
   python3 - <<'PYPOPUP' … PYPOPUP"
```

argv is assembled in `tmux::display_popup` (`crates/hive/src/tmux.rs:1227`);
the call site is `notify_ui.rs:250`.

- Size comes from `#{pane_width}`/`#{pane_height}`, position from tmux's
  popup-relative formats. Do not pass `#{pane_top}` as a numeric `-y`: tmux
  anchors a numeric `-y` by the popup's **bottom** edge, which throws a
  full-height popup for a lower split into the pane above.
  `#{popup_pane_top}` is the only correct anchor here — the same trap is
  documented at `crates/hive/assets/cvim/bin/cvim-command:208`.
- `-c` is omitted when the client is empty, and `_run_attention_script` blanks
  a client string that still contains the literal `#{client_tty}`
  (`notify_ui.rs:394`), so an unexpanded format never reaches tmux.
- The agent is `@hive-agent` on the pane (falling back to `target`), the label
  is `#{session_name}:#{window_index}`, the pane id is the target itself. All
  three are `shlex_quote`d into the env prefix.
- `python3` must be on PATH inside the popup, and nothing checks:
  `display_popup` discards its result, so a missing interpreter is a popup
  that opens and immediately closes (`-E`), leaving no evidence beyond the
  script's exit code in `attention.run`.

Everything the effect draws goes to the popup's own pty. The only byte Hive
writes to the target pane's tty on this path is the bell.

## The animation

Beats, in order (`notify_ui.rs:36-160`):

1. Four HUD corner brackets, redrawn each frame at margins interpolated by a
   cubic ease-out (`1 - (1 - t) ** 3`), converging from the pane edges toward
   the centre.
2. Ten random glyph runs per frame, 4-12 chars from `01ABCDEF/%#@{}[]`, in
   colour 28.
3. From the halfway frame on, a centred `SCAN <12 glyphs>` line in colour 82.
4. A boxed card alternating colour 220 / 46 carrying
   `TARGET LOCKED: <AGENT>` (the agent name upper-cased), with
   `window=<session:index> pane=<%id>` two rows below in colour 245.
5. Collapse through horizontal bars of width 40/24/10/2, clear, restore the
   cursor.

| phase | frames | delay | seconds |
|---|---|---|---|
| scan / converge | 14 | 0.032 | 0.448 |
| lock pulse | 4 | 0.055 | 0.220 |
| hold | — | 0.100 | 0.100 |
| collapse | 4 | 0.032 | 0.128 |

≈0.9s of sleeps, plus the script's trailing `sleep 0.18`.
`test_pane_attention_animation_timing_is_fast` (`notify_ui.rs:1450`) pins
those four constants against creep. `_run_attention_script` kills the script
at 5s (`notify_ui.rs:434`); the tmux call itself is bounded at 30s.

`random.seed(4143)` (`notify_ui.rs:48`) — the noise is the same on every play.

## The pane marker

The reliable half of the effect is the pane option plus the border format, not
the popup. `@hive-notify-active` is set at flash time when the animation is
armed (`notify_ui.rs:711`) and lives until the window is cleared, then set
again by the attention script for the popup's duration.
`_HIVE_PANE_BORDER_FORMAT` (`tmux.rs:970`) renders it as a bold colour-220
`[!]` prefix ahead of the member name, applied through
`enable_pane_border_status` (`tmux.rs:976`) and asserted against real tmux in
`crates/hive/tests/pane_border.rs:96`.

Popup resolution failures are silent by design: `attention_main` returns 0
before opening anything when the geometry does not parse
(`notify_ui.rs:222-227`), and `display_popup` never raises. The border marker
is what survives.

## Sharp edges

- **Client-local.** `display-popup` renders on one client. On a multi-client
  session only the client whose `after-select-window` fired sees the
  animation; the others just watch the flash disappear.
- **Small panes overdraw.** `cols`/`rows` are clamped *up* to 50×16
  (`notify_ui.rs:46-47`), and `at()` clips against the clamped values
  (`notify_ui.rs:69`). In a popup narrower than 50 columns or shorter than 16
  rows the layout is computed for a viewport the pane does not have and the
  drawing runs past its own edge.
- **The script is deleted only by its own EXIT trap.** Both live callers of
  `clear_stale_notify` pass `remove_attention: false` (`notify_ui.rs:585`,
  `hived.rs:2095`); no production path passes `true`. When the hived clears
  the flash on the active window before the select hook gets to it,
  `@hive-notify-attention` is dropped and the script is orphaned in `$TMPDIR`.
- **A failed script write leaves the window renamed.** `show_window_flash`
  renames first and propagates the write error with `?` (`notify_ui.rs:696`),
  before `@hive-notify-token` is set — and the select hook is gated on that
  token, so nothing restores the name. `@hive-notify-original-name` survives,
  so the next successful flash-and-clear on that window repairs it.
- **Break-pane moves are not chased.** Clearing only reconciles panes still in
  the window (`notify_ui.rs:538`); a pane carried to another window keeps its
  `@hive-notify-active` marker.
- It is loud on purpose: a full-pane cinematic second aimed at a pane the
  human has just chosen to look at. There is no theme switch and no opt-out
  short of disabling the `notify` plugin (which only silences the hived idle
  watcher, not `hive notify`).

## Observability

`<workspace>/run/notify.jsonl`, falling back to
`${XDG_CACHE_HOME:-~/.cache}/hive/notify.jsonl` when no workspace resolves
(`crates/hive/src/devlog.rs:49`, `:53`). Every event on this path is
business-path and never verbosity-filtered (`devlog.rs:15`, `:120`):

- `notify.call` — `pane`, `window`, `agent`, `client_mode`, `suppressed`
- `flash.start` / `flash.done` — token rotation, `animate`,
  `attention_script_created`
- `bell` — `tty_present`, `success`
- `cleanup_selected.start` — the token the hook saw
- `clear.start` / `clear.done` — `source` (`select_hook`,
  `hived.active_window`), `pane_active_matches`
- `attention.run` — `script_present`, `client_present`, `returncode`, or
  `error` (`missing_file`, `timeout`)

`attention.run` is the only record that the animation ran at all; the popup
writes nothing of its own.
