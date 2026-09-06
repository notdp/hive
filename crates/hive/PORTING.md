# Python-shaped conventions in the Rust crate

The Python tree is gone and nothing is being ported any more. `crates/hive/src/`
is the whole implementation. What survives from the port is shape: JSON key
order, value-comparison rules, output bytes. Those shapes survive because the
on-disk documents, the `<HIVE …>` envelope and the pytest suites were never
rewritten alongside the code. This file records which of them still bind on
new Rust, and why. (The port-era `_name` function prefixes are gone; AGENTS.md
carries the naming rule.)

## JSON documents

`Serialize` and `Deserialize` are derived only on wire types hive owns end to
end (`flow.rs::FlowOp`, whose serialization is the op-journal key). Every other
read goes through `serde_json::Value`, so keys hive does not know about survive
a read-modify-write: those documents are written by more than one version of
hive and read by things that are not hive. Anything that round-trips a document
someone else wrote stays a `Value`.

Key order is part of the format. `serde_json` carries the `preserve_order`
feature (`crates/hive/Cargo.toml`) so insertion order survives a `Value` round
trip, and `notify_debug` assembles its JSONL line by hand to match Python's
`json.dumps(..., ensure_ascii=False, separators=(",", ":"))`. Neither is visible
from a call site: dropping the feature or replacing the hand-built line reorders
files that existing readers diff.

Python value semantics decide what loads. `pyval::truthy` is Python
truthiness and gates whether a registry entry is valid at all; `registry::py_str`
is how `createdAt` values compare when a team name is recycled, and
`pyval::py_float_str` is how every writer formats them. Swapping any of these
for a Rust-native comparison changes which entries already on disk are accepted.

## Output that something parses

stdout is a contract wherever a peer or a test reads it. The e2e suite
`json.loads` the `hive team` payload. The `<HIVE …>` header built in
`message.rs` is re-parsed by the transcript viewer and by member skills. None of
those readers compile with the crate, so nothing fails at build time when the
bytes change.

`cli/help_text.rs` is hand-maintained help text in click's layout: the captured
output is the source and editing it is the only way to change it. The automated
coupling is three checks in `cli/mod.rs`:
`test_command_tree_declares_every_python_command` asserts every known command
exists as a clap subcommand, `test_every_help_path_has_help_text` asserts every
path `-h` can produce has an arm, and
`test_root_help_lists_every_visible_command_and_no_hidden_one` asserts the root
help lists a command exactly when its clap node is not hidden. Nothing catches
flags and options that drift from what the help text claims.

## `HOME`, not `home_dir()`

Every hive root resolves through `std::env::var("HOME")`.
`std::env::home_dir()` is no longer deprecated (it compiles warning-free on
rustc 1.93) but it falls back to the passwd database when `HOME` is unset: with
the variable removed it still returns the real home directory, walking straight
out of a redirected test root.

## `ponytail:`

A `ponytail:` comment marks a deliberate narrowing: a place where the Rust
covers less than the Python did, or less than the general case. Each one names
what it does not cover and what would justify widening it.

Keep the prefix on a deliberate shortcut, and grep for it before treating a gap
as an oversight. The marker is defined nowhere else in the repo.

## Assets stay in their own language

`crates/hive/assets/` ships as data and is embedded at compile time, never
transliterated into Rust. The cvim toolkit and the notify plugin manifest are
executed or read by something that is not this binary, so
rewriting them in Rust would mean reimplementing that interpreter's job; the
two grok `.tmTheme` palettes are parsed in process by the linked-in markdown
engine and stay byte-verbatim because that is the form it accepts. Embedding
keeps the single-binary install with nothing to lay out at install time.

## Comments that lie

The crate is the spec. Comments that say otherwise are port-era residue, not
instructions: the `cli` module docs still cite `src/hive/cli.py` as what they
were ported from.
