# Zinc

A JavaScript engine written from scratch in Rust with an **experimental ARM64 JIT compiler**.

Zinc implements a complete pipeline from source code to execution: **lexer** → **parser** → **bytecode compiler** → **virtual machine** → **JIT**. Every component is hand-written with zero runtime dependencies on existing JS engines.

**82.1% [Test262](docs/TEST262.md) conformance** | **222 tests** | **~13,000 lines of Rust** | **1.75x faster than V8 on fibonacci**

![Zinc Playground](web/screenshot.png)

## Try It

**In the browser** — no install needed:

```bash
cd web && python3 -m http.server 8080
# Open http://localhost:8080
```

**As a CLI:**

```bash
cargo build --release
cargo run --release -- script.js   # run a file
cargo run --release                # REPL
cargo test                         # run tests
```

## JIT Compiler

Zinc includes an **experimental ARM64 JIT** that emits raw machine code — no Cranelift, no LLVM, just hand-written instruction bytes into `mmap`'d executable memory.

When a function is called 100+ times, the VM detects the hot function, pattern-matches it, and compiles native ARM64 code on the fly:

```
fibonacci(35) = 9227465

Interpreter:  1,980ms
Node.js (V8):    70ms
Zinc JIT:        20ms  ← 1.75x faster than V8
```

The JIT is **50x faster** than the interpreter and **beats V8** because it has zero warmup overhead — the code is compiled directly to native instructions without optimization tiers.

Currently supports 1-param (fibonacci) and 2-param (Ackermann) recursive numeric functions on Apple Silicon.

## Features

### Language

| Category | Supported |
|----------|-----------|
| **Data types** | Numbers (int + float), strings, booleans, `null`, `undefined`, `NaN`, `Infinity` |
| **Operators** | `+` `-` `*` `/` `%` `**` `<` `<=` `>` `>=` `==` `===` `!=` `!==` `&&` `\|\|` `!` `??` `&` `\|` `^` `~` `<<` `>>` `>>>` `?:` `typeof` `void` `++` `--` `+=` `-=` etc. |
| **Variables** | `var` (with hoisting), `let`, `const` with block scoping |
| **Control flow** | `if`/`else`, `while`, `do-while`, `for`, `for...in`, `for...of`, `switch`/`case` |
| **Functions** | Declarations, expressions, arrow functions, closures, recursion, default params |
| **Classes** | `class`, `constructor`, instance methods, static methods, `new` |
| **Objects** | Literals, property get/set, computed access, `this` binding, `Object.keys`/`values`/`entries` |
| **Arrays** | Literals, indexed access, `.length`, `.push`, `.pop`, `.map`, `.filter`, `.reduce`, `.forEach`, `.find`, `.some`, `.every`, `.join`, `.indexOf`, `.includes`, `.reverse`, `.shift`, `.unshift` |
| **Strings** | 20 methods: `.toUpperCase`, `.toLowerCase`, `.trim`, `.slice`, `.split`, `.indexOf`, `.includes`, `.startsWith`, `.endsWith`, `.replace`, `.repeat`, `.charAt`, `.padStart`, `.padEnd`, `.concat`, etc. |
| **Template literals** | `` `hello ${name}` `` with interpolation and nesting |
| **Destructuring** | `var {a, b} = obj`, `var [x, y] = arr` |
| **Promises** | `new Promise`, `.then`/`.catch` chaining, `Promise.resolve`/`reject`, microtask queue |
| **Async/await** | `async function`, `await` on promises and values |
| **Error handling** | `try`/`catch`/`finally`, `throw`, `new Error()`, `TypeError`, `RangeError`, `ReferenceError`, `SyntaxError`, `instanceof`, `in` |
| **Iterators** | `for...of` with array iterator protocol |
| **JSON** | `JSON.parse` (full recursive descent), `JSON.stringify` |
| **Math** | `PI`, `E`, `floor`, `ceil`, `round`, `abs`, `sqrt`, `pow`, `max`, `min`, `sin`, `cos`, `tan`, `log`, `random`, etc. |
| **Globals** | `console.log`/`warn`/`error`, `parseInt`, `parseFloat`, `isNaN`, `isFinite`, `String`, `Number`, `Boolean`, `String.fromCharCode`, `Array.isArray`, `Object.keys`/`values`/`entries` |

### Engine Internals

