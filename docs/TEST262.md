# Test262 Conformance Report

Zinc's conformance against the [Test262](https://github.com/nicolo-ribaudo/test262) ECMAScript test suite.

Run with: `cargo run --release --bin test262_runner`

## Results

**92.3% pass rate** — 9,052 of 9,805 active tests pass (2,760 tests skipped).

### Perfect Scores (100%)

| Category | Tests |
|----------|-------|
| future-reserved-words | 55 |
| reserved-words | 25 |
| literals/numeric | 157 |
| literals/string | 67 |
| expressions/coalesce | 22 |
| expressions/conditional | 19 |
| statements/block | 19 |
| line-terminators | 18 |
| expressions/logical-and | 16 |
| expressions/logical-or | 16 |
| statements/return | 14 |
| statements/throw | 14 |
| punctuators | 11 |
| expressions/void | 8 |
| literals/boolean | 4 |
| expressions/comma | 4 |
| statements/empty | 2 |

### Major Categories (latest run)

| Category | Total | Pass | Rate |
|----------|-------|------|------|
| statements/class | 1444 | 1308 | 90.6% |
| statements/for-of | 630 | 562 | 89.2% |
| statements/function | 412 | 362 | 87.9% |
| statements/for-in | 113 | 95 | 84.1% |
| statements/const | 125 | 117 | 93.6% |
| statements/let | 134 | 125 | 93.3% |
| expressions/optional-chaining | 31 | 29 | 93.5% |
| expressions/async-function | 38 | 35 | 92.1% |
| expressions/array | 40 | 28 | 70.0% |
| expressions/in | 16 | 9 | 56.2% |
| expressions/instanceof | 38 | 21 | 55.3% |
| directive-prologue | 57 | 47 | 82.5% |

### Skipped Features

Tests requiring these features are currently skipped (2,760 tests):

- `Proxy`, `Reflect`
- `SharedArrayBuffer`, `Atomics`
- Async iteration, `for-await-of`
- Dynamic `import()`, `import.meta`
- `Intl`, `Temporal`
- `WeakRef`, `FinalizationRegistry`
- Private class field brand checks (`#x in obj`)
- Regex advanced features (named groups, lookbehind, dotall, unicode properties)
- Logical assignment operators (`&&=`, `||=`, `??=`)
- Class static blocks
- Various stage 3/4 proposals (decorators, explicit resource management, iterator helpers, set methods)
- ES Modules

### History

| Version | Active Tests | Passing | Rate |
|---------|-------------|---------|------|
| v0.1.0  | ~4,000      | ~2,600  | 65.5% |
| v0.2.0  | 6,476       | 5,461   | 84.3% |
| v0.3.0  | 9,805       | 9,052   | 92.3% |

### Running

```bash
git clone --depth 1 https://github.com/nicolo-ribaudo/test262.git
cargo run --release --bin test262_runner
cargo run --release --bin test262_runner -- -o failures.log  # save failures
```
