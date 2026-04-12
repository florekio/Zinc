# Test262 Conformance Report

Zinc's conformance against the [Test262](https://github.com/nicolo-ribaudo/test262) ECMAScript test suite.

Run with: `cargo run --release --bin test262_runner`

## Results

**65.5% pass rate** — 4,239 of 6,476 tests pass (1,227 additional tests skipped).

### Perfect Scores (100%) — 15 categories

| Category | Tests |
|----------|-------|
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

### Strong Categories (90%+)

| Category | Total | Pass | Rate |
|----------|-------|------|------|
| asi | 102 | 100 | 98.0% |
| block-scope | 126 | 122 | 96.8% |
| statements/if | 63 | 60 | 95.2% |
| expressions/async-function | 37 | 35 | 94.6% |
| statements/break | 18 | 17 | 94.4% |
| comments | 27 | 25 | 92.6% |
| left-shift | 40 | 36 | 90.0% |

### Major Categories

| Category | Total | Pass | Rate |
|----------|-------|------|------|
| expressions/assignment | 385 | 249 | 64.7% |
| expressions/compound-assignment | 443 | 340 | 76.7% |
| expressions/object | 756 | 237 | 31.3% |
| expressions/function | 234 | 81 | 34.6% |
| expressions/arrow-function | 313 | 148 | 47.3% |
| statements/for | 344 | 105 | 30.5% |
| statements/for-of | 592 | 381 | 64.4% |
| statements/try | 184 | 97 | 52.7% |
| statements/variable | 155 | 100 | 64.5% |
| statements/switch | 93 | 77 | 82.8% |
| function-code | 217 | 159 | 73.3% |
| types | 109 | 87 | 79.8% |

### Skipped Features

Tests requiring these features are skipped (1,227 tests):
- Proxy, Reflect
- SharedArrayBuffer, Atomics
- async iteration, for-await-of
- dynamic import, import.meta
- Various stage 3/4 proposals (decorators, explicit resource management, etc.)

### Running

```bash
git clone --depth 1 https://github.com/nicolo-ribaudo/test262.git
cargo run --release --bin test262_runner
```
