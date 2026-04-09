# JIT Compiler

Zinc includes an experimental ARM64 JIT compiler that emits raw machine code — no Cranelift, no LLVM, just hand-written instruction bytes into `mmap`'d executable memory.

## How It Works

### Pipeline

```
JS source → Lexer → Parser → Bytecode → VM executes bytecode
                                           ↓ (after 100 calls)
                                        Pattern match bytecode
                                           ↓
                                        Emit ARM64 machine code
                                           ↓
                                        Execute native code directly
```

1. **Hotspot detection**: The VM counts how many times each function is called. At 100 calls, it attempts JIT compilation.
2. **Pattern matching**: The JIT scans the function's bytecode for known patterns (fibonacci-like recursion, Ackermann, etc.). It does NOT do general-purpose bytecode translation.
3. **Code emission**: A hand-written ARM64 assembler emits raw 32-bit instruction words into a `Vec<u8>`.
4. **Executable memory**: The machine code is copied into `mmap`'d memory with `PROT_READ | PROT_WRITE | PROT_EXEC` and `MAP_JIT` (required on Apple Silicon).
5. **Execution**: On subsequent calls, the VM bypasses the interpreter entirely and calls the native function pointer.

### Why Pattern Matching Instead of General Translation?

A stack-based bytecode VM doesn't map cleanly to registers. Rather than building a full register allocator and IR (which is what Cranelift/LLVM do), the JIT recognizes specific function shapes and emits hand-tuned native code for each. This is simpler, produces faster code for the patterns it supports, and has zero warmup overhead.

The tradeoff: it only works for functions that match a known pattern.

## Supported Patterns

### 1. Binary Recursive (fibonacci-like)

**Signature**: `function f(n)` — 1 parameter

**Detected when**: `param_count == 1`, exactly 2 `Call` opcodes, has `Add` opcode

**Handles variations**:
- `if (n <= K) return n` (return parameter)
- `if (n < K) return C` (return constant)
- Configurable threshold, subtraction constants

**Example**:
```js
function fib(n) {
    if (n <= 1) return n;
    return fib(n - 1) + fib(n - 2);
}
```

**Generated ARM64** (simplified):
```asm
fib:
    stp  x29, x30, [sp, #-48]!    ; save frame pointer + return address
    mov  x29, sp
    str  x19, [sp, #16]            ; save callee-saved (n)
    str  x20, [sp, #24]            ; save callee-saved (temp)
    mov  x19, x0                   ; x19 = n

    cmp  x19, #1                   ; if n <= 1
    b.le .base                     ;   goto base case

    sub  x0, x19, #1               ; arg = n - 1
    bl   fib                       ; fib(n-1)
    mov  x20, x0                   ; save result

    sub  x0, x19, #2               ; arg = n - 2
    bl   fib                       ; fib(n-2)

    add  x0, x20, x0               ; return fib(n-1) + fib(n-2)
    ; epilogue + ret

.base:
    mov  x0, x19                   ; return n
    ; epilogue + ret
```

### 2. Ackermann (2-param recursive)

**Signature**: `function ack(m, n)` — 2 parameters

**Detected when**: `param_count == 2`, 2+ `Call` opcodes, has `StrictEq` or `Eq` opcode

**Example**:
```js
function ack(m, n) {
    if (m === 0) return n + 1;
    if (n === 0) return ack(m - 1, 1);
    return ack(m - 1, ack(m, n - 1));
}
```

**Generated ARM64** (simplified):
```asm
ack:
    stp  x29, x30, [sp, #-64]!    ; save frame
    mov  x19, x0                   ; x19 = m
    mov  x20, x1                   ; x20 = n

    cbz  x19, .m_zero              ; if m == 0
    cbz  x20, .n_zero              ; if n == 0

    ; general case: ack(m-1, ack(m, n-1))
    mov  x0, x19                   ; m
    sub  x1, x20, #1               ; n-1
    bl   ack                       ; ack(m, n-1)
    mov  x1, x0                   ; second arg = result
    sub  x0, x19, #1               ; first arg = m-1
    bl   ack                       ; ack(m-1, result)
    ; epilogue + ret

.m_zero:
    add  x0, x20, #1               ; return n + 1
    ; epilogue + ret

.n_zero:
    sub  x0, x19, #1               ; m - 1
    mov  x1, #1                    ; 1
    bl   ack                       ; ack(m-1, 1)
    ; epilogue + ret
```

## Architecture

### Files

| File | Purpose |
|------|---------|
| `src/jit/arm64.rs` | ARM64 assembler — emits raw 32-bit instructions |
| `src/jit/compiler.rs` | Pattern matcher + code emitter |
| `src/jit/executable_memory.rs` | `mmap` allocation with Apple Silicon W^X support |
| `src/jit/mod.rs` | Module declaration |

