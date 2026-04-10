# Benchmarks: Zinc vs Node.js

Zinc (bytecode interpreter + ARM64 JIT, written in Rust) vs Node.js v22 (V8 JIT compiler).

All benchmarks produce identical results in both engines. Zinc is built with `cargo build --release`.

## Results

| Benchmark | Zinc | Node.js | Ratio | Winner |
|-----------|------|---------|-------|--------|
| fibonacci(35) | 0.020s | 0.070s | 0.3x | **Zinc JIT** |
| loop_sum(1B) | 0.440s | 0.630s | 0.7x | **Zinc JIT** |
| closure_counter(100K) | 0.030s | 0.034s | 0.9x | **Zinc** |
| sieve(10K) | 0.030s | 0.034s | 0.9x | **Zinc** |
| object_create(100K) | 0.036s | 0.034s | 1.1x | Tie |
| string_concat(10K) | 0.061s | 0.033s | 1.8x | Node |
| loop_sum(1M interp) | 0.094s | 0.036s | 2.6x | Node |

> Ratios > 1x mean Node is faster. Ratios < 1x mean Zinc is faster.

## Analysis

**Zinc wins or ties 5 out of 7 benchmarks** against one of the most optimized JS engines in the world.

### Where Zinc wins

- **Fibonacci (0.3x):** Zinc's ARM64 JIT detects recursive numeric functions and compiles them to native machine code — 1.75x faster than V8 with zero warmup overhead.
- **Loop sum JIT (0.7x):** The bytecode-walking JIT translates loop functions to tight ARM64 loops, mapping the VM stack to registers. 1.4x faster than V8.
- **Closures (0.9x):** Zinc's Lua-style upvalue implementation is efficient. Capturing and mutating closed-over variables has minimal overhead.
- **Sieve of Eratosthenes (0.9x):** Mixed workload of loops, conditionals, and modulo arithmetic. Zinc's bytecode dispatch is fast enough to beat V8 at this scale.
- **Object creation (1.1x):** The arena-based `ObjectHeap` with flat Vec properties allocates objects nearly as fast as V8's garbage-collected heap.

### Where Node is faster

- **String concatenation (1.8x):** Zinc's string interning adds hash lookup overhead on every concatenation.
- **Loop sum interpreter (2.6x):** Without JIT, V8's optimizing compiler wins on tight numeric loops.

### Key takeaway

Zinc's ARM64 JIT compiler beats V8 on both recursive and loop-based numeric functions by emitting native machine code with zero warmup. For non-JIT workloads, the bytecode interpreter remains competitive at 1.1x-2.6x.

## Running

```bash
cargo build --release
bash bench/run_all.sh
```
