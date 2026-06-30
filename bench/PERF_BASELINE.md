# Performance Milestone — Phase 0 Baseline

Captured 2026-06-14 on the release build. Lighthouse target: run heavy
real-world JS (the DuckDuckGo SERP) to completion. The SERP's blocking
script is a JS `decodeURIComponent` loop applied across a large dataset that
exceeds the 50M-instruction fuel budget; `bench/decode.js` mirrors that shape.

## How to run

```
cargo build --release
bench/profile.sh          # table: wall time, ops dispatched, Mops/s, Node gap
bench/profile.sh hist     # + opcode histogram for the decode lighthouse
```

Profiling knobs on the `zinc` CLI:
- `ZINC_TIME=1`        — wall time + dispatched-op count + Mops/s
- `ZINC_OPCODE_HIST=1` — per-opcode execution histogram (stderr)
- `ZINC_TRACE_IP=1`    — per-instruction (chunk, ip, op) trace

## Baseline numbers

| bench    | zinc(s) | ops dispatched | Mops/s | node(s) | gap   |
|----------|--------:|---------------:|-------:|--------:|------:|
| decode   |   0.83  |   41.1M        |  49    |  0.033  |  25×  |
| numeric  |   3.22  |  460.0M        | 143    |  0.034  |  95×  |
| strings  |   6.82  |  341.0M        |  50    |  0.069  |  99×  |
| props    |   1.03  |   70.1M        |  68    |  0.021  |  49×  |

(`decode` = SERP hot path; `numeric` = tight loop the JIT *should* cover;
`strings` = char/concat heavy; `props` = property + method-call heavy.)

## Opcode profile (where the instructions go)

`decode.js` (function-local, string + call + branch heavy — the SERP shape):

| opcode      |  share |
|-------------|-------:|
| GetLocal    |  25.9% |
| Pop         |  10.9% |
| SetLocal    |   7.2% |
| JumpIfFalse |   6.3% |
| CallMethod  |   6.2% |
| Const       |   6.1% |
| Add         |   5.3% |
| Lt          |   3.6% |
| GetProperty |   3.6% |
| GetGlobal   |   3.0% |
| ToNumeric/Dup/Inc/Loop/StrictEq | ~2.7% each |

`strings.js` is dominated by `GetGlobal`+`SetGlobal` (~57%) — an artifact of
top-level `var`s being globals; real code in functions looks like `decode`.

## Findings → Phase 1 priorities (evidence-ranked)

1. **Dispatch overhead is the #1 cost.** ~44% of decode ops are trivial O(1)
   ops (`GetLocal` 26% + `SetLocal` 7% + `Pop` 11%) whose cost is almost
   entirely per-instruction dispatch + the per-op GC/fuel bookkeeping, not the
   op itself. Trivial-op throughput tops out ~143 Mops/s. **Highest leverage:**
   direct-threaded dispatch and trimming per-op work (verify the 1024-gated
   GC/fuel check truly isn't per-op; confirm the `match` lowers to a jump
   table). Expected broad ~2–3×.
2. **`CallMethod` (6%) + native string methods.** decode spends 6% dispatching
   `charAt`/`substr`/`fromCharCode`/`parseInt`. Add a `CallMethod` inline cache
   (only `GetProperty` has one today) and ensure native string methods are
   allocation-light.
3. **`Add` / string concat (5–10%).** Confirm string `+` isn't quadratic;
   add a rope/builder fast path.
4. **`Pop` (11%)** is pure stack churn — a compiler peephole (fuse
   `SetLocal;Pop`, drop dead `Pop`s) would help but is a compiler change.

## The JIT is effectively dormant

`numeric.js` is a top-level `for` loop with a `Loop` opcode — exactly what the
partial JIT (`src/jit`, chunk-0 numeric loops) targets — yet **all 460M ops
were still interpreted** (the JIT bailed; likely the `%`/global-var handling in
`jit_compile_partial` returns `None`). So the JIT contributes ~nothing to real
workloads today. Phase 3 (extend the JIT to hot *functions*) is the eventual
unlock; Phase 1/2 interpreter+intrinsic wins are the near-term path and also
make Phase 3 cheaper to validate.

## Reality check on the SERP

The SERP's decode script needs **>150M** such ops (it didn't finish at 150M).
At the current ~49 Mops/s that's ~3s+ for decode *alone*, before the cascade
errors (`$ undefined`, etc.). So Phase 1 (~2–3×) helps but won't render the
SERP by itself; the function-JIT (Phase 3) is required for the lighthouse.
Phase 1+2 are still worth doing first: broad wins for every page, lower risk,
and they de-risk the JIT work.

## Phase 1 results (interpreter tuning) — recalibrated

Measurement-driven. A CPU sample (macOS `sample`) of `strings.js` showed the
time is **spread across the inlined opcode handlers inside `run_until`** (one
mega-function), with non-inlined leaves (`malloc`/`free`/`memmove`,
`Interner::intern`, SipHash `Hasher::write`) only ~5–15% combined — i.e. the
cost is fundamental per-op work + allocation churn, not one hot call.

Applied (safe, zero-risk, kept):
- **FxHash interner** (was SipHash): the interner is on the hot path of every
  string op; FxHash is the right choice for a non-adversarial engine map.
- **Hoisted the per-instruction debug/profiling `OnceLock` loads** out of the
  dispatch loop (resolved once at `run_until` entry).

Measured delta: only **~2–4%** combined (decode 0.83→0.81s, strings 6.82→6.67s,
numeric 3.22→3.13s). Investigated but **not** pursued (poor risk/reward):
- Removing the per-op `ip >= code.len()` implicit-return check — would save a
  few lookups/op but relies on a guaranteed trailing terminator; `get_unchecked`
  makes a missing one UB. Not worth it.
- `Add`/string concat already uses deferred `ConsString` (not O(n²)); its cost
  is one heap allocation per `+` (10M+ for the string loop) — a string-builder /
  small-string optimization would help but is a real representation change.

**Conclusion / plan recalibration:** the interpreter is already well-tuned
(unchecked reads, GetProperty inline cache, deferred ConsString). Safe
interpreter micro-opts top out around **1.2–1.5×**, not the 2–3× originally
hoped. Combined with the dormant-JIT finding, the real lever is **Phase 3
(extend the JIT to hot functions)**. Recommendation: bank these small wins and
move effort to Phase 3; the decode lighthouse needs JIT-level throughput to run
in budget.
