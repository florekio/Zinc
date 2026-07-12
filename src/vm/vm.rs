use std::collections::HashMap;
use std::fmt;

use crate::compiler::chunk::Chunk;
use crate::compiler::opcode::OpCode;
use crate::compiler::chunk::ChunkFlags;
use crate::runtime::object::{GeneratorState, JsObject, ObjectHeap, ObjectId, ObjectKind, PromiseState, Property, trace_value};
use crate::runtime::value::Value;
use crate::util::interner::{Interner, StringId};

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Returns true for implementation-internal property keys (e.g. __class__, __get_x__,
/// __priv_#x__). These should not appear in for-in enumeration or Object.keys().
#[inline(always)]
fn is_internal_key(s: &str) -> bool {
    s.starts_with("__") && s.ends_with("__")
}

/// Format a finite f64 the way `Number.prototype.toString` does (shortest
/// round-trip decimal, with exponential notation for |x| < 1e-6 or |x| >= 1e21).
fn js_format_number(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if f == 0.0 {
        return "0".into();
    }
    let abs = f.abs();
    if !(1e-6..1e21).contains(&abs) {
        // Exponential notation. Rust's `{:e}` gives e.g. "1e-7" or "1e21";
        // JS spec requires "1e-7" / "1e+21" — i.e. positive exponents take "+".
        let raw = format!("{f:e}");
        // raw looks like "1e-7", "1e21", "1.5e10", "-1.5e-7", etc.
        if let Some(epos) = raw.find('e') {
            let exp = &raw[epos + 1..];
            if !exp.starts_with('-') && !exp.starts_with('+') {
                return format!("{}e+{}", &raw[..epos], exp);
            }
        }
        raw
    } else {
        format!("{f}")
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum VmError {
    TypeError(String),
    ReferenceError(String),
    RuntimeError(String),
    /// A throw that escaped a protected nested call (e.g. valueOf during a
    /// comparison opcode). The opcode handler is expected to catch this and
    /// re-route via `handle_throw` so the stack stays balanced.
    Throw(Value),
}

/// Classified operands for a numeric/bitwise binary operation.
pub(crate) enum ArithOperands {
    Numbers(f64, f64),
    BigInts(num_bigint::BigInt, num_bigint::BigInt),
    /// One operand is a BigInt and the other isn't — a TypeError.
    Mixed,
}

impl fmt::Display for VmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VmError::TypeError(msg) => write!(f, "TypeError: {msg}"),
            VmError::ReferenceError(msg) => write!(f, "ReferenceError: {msg}"),
            VmError::RuntimeError(msg) => write!(f, "RuntimeError: {msg}"),
            VmError::Throw(_) => write!(f, "Throw"),
        }
    }
}

impl std::error::Error for VmError {}

// ---------------------------------------------------------------------------
// VM
// ---------------------------------------------------------------------------

/// An upvalue: a reference to a variable that may still be on the stack (open)
/// or has been moved to the heap (closed).
#[derive(Clone, Debug)]
pub(crate) enum UpvalueLocation {
    /// Points to a stack slot (variable still on stack).
    Open(usize),
    /// Value has been closed over (moved to heap when enclosing function returned).
    Closed(Value),
}

/// One captured variable, shared (Lua-style) between every closure that
/// captures it: all holders see writes and the close transition through
/// the same Rc'd cell. Cloning an Upvalue clones the handle, not the
/// location — that sharing is what makes sibling closures communicating
/// through a captured variable (every scheduler / state machine in
/// minified bundles) actually work.
#[derive(Clone, Debug)]
pub(crate) struct Upvalue {
    pub(crate) cell: std::rc::Rc<std::cell::RefCell<UpvalueLocation>>,
}

impl Upvalue {
    pub(crate) fn get(&self, stack: &[Value]) -> Value {
        match &*self.cell.borrow() {
            UpvalueLocation::Open(idx) => stack.get(*idx).copied().unwrap_or(Value::undefined()),
            UpvalueLocation::Closed(v) => *v,
        }
    }
}

pub(crate) struct CallFrame {
    pub(crate) chunk_idx: usize,
    pub(crate) ip: usize,
    pub(crate) base: usize,
    pub(crate) upvalues: Vec<Upvalue>,
    /// The `this` value for this call.
    pub(crate) this_value: Value,
    /// If true, ReturnUndefined returns this_value instead.
    pub(crate) is_constructor: bool,
    /// If true, the next Call should propagate this_value (for super()).
    pub(crate) pending_super_call: bool,
    /// If Some, this frame belongs to a generator object.
    pub(crate) generator_id: Option<crate::runtime::object::ObjectId>,
    /// Number of actual arguments passed to this function call.
    #[allow(dead_code)]
    pub(crate) argc: usize,
    /// Snapshot of the actual argument values, captured at call time before
    /// locals can overwrite the same stack slots (needed for `arguments` object).
    pub(crate) saved_args: Vec<Value>,
    /// Cached `arguments` object id — allocated lazily on first reference and
    /// reused so identity holds (`arguments === arguments`) and writes
    /// (e.g. via `with`) are visible on subsequent reads.
    pub(crate) arguments_oid: Option<crate::runtime::object::ObjectId>,
    /// True if this frame is a derived-class constructor (one with an
    /// `extends` clause). When set, returning without calling `super()`
    /// must throw `ReferenceError` — the spec rule that prevents access to
    /// an uninitialized `this`.
    pub(crate) is_derived_ctor: bool,
    /// True once `super()` has been called inside this constructor frame.
    /// Used together with `is_derived_ctor` to detect missing-super returns.
    pub(crate) super_called: bool,
    /// The `new.target` value for this frame: the constructor used in the
    /// `new` expression, or `undefined` for non-constructor calls. Arrow
    /// functions inherit this from the enclosing scope at call time.
    pub(crate) new_target: Value,
    /// Set by the Call opcode while dispatching a `super()` call. After the
    /// parent constructor returns and yields an object, the child constructor
    /// must rebind its `this` to that object (per spec BindThisValue).
    pub(crate) await_super_result: bool,
    /// `with_stack` length when this frame was entered. On return the stack is
    /// truncated back to this depth, so a `return` out of a `with` block pops
    /// its scope (the lexical WithExit at the body's end is skipped by the jump).
    pub(crate) with_base: usize,
}

/// An active exception handler (pushed by PushExcHandler).
#[allow(dead_code)]
pub(crate) struct ExcHandler {
    pub(crate) catch_target: u16,
    pub(crate) finally_target: u16,
    pub(crate) stack_depth: usize,
    pub(crate) frame_idx: usize,
    /// with_stack length at handler-push time; on unwind, with_stack is truncated
    /// to this length so exiting via throw still pops `with` scopes correctly.
    pub(crate) with_depth: usize,
}

#[derive(Clone)]
pub(crate) enum Microtask {
    PromiseReaction {
        callback: Option<Value>,
        value: Value,
        result_promise: ObjectId,
        is_fulfilled: bool,
    },
}

/// Global opcode-execution histogram for profiling (ZINC_OPCODE_HIST=1).
/// Indexed by opcode byte; aggregates across every VM instance in the process.
pub static OPCODE_HIST: [std::sync::atomic::AtomicU64; 256] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 256];

/// Whether `ZINC_TRACE_CALLS` is set, cached after the first read. The Call and
/// CallMethod opcodes consult this on *every* call, so reading the env var each
/// time (a lock + syscall) was a measurable hot-path cost on call-heavy code.
#[inline]
fn trace_calls_enabled() -> bool {
    static TRACE_CALLS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *TRACE_CALLS.get_or_init(|| std::env::var("ZINC_TRACE_CALLS").is_ok())
}

/// Whether `ZINC_FUEL_TRACE` is set: enables the fuel-checkpoint sampling
/// profiler that pinpoints a runaway loop when the step limit is hit.
#[inline]
fn fuel_trace_enabled() -> bool {
    static FUEL_TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FUEL_TRACE.get_or_init(|| std::env::var("ZINC_FUEL_TRACE").is_ok())
}

/// Dump the opcode histogram (most-executed first) to stderr. Call once at
/// program end. Prints each opcode's count and share of total dispatched
/// instructions, plus the grand total — the basis for instructions/sec.
pub fn dump_opcode_histogram() {
    use std::sync::atomic::Ordering::Relaxed;
    let mut rows: Vec<(u8, u64)> = (0..=255u8)
        .map(|b| (b, OPCODE_HIST[b as usize].load(Relaxed)))
        .filter(|&(_, c)| c > 0)
        .collect();
    let total: u64 = rows.iter().map(|&(_, c)| c).sum();
    rows.sort_by_key(|r| std::cmp::Reverse(r.1));
    eprintln!("=== opcode histogram (total dispatched: {total}) ===");
    for (b, c) in rows {
        let name = if crate::compiler::opcode::OpCode::is_valid(b) {
            format!("{:?}", unsafe { std::mem::transmute::<u8, crate::compiler::opcode::OpCode>(b) })
        } else {
            format!("0x{b:02x}")
        };
        let pct = 100.0 * c as f64 / total as f64;
        eprintln!("{c:>14}  {pct:5.1}%  {name}");
    }
}

/// Inline cache entry for GetGlobal: (name_id, cached_value).
/// Keyed by (chunk_idx, bytecode_offset).
pub(crate) type GlobalIC = HashMap<(usize, usize), (StringId, Value)>;

