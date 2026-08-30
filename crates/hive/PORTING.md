# Python-shaped conventions in the Rust crate

The Python tree is gone and nothing is being ported any more. `crates/hive/src/`
is the whole implementation. What survives from the port is *shape* — function
names, JSON key order, value-comparison rules, output bytes — because the
on-disk documents, the `<HIVE …>` envelope and the pytest suites were never
rewritten alongside the code. This file records which of those shapes still bind
on new Rust, and marks the rest finished.

## Names

`crates/hive/src/<name>.rs` per subsystem, CLI transports under `adapters/`, a
module with children in a same-named directory (`transcript_tui.rs` +
`transcript_tui/interact.rs`).

Function names stay snake_case as the Python spec had them, private helpers
included: 344 functions in the crate are named `_something`. That leading
underscore is Python's privacy marker, not Rust's unused marker, and some of
those functions are `pub` — `hived::_pane_is_truly_busy` (`hived.rs:1116`) is
public and named in prose at `docs/runtime-model.md:122`. Renaming one compiles
clean and silently breaks the doc cross-reference; nothing checks it.

The convention outlived its origin. `transcript_view.rs` has no Python ancestor
and still declares `_clip`, `_md`, `_indent_block` (`transcript_view.rs:60`,
`:181`, `:185`).

## JSON documents

Nothing in the crate derives `Deserialize`. Every read goes through
`serde_json::Value`: `registry::load` (`registry.rs:103`) returns
`Map<String, Value>`, so keys hive does not know about survive a
read-modify-write. Five types derive `Serialize`, all of them wire types hive
owns end to end, with the camelCase spelled out (`bus.rs:126` —
`createdAt` / `msgId` / `inReplyTo`; `worktree.rs:512` —
`rename_all = "camelCase"`).

Key order is part of the format. `serde_json` carries the `preserve_order`
feature (`crates/hive/Cargo.toml`) so insertion order survives a `Value` round
trip, and `notify_debug` assembles its JSONL line by hand to match Python's
`json.dumps(..., ensure_ascii=False, separators=(",", ":"))`
(`notify_debug.rs:89`).

Python value semantics decide what loads. `registry::truthy` (`registry.rs:61`)
is Python truthiness and gates whether a registry entry is valid at all;
`registry::py_str` (`registry.rs:73`) is how `createdAt` values compare when a
team name is recycled. Swapping either for a Rust-native comparison changes
which entries already on disk are accepted.

## Output that something parses

stdout is a contract wherever a peer or a test reads it.
`tests/e2e/test_team_lifecycle_flow.py:36` matches `Team '<name>' created.`
literally and `json.loads` the `hive team --json` payload at `:40`. The
`<HIVE …>` header built at `runtime_state.rs:149` is re-parsed by the transcript
viewer (`transcript_view.rs:273`) and by member skills. Changing those bytes
breaks readers that do not compile with the crate.

`cli/help_text.rs` is static text served by `help_for` (`help_text.rs:9`). Its
header still says to regenerate it by running `src/hive/cli.py`; that file is
gone and the text is now the source. The only automated coupling left is
`test_command_tree_declares_every_python_command` (`cli/mod.rs:2878`), which
asserts every `_KNOWN_COMMANDS` entry exists as a clap subcommand, plus a
section-heading check on the root help. A flag or option that drifts from what
the help text claims is not caught by anything.

## `HOME`, not `home_dir()`

Every hive root resolves through `std::env::var("HOME")` — `registry.rs:49`,
`team.rs:29`, `settings.rs:15`, `context.rs:19`, `devlog.rs:21`, and each
adapter's CLI home. `std::env::home_dir()` is no longer deprecated (it compiles
warning-free on rustc 1.93) but it falls back to the passwd database when `HOME`
is unset: with the variable removed it still returns the real home directory,
walking straight out of a redirected test root. Read the variable.

## Tests

Unit tests live in `#[cfg(test)] mod tests` inside the file under test — 36
modules, 953 `#[test]`s. `crates/hive/tests/` holds only the cases that need a
real tmux server, and those create and kill their own detached sessions.

`cargo nextest run` is a requirement, not a preference. Tests set env vars and
never restore them: `transcript_tui.rs:2639` sets `HOME=/Users/dp` and `TZ=UTC`
and leaves both set. nextest gives each test its own process; plain `cargo test`
runs the whole lib in one, and that assignment leaks into whatever runs next.

A test that needs an isolated `$HIVE_HOME` takes `registry::TEST_ENV_LOCK`
(`registry.rs:35`, 20 call sites) and holds the guard as long as the `TempDir`.
`team.rs::configure_hive_home` (`team.rs:1488`) is the full form: `HIVE_HOME`,
`CODEX_HOME`, `CLAUDE_HOME`, `GROK_HOME` and `XDG_CACHE_HOME` redirected into
the temp dir, and the host's own identity vars (`HIVE_TEAM`, `HIVE_MEMBER`,
`CLAUDE_CODE_MESSAGING_SOCKET`, `CODEX_THREAD_ID`) removed. Its doc comment
calls it a mirror of `tests/conftest.py`'s `configure_hive_home` fixture — that
fixture no longer exists; today's `tests/conftest.py` is 16 lines and only
strips host agent env.

## `ponytail:`

23 comments in the crate carry a `ponytail:` prefix. Each marks a deliberate
narrowing, naming what it does not cover and what would justify widening it:

- `runtime_state.rs:17` — `splitlines` handles `\n` / `\r\n` / `\r`, not `\v`,
  `\f`, `\x85` or U+2028.
- `tmux.rs:102` — 10ms `try_wait` polling instead of signalfd plumbing, because
  tmux commands finish in single-digit ms.
- `claude_bg.rs:1471` — two pipes typing into one job interleave; the loser
  fails on the transcript compare rather than silently, and the fix if it ever
  bites is an flock on `hive-control/<jobId>.lock`.

Keep the prefix when you take a shortcut on purpose. It is defined nowhere else
in the repo.

## Embedded assets

`crates/hive/assets/` ships as data and is embedded at compile time, never
transliterated into Rust: `include_str!` for the cvim toolkit, the flow pylib
and the notify plugin manifest; `include_bytes!` for the two tmTheme files
(`transcript_view.rs:137`). The tree is materialized on first use and rewritten
when the on-disk copy drifts — cvim under `$HIVE_HOME/core_assets/cvim/`
(`cvim.rs:29`), the flow python client under `core_assets/pylib/`
(`flow.rs:592`), both through `core_hooks::materialize_asset_tree`; plugins
install under `$HIVE_HOME/plugins/` from the embedded string table at
`plugin_manager.rs:30`. `plugin_manager.rs:5` cites this file as the authority
for that choice.

## Finished — historical only

These were migration scaffolding and no longer describe any work:

- "The Python tree at `src/hive/` is the behavioral spec." There is no such
  tree; the crate is the spec.
- Porting one module at a time, writing `crate::<module>::<fn>` calls against
  Python signatures for modules that did not exist yet, then an integration pass
  to wire the seams. The crate compiles as a whole.
- Porting unit tests out of `tests/unit/` keeping their names. That directory is
  gone; `tests/` holds the pytest e2e and acceptance suites only.
- "Timeouts on sockets mirror the Python constants exactly." There is nothing
  left to mirror.

Stale labels survive in source comments and are not instructions: seven files
still cite `src/hive/…` in their module docs, `cli/help_text.rs:4` points at a
regeneration command that cannot run, and `flow.rs:410` says modules "are still
being ported".
