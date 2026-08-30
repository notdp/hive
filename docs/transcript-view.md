# The Transcript Mirror (`hive view`)

`hive view <SESSION_ID>` renders one Claude Code session read-only and
follows it live. It exists because an interactive claude session — a desktop
ccd, a session collected by `hive join` — has no attachable pty: `claude
attach` addresses jobs, and resuming would fork a second engine. `hive
attach` therefore substitutes `hive view <sessionId>` for any claude member
whose pane has no bg-job record, instead of the usual resume launcher
(`crates/hive/src/cli/rest.rs:1282`). The member's `sessionId` is in its
registry roster row and in `hive doctor <member>`'s runtime payload
(`crates/hive/src/hived.rs:1301`).

Three files carry it: `transcript_view.rs` (JSONL → display blocks, plus the
plain non-tty stream), `transcript_tui.rs` (ratatui renderer), and
`transcript_tui/interact.rs` (fold, selection, palette, `/find`).
`view_theme.rs` resolves the palette.

## Scope

This document covers the mirror's input contract, the shapes it decodes, the
display model, theme resolution, and where it is deliberately blind. It does
not cover message delivery itself — that is
[runtime-model.md](runtime-model.md).

## What It Mirrors, And Why It Cannot Do More

The **only** input is the session's transcript JSONL. `transcript_path`
scans every `~/.claude/projects/*/` directory for `<session_id>.jsonl` and
takes the newest mtime (`crates/hive/src/transcript_view.rs:44`); the id must
match the filename exactly, there is no prefix search. A missing file prints
`no transcript for session '<id>'` and exits 1. `follow` then picks the
renderer by `isatty(stdout)`: a tty gets the TUI, a pipe gets the legacy ANSI
stream (`transcript_view.rs:1646`).

There is no second channel. The TUI opens the file once
(`transcript_tui.rs:2496`), drains whatever has been appended before each
draw, and wakes at least every 250 ms (`POLL_MS`). It writes nothing
anywhere. No socket, no pty, no `claude attach`. Keystrokes go nowhere by
construction — not by policy, but because the process holds no handle that
could reach the session.

The consequence is the thing to internalize: **the Claude Code UI is not a
mirror of the transcript, so this is not a mirror of the UI.** The JSONL is
an append-only record of what was committed; the UI additionally holds live,
uncommitted state that never becomes a row. What that costs, concretely:

- A message sitting in the session's input queue is on the UI's screen but
  not here. The parser ignores `queue-operation` `enqueue` rows — an
  enqueued message can still be cancelled — and only draws terminal states
  (see below).
- Thinking text and image pixels are never in the file at all (next two
  sections).
- Row kinds the parser drops on the floor: `push_entries` returns early for
  any `type` outside `user` / `assistant` / `attachment` / `queue-operation`
  (`transcript_view.rs:1253`). Real sessions also write `system`,
  `file-history-snapshot`, `last-prompt`, `custom-title`, `agent-name`,
  `mode`, `permission-mode`, `atis-latch` — none of which render. Inside the
  `attachment` branch the same is true: only `queued_command` and
  `ultra_effort_enter` are read, so `compact_file_reference`,
  `command_permissions`, hook output, and the rest are dropped too.

## How a HIVE Message Arrives

Five carriers land in the same `HiveMessage` envelope
(`transcript_view.rs:295`). The first four are `user` rows; the fifth never
produces a `user` row at all.

1. **Bare** — typed straight into the pane. The row's text is nothing but
   `<HIVE …>body</HIVE>`.
