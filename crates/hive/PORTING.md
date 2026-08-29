# hive py→rs porting conventions

Read this before porting any module. The Python tree at `src/hive/` is the
behavioral spec; the e2e/acceptance pytest suites are the oracle. The goal is a
behavior-identical Rust binary, not a redesign.

## Layout & naming

- `src/hive/<name>.py` → `crates/hive/src/<name>.rs`; `src/hive/adapters/<name>.py`
  → `crates/hive/src/adapters/<name>.rs`. One module per Python file, same name.
- Keep Python function names in snake_case as-is. Keep module-level constants
  under the same names.
- A shared type is defined in the module where the Python class/dict is born
  (e.g. team registry documents live in `registry`, adapter profile types in
  `adapters::base`) and referenced as `crate::<module>::<Type>` everywhere else.
- Do not invent new behavior, flags, fields, or output text. Byte-identical
  stdout/stderr wherever tests or peers parse it (JSON payloads, `<HIVE ...>`
  envelopes, doctor output, error strings that tests match).

## Language conventions

- Edition 2021. `anyhow::Result` for fallible functions; return errors, don't
  panic, except for programmer invariants (`unreachable!`).
- JSON via `serde_json`. Field names must match the Python payloads exactly
  (`#[serde(rename_all = "camelCase")]` or explicit renames — check the actual
  JSON the Python code writes, not the Python attribute names). Unknown fields
  are preserved where Python round-trips dicts: model those documents as
  `serde_json::Value` or keep a `#[serde(flatten)] extra: Map<String, Value>`.
- Subprocess: `std::process::Command`. Unix sockets: `std::os::unix::net`.
  Threads: `std::thread` (no async runtime).
- Env/paths: `std::env`, `PathBuf`. `Path.home()` → `std::env::home_dir()` is
  deprecated; use the `HOME` env var like the Python code effectively does.
- Timeouts on sockets mirror the Python constants exactly.

## Tests

- Port the module's unit tests from `tests/unit/` (and cli-level logic tests
  where they test pure functions) into `#[cfg(test)] mod tests` in the same
  file. Keep the test names.
- Tests must not touch the real tmux server, real `~/.hive`, or the network.
  Use `tempfile::TempDir` and env-var redirection the same way conftest.py does
  (`HIVE_HOME`, `XDG_CACHE_HOME`).
- The suite runs under `cargo nextest run` (one process per test), so env-var
  mutation inside a test is safe and needs no restore. Plain `cargo test`
  shares one process and WILL cross-contaminate — don't chase those failures.

## Cross-module references during parallel porting

Other modules may not exist yet when you port yours. Write cross-module calls
as `crate::<module>::<fn>` matching the Python name and expected signature. If
you must assume a signature, take it from the Python definition. Never stub
another module inside your file. Your module must compile standalone in the
sense of: correct syntax, self-contained types, and only well-derived external
references (an integration pass wires the whole crate and fixes seams).

## Out of scope for module ports

- `core_assets/` and `plugins/` stay as data files; Rust embeds or locates them
  (integration pass decides; don't copy them).
- Version bumps, README, packaging.
