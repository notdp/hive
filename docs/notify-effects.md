# Notify effect: attention marks

What a notification leaves on tmux and who takes it off again. The mechanism
is `notify_ui.rs`; hive draws nothing itself.

## Fire time

`notify` (the hived's idle watcher, or a manual `hive notify`) resolves the
pane's window — the team window when the pane is a parked mirror in a hidden
window (`@hive-hidden`), since nobody looks at that one and its select hook
would never fire — and, unless that window is the one the most recent client
is already looking at, marks it:

- `@hive-notify-token` on the window — `<pane>:<ms>`, the identity of this
  fire; the select hook is gated on it being set;
- `@hive-notify-hook` on the window — the name of the hook that will clear it;
- `@hive-notify-text` on the window — `<agent>: <message>` (the message alone
  when the pane has no `@hive-agent`), with `#` doubled because the status
  line draws the value verbatim and `#[` would open a style;
- `@hive-notify-active` on the pane — the same token;

then rings the bell on the pane's tty. The marks are durable tmux state: they
persist unattended until something clears them, which is the point — the
human may be hours away.

Rendering is someone else's: the team session's status bar (`tmux/status.rs`,
see `runtime-model.md`) draws the pane's chip in the attention colour while
`@hive-notify-active` is set and prints `@hive-notify-text` at the head of its
second line; the pane border shows `[!]`. A window that is not a team-session
window shows only the border mark.

## Clearing

`mark_attention` installs one stable session hook,
`after-select-window[900001]`, that runs `hive notify-hook --cleanup-selected
<session>:<index> --client <tty>` when the token is set. `cleanup_selected_window`
reads the token and calls `clear_stale_notify`, which clears the
`@hive-notify-active` of every pane in the window whose value equals the
token, then the three window options, the token last: each clear is its own
tmux call, and whoever polls the token sees every other carrier gone with it. The hived clears the same way for the window a client is
already on (`idle_notify.rs::clear_active_window_token`), so a fire that lands
on the focused window does not stick.

Suppression at fire time and clearing on select are the same test from two
sides: the marks exist only while the human is not looking at the window.

## Known edges

- **The hook names the binary by absolute path.** `run-shell` runs with the
  tmux server's environment, so the hook command carries `HIVE_BIN` or
  `current_exe` resolved at fire time. A binary moved after the fire leaves
  the token in place until the hived's active-window clear reaches it.
- **The hived can beat the hook.** Both clear paths go through
  `clear_stale_notify`; whichever runs first wins and the other finds nothing
  to do. The `clear.*` records in `notify.jsonl` name their `source`.
- **Only the notified window's panes are reconciled.** A pane moved out of the
  window with `break-pane` while marked keeps its `@hive-notify-active` until
  a later fire on that pane overwrites it — except the parked mirror, whose
  mark `hive mirror on` and the attach heal clear when they join it back
  (`_join_parked_pane`).
- **A second fire replaces the first.** The token, text and pane mark are
  rewritten; nothing accumulates, and the bar shows the newest message only.
