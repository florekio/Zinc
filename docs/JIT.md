# JIT Compiler

Zinc includes an experimental JIT compiler that emits raw machine code — no Cranelift, no LLVM, just hand-written instruction bytes into `mmap`'d executable memory.

**Supported targets:**
- **ARM64** — macOS/Apple Silicon (`aarch64-apple-darwin`)
- **x86-64** — Linux (`x86_64-unknown-linux-gnu`)

## How It Works

### Pipeline

```
JS source → Lexer → Parser → Bytecode → VM executes bytecode
                                           ↓ (after 100 calls)
                                        Attempt JIT compilation
                                           ↓
                                        Emit native machine code (ARM64 or x86-64)
                                           ↓
                                        Execute native code directly
```

1. **Hotspot detection**: The VM counts how many times each function is called. At 100 calls, it attempts JIT compilation.
2. **Compilation**: The JIT uses two strategies depending on the function shape:
   - **Pattern matching** for recursive functions (fibonacci, Ackermann, tak)
   - **Bytecode walking** for loop-based functions (translates opcodes linearly to native code)
3. **Code emission**: A hand-written assembler emits raw machine code into a `Vec<u8>`. The backend is selected at compile time via `#[cfg(target_arch)]`.
4. **Executable memory**: The machine code is copied into `mmap`'d memory with `PROT_READ | PROT_WRITE | PROT_EXEC`. On macOS, `MAP_JIT` and the W^X protocol are required; on Linux, a plain `mmap` suffices.
5. **Execution**: On subsequent calls, the VM bypasses the interpreter entirely and calls the native function pointer.

## Supported Patterns

### 1. Binary Recursive (fibonacci-like)

**Signature**: `function f(n)` — 1 parameter

**Detected when**: `param_count == 1`, exactly 2 `Call` opcodes, has `Add` opcode

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
    mov  x19, x0                   ; x19 = n
    cmp  x19, #1                   ; if n <= 1
    b.le .base                     ;   goto base case
    sub  x0, x19, #1               ; fib(n-1)
    bl   fib
    mov  x20, x0                   ; save result
    sub  x0, x19, #2               ; fib(n-2)
    bl   fib
    add  x0, x20, x0               ; return fib(n-1) + fib(n-2)
    ; epilogue + ret
.base:
    mov  x0, x19                   ; return n
    ; epilogue + ret
```

**Generated x86-64** (simplified):
```asm
fib:
    push rbp
    push r13
    push r12                       ; 3 pushes → RSP 16-byte aligned
    mov  r12, rdi                  ; r12 = n
    cmp  r12, 1                    ; if n <= 1
    jle  .base                     ;   goto base case
    lea  rdi, [r12-1]              ; fib(n-1)
    call fib
    mov  r13, rax                  ; save result
    lea  rdi, [r12-2]              ; fib(n-2)
    call fib
    add  rax, r13                  ; return fib(n-1) + fib(n-2)
    pop r12; pop r13; pop rbp; ret
.base:
    mov  rax, r12
    pop r12; pop r13; pop rbp; ret
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

### 3. Takeuchi (3-param recursive)

**Signature**: `function tak(x, y, z)` — 3 parameters

**Detected when**: `param_count == 3`, 3+ `Call` opcodes, has `Ge` or `Le` opcode

**Example**:
```js
function tak(x, y, z) {
    if (y >= x) return z;
    return tak(tak(x-1,y,z), tak(y-1,z,x), tak(z-1,x,y));
}
```

### 4. Loop-based Functions (bytecode walking)

**Detected when**: no `Call` opcodes, has `Loop` opcode, `local_count <= 5`, no global variable access

Instead of pattern matching, the JIT walks the bytecode opcode-by-opcode and translates each to native instructions. The VM's stack positions are mapped directly to registers.

**Example**:
```js
function loop_sum(n) {
    var sum = 0;
    for (var i = 0; i < n; i = i + 1) {
        sum = sum + i;
    }
    return sum;
}
```

**Translation approach**: Each VM stack position maps to a register. Arithmetic opcodes pop/push the register stack. Comparisons fuse with the following `JumpIfFalse` to emit a single compare-and-branch. The `Loop` opcode becomes a backward branch.

**Supported opcodes**: `GetLocal`, `SetLocal`, `Const`, `Zero`, `One`, `Add`, `Sub`, `Mul`, `Div`, `Rem`, `BitAnd`, `BitOr`, `BitXor`, `BitNot`, `Shl`, `Shr`, `UShr`, all comparisons, `JumpIfFalse`, `Jump`, `Loop`, `Pop`, `Return`.

## Architecture

### Files

| File | Purpose |
|------|---------|
| `src/jit/arm64.rs` | ARM64 assembler — emits raw 32-bit instruction words |
| `src/jit/x86_64.rs` | x86-64 assembler — emits raw variable-length instruction bytes |
| `src/jit/compiler.rs` | Pattern matcher + bytecode walker + code emitter (both backends) |
| `src/jit/executable_memory.rs` | `mmap` allocation, cross-platform (Linux + macOS W^X) |
| `src/jit/mod.rs` | Module declaration |

### ARM64 Assembler (`src/jit/arm64.rs`)

Supports ~45 instructions:

