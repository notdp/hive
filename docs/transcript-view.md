# The transcript mirror (`hive view`)

## Why it exists

An interactive Claude session — a desktop ccd, a session collected by `hive
join` — has no attachable pty. `claude attach` addresses bg jobs only, and
`--resume` does not attach to the running engine: it forks a second one,
which mints a bg job that then steals the member's deliveries. The only
remaining surface is the transcript JSONL, which claude appends event by
event as the turn unfolds. The display layer substitutes the viewer for any
claude member with no bg-job row.

## The boundary

Input: one transcript JSONL, nothing else. Output: one setting, `view.theme`,
written when `/theme` is used. No socket, no pty, no registry, no bus, no
hived. Keystrokes reach the session by no path at all, a property of the
process rather than a policy it enforces, which makes the mirror safe to
point at a live session someone else is driving. How a message got into that
session is [runtime-model.md](runtime-model.md)'s subject, not this one's.

The Claude Code UI is not a mirror of the transcript, and the viewer is not a
mirror of the UI. The JSONL records what was committed. The UI additionally
holds live uncommitted state that never becomes a row; the most visible case
is a message sitting in the input queue, which is on the user's screen, is
absent from the JSONL, and may be cancelled before it ever becomes a row.

## What the transcript actually contains

The parse layer is written against the format as observed; claude documents
none of it. The facts below come from scanning the ~2,200 transcripts under
`~/.claude/projects` (2026-08, claude 2.1.2xx). They are observations, not a
contract; re-check them after a claude upgrade before trusting a parser
change.

**Row types.** Many more types appear in the file than render. Besides
`user` / `assistant` / `attachment` / `queue-operation`, real sessions write
`system`, `last-prompt`, `custom-title`, `mode`, `atis-latch`, `pr-link`,
`file-history-snapshot` and more; the `attachment` family is larger still.
Everything unrecognized is dropped: the mirror renders the conversation rows
and not the session bookkeeping around them.

**HIVE message carriers.** A HIVE message arrives in five carriers, and the
fifth leaves no `user` row: bare; wrapped in claude's peer-message injection
at turn start; the same wrapper folded into a running turn; the retired
`<channel>` block, still present in old transcripts; and, when it arrived
mid-turn, recorded only as a `queued_command` attachment plus
`queue-operation` rows. A viewer that reads `user` rows alone silently loses
the fifth, which is the case whenever a peer messages an agent already
working.

**`queue-operation`** is an append-only log with four verbs: `enqueue`,
`dequeue`, `remove` (with and without `reason`), `popAll`. Only two states
are terminal, and both mean the model saw the message: `dequeue`, after which
a `user` row follows, and `remove` with `reason: absorbed_mid_turn`, after
which none ever will. `enqueue` draws nothing because the message can still
be cancelled. Absorption usually also writes the richer `queued_command`
attachment, the only record carrying the sender's origin; about 5% of
absorbed rows carry no `content` field and are drawable only from the
attachment beside them, which in every observed case exists and carries the
words, in an array prompt the parser refuses. This gives two draw paths and a
dedupe set keyed on the raw text.

**The `queued_command` row is backdated.** Its `timestamp` is when the
message was accepted, not when the row was appended: 92% of them read earlier
than the row physically above them, by seconds. Blocks are placed at file
position, so a mid-turn message's clock can read earlier than the block above
it on screen. The only thing this would corrupt is the thinking-duration
anchor, which is computed from adjacent row timestamps; attachment rows
return before the anchor advances.

**11% of `queued_command` prompts are not strings.** When the mid-turn
message carried an image the prompt is an array, and the parser requires a
string, so those messages never appear.

**Thinking text is unreliable.** A `thinking` content block often arrives
carrying a `signature` and an empty `thinking` string. This is not a version
or model cutover: across the corpus roughly half the transcripts persist the
text, half persist nothing, and 255 files flip mid-session. The header
renders either way, its duration computed from adjacent row timestamps rather
than from the model, but expanding a thinking block returns prose or `(no
content)` with roughly even odds. The density ladder lost its middle rung for
that reason: a level whose only function is expanding thinking is useless
when the content is missing half the time.

**Image payloads are in the file and are never read.** A pasted screenshot is
a few hundred KB of base64, which would render as thousands of wrapped
scrollback lines. In user rows and inside `tool_result` arrays alike it is
replaced by a numbered chip that keeps its position among the words it
arrived with.

**Ultra effort has no field.** Assistant rows carry an `effort` and it never
says `ultra`; entering ultra writes one `ultra_effort_enter` attachment,
once, usually far above the tail window. The entire prelude is therefore
scanned for it before the first draw, and it is the only thing recovered from
above the window. Real transcripts also record `ultra_effort_exit`. Nothing
reads that row, so the badge latches: a session that dropped out of ultra
still badges as ultra.

## What the viewer does not render

**Mid-turn messages.** A mid-turn message that is neither a HIVE envelope nor
`origin.kind: "human"` never renders. That category holds runtime plumbing
such as task notifications, kept off the screen deliberately. Accepted cost:
a peer message in some future non-HIVE shape would be dropped with no trace
on screen.

**The render window.** Only the last couple hundred rows of the backlog are
rendered; everything above is read once and discarded, save the ultra scan.
Scrolling up reaches the top of that window, not the top of the session.

## The rail

A pane 24 columns wide or narrower gets a status column instead of the
transcript: the team window draws the mirror as a 14-column rail on its left.
Everything on it is read off the transcript the viewer already parses — the
name is the `[…]` badge of the session's latest `custom-title` row, else the
`to=` of the latest HIVE envelope on screen, else `mirror`; busy and its
timer are the parser's `busy()` / `turn_started_ms()`; the count is HIVE
envelopes seen since the viewer opened (the backlog sets the baseline); the
age and first words are the last user or assistant block's, a HIVE body
behind `▏`, a human prompt behind `❯`, assistant markdown bare, folded to
three rows. Nothing here reads the registry or tmux. Widen the pane and the
transcript is back on the next poll; narrow it and the rail is. In rail mode
`on_key` returns early: only `q` (and Ctrl+C / Ctrl+Q) does anything, the
palette and the block viewer are neither drawn nor reachable, and a click
lands on nothing. A pane 9 rows or taller ends with a ` q quit` hint.

## What it borrows from Grok

The look and most of the interaction semantics come from Grok Build's pager
in three ways, and which one applies determines how a change is made.

**Linked**: `xai-grok-markdown` at a pinned rev, driving every assistant and
thinking body, and `tui-scrollbar`, the same crates.io scrollbar grok itself
uses.

**Vendored**: `grok-night.tmTheme` and `grok-day.tmTheme`, byte-identical to
the pager's assets at that rev, embedded with `include_bytes!`. The rev pin
keeps syntect's code-fence colors matching the two palettes, which were
transcribed by hand from grok's theme structs; moving one without the other
drifts code fences out of the theme.

**Reimplemented**: selection, turn jumps, tool blocks, the thinking blend,
the block viewer, the turn-status row, the mouse constants. Each hive-side
component names its grok source in a doc comment; the full source sits in the
cargo checkout (path in `AGENTS.md`) and is the reference for any change to a
mirrored component. The constants that look arbitrary are grok's, chosen
against grok's layout.

Every divergence is downstream of two facts: this surface is read-only, and
it lives in tmux. The composer never accepts text and the turn-status row has
no `[stop]`. The OSC 11 probe skips grok's DCS-passthrough retry because tmux
≥ 3.2 answers the bare query from inside a pane. Auto is the default
appearance and detection failure falls light, where grok defaults dark and
falls dark. Density is hive's own, with no grok counterpart.