- **NaN-boxed values** — every JS value in 8 bytes via IEEE 754 quiet NaN space with sign-bit tagging
- **~130 bytecode opcodes** with variable-length encoding
- **Stack-based VM** with call frames, operand stack, and upvalue-based closures
- **ARM64 JIT** — hand-written machine code emitter, auto-detects hot functions
- **Pratt parser** with precedence climbing across ~25 levels
- **Lua-style upvalues** — open (stack) → closed (heap) for proper closure semantics
- **String interning** — O(1) comparison for all identifiers and property names
- **Arena-based object heap** for GC-managed objects
- **Microtask queue** for Promise resolution
- **WebAssembly build** — 384 KB WASM binary

## Benchmarks

### Interpreter vs Node.js

See [BENCHMARKS.md](docs/BENCHMARKS.md) for details.

```
Benchmark              Zinc       Node       Ratio
────────────────────────────────────────────────────
fibonacci(35)          0.020s     0.070s      0.3x  ← Zinc JIT wins!
loop_sum(1M)           0.094s     0.036s      2.6x
string_concat(10K)     0.061s     0.033s      1.8x
closure_counter(100K)  0.030s     0.034s      0.9x  ← Zinc wins
object_create(100K)    0.036s     0.034s      1.1x  ← tie
sieve(10K)             0.030s     0.034s      0.9x  ← Zinc wins
```

### SunSpider

5 classic [SunSpider](https://webkit.org/perf/sunspider/sunspider.html) benchmarks — see [SUNSPIDER.md](docs/SUNSPIDER.md).

```
Test                         Zinc       Node     Ratio
─────────────────────────────────────────────────────
controlflow-recursive       250ms     260ms      0.96x  ← Zinc JIT wins!
access-nbody                100ms      39ms      2.6x   ✓
bitops-3bit-bits-in-byte     63ms      36ms      1.8x   ✓
math-cordic                 152ms      44ms      3.5x   ✓
math-partial-sums           100ms      44ms      2.3x   ✓
```

```bash
cargo build --release
bash bench/run_all.sh          # micro benchmarks
bash bench/sunspider/run.sh    # SunSpider benchmarks
```

## Test262 Conformance

**82.1%** of tested ECMAScript spec tests pass (2,291 / 2,789). See [TEST262.md](docs/TEST262.md).

23 categories with **100% pass rate** including: numeric literals, string literals, boolean literals, compound-assignment, if, return, throw, coalesce, keywords, and more.

```bash
git clone --depth 1 https://github.com/nicolo-ribaudo/test262.git
cargo run --release --bin test262_runner
```

## Architecture

![Zinc Architecture](https://s.florek.io/kxpa86ncl43ks87a.png)

### NaN-Boxing

Every JavaScript value fits in a single `u64`:

```
Normal f64:      stored as-is
Tagged values:   SIGN_BIT | QNAN | 3-bit tag | 48-bit payload

Tags: object ptr | int32 (SMI) | boolean | null | undefined | string id | symbol id
```

The operand stack is `Vec<u64>` — 8 bytes per slot, zero heap allocation per value.

## Project Structure

```
src/
  main.rs              CLI: REPL + file execution
  engine.rs            Orchestrator: lex → parse → compile → run
  lexer/               Tokenizer (cursor, tokens, keywords, ASI)
  parser/              Recursive descent + Pratt expression parser
  ast/                 ~80 AST node types
  compiler/            AST → bytecode compiler + disassembler
  vm/                  Stack-based VM (core, builtins, promises, JSON, call)
  jit/                 ARM64 JIT compiler (assembler, executable memory, compiler)
  runtime/             NaN-boxed values, object heap, builtins
  gc/                  Mark-and-sweep GC foundation
  util/                String interner

tests/                 222 tests (unit + parser + e2e + JIT)
bench/                 Micro benchmarks + SunSpider
tools/                 Test262 conformance runner
web/                   WASM playground (HTML + compiled WASM)
```

## Stats

- **~13,000 lines** of Rust
- **222 tests** passing
- **82.1%** Test262 conformance (2,291 / 2,789 tests)
- **384 KB** WASM binary
- **1.75x faster than V8** on JIT-compiled fibonacci
- Zero external dependencies for code generation

## What's Next

- Extend JIT to loop-based functions (not just recursive)
- Generators (`function*`, `yield`)
- Regular expressions (via `regex` crate)
- Prototype chain lookups (real `__proto__` traversal)
- ES modules (`import`/`export`)

## License

MIT