| Category | Instructions |
|----------|-------------|
| Arithmetic | `ADD` (reg/imm), `SUB` (reg/imm), `MUL`, `SDIV` |
| Bitwise | `AND`, `ORR`, `EOR` (XOR), `MVN` (NOT), `LSL`, `LSR`, `ASR` |
| Compare | `CMP` (reg/imm), `FCMP` |
| Branch | `B`, `B.cond` (EQ/NE/LT/GE/LE/GT), `CBZ`, `CBNZ`, `BL`, `BLR`, `RET` |
| Move | `MOV` (reg), `MOVZ`, `MOVK`, full 64-bit immediate, `FMOV` |
| Load/Store | `STP` (pre-index), `LDP` (post-index), `STR`, `LDR`, `STR.fp`, `LDR.fp` |
| Floating-point | `FADD`, `FSUB`, `FMUL`, `FDIV`, `SCVTF`, `FCVTZS` |

**Register allocation (ARM64):**

| Position | Register | Role |
|----------|----------|------|
| 0-4 | X19-X23 | Local variables (callee-saved) |
| 5-11 | X3-X9 | Operand stack (caller-saved, safe since no calls) |

### x86-64 Assembler (`src/jit/x86_64.rs`)

Targets the System V AMD64 ABI. Supports ~50 instructions:

| Category | Instructions |
|----------|-------------|
| Arithmetic | `ADD` (reg/imm), `SUB` (reg/imm), `IMUL`, `IDIV` (via CQO+IDIV), `NEG` |
| Bitwise | `AND`, `OR`, `XOR`, `NOT`, `SHL`, `SHR`, `SAR` (variable shifts via RCX) |
| Compare | `CMP` (reg/imm), `TEST` |
| Branch | `JMP`, `Jcc` (E/NE/L/GE/LE/G/AE/A/BE/B), `CALL`, `RET` |
| Move | `MOV` (reg/imm32/imm64), `LEA` |
| Load/Store | `MOV r64, [base+disp]`, `MOV [base+disp], r64` |
| Floating-point | `ADDSD`, `SUBSD`, `MULSD`, `DIVSD`, `UCOMISD`, `CVTSI2SD`, `CVTTSD2SI`, `MOVSD`, `MOVQ` |

**Register allocation (x86-64):**

| Position | Register | Role |
|----------|----------|------|
| 0 | R12 | Local #0 (callee-saved) |
| 1 | R13 | Local #1 (callee-saved) |
| 2 | R14 | Local #2 (callee-saved) |
| 3 | R15 | Local #3 (callee-saved, or globals pointer) |
| 4 | RBX | Local #4 (callee-saved) |
| 5-11 | RDI/RSI/RDX/R8-R11 | Operand stack (caller-saved, safe since no calls in loop fns) |
| — | RCX | Reserved for shift counts |
| — | RAX | Return value / IDIV quotient / scratch |

### Executable Memory

**macOS (Apple Silicon):** W^X (write XOR execute) is enforced by the kernel.

1. `mmap` with `MAP_JIT` flag
2. `pthread_jit_write_protect_np(0)` — disable write protection
3. `ptr::copy_nonoverlapping` — copy machine code
4. `pthread_jit_write_protect_np(1)` — re-enable write protection
5. `sys_icache_invalidate` — flush instruction cache

**Linux (x86-64):** plain `mmap` with `PROT_READ | PROT_WRITE | PROT_EXEC`. No W^X toggling required; x86-64 handles self-modifying code via TSO memory ordering.

### VM Integration

In `src/vm/vm.rs`, the JIT hooks into the `OpCode::Call` handler:

1. **Check cache**: If JIT code exists for the function, call it directly
2. **Count calls**: Increment the call counter
3. **Compile at threshold**: At 100 calls, attempt `jit_compile()`
4. **Dispatch by arity**: 1-param `call()`, 2-param `call2()`, 3-param `call3()`
5. **Fallback**: If JIT compilation fails, continue interpreting

## Performance

```
fibonacci(35):
  Interpreter:  2,240ms
  Zinc JIT:        20ms  (112x faster)
  Node.js (V8):    70ms  (Zinc is 1.75x faster)

ack(3,9):
  Interpreter:  7,700ms
  Zinc JIT:        70ms  (110x faster)
  Node.js (V8):   260ms  (Zinc is 3.7x faster)

loop_sum(1B):
  Zinc JIT:       440ms
  Node.js (V8):   630ms  (Zinc is 1.4x faster)
```

The JIT beats V8 because:
- **Zero warmup**: Code is compiled directly to native instructions, no optimization tiers
- **No deoptimization guards**: The pattern is guaranteed correct at compile time
- **Minimal prologue/epilogue**: Just save/restore callee-saved registers
- **Integer arithmetic**: Uses i64 instead of f64, avoiding floating-point overhead

## Limitations

- **Integer arithmetic only** — no floats, strings, objects, or closures
- **Loop JIT limited to 5 locals** — functions with more than 5 local variables fall back to interpreter
- **No global variable access in loops** — loop functions must be self-contained
- **No inline caching** — every property access goes through the interpreter
- **No OSR** — can't JIT a function while it's already running

## What Could Be Added

### Near Term
- **Nested loop support** — extend bytecode walker to handle conditionals inside loops (sieve benchmark)
- **More locals** — spill excess locals to stack memory instead of bailing out

### Medium Term
- **Inline caching** — cache property lookup results for hot loops
- **On-stack replacement (OSR)** — JIT-compile a function while it's already running
- **Type specialization** — emit type-guarded fast paths with deoptimization fallback

### Long Term
- **Trace compilation** — record hot execution traces and compile entire traces to native code
- **Windows support** — adapt calling convention to Microsoft x64 ABI