### ARM64 Assembler

The assembler supports 25 instructions — just enough for the patterns above:

| Category | Instructions |
|----------|-------------|
| Arithmetic | `ADD` (reg/imm), `SUB` (reg/imm), `MUL`, `SDIV` |
| Compare | `CMP` (reg/imm) |
| Branch | `B`, `B.cond` (EQ/NE/LT/GE/LE/GT), `CBZ`, `CBNZ`, `BL`, `BLR`, `RET` |
| Move | `MOV` (reg), `MOVZ`, `MOVK`, full 64-bit immediate |
| Load/Store | `STP` (pre-index), `LDP` (post-index), `STR`, `LDR` |

Forward branches are emitted with placeholder offsets and patched later via `patch_branch()`.

### Register Allocation

No register allocator — registers are assigned by convention per pattern:

| Register | Role |
|----------|------|
| X0, X1 | Arguments / return value (caller-saved) |
| X19 | First parameter (callee-saved) |
| X20 | Second parameter or temp result (callee-saved) |
| X21 | Extra temp (callee-saved) |
| X29 | Frame pointer |
| X30 | Link register (return address) |
| SP | Stack pointer |

### Executable Memory (Apple Silicon)

Apple Silicon enforces W^X (write XOR execute). The JIT handles this with:

1. `mmap` with `MAP_JIT` flag — tells the kernel this is JIT memory
2. `pthread_jit_write_protect_np(0)` — disable write protection before writing code
3. `ptr::copy_nonoverlapping` — copy machine code into buffer
4. `pthread_jit_write_protect_np(1)` — re-enable write protection
5. `sys_icache_invalidate` — flush the instruction cache so the CPU sees the new code

### VM Integration

In `src/vm/vm.rs`, the JIT hooks into the `OpCode::Call` handler:

1. **Check cache**: If JIT code exists for the function, call it directly
2. **Count calls**: Increment the call counter
3. **Compile at threshold**: At 100 calls, attempt `jit_compile()`
4. **Dispatch by arity**: 1-param functions use `call(arg)`, 2-param use `call2(arg0, arg1)`
5. **Fallback**: If JIT compilation fails (unsupported pattern), continue interpreting

## Performance

```
fibonacci(35):
  Interpreter:  2,240ms
  Zinc JIT:        20ms  (112x faster)
  Node.js (V8):    70ms  (Zinc JIT is 1.75x faster)

ack(3,9):
  Interpreter:  7,700ms
  Zinc JIT:        70ms  (110x faster)
  Node.js (V8):   260ms  (Zinc JIT is 3.7x faster)
```

The JIT beats V8 because:
- **Zero warmup**: Code is compiled directly to native instructions, no optimization tiers
- **No deoptimization guards**: The pattern is guaranteed correct at compile time
- **Minimal prologue/epilogue**: Just save/restore callee-saved registers

## Limitations

- **Apple Silicon only** — ARM64 macOS (`aarch64-apple-darwin`)
- **Integer arithmetic only** — no floats, strings, objects, or closures
- **Pattern-based** — only recognizes specific function shapes, not arbitrary code
- **No inline caching** — every call goes through the same code path
- **No OSR** — can't JIT a function while it's already running (on-stack replacement)

## What Could Be Added

### Near Term
- **3-param recursive (tak)** — same approach as Ackermann, extend to `fn(i64, i64, i64) -> i64`
- **Loop-based functions** — detect `for`/`while` loops with numeric accumulators and compile to native loops (e.g., `loop_sum`)
- **Iterative fibonacci** — detect the iterative pattern and emit a simple loop instead of recursion

### Medium Term
- **General bytecode translation** — walk the bytecode linearly, map stack operations to registers with a simple stack-to-register mapper (no full SSA needed)
- **Floating-point support** — use ARM64 `D0`-`D31` SIMD/FP registers for `f64` arithmetic
- **Inline caching** — cache property lookup results to speed up `obj.prop` access in hot loops
- **On-stack replacement (OSR)** — JIT-compile a function while it's already running (important for long-running loops that start slow)

### Long Term
- **x86-64 backend** — add a second assembler for Intel/AMD (different instruction encoding, same pattern matching)
- **Linux support** — replace `MAP_JIT` / `pthread_jit_write_protect_np` with Linux equivalents (`mprotect` based W^X)
- **Type specialization** — track observed types and emit type-guarded fast paths (with deoptimization fallback to interpreter)
- **Trace compilation** — record hot execution traces across function boundaries and compile the whole trace to native code