pub struct Vm {
    pub(crate) chunks: Vec<Chunk>,
    pub(crate) frames: Vec<CallFrame>,
    pub(crate) stack: Vec<Value>,
    pub(crate) globals: HashMap<StringId, Value>,
    /// Fast global lookup by StringId index (parallel to HashMap for hot path).
    pub(crate) globals_vec: Vec<Value>,
    pub(crate) interner: Interner,
    pub(crate) heap: ObjectHeap,
    #[allow(dead_code)]
    pub(crate) global_ic: GlobalIC,
    #[allow(dead_code)]
    pub(crate) global_version: u64,
    #[allow(dead_code)]
    pub(crate) global_ic_version: HashMap<(usize, usize), u64>,
    pub(crate) exc_handlers: Vec<ExcHandler>,
    /// Lower bound for handle_throw unwinding. Set by `try_coerce_to_primitive_hint`
    /// (and similar) before calling into user code so a throw that would unwind
    /// past the current opcode is converted to `Err(VmError::Throw(_))`. The opcode
    /// handler can then catch it and re-throw via `handle_throw` once its own stack
    /// expectations are met.
    pub(crate) protect_throw_depth: usize,
    pub(crate) microtask_queue: Vec<Microtask>,
    /// Heap objects the embedder holds across VM re-entries (e.g. a
    /// pending promise it will settle from `host_promise_resolve`
    /// once an async operation finishes). Treated as GC roots until
    /// unpinned.
    pub(crate) host_roots: Vec<ObjectId>,
    /// Active `with`-scope objects (innermost last). Names resolve here before globals.
    pub(crate) with_stack: Vec<ObjectId>,
    /// With-scope objects captured by closures created inside a `with` body,
    /// keyed by closure_id. Re-pushed onto `with_stack` for the duration of a
    /// call so the closure still sees its lexical with-scope after the block
    /// exits (spec: the function's [[Environment]] includes the object env).
    pub(crate) closure_withs: std::collections::HashMap<usize, std::rc::Rc<Vec<ObjectId>>>,
    /// Names of top-level lexical bindings (let/const). They live in the
    /// globals map but are NOT properties of globalThis, and SetGlobal must
    /// not mirror them onto it.
    pub(crate) lex_globals: std::collections::HashSet<crate::util::interner::StringId>,
    /// Top-level lexical bindings whose declaration has not executed yet
    /// (script/eval-level TDZ): reads and writes throw ReferenceError.
    pub(crate) tdz_globals: std::collections::HashSet<crate::util::interner::StringId>,
    /// Lexical (this, new.target) captured by each arrow-function closure at
    /// creation, keyed by closure_id. Arrows must see their DEFINING scope's
    /// `this` — apply/call/bind and later calls from other contexts must not
    /// rebind it.
    pub(crate) closure_arrow_ctx: std::collections::HashMap<usize, (Value, Value)>,
    /// The defining scope's `arguments` object, captured by arrows whose
    /// body references `arguments` (chunk.uses_arguments). Keyed by
    /// closure_id; only materialized when needed.
    pub(crate) closure_arrow_args: std::collections::HashMap<usize, Value>,
    /// One-shot with_base override for the next frame pushed by
    /// call_function_this. Direct eval runs in the caller's scope, so its
    /// frame must inherit the caller's with-visibility instead of starting a
    /// fresh (empty) one.
    pub(crate) eval_inherit_with_base: Option<usize>,
    /// Set by MarkDirectEval for the immediately following Call: the callee
    /// was the syntactic identifier `eval`, so if it resolves to the eval
    /// builtin the call is a DIRECT eval (inherits contextual permissions).
    pub(crate) direct_eval_pending: bool,
    /// Lexical private-name environment chain per closure (innermost class
    /// first), keyed by closure_id. A closure created inside class code
    /// inherits its creator's chain; installing a closure on a class prepends
    /// that class evaluation. Used for brand checks: two evaluations of the
    /// same class text are distinct environments, so #x of one is invisible
    /// to the other even though both store it under the same mangled key.
    pub(crate) closure_private_env: std::collections::HashMap<usize, std::rc::Rc<Vec<ObjectId>>>,
    /// Completion value of the currently running script/eval. Value-producing
    /// statements update it via SetCompletion; Halt returns it. Saved/restored
    /// around nested `eval` so an inner eval can't corrupt the outer's value.
    pub(crate) script_completion: Value,
    /// Nesting depth of parameter-default evaluation. While > 0, a direct eval
    /// runs in the parameter scope and its var/function declarations may collide.
    pub(crate) param_scope_depth: usize,
    /// Upvalues for each closure, indexed by closure_id.
    pub(crate) closure_upvalues: Vec<Vec<Upvalue>>,
    /// Live `Open` upvalue cells keyed by absolute stack slot, so every
    /// closure capturing the same variable shares one cell. Entries are
    /// removed (and their cells flipped to `Closed`) when the slot dies.
    pub(crate) open_upvalues: std::collections::HashMap<usize, std::rc::Rc<std::cell::RefCell<UpvalueLocation>>>,
    /// Call counter per chunk index (for JIT hotspot detection).
    #[cfg(any(all(target_arch = "aarch64", target_os = "macos"), target_arch = "x86_64"))]
    pub(crate) call_counts: HashMap<usize, u32>,
    /// JIT-compiled native functions, keyed by chunk index.
    #[cfg(any(all(target_arch = "aarch64", target_os = "macos"), target_arch = "x86_64"))]
    pub(crate) jit_functions: HashMap<usize, crate::jit::compiler::JitFunction>,
    /// console.log output buffer (for testing)
    pub output: Vec<String>,
    /// When true, console.log/warn/error capture into `output` but do not print
    /// to stdout/stderr. Used by the test262 runner to keep $DONE markers off
    /// the orchestrator's pipes.
    pub silent_console: bool,
    /// Module cache: maps module path → exports ObjectId
    pub(crate) module_cache: HashMap<String, ObjectId>,
    /// Base directory for resolving relative module imports
    pub(crate) module_dir: Option<String>,
    /// Regex compilation cache
    pub(crate) regex_cache: crate::vm::regexp::RegexCache,
    /// Function prototype cache: maps packed function value → prototype ObjectId
    pub(crate) func_prototypes: HashMap<i32, ObjectId>,
    /// Singleton Object.prototype object
    pub(crate) object_prototype: ObjectId,
    /// Singleton Function.prototype object
    pub(crate) function_prototype: ObjectId,
    /// Singleton Array.prototype object
    pub(crate) array_prototype: ObjectId,
    /// Singleton Promise.prototype object — set on every new Promise so
    /// `Object.getPrototypeOf(p) === Promise.prototype` and prototype lookups
    /// for `then`/`catch`/`finally` still resolve.
    pub(crate) promise_prototype: ObjectId,
    /// Lazily-created shared prototype for builtin iterator objects
    /// (Array/Map/Set/Key iterators). Holds a native `next` so
    /// `Object.getPrototypeOf([].values()).next` is callable — core-js's
    /// defineIterator (DuckDuckGo's polyfills bundle) requires it.
    /// `.next()` CALLS still dispatch on ObjectKind first; this object
    /// serves property reads and getPrototypeOf.
    pub(crate) iterator_prototype: Option<ObjectId>,
    pub(crate) throw_type_error: Option<Value>,
    pub(crate) generator_function_proto: Option<ObjectId>,
    pub(crate) async_function_proto: Option<ObjectId>,
    /// Singleton Boolean.prototype object
    pub(crate) boolean_prototype: ObjectId,
    /// Singleton Number.prototype object
    pub(crate) number_prototype: ObjectId,
    /// Singleton Date.prototype object
    pub(crate) date_prototype: ObjectId,
    /// Singleton String.prototype object
    pub(crate) string_prototype: ObjectId,
    /// Singleton globalThis object — used as the default `this` for non-strict function calls
    pub(crate) global_this_oid: ObjectId,
    /// Cached Math object ID for fast dispatch
    pub(crate) math_oid: Option<ObjectId>,
    /// Cached JSON object ID for fast dispatch
    pub(crate) json_oid: Option<ObjectId>,
    /// Symbol descriptions: index = symbol_id, value = optional description StringId
    pub(crate) symbol_descriptions: Vec<Option<StringId>>,
    /// Global symbol registry backing `Symbol.for` / `Symbol.keyFor`.
    pub(crate) symbol_registry: std::collections::HashMap<String, u32>,
    /// Next symbol ID to allocate
    pub(crate) next_symbol_id: u32,
    /// Well-known symbol IDs
    pub(crate) sym_iterator: u32,
    pub(crate) sym_has_instance: u32,
    pub(crate) sym_to_primitive: u32,
    pub(crate) sym_to_string_tag: u32,
    pub(crate) sym_species: u32,
    pub(crate) sym_unscopables: u32,
    pub(crate) sym_async_iterator: u32,
    pub(crate) sym_match_all: u32,
    /// Per-function property overrides/deletions: key = (sentinel, StringId), None = deleted, Some(v) = overridden.
    pub(crate) fn_property_overrides: HashMap<(i32, StringId), Option<Value>>,
    /// Dynamic exclusion buffer for object rest destructuring with computed keys.
    pub(crate) computed_exclusions: Vec<Value>,
    /// Fuel counter: instructions executed (incremented in 1024-step chunks).
    pub(crate) steps: u64,
    /// Max instructions before returning an error. 0 = unlimited.
    pub(crate) max_steps: u64,
    /// Wall-clock deadline (set with max_steps): catches tests whose metered
    /// steps each do heavy NATIVE work (O(n) string ops), which the step
    /// budget alone bounds far too loosely.
    pub(crate) deadline: Option<std::time::Instant>,
    /// Sampling profiler for diagnosing runaway loops (ZINC_FUEL_TRACE=1): at
    /// each fuel checkpoint (every 1024 instructions) the current (chunk_idx,
    /// source line) is tallied. On fuel exhaustion the hottest sites are dumped
    /// — they point straight at the spinning loop. Empty/unused when off.
    pub(crate) fuel_samples: HashMap<(u32, u32), u64>,
    /// Diagnostic (ZINC_FUEL_TRACE): receiver-kind tally for CallMethod string
    /// dispatch — [interned-ASCII, interned-non-ASCII, inline, cons/flat].
    /// Reveals whether a hot string-method loop is hitting the O(1) fast path
    /// or the O(n) fallback (e.g. a ConsString receiver).
    pub(crate) string_recv_kinds: [u64; 4],
    /// Diagnostic (ZINC_FUEL_TRACE): per-chunk function-entry counts, to tell a
    /// runaway *caller* (one chunk entered millions of times) from one call
    /// over a huge input. Keyed by chunk_idx.
    pub(crate) fuel_call_counts: HashMap<u32, u64>,
    /// Private-method brand timing: instance oid → class-prototype oids whose
    /// private methods/accessors are NOT yet installed (the instance is still
    /// being constructed and `super()` for those derived levels hasn't returned).
    /// Empty/absent ⇒ all private members accessible (the normal case), so this
    /// never gates ordinary objects. Used only to make `this.#m()` throw when a
    /// base constructor reaches a not-yet-constructed subclass's private member.
    pub(crate) pending_private_brands: HashMap<ObjectId, Vec<ObjectId>>,
}

