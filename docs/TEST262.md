# Test262 Conformance Report

Zinc's conformance against the [Test262](https://github.com/nicolo-ribaudo/test262) ECMAScript test suite.

Run with: `cargo run --release --bin test262_runner`

## Results

**95.6% pass rate** — 9,369 of 9,805 active tests pass (2,760 tests skipped).

### Perfect Scores (100%)

| Category | Tests |
|----------|-------|
| literals/numeric | 157 |
| statementList | 80 |
| literals/string | 67 |
| white-space | 65 |
| future-reserved-words | 55 |
| expressions/template-literal | 55 |
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
| statements/class | 2782 | 2684 | 96.5% |
| statements/for-of | 630 | 600 | 95.2% |
| statements/function | 416 | 380 | 91.3% |
| statements/for-in | 113 | 98 | 86.7% |
| statements/const | 125 | 123 | 98.4% |
| statements/let | 134 | 131 | 97.8% |
| expressions/object | 792 | 773 | 97.6% |
| expressions/arrow-function | 320 | 308 | 96.2% |
| expressions/optional-chaining | 31 | 29 | 93.5% |
| expressions/async-function | 38 | 35 | 92.1% |
| expressions/array | 40 | 39 | 97.5% |
| expressions/in | 34 | 24 | 70.6% |
| expressions/instanceof | 38 | 27 | 71.1% |
| directive-prologue | 57 | 55 | 96.5% |
| function-code | 217 | 198 | 91.2% |
| computed-property-names | 48 | 46 | 95.8% |

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
| v0.4.0  | 9,805       | 9,369   | 95.6% |

### Running

```bash
git clone --depth 1 https://github.com/nicolo-ribaudo/test262.git
cargo run --release --bin test262_runner
cargo run --release --bin test262_runner -- -o failures.log  # save failures
```
