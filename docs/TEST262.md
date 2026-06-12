# Test262 Conformance Report

Zinc's conformance against the [Test262](https://github.com/nicolo-ribaudo/test262) ECMAScript test suite.

Run with: `cargo run --release --bin test262_runner`

## Results

**89.9% pass rate** — 14,491 of 16,116 active tests pass (2,989 tests skipped).

The pass rate dropped from 92.6% because earlier silent assertion no-ops
(`assert.sameValue` / `assert.throws` weren't actually being called when invoked
on a function value with user-set properties, so tests "passed" by inaction)
were fixed in this round. The underlying engine is materially more conformant —
we now report against thousands of tests whose latent bugs had been hidden.
Many of the resulting failures are themselves now-fixed (strict-mode property
writes / deletes, derived-class missing-super, eager generator parameters,
iterator-close protocol, computed-key class methods, `__proto__: val` literals,
etc.). Remaining failures are concentrated in eval-time strict early errors,
private-field brand checks, completion-value semantics, and TDZ for params.

### Perfect Scores (100%)

| Category | Tests |
|----------|-------|
| identifiers | 260 |
| expressions/assignmenttargettype | 308 |
| destructuring | 17 |
| future-reserved-words | 55 |
| reserved-words | 25 |
| keywords | 25 |
| expressions/conditional | 20 |
| expressions/logical-not | 19 |
| statements/block | 19 |
| expressions/logical-and | 17 |
| expressions/logical-or | 17 |
| statements/return | 15 |
| statements/throw | 14 |
| expressions/coalesce | 22 |
| punctuators | 11 |
| expressions/grouping | 9 |
| expressions/void | 9 |
| expressions/this | 6 |
| expressions/concatenation | 5 |
| expressions/comma | 5 |
| literals/boolean | 4 |
| literals/null | 3 |
| expressions/relational | 1 |

### Major Categories (latest run)

| Category | Total | Pass | Rate |
|----------|-------|------|------|
| statements/class | 3112 | 2519 | 80.9% |
| expressions/class | 2831 | 2331 | 82.3% |
| expressions/object | 864 | 738 | 85.4% |
| statements/for-of | 733 | 638 | 87.0% |
| expressions/assignment | 480 | 375 | 78.1% |
| expressions/compound-assignment | 454 | 386 | 85.0% |
| statements/function | 451 | 369 | 81.8% |
| statements/for | 380 | 360 | 94.7% |
| expressions/arrow-function | 343 | 280 | 81.6% |
| eval-code | 343 | 158 | 46.1% |
| expressions/generators | 290 | 244 | 84.1% |
| statements/generators | 266 | 238 | 89.5% |
| expressions/function | 264 | 218 | 82.6% |
| function-code | 217 | 188 | 86.6% |
| arguments-object | 203 | 153 | 75.4% |
| statements/variable | 178 | 157 | 88.2% |
| statements/with | 173 | 110 | 63.6% |
| statements/const | 136 | 130 | 95.6% |
| statements/let | 145 | 138 | 95.2% |
| statements/for-in | 113 | 90 | 79.6% |
| expressions/async-function | 93 | 79 | 84.9% |
| statements/switch | 93 | 64 | 68.8% |
| expressions/super | 92 | 70 | 76.1% |
| expressions/logical-assignment | 78 | 78 | 100.0% |
| statements/async-function | 74 | 52 | 70.3% |

### Skipped Features

Tests requiring these features are currently skipped (2,986 tests):

- `Proxy`, `Reflect`
- `SharedArrayBuffer`, `Atomics`
- Async iteration, `for-await-of`
- Dynamic `import()`, `import.meta`
- `Intl`, `Temporal`
- `WeakRef`, `FinalizationRegistry`
- Private class field brand checks (`#x in obj`)
- Regex advanced features (lookbehind, dotall, unicode properties, v-flag, match-indices)
- Various stage 3/4 proposals (decorators, explicit resource management, iterator helpers, set methods)
- ES Modules

### History

| Version | Active Tests | Passing | Rate |
|---------|-------------|---------|------|
| v0.1.0  | ~4,000      | ~2,600  | 65.5% |
| v0.2.0  | 6,476       | 5,461   | 84.3% |
| v0.3.0  | 9,805       | 9,052   | 92.3% |
| v0.4.0  | 9,805       | 9,385   | 95.7% |
| post-v0.4.0 a | 16,010  | 14,821  | 92.6% |
| post-v0.4.0 b | 15,947  | 13,350  | 83.7% |
| post-v0.4.0 c | 15,947  | 13,866  | 87.0% |
| post-v0.4.0 d | 15,855  | 14,215  | 89.7% |
| post-v0.4.0 e | 15,855  | 14,285  | 90.1% |
| post-v0.4.0 f | 16,116  | 14,491  | 89.9% |

The first post-v0.4.0 jump reflects unskipping features the engine already
implemented (Symbol.asyncIterator, Symbol.matchAll, change-array-by-copy,
logical-assignment) and adding the `$DONE` async harness, `with`,
`class { static {} }`, and named regex groups support.

The post-v0.4.0 e bump (+70) comes from switching the RegExp backend from
the `regex` crate to `fancy-regex`, adding lookahead, lookbehind, and
backreference support.

The post-v0.4.0 f row is denominator honesty plus real fixes: `statements/try`
and `expressions/yield` (261 tests) used to CRASH their runner subprocess —
a `return` inside `try` leaked its exception handler, and a later unrelated
throw jumped mid-instruction — so neither category was counted at all.
With return/break/continue now unwinding handlers (and inlining `finally`
bodies per spec), `statements/try` completes at 177/198 and the absolute
pass count rises by 190; the rate dips because the recovered categories
include hard yield* protocol tests that were never measured before.

The second drop (92.6% → 83.7%) is honesty, not regression. Method dispatch on
function values with user-set properties (`f.method = fn; f.method(args)`) was
silently a no-op, so any test using `assert.sameValue`/`assert.throws` etc. on
the harness's user-defined `assert` object "passed" by inaction. Fixing that
exposed thousands of latent assertion failures, many of which have since been
fixed (strict-mode property writes/deletes throw, derived-class missing-super
throws ReferenceError, eager generator parameter destructuring, full iterator
protocol with conditional close, computed-key class methods, `__proto__: val`
literals, ToPropertyKey for undefined/null/boolean keys, etc.). The remaining
failures concentrate in eval-time strict early errors, private-field brand
checks, completion-value semantics in eval, and parameter TDZ — each its own
spec rule, not a single underlying bug.

### Running

```bash
git clone --depth 1 https://github.com/nicolo-ribaudo/test262.git
cargo run --release --bin test262_runner
cargo run --release --bin test262_runner -- -o failures.log  # save failures
```
