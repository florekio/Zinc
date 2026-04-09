# Zinc

A JavaScript engine written from scratch in Rust. Also compiles to **WebAssembly** and runs in the browser.

Zinc implements a complete pipeline from source code to execution: **lexer** → **parser** → **bytecode compiler** → **virtual machine**. Every component is hand-written with zero runtime dependencies on existing JS engines.

**81.7% [Test262](docs/TEST262.md) conformance** | **217 tests** | **~13,000 lines of Rust**

![Zinc Playground](web/screenshot.png)

## Try It

**In the browser** — no install needed:

```bash
cd web && python3 -m http.server 8080
# Open http://localhost:8080
```

**As a CLI:**

```bash
# Build
cargo build --release

# Run a JS file
cargo run --release -- script.js

# Interactive REPL
cargo run --release

# Run tests
cargo test

# Run Test262 conformance suite
cargo run --release --bin test262_runner
```

**Build the WASM playground from source:**

```bash
wasm-pack build --target web --out-dir web/pkg
```

## What It Can Do

```javascript
// Classes with methods
class Animal {
    constructor(name) { this.name = name; }
    speak() { return `${this.name} makes a sound`; }
}
var dog = new Animal("Rex");
console.log(dog.speak()); // Rex makes a sound

// Closures
function makeCounter() {
    var count = 0;
    return function() { return ++count; };
}
var c = makeCounter();
console.log(c(), c(), c()); // 1 2 3

// Promises + async/await
async function fetchData() {
    var x = await Promise.resolve(10);
    var y = await Promise.resolve(20);
    return x + y;
}
fetchData().then(function(v) { console.log(v); }); // 30

// Higher-order array methods
var result = [1, 2, 3, 4, 5]
    .filter(function(x) { return x % 2 !== 0; })
    .map(function(x) { return x * x; })
    .reduce(function(a, b) { return a + b; }, 0);
console.log(result); // 35

// Destructuring
var { name, age } = { name: "Alice", age: 30 };
var [a, b, c] = [10, 20, 30];

// Error handling
try {
    throw new TypeError("something went wrong");
} catch (e) {
    console.log(e.name + ": " + e.message);
}

// JSON
var data = JSON.parse('{"users": [{"name": "Bob"}, {"name": "Eve"}]}');
console.log(data.users[0].name); // Bob

// Math
console.log(Math.sqrt(144));     // 12
console.log(Math.max(1, 5, 3));  // 5
```

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
| **Number** | `Number.NaN`, `POSITIVE_INFINITY`, `MAX_SAFE_INTEGER`, `isNaN`, `isFinite`, `isInteger` |
| **Wrapper objects** | `new Number()`, `new Boolean()`, `new String()` with proper ToPrimitive coercion |
| **Globals** | `console.log`/`warn`/`error`, `parseInt`, `parseFloat`, `isNaN`, `isFinite`, `String`, `Number`, `Boolean`, `String.fromCharCode`, `Array.isArray` |

### Engine Internals

- **NaN-boxed values** — every JS value in 8 bytes via IEEE 754 quiet NaN space with sign-bit tagging
- **~130 bytecode opcodes** with variable-length encoding
- **Stack-based VM** with call frames, operand stack, and upvalue-based closures
- **Pratt parser** with precedence climbing across ~25 levels
- **Lua-style upvalues** — open (stack) → closed (heap) for proper closure semantics
- **String interning** — O(1) comparison for all identifiers and property names
- **Arena-based object heap** for GC-managed objects
- **Mark-and-sweep GC** foundation
- **Microtask queue** for Promise resolution
- **WebAssembly build** — 384 KB WASM binary

## Benchmarks

Zinc vs Node.js v22 (V8 JIT) — see [BENCHMARKS.md](docs/BENCHMARKS.md) for details.

```
Benchmark              Zinc       Node       Ratio
────────────────────────────────────────────────────
fibonacci(35)          1.955s     0.093s     21.0x
loop_sum(1M)           0.094s     0.036s      2.6x
string_concat(10K)     0.061s     0.033s      1.8x
closure_counter(100K)  0.030s     0.034s      0.9x  ← Zinc wins
object_create(100K)    0.036s     0.034s      1.1x  ← tie
sieve(10K)             0.030s     0.034s      0.9x  ← Zinc wins
```

Zinc **matches or beats Node.js** on 3 of 6 benchmarks. The gap on fibonacci (21x) is the expected difference between a bytecode interpreter and a JIT compiler.

```bash
cargo build --release && bash bench/run_all.sh
```

## Test262 Conformance

**81.7%** of tested ECMAScript spec tests pass (2,181 / 2,670). See [TEST262.md](docs/TEST262.md) for the full breakdown.

17 categories with **100% pass rate**: numeric literals, string literals, boolean literals, void, grouping, return, throw, block, empty, expression, punctuators, keywords, line-terminators, coalesce, relational, this, future-reserved-words.

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
  vm/                  Stack-based VM with call frames + microtask queue
  runtime/             NaN-boxed values, object heap, builtins
  gc/                  Mark-and-sweep GC foundation
  util/                String interner

tests/                 217 tests (unit + parser + end-to-end)
bench/                 Benchmark suite (Zinc vs Node.js)
tools/                 Test262 conformance runner
web/                   WASM playground (HTML + compiled WASM)
```

## Stats

- **~13,000 lines** of Rust
- **217 tests** passing
- **81.7%** Test262 conformance (2,181 / 2,670 tests)
- **31 source files**
- **384 KB** WASM binary
- Zero unsafe in hot paths (unsafe only in GC foundation)

## What's Next

- Generators (`function*`, `yield`)
- Regular expressions (via `regex` crate)
- Prototype chain lookups (real `__proto__` traversal)
- ES modules (`import`/`export`)
- Deploy playground to GitHub Pages
- Inline caching for property access performance

## License

MIT

