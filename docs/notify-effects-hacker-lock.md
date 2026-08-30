# Notify Effect: Hacker Lock

The full-pane attention animation Hive plays on a pane it flashed, ending on a
locked-target card. The mechanism is `notify_ui.rs`.

## Why it plays on arrival

Firing a notification and playing the animation are deliberately split. Fire
time gets only the durable, cheap signals and draws nothing; the cinematic
second is spent after the human selects the window, once per flash. Animating a
pane nobody is looking at buys nothing, and the fire-time signals are the ones
that have to persist unattended.

It is loud on purpose: a full-pane effect aimed at a pane the human has just
chosen to look at. There is no theme switch and no opt-out. Disabling the
`notify` plugin silences the hived's idle watcher only — the manual
`hive notify` is not plugin-gated.

## The border marker is the reliable half

The popup is best-effort and fails silently by construction; nothing on this
path retries or reports. What survives a lost popup is `@hive-notify-active` on
the pane, so the attention signal is carried by tmux state rather than by the
animation. Changes here are judged on whether the marker still lands; the popup
is allowed to be lost.

## Known edges

- **`python3` is an unshipped dependency.** The animation is the last thing on
  this path that the single Rust binary cannot run by itself — the equivalent
  inline interpreter probes elsewhere (cvim) were replaced by hidden
  subcommands, this one was not — and nothing checks for an interpreter. A
  missing `python3` is a popup that opens and closes immediately (`-E`), and
  nothing records it: the attention script discards the popup's result and
  exits 0 either way, so the `returncode` on the `attention.run` record reads
  the same for a failed play and a successful one.
- **The popup is client-local.** `display-popup` renders on one client. On a
  multi-client session only the client whose `after-select-window` fired sees
  the animation; the others just watch the flash disappear.
- **Small panes overdraw.** The animation clamps its viewport *up* to 50x16. In
  a pane narrower or shorter than that, the layout is computed for a viewport
  the pane does not have and the drawing runs past its own edge.
- **The per-flash script is deleted only by its own EXIT trap.** Both live
  callers of `clear_stale_notify` pass `remove_attention: false`. When the hived
  clears the flash on the active window before the select hook gets to it, the
  window option pointing at the script is dropped and the script is orphaned in
  `$TMPDIR`.
- **A failed script write leaves the window renamed.** The rename happens before
  the flash token is set, and the select hook is gated on that token, so a write
  error strands the badge in the window name. The original name is stored
  separately, so the next successful flash-and-clear on that window repairs it.
