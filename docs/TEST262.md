# Test262 Conformance Report

Zinc's conformance against the [Test262](https://github.com/nicolo-ribaudo/test262) ECMAScript test suite.

Run with: `cargo run --release --bin test262_runner`

## Results

**92.6% pass rate** — 14,821 of 16,010 active tests pass (2,988 tests skipped).

The active count nearly doubled from v0.4.0 after the runner stopped pre-skipping
features that already work (`Symbol.asyncIterator`, `Symbol.matchAll`, change-array-by-copy,
logical-assignment), wired up the `$DONE` async harness, implemented `with`, and turned
on `class { static { … } }`. The pass rate dipped a few points but the underlying
implementation is the same — we now report against thousands of tests we previously
hid behind `should_skip()`.

### Perfect Scores (100%)

| Category | Tests |
|----------|-------|
| literals/numeric | 157 |
| block-scope | 126 |
| statementList | 80 |
| literals/string | 67 |
| white-space | 65 |
| statements/if | 63 |
| future-reserved-words | 55 |
| expressions/template-literal | 55 |
| expressions/async-function | 38 |
| expressions/strict-equals | 29 |
| expressions/strict-does-not-equals | 29 |
| keywords | 25 |
| reserved-words | 25 |
| expressions/coalesce | 22 |
| statements/block | 19 |
| expressions/conditional | 19 |
| line-terminators | 18 |
| expressions/logical-not | 18 |
| expressions/logical-and | 16 |
| expressions/logical-or | 16 |
| statements/throw | 14 |
| statements/return | 14 |
| punctuators | 11 |
| rest-parameters | 11 |
| expressions/void | 8 |
| expressions/this | 6 |
| expressions/concatenation | 5 |
| expressions/comma | 4 |
| literals/boolean | 4 |
| literals/null | 3 |
| statements/empty | 2 |
| expressions/relational | 1 |

### Major Categories (latest run)

| Category | Total | Pass | Rate |
|----------|-------|------|------|
| statements/class | 3112 | 2856 | 91.8% |
| expressions/class | 2831 | 2606 | 92.1% |
| expressions/object | 864 | 833 | 96.4% |
| statements/for-of | 733 | 688 | 93.9% |
| expressions/assignment | 480 | 451 | 94.0% |
| expressions/compound-assignment | 454 | 410 | 90.3% |
| statements/function | 451 | 405 | 89.8% |
| statements/for | 380 | 372 | 97.9% |
| expressions/arrow-function | 343 | 333 | 97.1% |
| eval-code | 343 | 245 | 71.4% |
| expressions/generators | 290 | 280 | 96.6% |
| statements/generators | 266 | 257 | 96.6% |
| expressions/function | 264 | 258 | 97.7% |
| function-code | 217 | 202 | 93.1% |
| arguments-object | 203 | 166 | 81.8% |
| statements/variable | 178 | 172 | 96.6% |
| statements/with | 173 | 126 | 72.8% |
| statements/const | 136 | 134 | 98.5% |
| statements/let | 145 | 143 | 98.6% |
| statements/for-in | 113 | 98 | 86.7% |
| expressions/async-function | 93 | 83 | 89.2% |
| statements/switch | 93 | 77 | 82.8% |
| expressions/super | 92 | 77 | 83.7% |
| expressions/logical-assignment | 78 | 78 | 100.0% |
| statements/async-function | 74 | 66 | 89.2% |

### Skipped Features

Tests requiring these features are currently skipped (2,988 tests):

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
| post-v0.4.0 | 16,010  | 14,821  | 92.6% |

The post-v0.4.0 jump in active tests (9,805 → 16,010) reflects unskipping
features the engine already implemented and adding `$DONE`/`with`/`static {}`/
named regex groups support.

### Running

```bash
git clone --depth 1 https://github.com/nicolo-ribaudo/test262.git
cargo run --release --bin test262_runner
cargo run --release --bin test262_runner -- -o failures.log  # save failures
```
