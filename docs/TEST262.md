# Test262 Conformance Report

Zinc's conformance against the [Test262](https://github.com/nicolo-ribaudo/test262) ECMAScript test suite.

Run with: `cargo run --release --bin test262_runner`

## Results

**81.7% pass rate** — 2,181 of 2,670 tests pass (4,594 additional tests skipped).

### Perfect Scores (100%) — 17 categories

| Category | Tests |
|----------|-------|
| literals/numeric | 144 |
| literals/string | 48 |
| future-reserved-words | 30 |
| keywords | 25 |
| expressions/coalesce | 21 |
| line-terminators | 18 |
| statements/block | 15 |
| statements/return | 14 |
| statements/throw | 13 |
| punctuators | 11 |
| expressions/void | 8 |
| expressions/grouping | 6 |
| literals/boolean | 4 |
| expressions/this | 2 |
| statements/empty | 1 |
| statements/expression | 1 |
| expressions/relational | 1 |

### Running

```bash
git clone --depth 1 https://github.com/nicolo-ribaudo/test262.git
cargo run --release --bin test262_runner
```