impl Vm {
    // ---- Construction ------------------------------------------------------











    // ---- Promise helpers ----











    pub fn take_interner(self) -> Interner {
        self.interner
    }

    pub fn interner(&self) -> &Interner {
        &self.interner
    }

    // ---- Stack helpers -----------------------------------------------------

    #[inline]
    pub(crate) fn push(&mut self, value: Value) {
        self.stack.push(value);
    }

    /// Throw a TypeError as a JS exception (caught by try/catch) or propagate as VmError.
    pub(crate) fn throw_type_error(&mut self, msg: &str) -> Result<(), VmError> {
        // Route through handle_throw: it honors protect_throw_depth, so a
        // TypeError raised inside a protected nested call (field initializer
        // thunks, getters, …) bubbles back to the calling opcode instead of
        // unwinding the outer dispatch state out from under it. (The old
        // inline unwind popped the outer handler directly, and the caller's
        // continuation then pushed on top of the redirected stack — a catch
        // block could observe the half-constructed receiver instead of the
        // error.)
        let err = self.make_native_error("TypeError", msg);
        self.handle_throw(err)
    }

    #[inline(always)]
    pub(crate) fn pop(&mut self) -> Result<Value, VmError> {
        self.stack
            .pop()
            .ok_or_else(|| VmError::RuntimeError("stack underflow".into()))
    }

    #[inline(always)]
    pub(crate) fn peek(&self) -> Result<Value, VmError> {
        self.stack
            .last()
            .copied()
            .ok_or_else(|| VmError::RuntimeError("stack underflow".into()))
    }

    // ---- Bytecode read helpers --------------------------------------------

    #[inline(always)]
    pub(crate) fn cur_chunk(&self) -> usize {
        unsafe { self.frames.last().unwrap_unchecked().chunk_idx }
    }

    /// Check if the current frame is in strict mode.
    #[inline(always)]
    #[allow(dead_code)]
    pub(crate) fn is_strict(&self) -> bool {
        self.chunks[self.cur_chunk()].flags.contains(ChunkFlags::STRICT)
    }

    #[inline(always)]
    pub(crate) fn cur_ip(&self) -> usize {
        unsafe { self.frames.last().unwrap_unchecked().ip }
    }

    #[inline(always)]
    pub(crate) fn read_byte(&mut self) -> u8 {
        let frame = unsafe { self.frames.last_mut().unwrap_unchecked() };
        let byte = unsafe { *self.chunks.get_unchecked(frame.chunk_idx).code.get_unchecked(frame.ip) };
        frame.ip += 1;
        byte
    }

    #[inline(always)]
    pub(crate) fn read_u16(&mut self) -> u16 {
        let frame = unsafe { self.frames.last_mut().unwrap_unchecked() };
        let code = &self.chunks[frame.chunk_idx].code;
        let val = ((*unsafe { code.get_unchecked(frame.ip) } as u16) << 8)
            | (*unsafe { code.get_unchecked(frame.ip + 1) } as u16);
        frame.ip += 2;
        val
    }

    #[inline]
    pub(crate) fn read_i16(&mut self) -> i16 {
        let frame = self.frames.last_mut().unwrap();
        let val = self.chunks[frame.chunk_idx].read_i16(frame.ip);
        frame.ip += 2;
        val
    }

    // ---- Numeric helpers --------------------------------------------------







    // ---- String coercion helpers ------------------------------------------



    // ---- JS throw / native error helpers ---------------------------------

    /// IterableToList for AggregateError / Promise.any: arrays, Sets and
    /// strings are the iterables that appear in practice; anything else
    /// that is not obviously iterable throws a TypeError.
    pub(crate) fn simple_iterable_to_list(&mut self, v: Value) -> Result<Vec<Value>, crate::vm::VmError> {
        if let Some(oid) = v.as_object_id()
            && let Some(obj) = self.heap.get(oid)
        {
            match &obj.kind {
                crate::runtime::object::ObjectKind::Array(elements) => {
                    return Ok(elements.iter()
                        .map(|e| if e.is_empty_marker() { Value::undefined() } else { *e })
                        .collect());
                }
                crate::runtime::object::ObjectKind::Set { entries } => {
                    return Ok(entries.clone());
                }
                crate::runtime::object::ObjectKind::ConsString { .. }
                | crate::runtime::object::ObjectKind::FlatString { .. } => {
                    let s = self.value_to_string(v);
                    return Ok(s.chars().map(|c| {
                        let sid = self.interner.intern(&c.to_string());
                        Value::string(sid)
                    }).collect());
                }
                _ => {}
            }
        }
        if v.is_string() {
            let s = self.value_to_string(v);
            return Ok(s.chars().map(|c| {
                let sid = self.interner.intern(&c.to_string());
                Value::string(sid)
            }).collect());
        }
        // Generic objects: run the observable iterator protocol —
        // @@iterator lookup (getters fire), call it, walk next()/done/value.
        if let Some(oid) = v.as_object_id() {
            let method = self.getter_aware_get(oid, "__sym_0__")?.unwrap_or(Value::undefined());
            if !self.value_callable(method) {
                let err = self.make_native_error("TypeError", "argument is not iterable");
                return Err(crate::vm::VmError::Throw(err));
            }
            let prev_protect = self.protect_throw_depth;
            self.protect_throw_depth = self.frames.len() + 1;
            let result = (|| {
                let iter = self.call_function_this(method, v, &[])?;
                let Some(iter_oid) = iter.as_object_id() else {
                    let err = self.make_native_error("TypeError", "iterator result is not an object");
                    return Err(crate::vm::VmError::Throw(err));
                };
                let mut out = Vec::new();
                loop {
                    if out.len() > 1_000_000 {
                        let err = self.make_native_error("RangeError", "iterable too large");
                        return Err(crate::vm::VmError::Throw(err));
                    }
                    let next = self.getter_aware_get(iter_oid, "next")?.unwrap_or(Value::undefined());
                    if !self.value_callable(next) {
                        let err = self.make_native_error("TypeError", "iterator.next is not callable");
                        return Err(crate::vm::VmError::Throw(err));
                    }
                    let res = self.call_function_this(next, iter, &[])?;
                    let Some(res_oid) = res.as_object_id() else {
                        let err = self.make_native_error("TypeError", "iterator result is not an object");
                        return Err(crate::vm::VmError::Throw(err));
                    };
                    let done = self.getter_aware_get(res_oid, "done")?.unwrap_or(Value::undefined());
                    if self.truthy(done) {
                        break;
                    }
                    let val = self.getter_aware_get(res_oid, "value")?.unwrap_or(Value::undefined());
                    out.push(val);
                }
                Ok(out)
            })();
            self.protect_throw_depth = prev_protect;
            return result;
        }
        let err = self.make_native_error("TypeError", "argument is not iterable");
        Err(crate::vm::VmError::Throw(err))
    }

    /// Construct an AggregateError object: `errors` own property (from the
    /// already-collected list), optional message, stack, -539 prototype.
    pub(crate) fn make_aggregate_error(&mut self, errors: Vec<Value>, message_arg: Value) -> Value {
        match self.try_make_aggregate_error(errors, message_arg) {
            Ok(v) => v,
            Err(crate::vm::VmError::Throw(e)) => {
                let _ = self.handle_throw(e);
                Value::undefined()
            }
            Err(_) => Value::undefined(),
        }
    }

