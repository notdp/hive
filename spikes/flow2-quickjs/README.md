# flow v2 spike: QuickJS dialect over an async Rust FlowEnv

PR 1 of the flow v2 plan (JS scripting surface replacing the Python shell).
Standalone crate, **not** a workspace member; disposable by design. The
engine verdict below is the deliverable — the code exists to back it.

## Verdict

**rquickjs 0.12 GO.** Every engine-risk question resolved on the first
compile, no workarounds needed. boa was not tried — nothing here left a
reason to.

| Question | Result |
| --- | --- |
| `export const meta` static parse, zero body execution | balanced-brace text scan + standalone eval of the literal; test proves 0 ops fired |
| async Rust host fns as the op seam | `Func::from(Async(closure))`, one `__flow_op(op, json) -> json` primitive, dialect built in a JS prelude on top (mirrors the flow-op protocol shape) |
| do concurrent `agent()` calls actually overlap | yes: 3 agents with 300ms wait-reply finish in <700ms wall on a current-thread runtime, and the three wait windows overlap pairwise (asserted) |
| real Promises (`.then` chains — 44% of the corpus uses them) | native |
| determinism poisons (`Date.now`/`Math.random`/argless `new Date`) | JS prelude Proxy + override; argful `new Date(0)` still works |
| CCD error contracts (parallel→null never rejects; pipeline stage throw drops item) | implemented in ~15 lines of prelude, asserted in torture.js |
| the corpus dialect (template composition, spread, `?.`/`??`, while, Set/flatMap, hand-rolled retry) | all pass torture.js |
| binary cost | spike release binary 2.1M total → QuickJS adds roughly ~1–1.5M to hive [推断: spike 含 tokio/serde,未做逐项归因] |

## Shape that carries into PR 2

- **One async host primitive** (`__flow_op`), everything else is an embedded
  JS prelude — the prelude replaces the materialized pylib, but ships inside
  the binary and is engine-tested, so the "two-sided change" trap dies.
- Single-threaded JS + async ops = `parallel()` is `Promise.all` in the
  prelude; no cross-thread closure problem at all.
- Script wrapper: `(async () => { <body with export stripped> })()` →
  `Promise::into_future()`; top-level `await` and `return` both work.
- meta parse limitation: the brace scanner assumes a pure literal (no `${}`
  in meta strings) — same contract CCD imposes; production should reject
  interpolation explicitly.

## Not covered here (ordinary work, no engine risk)

schema validation of replies (validate + bounded re-ask), journal/resume,
real tmux spawns, error-message polish.

## Run it

```
cargo test            # 4 tests: meta / concurrency / torture / demo
cargo run             # scripts/demo.js against the mock env
cargo run -- scripts/torture.js
```
