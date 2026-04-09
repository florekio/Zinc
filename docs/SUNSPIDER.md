# SunSpider Benchmark Results

Zinc vs Node.js v22 on classic [SunSpider](https://webkit.org/perf/sunspider/sunspider.html) benchmark tests.

All tests produce identical output in both engines.

## Results

| Test | Zinc | Node.js | Ratio | Description |
|------|------|---------|-------|-------------|
| access-nbody | 100ms | 39ms | 2.6x | N-body physics simulation (object property access) |
| bitops-3bit-bits-in-byte | 63ms | 36ms | 1.8x | Bitwise operations in tight loop |
| math-cordic | 152ms | 44ms | 3.5x | CORDIC trigonometry algorithm |
| math-partial-sums | 100ms | 44ms | 2.3x | Mathematical series computation (sin, cos, pow) |
| controlflow-recursive | 40,915ms | 309ms | 132.4x | Ackermann + fibonacci + tak (extreme recursion) |

## Analysis

**Zinc is 1.8x-3.5x slower than Node.js** on real-world computational benchmarks — very competitive for a bytecode interpreter vs a JIT compiler.

- **Bitwise ops (1.8x)**: Nearly native speed. NaN-boxed integers and Rust's bitwise ops are efficient.
- **Partial sums (2.3x)**: Math-heavy loop with `Math.sin`, `Math.cos`, `Math.pow`. Dispatch overhead is the bottleneck.
- **N-body (2.6x)**: Object property access + `Math.sqrt`. Shows the object heap is reasonably fast.
- **CORDIC (3.5x)**: Tight loop with array access and bitwise shifts.
- **Recursive (132.4x)**: Ackermann function `ack(3,9)` makes millions of recursive calls. This is the known weakness — function call overhead in a bytecode interpreter vs JIT-compiled native calls.

## Running

```bash
cargo build --release
bash bench/sunspider/run.sh
```
