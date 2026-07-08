# Transcript Signal Model

This document explains the CLI-specific transcript/JCL evidence Hive currently
uses for runtime decisions.

It exists to answer a narrow question:

- What do Claude Code and Droid transcripts actually look like?
- Which events count as open/close/read signals in current code?

Codex has no transcript probe: a daemon-backed codex pane reports
`busy` / `inputState` / `turnPhase` natively over its app-server socket (see
"Codex Native Runtime" in `docs/runtime-model.md`), and an embedded
(daemon-less) codex is unsupported — hive rejects it at team entry and its
runtime state reads as unresolved/unknown.

It does not define tmux output activity. `busy` is documented separately in
`docs/runtime-model.md`; `busy` is primarily an output-activity signal and
uses transcript jsonl mtime only as a phantom-redraw gate, not as a primary
source.

## Important Clarification

`busy` has two source branches that are OR'd together:

1. tmux control-mode output activity, gated by transcript jsonl mtime to
   suppress TUI repaint phantoms
2. transcript `turnPhase` ∈ active-turn set
   (`tool_open` / `tool_result_pending_reply` / `user_prompt_pending` /
   `input_backlog`)

The transcript layer therefore *can* drive `busy` directly when the
turn-phase probe says the agent is mid-flight — branch 2 catches the
streaming-gap case where the output branch alone would flap to false.

`turnPhase` itself comes from transcript/JCL parsing.

## Concepts

### Open / Close

The strongest transcript signal Hive uses is an open-without-close pattern:

- an open event starts some work
- a close event ends that work
- open without close is strong negative evidence

This is the basis for the internal reasoning concept often described as
"hard busy".

### Hard Busy

`hard busy` is not a public field. It is a reasoning concept:

- a tool/task open event exists
- the corresponding close event has not appeared yet

Examples:

- Claude: `tool_use` without matching `tool_result`
- Droid: `tool_use` without matching `tool_result`

### Turn Phase

`turnPhase` is the exported interface built on top of transcript/JCL signals.
It reports a single token describing the transcript tail. Consumers choose their
own subsets (see `docs/runtime-model.md`):

- `tool_open` — hard-busy (tool_use open)
- `input_backlog` — strategy-level busy (an unresolved queue enqueue is the newest decisive evidence)
- `turn_closed` — turn collapsed
- `tool_result_pending_reply` — tool result observed, assistant hasn't continued
- `user_prompt_pending` — user prompt observed, assistant hasn't acked
- `assistant_text_idle` — assistant text without stop_reason
- `unknown_evidence` — no reliable probe evidence

Hard busy is a subset of "not closed" but not the only member.

## Claude Code Transcript

Hive expects Claude transcript records in JSONL form and looks at:

- `type`
- `subtype`
- `operation`
- `message.stop_reason`
- `message.content[*].type`

### Signals Used


- queue backlog from `queue-operation`
  - `enqueue` increments backlog
  - `dequeue` / `remove` decrement backlog
  - backlog > 0 and not superseded by later turn evidence => `turnPhase=input_backlog`
- `assistant` with:
  - `stop_reason=tool_use`
  - or any `content[*].type == tool_use`
  - => `turnPhase=tool_open`


- `system.subtype=turn_duration`
- `system.subtype=stop_hook_summary` with `preventedContinuation=false`

Both map to:

- `turnPhase=turn_closed`


- `user` carrying `tool_result`
  - => `tool_result_pending_reply`
- real user text
  - => `user_prompt_pending`
- assistant text without stronger open/close evidence
  - => `assistant_text_idle`

## Droid Transcript

Hive treats Droid transcript as message-oriented JSONL and looks at:

- message role
- `content[*].type`
- `tool_use` / `tool_result`

### Signals Used


- assistant message containing `tool_use`
  - => `turnPhase=tool_open`


- none from the simple transcript probe alone


- `tool_result`
  - => `tool_result_pending_reply`
- real user text (ignoring `<system-reminder>`)
  - => `user_prompt_pending`
- assistant text without `tool_use`
  - => `assistant_text_idle`

## What Does Not Count

The following do not count as transcript-derived `busy` triggers:

- any transcript tail heuristic outside the recognised `turnPhase` set
- any single “there was output” observation
- the transcript jsonl mtime alone — that's the phantom-redraw gate on
  the output branch, not a standalone source

`busy=true` requires either the output branch (with mtime gate) or a
`turnPhase` in the active-turn set; nothing else.

## Why Two Docs

`docs/runtime-model.md` answers:

- what public runtime fields exist
- what they mean
- what uses them

This document answers:

- what raw transcript/JCL structures exist
- which exact events currently map into those runtime fields