    /// AggregateError construction with observable ToString(message).
    pub(crate) fn try_make_aggregate_error(&mut self, errors: Vec<Value>, mut message_arg: Value) -> Result<Value, crate::vm::VmError> {
        if message_arg.is_symbol() {
            let err = self.make_native_error("TypeError", "Cannot convert a Symbol value to a string");
            return Err(crate::vm::VmError::Throw(err));
        }
        if message_arg.is_object() {
            message_arg = self.try_coerce_to_primitive_hint(message_arg, "string")?;
            if message_arg.is_symbol() {
                let err = self.make_native_error("TypeError", "Cannot convert a Symbol value to a string");
                return Err(crate::vm::VmError::Throw(err));
            }
        }
        let mut err = crate::runtime::object::JsObject::ordinary();
        err.prototype = self.func_prototypes.get(&-539).copied().or(Some(self.object_prototype));
        let msg = if message_arg.is_undefined() { String::new() } else { self.value_to_string(message_arg) };
        if !message_arg.is_undefined() {
            let msg_key = self.interner.intern("message");
            let msg_id = self.interner.intern(&msg);
            err.define_property(msg_key, crate::runtime::object::Property::with_flags(
                Value::string(msg_id),
                crate::runtime::object::Property::WRITABLE | crate::runtime::object::Property::CONFIGURABLE,
            ));
        }
        let stack_key = self.interner.intern("stack");
        let stack_id = self.interner.intern(&format!("AggregateError: {msg}"));
        err.set_property(stack_key, Value::string(stack_id));
        let mut list_obj = crate::runtime::object::JsObject::array(errors);
        list_obj.prototype = Some(self.array_prototype);
        let list = Value::object_id(self.heap.allocate(list_obj));
        let errors_key = self.interner.intern("errors");
        err.define_property(errors_key, crate::runtime::object::Property::with_flags(
            list,
            crate::runtime::object::Property::WRITABLE | crate::runtime::object::Property::CONFIGURABLE,
        ));
        Ok(Value::object_id(self.heap.allocate(err)))
    }

    /// Create a native JS error object with `name` and `message` properties.
    pub(crate) fn make_native_error(&mut self, name: &str, message: &str) -> Value {
        let mut err = crate::runtime::object::JsObject::ordinary();
        let name_sid = self.interner.intern(name);
        let msg_sid  = self.interner.intern(message);
        // Set prototype to the error type's prototype for instanceof checks
        let err_sentinel: i32 = match name {
            "TypeError" => -511, "RangeError" => -512, "ReferenceError" => -513,
            "SyntaxError" => -514, "EvalError" => -515, "URIError" => -516,
            "AggregateError" => -539, _ => -510,
        };
        err.prototype = self.func_prototypes.get(&err_sentinel).copied()
            .or(Some(self.object_prototype));
        let msg_key = self.interner.intern("message");
        err.set_property(msg_key, Value::string(msg_sid));
        let stack_key = self.interner.intern("stack");
        let stack_str = format!("{name}: {message}");
        let stack_sid = self.interner.intern(&stack_str);
        err.set_property(stack_key, Value::string(stack_sid));
        let _ = name_sid; // name now comes from prototype
        let oid = self.heap.allocate(err);
        Value::object_id(oid)
    }

    /// Step a built-in iterator (Array/Map/Set/Key) and produce a fresh
    /// `{ value, done }` result object. Returns `None`-ish (false done) only when
    /// the iterator is unknown — caller should handle that case.
    pub(crate) fn iterator_next_step(&mut self, iter_oid: ObjectId) -> Result<Value, VmError> {
        let iter_info: Option<(Option<ObjectId>, usize, bool)> = self.heap.get(iter_oid).and_then(|iter| {
            match &iter.kind {
                ObjectKind::ArrayIterator(arr_id, idx) => Some((Some(*arr_id), *idx, false)),
                ObjectKind::MapIterator(map_id, idx) => Some((Some(*map_id), *idx, false)),
                ObjectKind::SetIterator(set_id, idx) => Some((Some(*set_id), *idx, false)),
                ObjectKind::KeyIterator(_, idx) => Some((None, *idx, true)),
                _ => None,
            }
        });
        let info = match iter_info {
            Some(i) => i,
            None => {
                // Generators resume through their own machinery; keep the
                // silent done=true fallback for them. Anything else is an
                // incompatible receiver per spec (%MapIteratorPrototype%.next
                // .call({}) must throw).
                let is_gen = self.heap.get(iter_oid)
                    .is_some_and(|o| matches!(o.kind, ObjectKind::Generator { .. }));
                if is_gen {
                    return self.make_iter_result(Value::undefined(), true);
                }
                let err = self.make_native_error(
                    "TypeError",
                    "Iterator next called on incompatible receiver",
                );
                return Err(VmError::Throw(err));
            }
        };
        let (value, done) = if info.2 {
            // Key iterator
            let keys: Vec<_> = self.heap.get(iter_oid)
                .and_then(|o| if let ObjectKind::KeyIterator(ref k, _) = o.kind { Some(k.clone()) } else { None })
                .unwrap_or_default();
            let idx = info.1;
            if idx < keys.len() { (Value::string(keys[idx]), false) } else { (Value::undefined(), true) }
        } else {
            let src_oid = info.0.unwrap();
            let idx = info.1;
            let is_map = matches!(self.heap.get(iter_oid).map(|o| &o.kind), Some(ObjectKind::MapIterator(..)));
            let is_set = matches!(self.heap.get(iter_oid).map(|o| &o.kind), Some(ObjectKind::SetIterator(..)));
            if is_map {
                let entry = self.heap.get(src_oid).and_then(|o| {
                    if let ObjectKind::Map { ref entries } = o.kind { entries.get(idx).copied() } else { None }
                });
                if let Some((k, v)) = entry {
                    let pair = JsObject::array(vec![k, v]);
                    let pair_id = self.heap.allocate(pair);
                    (Value::object_id(pair_id), false)
                } else { (Value::undefined(), true) }
            } else if is_set {
                let elem = self.heap.get(src_oid).and_then(|o| {
                    if let ObjectKind::Set { ref entries } = o.kind { entries.get(idx).copied() } else { None }
                });
                if let Some(v) = elem { (v, false) } else { (Value::undefined(), true) }
            } else if let Some(ta_len) = self.typed_array_len(src_oid) {
                // Typed array iterator.
                let kind_key = self.interner.intern("__iter_kind__");
                let kind_str = self.heap.get(iter_oid)
                    .and_then(|o| o.get_property(kind_key))
                    .and_then(|v| v.as_string_id())
                    .map(|sid| self.interner.resolve(sid).to_owned());
                if idx >= ta_len {
                    (Value::undefined(), true)
                } else {
                    let elem = self.typed_array_get(src_oid, idx).unwrap_or(Value::undefined());
                    match kind_str.as_deref() {
                        Some("keys") => (Value::int(idx as i32), false),
                        Some("entries") => {
                            let pair = JsObject::array(vec![Value::int(idx as i32), elem]);
                            (Value::object_id(self.heap.allocate(pair)), false)
                        }
                        _ => (elem, false),
                    }
                }
            } else {
                // Array iterator. Honor the optional `__iter_kind__` tag for keys/entries.
                let elem = self.heap.get(src_oid).and_then(|o| {
                    if let ObjectKind::Array(ref e) = o.kind { e.get(idx).copied() } else { None }
                });
                let arr_len = self.heap.get(src_oid).map(|o| {
                    if let ObjectKind::Array(ref e) = o.kind { e.len() } else { 0 }
                }).unwrap_or(0);
                let kind_key = self.interner.intern("__iter_kind__");
                let kind_str = self.heap.get(iter_oid)
                    .and_then(|o| o.get_property(kind_key))
                    .and_then(|v| v.as_string_id())
                    .map(|sid| self.interner.resolve(sid).to_owned());
                if idx >= arr_len {
                    (Value::undefined(), true)
                } else {
                    match kind_str.as_deref() {
                        Some("keys") => (Value::int(idx as i32), false),
                        Some("entries") => {
                            let v = elem.unwrap_or(Value::undefined());
                            let pair = JsObject::array(vec![Value::int(idx as i32), v]);
                            (Value::object_id(self.heap.allocate(pair)), false)
                        }
                        _ => (elem.unwrap_or(Value::undefined()), false),
                    }
                }
            }
        };
        // Advance the iterator index
        if let Some(iter) = self.heap.get_mut(iter_oid) {
            let new_idx = info.1 + 1;
            match &mut iter.kind {
                ObjectKind::ArrayIterator(_, i)
                | ObjectKind::MapIterator(_, i)
                | ObjectKind::SetIterator(_, i)
                | ObjectKind::KeyIterator(_, i) => *i = new_idx,
                _ => {}
            }
        }
        self.make_iter_result(value, done)
    }

    /// Shared core of `<`, `<=`, `>`, `>=`. Coerces both sides to primitives
    /// (with throw-protection) and applies `str_cmp` for two strings or `num_cmp`
    /// otherwise.
    pub(crate) fn relational_compare(
        &mut self,
        av: Value,
        bv: Value,
        num_cmp: fn(f64, f64) -> bool,
        str_cmp: fn(&str, &str) -> bool,
        ord_cmp: fn(std::cmp::Ordering) -> bool,
    ) -> Result<bool, VmError> {
        let a = self.try_coerce_to_primitive_hint(av, "number")?;
        let b = self.try_coerce_to_primitive_hint(bv, "number")?;
        if self.is_string_like(a) && self.is_string_like(b) {
            let sa = self.flatten_cons_to_string(a);
            let sb = self.flatten_cons_to_string(b);
            return Ok(str_cmp(&sa, &sb));
        }
        // A Symbol operand has no relational ordering — ToNumeric throws.
        if a.is_symbol() || b.is_symbol() {
            let err = self.make_native_error("TypeError", "Cannot convert a Symbol value to a number");
            return Err(VmError::Throw(err));
        }
        // BigInt relational comparison (mathematical, mixes with Number/String).
        // An unparseable BigInt-vs-String or a NaN operand makes the result false.
        if self.is_bigint(a) || self.is_bigint(b) {
            let ord = if let (Some(x), Some(y)) = (self.as_bigint(a), self.as_bigint(b)) {
                Some(x.cmp(&y))
            } else if let Some(x) = self.as_bigint(a) {
                if self.is_string_like(b) {
                    let s = self.flatten_cons_to_string(b);
                    string_to_bigint(&s).map(|y| x.cmp(&y))
                } else {
                    bigint_cmp_f64(&x, self.to_f64(b))
                }
            } else {
                let y = self.as_bigint(b).unwrap();
                if self.is_string_like(a) {
                    let s = self.flatten_cons_to_string(a);
                    string_to_bigint(&s).map(|x| x.cmp(&y))
                } else {
                    bigint_cmp_f64(&y, self.to_f64(a)).map(|o| o.reverse())
                }
            };
            return Ok(ord.map(ord_cmp).unwrap_or(false));
        }
        Ok(num_cmp(self.to_f64(a), self.to_f64(b)))
    }

