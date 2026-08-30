# Python-shaped conventions in the Rust crate

The Python tree is gone and nothing is being ported any more. `crates/hive/src/`
is the whole implementation. What survives from the port is *shape* — function
names, JSON key order, value-comparison rules, output bytes — because the
on-disk documents, the `<HIVE …>` envelope and the pytest suites were never
rewritten alongside the code. This file records which of those shapes still bind
on new Rust, and why.

## Underscore-prefixed names

AGENTS.md carries the rule. Two things it does not say.

Prose names these symbols. `docs/daemon-control-socket.md` names
`_daemon_control_sock` and `_config_dir`; AGENTS.md names the first as well. Renaming either compiles clean
and silently breaks the cross-reference; nothing checks it.

The convention outlived its origin, deliberately. `transcript_view.rs` has no
Python ancestor and still carries the prefix. New code matches the crate, not
the language default.

## JSON documents

Nothing in the crate derives `Deserialize`. Every read goes through
`serde_json::Value`, so keys hive does not know about survive a
read-modify-write. These documents are written by more than one version of hive
and read by things that are not hive.

`Serialize` is derived only on wire types hive owns end to end. Anything that
round-trips a document someone else wrote stays a `Value`.

Key order is part of the format. `serde_json` carries the `preserve_order`
feature (`crates/hive/Cargo.toml`) so insertion order survives a `Value` round
trip, and `notify_debug` assembles its JSONL line by hand to match Python's
`json.dumps(..., ensure_ascii=False, separators=(",", ":"))`. Neither is visible
from a call site: dropping the feature or replacing the hand-built line reorders
files that existing readers diff.

Python value semantics decide what loads. `registry::truthy` is Python
truthiness and gates whether a registry entry is valid at all; `registry::py_str`
is how `createdAt` values compare when a team name is recycled. Swapping either
for a Rust-native comparison changes which entries already on disk are accepted.

## Output that something parses

stdout is a contract wherever a peer or a test reads it. The e2e suite matches
`Team '<name>' created.` literally and `json.loads` the `hive team` payload. The
`<HIVE …>` header built in `runtime_state` is re-parsed by the transcript viewer
and by member skills. None of those readers compile with the crate, so nothing
fails at build time when the bytes change.

`cli/help_text.rs` is byte-captured click output, and its own header still tells
you to regenerate it by running `src/hive/cli.py`. That file is gone; the
captured text is now the source and hand-editing it is the only way to change
it. Two checks are the whole automated coupling:
`test_command_tree_declares_every_python_command` asserts every known command
exists as a clap subcommand, and `test_render_root_help_sections_present`
reads the captured root help for its section headings and for hidden commands
leaking. Flags and options that drift from what the help text claims are
caught by nothing.

## `HOME`, not `home_dir()`

Every hive root resolves through `std::env::var("HOME")`.
`std::env::home_dir()` is no longer deprecated (it compiles warning-free on
rustc 1.93) but it falls back to the passwd database when `HOME` is unset: with
the variable removed it still returns the real home directory, walking straight
out of a redirected test root. Read the variable.

## `ponytail:`

A `ponytail:` comment marks a deliberate narrowing — a place where the Rust
covers less than the Python did, or less than the general case, on purpose. Each
one names what it does not cover and what would justify widening it.

Keep the prefix when you take a shortcut on purpose, and grep it before treating
a gap as an oversight. The marker is defined nowhere else in the repo, and it is
not Rust-only — the embedded pylib carries one.

## Assets stay in their own language

`crates/hive/assets/` ships as data and is embedded at compile time, never
transliterated into Rust. The cvim toolkit, the flow pylib and the notify
plugin manifest are executed or read by something that is not this binary, so
rewriting them in Rust would mean reimplementing that interpreter's job; the
two grok `.tmTheme` palettes are parsed in process by the linked-in markdown
engine and stay byte-verbatim because that is what it accepts. Embedding keeps the single-binary install with nothing to lay
out at install time.

## Comments that lie

The crate is the spec. Comments that say otherwise are port-era residue, not
instructions: several module docs still cite `src/hive/…` paths, `flow.rs`
describes modules that "are still being ported", and `team.rs`'s test helper
calls itself a mirror of a `tests/conftest.py` fixture that no longer exists.
