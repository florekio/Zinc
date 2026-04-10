# Test262 Conformance Report

Zinc's conformance against the [Test262](https://github.com/nicolo-ribaudo/test262) ECMAScript test suite.

Run with: `cargo run --release --bin test262_runner`

## Results

**84.9% pass rate** — 2,367 of 2,789 tests pass (4,887 additional tests skipped).

### Perfect Scores (100%) — 23 categories

| Category | Tests |
|----------|-------|
| expressions/compound-assignment | 264 |
| literals/numeric | 144 |
| literals/string | 48 |
| statements/if | 43 |
| future-reserved-words | 30 |
| keywords | 25 |
| expressions/coalesce | 21 |
| line-terminators | 18 |
| statements/break | 18 |
| expressions/conditional | 17 |
| statements/block | 15 |
| statements/return | 14 |
| expressions/logical-and | 14 |
| expressions/logical-or | 14 |
| statements/throw | 13 |
| punctuators | 11 |
| expressions/void | 8 |
| expressions/grouping | 6 |
| expressions/comma | 4 |
| literals/boolean | 4 |
| expressions/this | 2 |
| statements/empty | 1 |
| statements/expression | 1 |

### Running

```bash
git clone --depth 1 https://github.com/nicolo-ribaudo/test262.git
cargo run --release --bin test262_runner
```