    /// Throw a JS value through the exception-handler machinery.
    /// If a handler is found the stack/frames are unwound and `Ok(())` is
    /// returned (caller must `continue` the main dispatch loop).
    /// If no handler exists the value is stringified and returned as
    /// `Err(VmError::RuntimeError)`.
    /// Allocate a new Promise with `Promise.prototype` as its [[Prototype]].
    pub(crate) fn allocate_promise(&mut self) -> ObjectId {
        let mut p = JsObject::promise();
        p.prototype = Some(self.promise_prototype);
        self.heap.allocate(p)
    }

    pub(crate) fn handle_throw(&mut self, val: Value) -> Result<(), VmError> {
        // Protected nested call (e.g. valueOf during a comparison opcode):
        // if the handler that would catch this throw lives in a frame strictly
        // below the protect depth, the throw escapes the nested call. Bubble
        // it up as a VmError so the calling opcode can re-throw at its own level.
        if self.protect_throw_depth > 0
            && let Some(handler) = self.exc_handlers.last()
            && handler.frame_idx + 1 < self.protect_throw_depth
        {
            return Err(VmError::Throw(val));
        }
        if let Some(handler) = self.exc_handlers.pop() {
            for frame in self.frames.iter().skip(handler.frame_idx + 1) {
                if let Some(gid) = frame.generator_id
                    && let Some(obj) = self.heap.get_mut(gid)
                    && let crate::runtime::object::ObjectKind::Generator { state, .. } = &mut obj.kind
                {
                    *state = crate::runtime::object::GeneratorState::Completed;
                }
            }
            while self.frames.len() > handler.frame_idx + 1 {
                self.frames.pop();
            }
            self.truncate_stack(handler.stack_depth);
            // Pop any with-scopes entered since the handler was pushed.
            self.with_stack.truncate(handler.with_depth);
            self.push(val);
            self.frames.last_mut().unwrap().ip = handler.catch_target as usize;
            Ok(())
        } else {
            // Bubble up the actual exception value. Outer code (e.g. the engine
            // entry point or the async-function wrapper) decides whether to
            // stringify it or use it as-is for promise rejection.
            Err(VmError::Throw(val))
        }
    }