2. **Claude's peer-message wrapper at turn start** — lead line `Another
   Claude session sent a message:` above the envelope, then a trailing safety
   paragraph beginning `This came from another Claude session`
   (`transcript_view.rs:257-259`). Peeled to `injected: true, mid_turn:
   false`.
3. **The same wrapper folded into a running turn** — lead line `Another
   Claude session sent a message while you were working:`. Peeled to
   `injected: true, mid_turn: true`.
4. **The retired `<channel source=… msg_id=…>` block**, still present in old
   transcripts. Stripped before the injection wrapper is even tested
   (`strip_channel_wrapper`, `transcript_view.rs:262`).
5. **Absorbed mid-turn** — recorded *only* as a `queued_command` attachment
   plus `queue-operation` rows. No `user` row ever follows, so a viewer that
   reads only `user` rows silently drops it.

Parsing is deliberately strict: once the wrapper is peeled the row must be
*nothing but* the envelope — starts with `<HIVE`, ends with `</HIVE>`, and
the character after `HIVE` is whitespace or `>`. Prose that merely quotes the
tag (skill docs, this repo's own specs) stays ordinary user text. Attributes
read: `from`, `msgId`, `reply-to`, `artifact`; everything else is ignored.

Each distinct `from` draws a different Nerd Font avatar — the sender name is
FNV-1a hashed into a 7-glyph pool, then walked forward past anything a
teammate already took (`transcript_view.rs:253`, `:943`). Stable per
transcript, and it survives until the pool is exhausted.

### The queue state machine

`queue-operation` rows form an append-only log. Across the transcripts under
`~/.claude/projects` the operations observed are `enqueue`, `dequeue`,
`remove` (with and without `reason`), and `popAll`. Only two are terminal,
and **both mean the model saw the message**:

| row | meaning | what draws it |
|---|---|---|
| `enqueue` | queued, still cancellable | nothing — not terminal |
| `dequeue` | left the queue to open its own turn | the `user` row that follows |
| `remove` + `reason: absorbed_mid_turn` | folded into the turn already running | the `queued_command` attachment, else this row |
| `remove` (no reason), `popAll` | cancelled / cleared | nothing |

Absorption usually also writes the richer `queued_command` attachment, which
is the one that carries the origin, so that is the preferred draw
(`push_queued_command`, `transcript_view.rs:1147`). A few absorptions leave
only the `queue-operation` row, and a HIVE envelope reaching its terminal
state unrendered is drawn from there instead (`push_absorbed_queue_row`,
`:1193`). A `queued_texts` set keeps the two paths — and a later duplicate
`user` row — from drawing the same text twice.

The attachment is drawn when it parses as a HIVE envelope **or** its
`attachment.origin.kind` is `human`. Runtime plumbing (task notifications)
carries no `origin` and stays out.

**The attachment row is backdated.** Its `timestamp` is stamped when the
message was accepted, not when the row was appended, so it can precede rows
already written above it. From a real transcript
(`~/.claude/projects/-Users-notdp--dotfiles--claude-worktrees-pxsxzj-xidian-open-250561/3b6d3207-…jsonl`):
line 15 is a `user` row at `2026-08-08T00:46:55.637Z`, line 17 the
`queued_command` at `00:46:48.984Z` — seven seconds earlier, two lines later.
The block is placed at file position, so its clock can read *earlier* than
the block above it. This does not corrupt anything else: `push_entries`
returns before `prev_row_ms` is updated for attachment rows, so a backdated
timestamp never becomes a thinking-duration anchor.

## What Claude Does Not Persist

**Thinking text is gone.** A `thinking` content block in a live transcript
carries a `signature` and an empty `thinking` string:

```json
{"type": "thinking", "thinking": "", "signature": "CAIS/wUKpgEIERgCKkC9u8ia…"}
```

The mirror still renders a `Thought for 14.3s` header — the duration is
computed from adjacent row timestamps, not from the model — but expanding one
shows nothing, and the block viewer says `(no content)`. This is why the
density ladder lost its middle rung: a `thinking` level between normal and
verbose expanded a row of empty blocks
(`crates/hive/src/transcript_tui/interact.rs:30`).

**Image payloads are never read.** An `image` content block in a user row
becomes a path-free numbered chip `[Image #N]`, keeping its position among
the words it arrived with (`image_chip`, `transcript_view.rs:676`). The same
chip replaces `image` blocks inside a `tool_result` array before that array
is serialized into the outcome text (`summarize_images`, `:682`) — without
it, a screenshot poured a few hundred KB of base64 into the scrollback as
thousands of wrapped lines.

**Ultra effort arrives as an attachment, not a field.** Assistant rows carry
an `effort` field, but it never says `ultra`; entering ultra writes
`{"type":"attachment","attachment":{"type":"ultra_effort_enter","reminderType":"full"|"sparse"}}`
once and rarely. Because that row usually sits far above the tail window, the
TUI scans every line of the prelude for it before rendering anything —
`note_session_state` (`transcript_view.rs:1036`) is called on the whole
pre-tail slice at `transcript_tui.rs:2505`. It is the *only* thing recovered
from above the window; branch, cwd, model, and context usage come from tail
rows alone.

## The Display Model

Seven block kinds (`DisplayBlock`, `transcript_view.rs:471`): `User`,
`ToolGroup`, `Run`, `Tool`, `Thinking`, `Assistant`, `WorkedFor`. Every block
is wrapped in an `Entry` carrying a `u64` id minted once at birth, monotonic
in display order, surviving finalization (`:502`, `alloc_id` `:986`) — that
id is what fold state, selection, and the render cache key on across live
re-parses.

Blocks finalize late. Read-only tools aggregate into one `ToolGroup` — `Read`
(bucketed as *skill* when the path ends `SKILL.md`), `Grep`/`Glob`, `LS`,
`WebFetch`, `WebSearch`, `Skill` (`member_kind`, `:823`) — and the group
stays open until a non-member is queued behind it. `Bash` becomes a `Run`;
anything else a `Tool`; both finalize when their `tool_result` attaches
(`drain_settled`, `:1097`). The renderer draws the still-open tail from
`pending_entries`, so a running tool is on screen with its final id already
assigned.

`WorkedFor` is emitted only when the *next* user message closes the turn, so
the live view synthesizes the line as soon as a turn settles
(`open_turn_worked_secs`, `:1022`; `transcript_tui.rs:1450`).

**Folding.** Four families (`FoldKind`, `interact.rs:16`):

| family | blocks | default |
|---|---|---|
| `Thinking` | Thinking | expanded at Verbose only |
| `Tool` | ToolGroup, Run, Tool | expanded at Verbose only |
| `User` | user bands, HIVE bands | collapsed to 3 lines at every density |
| `Fixed` | Assistant, WorkedFor | never folds |

Per-block pins layer over the density default; a pin that matches the default
erases itself, and changing density clears every pin (`set_density`,
`interact.rs:118`).

**Density has two levels**, Normal and Verbose, cycled with Ctrl+O. The
current one is named on the composer box's bottom border, next to the model
and effort badge (`draw_composer`, `transcript_tui.rs:2115`).

**Selection** walks selectable entries; `WorkedFor` is the only
non-selectable kind. Moving down past the last entry re-engages follow and
jumps to the bottom (`SelectMove::Overscroll`). Shift+Left/Right jump between
turn anchors — `User` blocks are the anchors — and Shift+Left is two-stage:
from inside a response it snaps to that turn's own prompt first, then walks
back (`prev_turn`, `interact.rs:219`). The selected block is framed by
fg-only corners and sides drawn one row outside it, dashed `┆` where the
viewport clips it.

**Block viewer** (Enter or Ctrl+F) is a centred overlay showing one block in
full: user text *including* whatever wrapper carried it, assistant and
thinking as markdown, `Run` as `$ command` plus outcome, `Tool` as its full
pretty-printed input JSON plus outcome, `ToolGroup` member by member
(`viewer_lines`, `transcript_tui.rs:1211`).

**Slash palette** (`/`) types into the composer box with the dropdown
anchored above it. Four commands (`PALETTE_COMMANDS`, `interact.rs:303`):
`/theme`, `/view <normal|verbose>`, `/find <text>`, `/quit`. The typed word
is matched as a case-insensitive subsequence; Enter on a bare `/view` or
`/find` autocompletes to `"<name> "` and stays open.

**`/find`** is a case-insensitive substring search over a per-block search
string (`search_text`, `transcript_tui.rs:986` — tool results and input JSON
included), skipping `WorkedFor`, starting after the current selection and
wrapping around. `n` / `N` cycle forward and back.

### Key map

Scrollback:

| key | action |
|---|---|
| `↑` `↓` | select previous / next block (down past the last re-engages follow) |
| `←` `→` | collapse / expand the selected block |
| `Shift+←` `Shift+→` | previous / next turn prompt, snapped to the top |
| `Enter`, `Ctrl+F` | open the block viewer |
| `Ctrl+E` | expand every thinking block, or collapse them all |
| `Ctrl+O` | cycle density Normal ↔ Verbose |
| `/` | slash palette |
| `n` `N` | next / previous `/find` match |
| `j` `k`, `Ctrl+J` `Ctrl+K` | scroll one line |
| `Ctrl+D` `Ctrl+U` | half page |
| `PageDown` `PageUp` | page (viewport − 2 rows) |
| `g` `G` | top / bottom (`G` re-engages follow) |
| wheel | 3 lines |
| click / double-click | select / toggle fold (300 ms window) |
| `q`, `Ctrl+C`, `Ctrl+Q` | quit |

Block viewer:

| key | action |
|---|---|
| `Enter`, `Esc`, `q`, `Ctrl+F` | close |
| `j` `k`, `↑` `↓`, `Ctrl+J` `Ctrl+K` | one line |
| `Ctrl+D` `Ctrl+U` | half page |
| `PageDown` `PageUp` | page |
| `g` `G` | ends |

Palette:

| key | action |
|---|---|
| `↑` `↓` | move in the dropdown |
| `Enter` | run, or autocomplete `/view` / `/find` |
| `Backspace` | delete; deleting the leading `/` closes |
| `Esc` | close |

`Ctrl+C` and `Ctrl+Q` quit from anywhere, palette and viewer included
(`on_key`, `transcript_tui.rs:1622`). Bare `q` quits only from the
scrollback — in the palette it types.

## Theme Resolution

Two palettes, `GROKNIGHT` (dark) and `GROKDAY` (light), both transcribed from
grok's own theme structs (`view_theme.rs:100`, `:145`). The precedence chain
(`resolve_pref`, `:257`):

1. `HIVE_VIEW_THEME`
2. `view.theme` in `$HIVE_HOME/settings.json`
3. otherwise `auto`

Unparseable values fall through to the next source rather than erroring.
Accepted names: `auto`/`system`, `dark`/`night`/`groknight`/`grok-night`,
`light`/`day`/`grokday`/`grok-day` (`parse_theme_pref`, `:246`).

`auto` runs a three-step detection chain (`detect_appearance`, `:303`):

1. the `HIVE_APPEARANCE` stamp — `dark`/`night` or `light`/`day`;
2. an OSC 11 background query — the bare `ESC ] 11 ; ? BEL`, 500 ms deadline,
   answered by tmux ≥ 3.2 from inside a pane. The reply's `rgb:RRRR/GGGG/BBBB`
   is converted to BT.709 relative luminance with sRGB gamma; `Y < 0.5` is
   dark (`:368`). Skipped entirely unless both stdin and stdout are ttys;
3. `COLORFGBG` polarity — last field is the background; 0-6 and 8 dark, 7 and
   9-15 light.

Detection failure or ambiguity resolves **light** (`resolve_kind`, `:266`).
Grok falls dark here; this is a deliberate delta.

The whole resolution must run before crossterm owns the terminal, because the
OSC 11 probe reads raw stdin itself — hence `active_theme_kind()` is the
first statement of `run()` (`transcript_tui.rs:2492`, `:2495`), ahead of
`enable_raw_mode`.

`/theme` switches live and persists the result to `view.theme`. `/theme auto`
*mid-session* re-detects from the env stamps only: the OSC 11 probe needs a
raw tty that crossterm now holds (`apply_theme`, `transcript_tui.rs:1578`).

The markdown engine is themed from the same struct: `grok_md` builds grok's
`MarkdownStyle` from the theme's `md_*` fields and picks the matching
syntect theme, `grok-night.tmTheme` or `grok-day.tmTheme`
(`transcript_view.rs:71`). The plain piped stream has no terminal to probe
and is hardcoded to groknight.

## Sharp Edges

**The tail window is 200 rows.** `run()` renders only the last `TAIL_EVENTS`
lines of the backlog (`transcript_tui.rs:59`, `:2503`); everything above is
read once and thrown away, save the `ultra_effort_enter` scan. Scrolling up
reaches the top of that window, not the top of the session. The mirror is a
tail, not an archive — for the full history, read the JSONL. The plain
non-tty stream's window is 40 rows (`transcript_view.rs:35`).

**One tool result stores at most 512 KiB.** Longer results are cut at a char
boundary and flagged; the renderer appends a muted `… output truncated`
(`TOOL_RESULT_MAX_BYTES`, `transcript_view.rs:42`). In the TUI a collapsed
tool block shows no output at all — expand it, or open the block viewer. The
plain stream's one-line summary clips at 160 chars on top of the cap
(`first_line`, `:593`).

**`ultra` latches for the rest of the file.** `self.ultra` is set by
`ultra_effort_enter` and never cleared, so the composer badge keeps saying
`(ultra)` (`effort()`, `transcript_view.rs:1058`). Real transcripts *do*
record leaving — `{"attachment":{"type":"ultra_effort_exit"},"type":"attachment"}`
appears across sessions under `~/.claude/projects` — and the parser reads
neither that row nor a repeated `enter`. A session that dropped out of ultra
still badges as ultra.

**A `queued_command` whose `prompt` is not a string is dropped.**
`push_queued_command` requires `attachment.prompt` to be a JSON string
(`transcript_view.rs:1152`). Real transcripts carry array prompts when the
mid-turn message included an image — those messages never appear.

**An `absorbed_mid_turn` row without `content` is invisible.** A minority of
them carry no content field; if no `queued_command` attachment covered the
same message, nothing draws it.

**A mid-turn message that is neither a HIVE envelope nor `origin.kind:
"human"` never appears** (`transcript_view.rs:1164`). That is where task
notifications live, and it is intentional — but it also means a peer message
in some future non-HIVE shape would vanish silently.

**The file is opened once and never reopened.** The reader holds one `File`
handle from startup (`transcript_tui.rs:2496`) and only reads appended bytes.
Nothing checks inode or size, so a transcript rotated or truncated in place
leaves the viewer reading a stale descriptor.

**Nerd Font glyphs are unconditional.** The user icon, the agent avatars, and
the block bullets are all private-use codepoints
(`USER_ICON`, `transcript_tui.rs:429`; `AGENT_ICONS`,
`transcript_view.rs:253`). A terminal without a patched font shows tofu.

**`HIVE_VIEW_BAND`** picks the HIVE band treatment — `2` for a shallow
neutral fill, anything else for the default rail-only look
(`hive_band_style`, `transcript_tui.rs:499`). It is an unfinished look
selector, not a supported setting.

## What This Borrows From Grok, And Where That Source Is

The look and most of the interaction semantics are ported from Grok Build's
pager. One crate is actually linked; the rest was read and reimplemented.

Linked (`crates/hive/Cargo.toml`):

- `xai-grok-markdown`, git `github.com/xai-org/grok-build` rev `bc7f02e` —
  the markdown engine behind every assistant and thinking body, driven
  through a `MarkdownStyle` built from the active `ViewTheme`.
- `tui-scrollbar` 0.2 — the same crates.io scrollbar grok's
  `xai-grok-pager-render/src/render/scrollbar.rs` uses.

Vendored into this repo: `crates/hive/assets/grok-night.tmTheme` and
`grok-day.tmTheme`, byte-identical to
`xai-grok-pager-render/assets/` at that rev, embedded with `include_bytes!`
and handed to syntect for code-fence highlighting.

Reimplemented against the upstream source, which cargo keeps at
`~/.cargo/git/checkouts/grok-build-<hash>/bc7f02e/crates/codegen/`. The
hive-side comments cite these by their grok path, so the next change starts
by reading them there:

| hive code | grok source |
|---|---|
| `ViewTheme` palettes | `xai-grok-pager-render/src/theme/{groknight,grokday}.rs` |
| theme + appearance resolution | `.../theme/{cache,env_appearance,system_appearance,osc11}.rs` |
| selection, turn jumps, scroll-into-view | `xai-grok-pager/src/scrollback/state/{selection,nav}.rs` |
| tool execution blocks, output bands | `xai-grok-pager/src/scrollback/blocks/tool/execute.rs` |
| thinking block and its blend | `xai-grok-pager/src/scrollback/blocks/thinking.rs` |
| turn-status row and spinner | `xai-grok-pager/src/views/turn_status.rs` |
| block viewer overlay | `xai-grok-pager/src/views/block_viewer.rs` |
| wheel and multi-click constants | `xai-grok-pager-render/src/input/mouse.rs` |
| image chip | `xai-grok-pager-render/src/prompt_images.rs` |
| braille spinner frames | `xai-grok-pager-render/src/glyphs.rs` |

Deliberate divergences from grok, all of them because this surface is
read-only or lives in tmux: the composer box never accepts text and the
turn-status row carries no `[stop]`; auto is the default appearance and
detection failure falls light instead of dark; the OSC 11 probe skips the
DCS-passthrough retry because tmux ≥ 3.2 answers the bare query; and density
is hive's own two-level idea, not a grok concept.