    /// Object constructor statics (`Object.keys`, `Object.create`, …).
    /// Shared by the method-call path (`Object.keys(o)`) and the
    /// extracted-value sentinels (`var c = Object.create; c(proto)`),
    /// which minified bundles and core-js feature detection rely on.
    /// Returns Ok(None) for names that aren't Object statics.
    pub(crate) const OBJECT_STATIC_NAMES: &'static [&'static str] = &[
        "keys", "values", "entries", "create", "defineProperty",
        "defineProperties", "getOwnPropertyDescriptor", "getOwnPropertyNames",
        "getOwnPropertyDescriptors", "freeze", "seal", "isFrozen", "isSealed",
        "is", "getPrototypeOf", "setPrototypeOf", "preventExtensions",
        "isExtensible", "hasOwn", "fromEntries", "getOwnPropertySymbols",
    ];


    /// Symbol.prototype, lazily created (func_prototypes[-570]) —
    /// core-js's description polyfill probes `"description" in
    /// Symbol.prototype` and wraps the constructor if it can't.
    pub(crate) fn symbol_prototype_oid(&mut self) -> ObjectId {
        if let Some(&p) = self.func_prototypes.get(&-570) {
            return p;
        }
        let mut sp = JsObject::ordinary();
        sp.prototype = Some(self.object_prototype);
        let ctor_key = self.interner.intern("constructor");
        sp.set_property(ctor_key, Value::function(-570));
        let p = self.heap.allocate(sp);
        self.func_prototypes.insert(-570, p);
        p
    }

    /// BigInt.prototype, lazily created (func_prototypes[-638]). Holds the
    /// `toString` (-639) and `valueOf` (-640) sentinels and `constructor`.
    pub(crate) fn bigint_prototype_oid(&mut self) -> ObjectId {
        if let Some(&p) = self.func_prototypes.get(&-638) {
            return p;
        }
        let mut bp = JsObject::ordinary();
        bp.prototype = Some(self.object_prototype);
        let ctor_key = self.interner.intern("constructor");
        bp.set_property(ctor_key, Value::function(-638));
        let ts = self.interner.intern("toString");
        bp.define_property(ts, crate::runtime::object::Property::with_flags(
            Value::function(-639), crate::runtime::object::Property::WRITABLE | crate::runtime::object::Property::CONFIGURABLE));
        let vo = self.interner.intern("valueOf");
        bp.define_property(vo, crate::runtime::object::Property::with_flags(
            Value::function(-640), crate::runtime::object::Property::WRITABLE | crate::runtime::object::Property::CONFIGURABLE));
        let p = self.heap.allocate(bp);
        self.func_prototypes.insert(-638, p);
        p
    }


    /// Lazily create the shared builtin-iterator prototype: an ordinary
    /// object (chained to Object.prototype) with native `next` / `return`
    /// that step the receiver via iterator_next_step. Assigned as the
    /// `prototype` of Array/Map/Set/Key iterator objects at creation.
    pub(crate) fn iterator_prototype_oid(&mut self) -> ObjectId {
        if let Some(oid) = self.iterator_prototype {
            return oid;
        }
        let mut proto = JsObject::ordinary();
        proto.prototype = Some(self.object_prototype);
        let next_id = self.interner.intern("next");
        let next_fn: crate::runtime::object::NativeFn = std::sync::Arc::new(
            |vm: &mut Vm, this: Value, _args: &[Value]| -> Result<Value, Value> {
                let Some(oid) = this.as_object_id() else {
                    let err = vm.make_native_error("TypeError", "next called on non-iterator");
                    return Err(err);
                };
                match vm.iterator_next_step(oid) {
                    Ok(v) => Ok(v),
                    Err(VmError::Throw(v)) => Err(v),
                    Err(e) => {
                        let msg = format!("{e:?}");
                        Err(vm.make_native_error("Error", &msg))
                    }
                }
            },
        );
        let next_obj = JsObject {
            properties: Vec::new(),
            prototype: None,
            kind: ObjectKind::Function(crate::runtime::object::FunctionKind::Native { name: next_id, func: next_fn }),
            marked: false,
            extensible: true,
        };
        let next_val = Value::object_id(self.heap.allocate(next_obj));
        proto.set_property(next_id, next_val);
        // %IteratorPrototype%[Symbol.iterator]() returns `this` per spec —
        // makes builtin iterators themselves iterable (`new Map(m.entries())`,
        // spread over extracted iterators, ...).
        let sym_key = self.interner.intern(&format!("__sym_{}__", self.sym_iterator));
        let self_id = self.interner.intern("[Symbol.iterator]");
        let self_fn: crate::runtime::object::NativeFn = std::sync::Arc::new(
            |_vm: &mut Vm, this: Value, _args: &[Value]| -> Result<Value, Value> { Ok(this) },
        );
        let self_obj = JsObject {
            properties: Vec::new(),
            prototype: None,
            kind: ObjectKind::Function(crate::runtime::object::FunctionKind::Native { name: self_id, func: self_fn }),
            marked: false,
            extensible: true,
        };
        let self_val = Value::object_id(self.heap.allocate(self_obj));
        proto.set_property(sym_key, self_val);
        let oid = self.heap.allocate(proto);
        self.iterator_prototype = Some(oid);
        oid
    }

    /// Date.prototype[@@toPrimitive]: OrdinaryToPrimitive with an explicit
    /// hint, callable on ANY object receiver, with observable method Gets.
    pub(crate) fn init_date_to_primitive(&mut self) {
        let name_id = self.interner.intern("[Symbol.toPrimitive]");
        let func: crate::runtime::object::NativeFn = std::sync::Arc::new(
            |vm: &mut Vm, this: Value, args: &[Value]| -> Result<Value, Value> {
                let Some(oid) = this.as_object_id() else {
                    return Err(vm.make_native_error(
                        "TypeError",
                        "Date.prototype[Symbol.toPrimitive] called on a non-object",
                    ));
                };
                let hint_val = args.first().copied().unwrap_or(Value::undefined());
                let hint = if hint_val.is_string() || vm.is_cons_string(hint_val) {
                    vm.value_to_string(hint_val)
                } else {
                    String::new()
                };
                let order: [&str; 2] = match hint.as_str() {
                    "number" => ["valueOf", "toString"],
                    "string" | "default" => ["toString", "valueOf"],
                    _ => {
                        return Err(vm.make_native_error(
                            "TypeError",
                            "Invalid hint: expected \"default\", \"string\", or \"number\"",
                        ));
                    }
                };
                for m in order {
                    // Builtin prototype methods (Date valueOf/toString, …)
                    // materialize lazily — reify before the observable Get.
                    let m_id = vm.interner.intern(m);
                    vm.ensure_chain_method(oid, m_id);
                    let method = match vm.getter_aware_get(oid, m) {
                        Ok(v) => v.unwrap_or(Value::undefined()),
                        Err(VmError::Throw(t)) => return Err(t),
                        Err(e) => return Err(vm.make_native_error("Error", &format!("{e:?}"))),
                    };
                    if vm.value_callable(method) {
                        let prev = vm.protect_throw_depth;
                        vm.protect_throw_depth = vm.frames.len() + 1;
                        let r = vm.call_function_this(method, this, &[]);
                        vm.protect_throw_depth = prev;
                        match r {
                            Ok(v) if !v.is_object() || v.is_symbol() || vm.is_cons_string(v) || vm.is_flat_string(v) => return Ok(v),
                            Ok(_) => {}
                            Err(VmError::Throw(t)) => return Err(t),
                            Err(e) => return Err(vm.make_native_error("Error", &format!("{e:?}"))),
                        }
                    }
                }
                Err(vm.make_native_error("TypeError", "Cannot convert object to primitive value"))
            },
        );
        let mut fn_obj = JsObject {
            properties: Vec::new(),
            prototype: Some(self.function_prototype),
            kind: ObjectKind::Function(crate::runtime::object::FunctionKind::Native { name: name_id, func }),
            marked: false,
            extensible: true,
        };
        let name_key = self.interner.intern("name");
        let len_key = self.interner.intern("length");
        fn_obj.define_property(name_key, Property::with_flags(Value::string(name_id), Property::CONFIGURABLE));
        fn_obj.define_property(len_key, Property::with_flags(Value::int(1), Property::CONFIGURABLE));
        let v = Value::object_id(self.heap.allocate(fn_obj));
        // { writable: false, enumerable: false, configurable: true } per spec.
        let sym_key = self.interner.intern("__sym_2__");
        let dp = self.date_prototype;
        if let Some(p) = self.heap.get_mut(dp) {
            p.define_property(sym_key, Property::with_flags(v, Property::CONFIGURABLE));
        }
    }

    /// JSON.parse / JSON.stringify as real function properties (descriptor
    /// tests read them via getOwnPropertyDescriptor).
    pub(crate) fn init_json_methods(&mut self) {
        let json_name = self.interner.intern("JSON");
        let Some(json_oid) = self.globals.get(&json_name).and_then(|v| v.as_object_id()) else { return };
        for (mname, mlen) in [("parse", 2i32), ("stringify", 3)] {
            let name_id = self.interner.intern(mname);
            let m = mname.to_string();
            let func: crate::runtime::object::NativeFn = std::sync::Arc::new(
                move |vm: &mut Vm, _this: Value, args: &[Value]| -> Result<Value, Value> {
                    let mid = vm.interner.intern(&m);
                    match vm.exec_json_method(mid, args) {
                        Ok(v) => Ok(v),
                        Err(VmError::Throw(t)) => Err(t),
                        Err(e) => Err(vm.make_native_error("Error", &format!("{e:?}"))),
                    }
                },
            );
            let mut fn_obj = JsObject {
                properties: Vec::new(),
                prototype: Some(self.function_prototype),
                kind: ObjectKind::Function(crate::runtime::object::FunctionKind::Native { name: name_id, func }),
                marked: false,
                extensible: true,
            };
            let name_key = self.interner.intern("name");
            let len_key = self.interner.intern("length");
            fn_obj.define_property(name_key, Property::with_flags(Value::string(name_id), Property::CONFIGURABLE));
            fn_obj.define_property(len_key, Property::with_flags(Value::int(mlen), Property::CONFIGURABLE));
            let v = Value::object_id(self.heap.allocate(fn_obj));
            if let Some(o) = self.heap.get_mut(json_oid) {
                o.define_property(name_id, Property::with_flags(v, Property::WRITABLE | Property::CONFIGURABLE));
            }
        }
    }

    /// Annex B.2.2 legacy accessors on Object.prototype:
    /// __defineGetter__/__defineSetter__/__lookupGetter__/__lookupSetter__.
    pub(crate) fn init_legacy_accessors(&mut self) {
        for which in ["__defineGetter__", "__defineSetter__", "__lookupGetter__", "__lookupSetter__"] {
            let name_id = self.interner.intern(which);
            let which_owned = which.to_string();
            let func: crate::runtime::object::NativeFn = std::sync::Arc::new(
                move |vm: &mut Vm, this: Value, args: &[Value]| -> Result<Value, Value> {
                    // ToObject(this): null/undefined throw.
                    if this.is_nullish() {
                        return Err(vm.make_native_error(
                            "TypeError",
                            &format!("Object.prototype.{which_owned} called on null or undefined"),
                        ));
                    }
                    let is_define = which_owned.starts_with("__define");
                    let is_getter = which_owned.contains("Getter");
                    let key_val = args.first().copied().unwrap_or(Value::undefined());
                    let key_str = if key_val.is_symbol() {
                        format!("__sym_{}__", key_val.as_symbol_id().unwrap())
                    } else {
                        vm.value_to_string(key_val)
                    };
                    let half = if is_getter { format!("__get_{key_str}__") } else { format!("__set_{key_str}__") };
                    let half_id = vm.interner.intern(&half);
                    if is_define {
                        let f = args.get(1).copied().unwrap_or(Value::undefined());
                        if !vm.value_callable(f) {
                            return Err(vm.make_native_error(
                                "TypeError",
                                if is_getter { "Getter must be a function" } else { "Setter must be a function" },
                            ));
                        }
                        let Some(oid) = this.as_object_id() else {
                            // Primitive receivers: the wrapper is transient; the
                            // define is unobservable, return undefined.
                            return Ok(Value::undefined());
                        };
                        // Existing non-configurable property → TypeError.
                        let key_id = vm.interner.intern(&key_str);
                        let get_id = vm.interner.intern(&format!("__get_{key_str}__"));
                        let set_id = vm.interner.intern(&format!("__set_{key_str}__"));
                        let non_config = vm.heap.get(oid).is_some_and(|o| {
                            [key_id, get_id, set_id].iter().any(|k| {
                                o.get_property_descriptor(*k).is_some_and(|p| !p.is_configurable())
                            })
                        });
                        if non_config {
                            return Err(vm.make_native_error(
                                "TypeError",
                                &format!("Cannot redefine property: {key_str}"),
                            ));
                        }
                        let extensible = vm.heap.get(oid).map(|o| o.extensible).unwrap_or(true);
                        let exists = vm.heap.get(oid).is_some_and(|o| {
                            [key_id, get_id, set_id].iter().any(|k| o.has_own_property(*k))
                        });
                        if !exists && !extensible {
                            return Err(vm.make_native_error(
                                "TypeError",
                                &format!("Cannot define property {key_str}, object is not extensible"),
                            ));
                        }
                        if let Some(obj) = vm.heap.get_mut(oid) {
                            // Converting a data property: drop the data slot.
                            obj.delete_property(key_id);
                            obj.define_property(half_id, Property::with_flags(
                                f,
                                Property::ENUMERABLE | Property::CONFIGURABLE,
                            ));
                        }
                        Ok(Value::undefined())
                    } else {
                        // Private class accessors share the __get_#m__ storage
                        // but are NOT observable properties.
                        if key_str.starts_with('#') {
                            return Ok(Value::undefined());
                        }
                        // Lookup: the chain walk stops at the FIRST object that
                        // owns the property at all (either accessor half or a
                        // data slot) and reports that level's half — an
                        // accessor with only the other half yields undefined.
                        let other_half = if is_getter { format!("__set_{key_str}__") } else { format!("__get_{key_str}__") };
                        let other_half_id = vm.interner.intern(&other_half);
                        let key_id = vm.interner.intern(&key_str);
                        let mut cur = this.as_object_id();
                        while let Some(oid) = cur {
                            if let Some(v) = vm.heap.get(oid).and_then(|o| o.get_property(half_id)) {
                                return Ok(v);
                            }
                            let owns = vm.heap.get(oid).is_some_and(|o| {
                                o.has_own_property(other_half_id) || o.has_own_property(key_id)
                            });
                            if owns {
                                return Ok(Value::undefined());
                            }
                            cur = vm.heap.get(oid).and_then(|o| o.prototype);
                        }
                        Ok(Value::undefined())
                    }
                },
            );
            let mut fn_obj = JsObject {
                properties: Vec::new(),
                prototype: Some(self.function_prototype),
                kind: ObjectKind::Function(crate::runtime::object::FunctionKind::Native { name: name_id, func }),
                marked: false,
                extensible: true,
            };
            let name_key = self.interner.intern("name");
            let len_key = self.interner.intern("length");
            let fn_len = if which.starts_with("__define") { 2 } else { 1 };
            fn_obj.define_property(name_key, Property::with_flags(Value::string(name_id), Property::CONFIGURABLE));
            fn_obj.define_property(len_key, Property::with_flags(Value::int(fn_len), Property::CONFIGURABLE));
            let v = Value::object_id(self.heap.allocate(fn_obj));
            let op = self.object_prototype;
            if let Some(p) = self.heap.get_mut(op) {
                p.define_property(name_id, Property::with_flags(v, Property::WRITABLE | Property::CONFIGURABLE));
            }
        }
    }

    /// %ThrowTypeError%: the singleton poison-pill accessor for strict-mode
    /// `arguments.callee` (get === set, frozen, non-extensible, name "" /
    /// length 0).
    pub(crate) fn throw_type_error_fn(&mut self) -> Value {
        if let Some(v) = self.throw_type_error {
            return v;
        }
        let name_id = self.interner.intern("");
        let func: crate::runtime::object::NativeFn = std::sync::Arc::new(
            |vm: &mut Vm, _this: Value, _args: &[Value]| -> Result<Value, Value> {
                Err(vm.make_native_error(
                    "TypeError",
                    "'caller', 'callee', and 'arguments' properties may not be accessed on strict mode functions or the arguments objects for calls to them",
                ))
            },
        );
        let mut fn_obj = JsObject {
            properties: Vec::new(),
            prototype: Some(self.function_prototype),
            kind: ObjectKind::Function(crate::runtime::object::FunctionKind::Native { name: name_id, func }),
            marked: false,
            extensible: false,
        };
        let name_key = self.interner.intern("name");
        let len_key = self.interner.intern("length");
        fn_obj.define_property(name_key, Property::with_flags(Value::string(name_id), 0));
        fn_obj.define_property(len_key, Property::with_flags(Value::int(0), 0));
        let v = Value::object_id(self.heap.allocate(fn_obj));
        self.throw_type_error = Some(v);
        v
    }

    /// %GeneratorFunction.prototype%: the [[Prototype]] of every generator
    /// function, carrying @@toStringTag "GeneratorFunction" and a
    /// constructor that dynamically compiles `function*` source.
    pub(crate) fn generator_function_proto_oid(&mut self) -> ObjectId {
        self.dynamic_fn_proto_oid("GeneratorFunction", "function*")
    }

    pub(crate) fn async_function_proto_oid(&mut self) -> ObjectId {
        self.dynamic_fn_proto_oid("AsyncFunction", "async function")
    }

    fn dynamic_fn_proto_oid(&mut self, tag: &'static str, keyword: &'static str) -> ObjectId {
        let cache = if tag == "GeneratorFunction" { self.generator_function_proto } else { self.async_function_proto };
        if let Some(oid) = cache {
            return oid;
        }
        let mut proto = JsObject::ordinary();
        proto.prototype = Some(self.function_prototype);
        let tag_key = self.interner.intern(&format!("__sym_{}__", self.sym_to_string_tag));
        let tag_val = self.interner.intern(tag);
        proto.define_property(tag_key, Property::with_flags(Value::string(tag_val), Property::CONFIGURABLE));
        // The intrinsic constructor: callable and constructable.
        let ctor_name = self.interner.intern(tag);
        let func: crate::runtime::object::NativeFn = std::sync::Arc::new(
            move |vm: &mut Vm, _this: Value, args: &[Value]| -> Result<Value, Value> {
                match vm.construct_function_kind(args, keyword) {
                    Ok(v) => Ok(v),
                    Err(VmError::Throw(t)) => Err(t),
                    Err(e) => Err(vm.make_native_error("Error", &format!("{e:?}"))),
                }
            },
        );
        let mut ctor_obj = JsObject {
            properties: Vec::new(),
            prototype: Some(self.function_prototype),
            kind: ObjectKind::Function(crate::runtime::object::FunctionKind::Native { name: ctor_name, func }),
            marked: false,
            extensible: true,
        };
        let name_key = self.interner.intern("name");
        let len_key = self.interner.intern("length");
        ctor_obj.define_property(name_key, Property::with_flags(Value::string(ctor_name), Property::CONFIGURABLE));
        ctor_obj.define_property(len_key, Property::with_flags(Value::int(1), Property::CONFIGURABLE));
        let proto_oid = self.heap.allocate(proto);
        // ctor.prototype = %GeneratorFunctionPrototype% (non-writable,
        // non-enumerable, non-configurable per spec).
        let proto_key = self.interner.intern("prototype");
        ctor_obj.define_property(proto_key, Property::with_flags(Value::object_id(proto_oid), 0));
        let ctor_val = Value::object_id(self.heap.allocate(ctor_obj));
        let ctor_key = self.interner.intern("constructor");
        if let Some(p) = self.heap.get_mut(proto_oid) {
            p.define_property(ctor_key, Property::with_flags(ctor_val, Property::CONFIGURABLE));
        }
        if tag == "GeneratorFunction" {
            self.generator_function_proto = Some(proto_oid);
        } else {
            self.async_function_proto = Some(proto_oid);
        }
        proto_oid
    }

    /// Per-kind iterator prototype (%ArrayIteratorPrototype%, …): an object
    /// with [[Prototype]] = %IteratorPrototype%, its own spec-shaped `next`,
    /// and @@toStringTag `"<tag> Iterator"`. Cached per tag.
    pub(crate) fn kind_iterator_prototype(&mut self, tag: &str) -> ObjectId {
        let cache_key = self.interner.intern(&format!("__iterproto_{tag}__"));
        let base = self.iterator_prototype_oid();
        if let Some(v) = self.heap.get(base).and_then(|o| o.get_property(cache_key))
            && let Some(oid) = v.as_object_id()
        {
            return oid;
        }
        let mut proto = JsObject::ordinary();
        proto.prototype = Some(base);
        // Own `next` — same machinery, distinct function identity per kind.
        let next_id = self.interner.intern("next");
        let next_fn: crate::runtime::object::NativeFn = std::sync::Arc::new(
            |vm: &mut Vm, this: Value, _args: &[Value]| -> Result<Value, Value> {
                let Some(oid) = this.as_object_id() else {
                    let err = vm.make_native_error("TypeError", "next called on non-iterator");
                    return Err(err);
                };
                match vm.iterator_next_step(oid) {
                    Ok(v) => Ok(v),
                    Err(VmError::Throw(v)) => Err(v),
                    Err(e) => {
                        let msg = format!("{e:?}");
                        Err(vm.make_native_error("Error", &msg))
                    }
                }
            },
        );
        let mut next_obj = JsObject {
            properties: Vec::new(),
            prototype: Some(self.function_prototype),
            kind: ObjectKind::Function(crate::runtime::object::FunctionKind::Native { name: next_id, func: next_fn }),
            marked: false,
            extensible: true,
        };
        let name_key = self.interner.intern("name");
        let len_key = self.interner.intern("length");
        next_obj.define_property(name_key, Property::with_flags(Value::string(next_id), Property::CONFIGURABLE));
        next_obj.define_property(len_key, Property::with_flags(Value::int(0), Property::CONFIGURABLE));
        let next_val = Value::object_id(self.heap.allocate(next_obj));
        proto.define_property(next_id, Property::with_flags(next_val, Property::WRITABLE | Property::CONFIGURABLE));
        // @@toStringTag: { value: "<tag> Iterator", w: false, e: false, c: true }
        let tag_key = self.interner.intern(&format!("__sym_{}__", self.sym_to_string_tag));
        let tag_val = self.interner.intern(&format!("{tag} Iterator"));
        proto.define_property(tag_key, Property::with_flags(Value::string(tag_val), Property::CONFIGURABLE));
        let oid = self.heap.allocate(proto);
        // Cache on %IteratorPrototype% under a hidden non-enumerable key.
        if let Some(b) = self.heap.get_mut(base) {
            b.define_property(cache_key, Property::with_flags(Value::object_id(oid), 0));
        }
        oid
    }


    // ---- Function own-property helpers ------------------------------------



    // ---- BigInt helpers ----------------------------------------------------











    // ---- ConsString helpers -----------------------------------------------










    // ---- Abstract equality (simplified) -----------------------------------






    /// Dump diagnostics when the fuel limit is hit (ZINC_FUEL_TRACE=1): the
    /// current call-stack backtrace plus the hottest (chunk, line) sites from
    /// the sampling profiler — the runaway loop is whatever sits at the top.
    fn dump_fuel_trace(&self) {
        eprintln!("=== fuel exhausted ({} steps); call stack (innermost last) ===", self.steps);
        for (depth, f) in self.frames.iter().enumerate() {
            let chunk = &self.chunks[f.chunk_idx];
            let name = self.interner.resolve(chunk.name);
            let line = chunk.get_line(f.ip as u32);
            let name = if name.is_empty() { "<anonymous>" } else { name };
            eprintln!("  #{depth:<2} chunk '{name}' (idx {}) line {line} ip {}", f.chunk_idx, f.ip);
        }
        let mut hot: Vec<_> = self.fuel_samples.iter().collect();
        hot.sort_by_key(|e| std::cmp::Reverse(*e.1));
        let total: u64 = self.fuel_samples.values().sum();
        eprintln!("=== hottest sites (chunk idx : source line — sample share of {total} checkpoints) ===");
        for entry in hot.iter().take(20) {
            let (chunk_idx, line) = *entry.0;
            let count = *entry.1;
            let name = self.interner.resolve(self.chunks[chunk_idx as usize].name);
            let name = if name.is_empty() { "<anonymous>" } else { name };
            let pct = 100.0 * count as f64 / total.max(1) as f64;
            eprintln!("  {count:>8} ({pct:4.1}%)  chunk {chunk_idx} '{name}' line {line}");
        }
        let k = self.string_recv_kinds;
        eprintln!(
            "=== string-method receiver kinds: interned-ASCII={} interned-nonASCII={} inline={} cons/flat={} ===",
            k[0], k[1], k[2], k[3]
        );
        let mut calls: Vec<_> = self.fuel_call_counts.iter().collect();
        calls.sort_by_key(|e| std::cmp::Reverse(*e.1));
        eprintln!("=== most-entered chunks (sampled every 1024 instr; relative magnitude) ===");
        for entry in calls.iter().take(12) {
            let chunk_idx = *entry.0;
            let count = *entry.1;
            let name = self.interner.resolve(self.chunks[chunk_idx as usize].name);
            let name = if name.is_empty() { "<anonymous>" } else { name };
            eprintln!("  {count:>8}  chunk {chunk_idx} '{name}'");
        }
    }

    /// Write `val` to function-value property `name_id`, honoring read-only
    /// builtins and strict/arrow restricted properties. Shared by SetProperty
    /// (dot) and SetElement (computed) so both behave identically. Returns
    /// `Some(msg)` if the caller should throw a TypeError, else `None`
    /// (stored, or a silent non-strict no-op).
    fn write_fn_property(
        &mut self,
        sentinel: i32,
        name_id: StringId,
        val: Value,
        in_strict: bool,
    ) -> Option<String> {
        let name_str = self.interner.resolve(name_id).to_owned();
        let is_readonly = matches!((sentinel, name_str.as_str()),
            (-505, "NaN") | (-505, "POSITIVE_INFINITY") | (-505, "NEGATIVE_INFINITY")
            | (-505, "MAX_VALUE") | (-505, "MIN_VALUE")
            | (-505, "MAX_SAFE_INTEGER") | (-505, "MIN_SAFE_INTEGER")
            | (-505, "EPSILON")
            | (-570, "iterator") | (-570, "hasInstance") | (-570, "toPrimitive")
            | (-570, "toStringTag") | (-570, "species") | (-570, "unscopables")
            | (-570, "asyncIterator") | (-570, "matchAll")
            | (-507, "prototype") | (-508, "prototype") | (-551, "prototype")
        )
        // Built-in constructor .prototype is non-writable across the board
        // (Promise, Date, the error constructors, collections, …).
        || (name_str == "prototype" && sentinel < 0 && self.func_prototypes.contains_key(&sentinel))
        // Typed-array constructor BYTES_PER_ELEMENT is non-writable.
        || (name_str == "BYTES_PER_ELEMENT" && crate::vm::typedarray::kind_for_sentinel(sentinel).is_some())
        // Every function's own `length` and `name` are non-writable (writable:
        // false, configurable: true) per spec — assignment no-ops/throws; only
        // defineProperty (a different path) can change them.
        || name_str == "length" || name_str == "name";
        if is_readonly {
            if in_strict {
                return Some(format!("Cannot assign to read only property '{name_str}'"));
            }
            return None; // non-strict: silent no-op
        }
        if matches!(name_str.as_str(), "caller" | "arguments") && sentinel >= 0 {
            let chunk_idx = (sentinel & 0xFFFF) as usize;
            let is_restricted = chunk_idx < self.chunks.len()
                && (self.chunks[chunk_idx].flags.contains(ChunkFlags::ARROW)
                    || self.chunks[chunk_idx].flags.contains(ChunkFlags::STRICT));
            if is_restricted {
                return Some(format!("'{name_str}' may not be set on strict mode functions"));
            }
        }
        self.fn_property_overrides.insert((sentinel, name_id), Some(val));
        // Keep func_prototypes in sync so `obj instanceof F` reads the
        // user-set prototype, not the auto-generated one.
        if name_str == "prototype"
            && let Some(proto_oid) = val.as_object_id()
        {
            self.func_prototypes.insert(sentinel, proto_oid);
        }
        None
    }

    // ---- Main execution loop ----------------------------------------------

    pub fn run(&mut self) -> Result<Value, VmError> {
        #[cfg(any(all(target_arch = "aarch64", target_os = "macos"), target_arch = "x86_64"))]
        self.try_partial_jit();

        let result = match self.run_until(0) {
            Ok(v) => v,
            Err(VmError::Throw(val)) => {
                // Top-level uncaught throw: stringify the exception value into
                // a RuntimeError. Inner code that needs the original value
                // (e.g. async function wrapper) handles Throw before reaching
                // here.
                let msg = self.value_to_string(val);
                return Err(VmError::RuntimeError(msg));
            }
            Err(e) => return Err(e),
        };
        // Flatten any ConsString result to a TAG_STRING so callers get a normal string value
        if self.is_cons_string(result) {
            let id = self.flatten_to_string_id(result);
            Ok(Value::string(id))
        } else {
            Ok(result)
        }
    }

    /// Append a freshly-compiled chunk tree to this VM and execute the new
    /// top chunk as a fresh script-level frame. Globals, the heap, and any
    /// previously-registered host functions are preserved across calls — this
    /// is how the embedder runs multiple `<script>` tags on a single
    /// long-lived `Engine`.
    pub fn load_and_run(&mut self, chunk: Chunk) -> Result<Value, VmError> {
        // Reset the fuel counter per top-level script run. `max_steps` is an
        // infinite-loop guard for a SINGLE script execution, not a budget for
        // the whole page — without this reset `steps` accumulated across every
        // `<script>` on the page, so a heavy page (e.g. DuckDuckGo's SERP, with
        // many large bundles) blew the total and every script after the
        // tipping point died with "execution limit exceeded", leaving globals
        // like `DDG.Pages.SERP` undefined.
        self.steps = 0;
        if !self.fuel_samples.is_empty() {
            self.fuel_samples.clear();
            self.string_recv_kinds = [0; 4];
            self.fuel_call_counts.clear();
        }
        // Fresh completion value per top-level run; Halt returns this.
        self.script_completion = Value::undefined();
        // Flatten the new chunk tree onto the existing chunks vec; the new
        // top chunk lands at `top_idx`, and its sub-chunks follow.
        let top_idx = self.chunks.len();
        // Debug aid: ZINC_DISASM_CHUNK=<name|index|*> dumps matching chunks
        // BEFORE flattening (the disassembler needs child_chunks intact to
        // size inline Closure descriptors). Indices mirror flatten order.
        if let Ok(want) = std::env::var("ZINC_DISASM_CHUNK") {
            fn walk(c: &Chunk, idx: &mut usize, want: &str, vm_interner: &Interner) {
                let cname = vm_interner.resolve(c.name);
                if want == "*" {
                    eprintln!(
                        "[chunk {} name='{}' locals={} params={} upvalues={} code={}B]",
                        idx, cname, c.local_count, c.param_count, c.upvalue_count, c.code.len()
                    );
                } else if want == idx.to_string() || cname == want {
                    eprintln!("==== disasm {want} (chunk {idx}) ====");
                    eprintln!("{}", crate::compiler::disassemble::disassemble(c, vm_interner));
                }
                *idx += 1;
                for child in &c.child_chunks {
                    walk(child, idx, want, vm_interner);
                }
            }
            let mut idx = top_idx;
            walk(&chunk, &mut idx, &want, &self.interner);
        }
        Self::flatten_chunk(chunk, &mut self.chunks);
        // Push a fresh top-level frame for the new script. Its base is the
        // current stack length so it doesn't clobber any persisted state.
        let base = self.stack.len();
        let stop_depth = self.frames.len();
        // Slot -1 of the new frame holds the "callee" placeholder; push one.
        self.stack.push(Value::undefined());
        let global_this = Value::object_id(self.global_this_oid);
        self.frames.push(CallFrame {
            chunk_idx: top_idx,
            ip: 0,
            base: base + 1,
            upvalues: Vec::new(),
            this_value: global_this,
            is_constructor: false,
            pending_super_call: false,
            generator_id: None,
            argc: 0,
            saved_args: Vec::new(),
            arguments_oid: None,
            is_derived_ctor: false,
            super_called: false,
            new_target: Value::undefined(),
            await_super_result: false,
            with_base: self.with_stack.len(),
        });
        let result = match self.run_until(stop_depth) {
            Ok(v) => v,
            Err(VmError::Throw(val)) => {
                let msg = self.value_to_string(val);
                return Err(VmError::RuntimeError(msg));
            }
            Err(e) => return Err(e),
        };
        if self.is_cons_string(result) {
            let id = self.flatten_to_string_id(result);
            Ok(Value::string(id))
        } else {
            Ok(result)
        }
    }

    /// Attempt to JIT-compile and run the loop portion of chunk 0.
    /// If successful, updates globals and advances the initial frame's IP
    /// so the interpreter resumes after the JIT-ed bytecode.
    #[cfg(any(all(target_arch = "aarch64", target_os = "macos"), target_arch = "x86_64"))]
    fn try_partial_jit(&mut self) {
        use crate::compiler::opcode::OpCode;
        let chunk0 = &self.chunks[0];
        if !chunk0.code.contains(&(OpCode::Loop as u8)) { return; }

        let result = crate::jit::compiler::jit_compile_partial(chunk0, &self.globals_vec);
        let (jit_fn, stop_ip, globals_order) = match result {
            Some(r) => r,
            None => return,
        };

        // Pre-unbox each global to i64
        let mut jit_globals: Vec<i64> = globals_order.iter().map(|&sid| {
            let val = if (sid as usize) < self.globals_vec.len() {
                self.globals_vec[sid as usize]
            } else {
                Value::null()
            };
            jit_unbox(val)
        }).collect();

        // Run the JIT loop
        jit_fn.call_globals(jit_globals.as_mut_ptr());

        // Write results back into globals_vec and the HashMap
        for (i, &sid) in globals_order.iter().enumerate() {
            let val = jit_rebox(jit_globals[i]);
            let idx = sid as usize;
            if idx >= self.globals_vec.len() {
                self.globals_vec.resize(idx + 1, Value::null());
            }
            self.globals_vec[idx] = val;
            self.globals.insert(crate::util::interner::StringId(sid), val);
        }
        self.global_version += 1;

        // Skip the JIT-ed bytes in the interpreter
        self.frames[0].ip = stop_ip;
    }

}

mod convert;
mod dispatch;
mod heap;
mod init;
mod misc;
mod object_static;
mod scope;

pub(crate) use misc::*;

#[cfg(test)]
mod tests;
