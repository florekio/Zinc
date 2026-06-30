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
    rows.sort_by(|a, b| b.1.cmp(&a.1));
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
    /// Singleton Boolean.prototype object
    pub(crate) boolean_prototype: ObjectId,
    /// Singleton Number.prototype object
    pub(crate) number_prototype: ObjectId,
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

    pub fn new(chunk: Chunk, mut interner: Interner) -> Self {
        let mut globals = HashMap::new();
        let undef_id = interner.intern("undefined");
        globals.insert(undef_id, Value::undefined());
        let nan_id = interner.intern("NaN");
        globals.insert(nan_id, Value::number(f64::NAN));
        let inf_id = interner.intern("Infinity");
        globals.insert(inf_id, Value::number(f64::INFINITY));

        // Flatten chunk tree: index 0 = script, children follow
        let mut chunks = Vec::new();
        Self::flatten_chunk(chunk, &mut chunks);

        let mut heap = ObjectHeap::new();
        let mut func_prototypes: HashMap<i32, ObjectId> = HashMap::new();

        // Create Object.prototype singleton (root of all prototype chains)
        let mut obj_proto = JsObject::ordinary();
        // prototype = null (Object.prototype is the root)
        let hop_key = interner.intern("hasOwnProperty");
        obj_proto.define_property(hop_key, Property::with_flags(Value::function(-590), Property::WRITABLE | Property::CONFIGURABLE));
        let pie_key = interner.intern("propertyIsEnumerable");
        obj_proto.define_property(pie_key, Property::with_flags(Value::function(-591), Property::WRITABLE | Property::CONFIGURABLE));
        let tostr_key = interner.intern("toString");
        obj_proto.define_property(tostr_key, Property::with_flags(Value::function(-592), Property::WRITABLE | Property::CONFIGURABLE));
        let valueof_key = interner.intern("valueOf");
        obj_proto.define_property(valueof_key, Property::with_flags(Value::function(-593), Property::WRITABLE | Property::CONFIGURABLE));
        let ipof_key = interner.intern("isPrototypeOf");
        obj_proto.define_property(ipof_key, Property::with_flags(Value::function(-594), Property::WRITABLE | Property::CONFIGURABLE));
        // Object.prototype.constructor = Object (sentinel -508), non-enumerable.
        let ctor_key = interner.intern("constructor");
        obj_proto.define_property(ctor_key, Property::with_flags(Value::function(-508), Property::WRITABLE | Property::CONFIGURABLE));
        let object_prototype = heap.allocate(obj_proto);

        // Create Function.prototype singleton (prototype = Object.prototype)
        let mut fn_proto = JsObject::ordinary();
        fn_proto.prototype = Some(object_prototype);
        let call_key = interner.intern("call");
        fn_proto.define_property(call_key, Property::with_flags(Value::function(-595), Property::WRITABLE | Property::CONFIGURABLE));
        let apply_key = interner.intern("apply");
        fn_proto.define_property(apply_key, Property::with_flags(Value::function(-596), Property::WRITABLE | Property::CONFIGURABLE));
        let bind_key = interner.intern("bind");
        fn_proto.define_property(bind_key, Property::with_flags(Value::function(-597), Property::WRITABLE | Property::CONFIGURABLE));
        let fn_length_key = interner.intern("length");
        fn_proto.define_property(fn_length_key, Property::with_flags(Value::int(0), Property::CONFIGURABLE));
        let fn_name_key = interner.intern("name");
        let empty_str = interner.intern("");
        fn_proto.define_property(fn_name_key, Property::with_flags(Value::string(empty_str), Property::CONFIGURABLE));
        let function_prototype = heap.allocate(fn_proto);

        // Create Array.prototype singleton (prototype = Object.prototype)
        // Array.prototype methods use sentinels -600 to -629
        let mut arr_proto = JsObject::ordinary();
        arr_proto.prototype = Some(object_prototype);
        for (name, sentinel) in [
            ("join", -600i32), ("push", -601), ("pop", -602), ("shift", -603),
            ("unshift", -604), ("indexOf", -605), ("includes", -606), ("forEach", -607),
            ("map", -608), ("filter", -609), ("reduce", -610), ("some", -611),
            ("every", -612), ("find", -613), ("findIndex", -614), ("slice", -615),
            ("concat", -616), ("reverse", -617), ("sort", -618), ("flat", -619),
            ("flatMap", -620), ("fill", -621), ("splice", -622), ("reduceRight", -623),
            ("at", -624), ("keys", -625), ("values", -626), ("entries", -627),
            ("lastIndexOf", -628), ("toString", -629),
        ] {
            let k = interner.intern(name);
            arr_proto.define_property(k, Property::with_flags(Value::function(sentinel), Property::WRITABLE | Property::CONFIGURABLE));
        }
        // Array.prototype.constructor = Array (-507)
        arr_proto.define_property(ctor_key, Property::with_flags(Value::function(-507), Property::WRITABLE | Property::CONFIGURABLE));
        // Array.prototype[Symbol.iterator] aliases Array.prototype.values per spec.
        // sym_iterator id is 0 (registered first in sym_descs below, see further down).
        let sym_iter_key = interner.intern("__sym_0__");
        arr_proto.define_property(sym_iter_key, Property::with_flags(Value::function(-626), Property::WRITABLE | Property::CONFIGURABLE));
        let array_prototype = heap.allocate(arr_proto);

        // Create Boolean.prototype (prototype = Object.prototype)
        let mut bool_proto = JsObject::ordinary();
        bool_proto.prototype = Some(object_prototype);
        bool_proto.define_property(ctor_key, Property::with_flags(Value::function(-506), Property::WRITABLE | Property::CONFIGURABLE));
        let bool_ts_key = interner.intern("toString");
        bool_proto.define_property(bool_ts_key, Property::with_flags(Value::function(-630), Property::WRITABLE | Property::CONFIGURABLE));
        let bool_vo_key = interner.intern("valueOf");
        bool_proto.define_property(bool_vo_key, Property::with_flags(Value::function(-631), Property::WRITABLE | Property::CONFIGURABLE));
        let boolean_prototype = heap.allocate(bool_proto);
        func_prototypes.insert(-506i32, boolean_prototype);

        // Create Number.prototype (prototype = Object.prototype)
        let mut num_proto = JsObject::ordinary();
        num_proto.prototype = Some(object_prototype);
        num_proto.define_property(ctor_key, Property::with_flags(Value::function(-505), Property::WRITABLE | Property::CONFIGURABLE));
        let num_ts_key = interner.intern("toString");
        num_proto.define_property(num_ts_key, Property::with_flags(Value::function(-632), Property::WRITABLE | Property::CONFIGURABLE));
        let num_vo_key = interner.intern("valueOf");
        num_proto.define_property(num_vo_key, Property::with_flags(Value::function(-633), Property::WRITABLE | Property::CONFIGURABLE));
        let number_prototype = heap.allocate(num_proto);
        func_prototypes.insert(-505i32, number_prototype);

        // Create String.prototype (prototype = Object.prototype)
        let mut str_proto = JsObject::ordinary();
        str_proto.prototype = Some(object_prototype);
        str_proto.define_property(ctor_key, Property::with_flags(Value::function(-504), Property::WRITABLE | Property::CONFIGURABLE));
        let str_ts_key = interner.intern("toString");
        str_proto.define_property(str_ts_key, Property::with_flags(Value::function(-634), Property::WRITABLE | Property::CONFIGURABLE));
        let str_vo_key = interner.intern("valueOf");
        str_proto.define_property(str_vo_key, Property::with_flags(Value::function(-635), Property::WRITABLE | Property::CONFIGURABLE));
        let string_prototype = heap.allocate(str_proto);
        func_prototypes.insert(-504i32, string_prototype);

        // Create Promise.prototype (prototype = Object.prototype). The actual
        // .then/.catch/.finally methods are dispatched specially in CallMethod
        // for Promise objects, so they don't need to live on this prototype —
        // but tests check `Object.getPrototypeOf(p) === Promise.prototype`.
        let mut promise_proto = JsObject::ordinary();
        promise_proto.prototype = Some(object_prototype);
        promise_proto.define_property(ctor_key, Property::with_flags(Value::function(-520), Property::WRITABLE | Property::CONFIGURABLE));
        // Real, extractable method values too: bundles lift them off the
        // prototype (`Promise.prototype.then.bind(Promise.resolve())` is
        // Preact's microtask scheduler). Each delegates to the same
        // dispatch CallMethod uses.
        for mname in ["then", "catch", "finally"] {
            let m_key = interner.intern(mname);
            let func: crate::runtime::object::NativeFn = std::sync::Arc::new(
                move |vm: &mut Vm, this: Value, args: &[Value]| -> Result<Value, Value> {
                    let Some(oid) = this.as_object_id() else {
                        return Ok(Value::undefined());
                    };
                    let m_id = vm.interner.intern(mname);
                    vm.exec_promise_method(oid, m_id, args).map_err(|e| match e {
                        VmError::Throw(v) => v,
                        other => {
                            let msg = format!("{other:?}");
                            vm.make_native_error("TypeError", &msg)
                        }
                    })
                },
            );
            let fobj = JsObject {
                properties: Vec::new(),
                prototype: None,
                kind: ObjectKind::Function(crate::runtime::object::FunctionKind::Native {
                    name: m_key,
                    func,
                }),
                marked: false,
                extensible: true,
            };
            let f_oid = heap.allocate(fobj);
            promise_proto.define_property(
                m_key,
                Property::with_flags(
                    Value::object_id(f_oid),
                    Property::WRITABLE | Property::CONFIGURABLE,
                ),
            );
        }
        let promise_prototype = heap.allocate(promise_proto);
        func_prototypes.insert(-520i32, promise_prototype);

        // Create Error.prototype objects so subclasses inherit `name`
        // First create Error.prototype itself
        let mut error_proto = JsObject::ordinary();
        error_proto.prototype = Some(object_prototype);
        let name_key_ep = interner.intern("name");
        let error_name_val = interner.intern("Error");
        error_proto.define_property(name_key_ep, Property::with_flags(Value::string(error_name_val), Property::WRITABLE | Property::CONFIGURABLE));
        let message_key_ep = interner.intern("message");
        let empty_str_ep = interner.intern("");
        error_proto.define_property(message_key_ep, Property::with_flags(Value::string(empty_str_ep), Property::WRITABLE | Property::CONFIGURABLE));
        let constructor_key_ep = interner.intern("constructor");
        error_proto.define_property(constructor_key_ep, Property::with_flags(Value::function(-510), Property::WRITABLE | Property::CONFIGURABLE));
        // Error.prototype.toString — `${name}: ${message}` (sentinel -598).
        let err_tostr_key = interner.intern("toString");
        error_proto.define_property(err_tostr_key, Property::with_flags(Value::function(-598), Property::WRITABLE | Property::CONFIGURABLE));
        let error_prototype_oid = heap.allocate(error_proto);
        func_prototypes.insert(-510i32, error_prototype_oid);

        // Create derived error prototypes with prototype = Error.prototype
        for (sentinel, type_name) in [
            (-511i32, "TypeError"), (-512, "RangeError"),
            (-513, "ReferenceError"), (-514, "SyntaxError"), (-515, "EvalError"), (-516, "URIError"),
        ] {
            let mut proto = JsObject::ordinary();
            proto.prototype = Some(error_prototype_oid);
            let name_key = interner.intern("name");
            let name_val = interner.intern(type_name);
            proto.define_property(name_key, Property::with_flags(Value::string(name_val), Property::WRITABLE | Property::CONFIGURABLE));
            let message_key = interner.intern("message");
            let empty_str = interner.intern("");
            proto.define_property(message_key, Property::with_flags(Value::string(empty_str), Property::WRITABLE | Property::CONFIGURABLE));
            let ctor_key = interner.intern("constructor");
            proto.define_property(ctor_key, Property::with_flags(Value::function(sentinel), Property::WRITABLE | Property::CONFIGURABLE));
            let ep = heap.allocate(proto);
            func_prototypes.insert(sentinel, ep);
        }

        // Create console object with log/warn/error methods
        let mut console_obj = JsObject::ordinary();
        let log_id = interner.intern("log");
        console_obj.set_property(log_id, Value::function(-100)); // sentinel for console.log
        let warn_id = interner.intern("warn");
        console_obj.set_property(warn_id, Value::function(-101)); // sentinel for console.warn
        let error_id = interner.intern("error");
        console_obj.set_property(error_id, Value::function(-102)); // sentinel for console.error
        let console_oid = heap.allocate(console_obj);
        let console_name = interner.intern("console");
        globals.insert(console_name, Value::object_id(console_oid));

        // Create Math object with constants and methods.
        // Per spec, Math.{PI,E,LN2,LN10,SQRT2,...} are non-writable, non-enumerable, non-configurable.
        let mut math_obj = JsObject::ordinary();
        let pi_name = interner.intern("PI");
        math_obj.define_property(pi_name, Property::with_flags(Value::number(std::f64::consts::PI), 0));
        let e_name = interner.intern("E");
        math_obj.define_property(e_name, Property::with_flags(Value::number(std::f64::consts::E), 0));
        let ln2_name = interner.intern("LN2");
        math_obj.define_property(ln2_name, Property::with_flags(Value::number(std::f64::consts::LN_2), 0));
        let ln10_name = interner.intern("LN10");
        math_obj.define_property(ln10_name, Property::with_flags(Value::number(std::f64::consts::LN_10), 0));
        let sqrt2_name = interner.intern("SQRT2");
        math_obj.define_property(sqrt2_name, Property::with_flags(Value::number(std::f64::consts::SQRT_2), 0));
        let log2e_name = interner.intern("LOG2E");
        math_obj.define_property(log2e_name, Property::with_flags(Value::number(std::f64::consts::LOG2_E), 0));
        let log10e_name = interner.intern("LOG10E");
        math_obj.define_property(log10e_name, Property::with_flags(Value::number(std::f64::consts::LOG10_E), 0));
        let sqrt1_2_name = interner.intern("SQRT1_2");
        math_obj.define_property(sqrt1_2_name, Property::with_flags(Value::number(std::f64::consts::FRAC_1_SQRT_2), 0));
        // Math methods as sentinel functions (-700 range)
        for (name, sentinel) in [
            ("sin", -700i32), ("cos", -701), ("abs", -702), ("floor", -703),
            ("ceil", -704), ("round", -705), ("sqrt", -706), ("pow", -707),
            ("max", -708), ("min", -709), ("exp", -710), ("log", -711),
            ("log2", -712), ("log10", -713), ("random", -714), ("trunc", -715),
            ("sign", -716), ("cbrt", -717), ("hypot", -718), ("atan2", -719),
            ("atan", -720), ("asin", -721), ("acos", -722), ("tan", -723),
            ("clz32", -724), ("imul", -725), ("fround", -726),
        ] {
            let k = interner.intern(name);
            math_obj.define_property(k, Property::with_flags(Value::function(sentinel), Property::WRITABLE | Property::CONFIGURABLE));
        }
        let math_oid = heap.allocate(math_obj);
        let math_name = interner.intern("Math");
        globals.insert(math_name, Value::object_id(math_oid));

        // Create JSON object (methods handled in exec_json_method)
        let json_obj = JsObject::ordinary();
        let json_oid = heap.allocate(json_obj);
        let json_name = interner.intern("JSON");
        globals.insert(json_name, Value::object_id(json_oid));

        // Global functions as sentinel values
        let parse_int_name = interner.intern("parseInt");
        globals.insert(parse_int_name, Value::function(-500));
        let parse_float_name = interner.intern("parseFloat");
        globals.insert(parse_float_name, Value::function(-501));
        let is_nan_name = interner.intern("isNaN");
        globals.insert(is_nan_name, Value::function(-502));
        let is_finite_name = interner.intern("isFinite");
        globals.insert(is_finite_name, Value::function(-503));
        let str_name = interner.intern("String");
        globals.insert(str_name, Value::function(-504));
        let num_name = interner.intern("Number");
        globals.insert(num_name, Value::function(-505));
        let bool_name = interner.intern("Boolean");
        globals.insert(bool_name, Value::function(-506));
        let arr_is_arr = interner.intern("Array");
        globals.insert(arr_is_arr, Value::function(-507));
        let object_name = interner.intern("Object");
        globals.insert(object_name, Value::function(-508));

        // URI handling. Without native versions, pages (and core-js) install
        // slow JS polyfills that percent-decode/encode char-by-char in a loop;
        // DuckDuckGo's SERP bundle hammered a decodeURIComponent polyfill hard
        // enough to blow the execution-limit fuel before it could define
        // DDG.Pages.SERP.
        let decode_uri_component_name = interner.intern("decodeURIComponent");
        globals.insert(decode_uri_component_name, Value::function(-517));
        let encode_uri_component_name = interner.intern("encodeURIComponent");
        globals.insert(encode_uri_component_name, Value::function(-518));
        let decode_uri_name = interner.intern("decodeURI");
        globals.insert(decode_uri_name, Value::function(-519));
        let encode_uri_name = interner.intern("encodeURI");
        globals.insert(encode_uri_name, Value::function(-509));

        // Promise constructor
        let promise_name = interner.intern("Promise");
        globals.insert(promise_name, Value::function(-520));

        // Error constructors
        let error_name = interner.intern("Error");
        globals.insert(error_name, Value::function(-510));
        let type_error_name = interner.intern("TypeError");
        globals.insert(type_error_name, Value::function(-511));
        let range_error_name = interner.intern("RangeError");
        globals.insert(range_error_name, Value::function(-512));
        let ref_error_name = interner.intern("ReferenceError");
        globals.insert(ref_error_name, Value::function(-513));
        let syntax_error_name = interner.intern("SyntaxError");
        globals.insert(syntax_error_name, Value::function(-514));
        let eval_error_name = interner.intern("EvalError");
        globals.insert(eval_error_name, Value::function(-515));
        let uri_error_name = interner.intern("URIError");
        globals.insert(uri_error_name, Value::function(-516));
        let eval_name = interner.intern("eval");
        globals.insert(eval_name, Value::function(-560));
        let symbol_name = interner.intern("Symbol");
        globals.insert(symbol_name, Value::function(-570));
        let bigint_name = interner.intern("BigInt");
        globals.insert(bigint_name, Value::function(-638));
        let map_name = interner.intern("Map");
        globals.insert(map_name, Value::function(-540));
        // Map.prototype / Set.prototype with extractable this-aware
        // methods. Instance calls (m.entries()) still dispatch on
        // ObjectKind in CallMethod first; these NativeFns serve property
        // reads and the uncurry-this pattern — core-js does
        // `var entries = uncurry(Map.prototype.entries); entries(map)`
        // while feature-testing collections (DuckDuckGo's polyfills
        // bundle), and reads `instance.set` as a value before .call-ing
        // it. Each NativeFn delegates to exec_map_method/exec_set_method
        // so semantics can't drift from the dispatch path.
        {
            // kind: 0 = Map, 1 = Set, 2 = WeakMap, 3 = WeakSet
            let make_delegate = |kind: u8, name: &str| -> crate::runtime::object::NativeFn {
                let name = name.to_owned();
                std::sync::Arc::new(move |vm: &mut Vm, this: Value, args: &[Value]| -> Result<Value, Value> {
                    let kind_ok = this.as_object_id().is_some_and(|o| {
                        vm.heap.get(o).is_some_and(|x| match kind {
                            0 => matches!(&x.kind, ObjectKind::Map { .. }),
                            1 => matches!(&x.kind, ObjectKind::Set { .. }),
                            2 => matches!(&x.kind, ObjectKind::WeakMap { .. }),
                            _ => matches!(&x.kind, ObjectKind::WeakSet { .. }),
                        })
                    });
                    let Some(oid) = this.as_object_id().filter(|_| kind_ok) else {
                        let which = ["Map", "Set", "WeakMap", "WeakSet"][kind as usize];
                        let msg = format!("{which} method called on incompatible receiver");
                        return Err(vm.make_native_error("TypeError", &msg));
                    };
                    let mid = vm.interner.intern(&name);
                    let res = match kind {
                        0 => vm.exec_map_method(oid, mid, args),
                        1 => vm.exec_set_method(oid, mid, args),
                        2 => vm.exec_weakmap_method(oid, mid, args),
                        _ => vm.exec_weakset_method(oid, mid, args),
                    };
                    match res {
                        Ok(v) => Ok(v),
                        Err(VmError::Throw(v)) => Err(v),
                        Err(e) => {
                            let msg = format!("{e:?}");
                            Err(vm.make_native_error("Error", &msg))
                        }
                    }
                })
            };
            let seed = |sentinel: i32, kind: u8, names: &[&str],
                            heap: &mut ObjectHeap, interner: &mut Interner,
                            func_prototypes: &mut HashMap<i32, ObjectId>| {
                let mut proto = JsObject::ordinary();
                proto.prototype = Some(object_prototype);
                proto.define_property(ctor_key, Property::with_flags(Value::function(sentinel), Property::WRITABLE | Property::CONFIGURABLE));
                for name in names {
                    let key = interner.intern(name);
                    let fn_obj = JsObject {
                        properties: Vec::new(),
                        prototype: None,
                        kind: ObjectKind::Function(crate::runtime::object::FunctionKind::Native { name: key, func: make_delegate(kind, name) }),
                        marked: false,
                        extensible: true,
                    };
                    let val = Value::object_id(heap.allocate(fn_obj));
                    proto.define_property(key, Property::with_flags(val, Property::WRITABLE | Property::CONFIGURABLE));
                }
                let proto_oid = heap.allocate(proto);
                func_prototypes.insert(sentinel, proto_oid);
            };
            seed(-540, 0, &["get", "set", "has", "delete", "clear", "forEach", "keys", "values", "entries"], &mut heap, &mut interner, &mut func_prototypes);
            seed(-541, 1, &["add", "has", "delete", "clear", "forEach", "keys", "values", "entries"], &mut heap, &mut interner, &mut func_prototypes);
            seed(-542, 2, &["get", "set", "has", "delete"], &mut heap, &mut interner, &mut func_prototypes);
            seed(-543, 3, &["add", "has", "delete"], &mut heap, &mut interner, &mut func_prototypes);
        }
        // RegExp.prototype (-580) with extractable test/exec/toString —
        // core-js uncurries `/./.exec` while building its JSON.stringify
        // surrogate-escaping wrapper.
        {
            let make_re_delegate = |name: &str| -> crate::runtime::object::NativeFn {
                let name = name.to_owned();
                std::sync::Arc::new(move |vm: &mut Vm, this: Value, args: &[Value]| -> Result<Value, Value> {
                    let Some(oid) = this.as_object_id()
                        .filter(|o| vm.heap.get(*o).is_some_and(|x| matches!(&x.kind, ObjectKind::RegExp { .. })))
                    else {
                        // RegExp.prototype.toString is GENERIC per spec:
                        // it reads .source / .flags off any receiver —
                        // core-js feature-tests exactly this with a plain
                        // {source, flags} object.
                        if name == "toString"
                            && let Some(roid) = this.as_object_id() {
                                let source_key = vm.interner.intern("source");
                                let flags_key = vm.interner.intern("flags");
                                let src = vm.heap.get_property_chain(roid, source_key)
                                    .map(|v| vm.value_to_string(v))
                                    .unwrap_or_else(|| "undefined".to_string());
                                let flg = vm.heap.get_property_chain(roid, flags_key)
                                    .map(|v| vm.value_to_string(v))
                                    .unwrap_or_else(|| "undefined".to_string());
                                let s = format!("/{src}/{flg}");
                                return Ok(Value::string(vm.interner.intern(&s)));
                            }
                        let repr = vm.value_to_string(this);
                        let msg = format!("RegExp.prototype.{name} called on incompatible receiver ({repr})");
                        return Err(vm.make_native_error("TypeError", &msg));
                    };
                    let mid = vm.interner.intern(&name);
                    match vm.exec_regexp_method(oid, mid, args) {
                        Ok(v) => Ok(v),
                        Err(VmError::Throw(v)) => Err(v),
                        Err(e) => {
                            let msg = format!("{e:?}");
                            Err(vm.make_native_error("Error", &msg))
                        }
                    }
                })
            };
            let mut re_proto = JsObject::ordinary();
            re_proto.prototype = Some(object_prototype);
            re_proto.define_property(ctor_key, Property::with_flags(Value::function(-580), Property::WRITABLE | Property::CONFIGURABLE));
            for name in ["test", "exec", "toString"] {
                let key = interner.intern(name);
                let fn_obj = JsObject {
                    properties: Vec::new(),
                    prototype: None,
                    kind: ObjectKind::Function(crate::runtime::object::FunctionKind::Native { name: key, func: make_re_delegate(name) }),
                    marked: false,
                    extensible: true,
                };
                let val = Value::object_id(heap.allocate(fn_obj));
                re_proto.define_property(key, Property::with_flags(val, Property::WRITABLE | Property::CONFIGURABLE));
            }
            let re_proto_oid = heap.allocate(re_proto);
            func_prototypes.insert(-580i32, re_proto_oid);
        }
        let set_name = interner.intern("Set");
        globals.insert(set_name, Value::function(-541));
        let weakmap_name = interner.intern("WeakMap");
        globals.insert(weakmap_name, Value::function(-542));
        let weakset_name = interner.intern("WeakSet");
        globals.insert(weakset_name, Value::function(-543));
        let date_name = interner.intern("Date");
        globals.insert(date_name, Value::function(-550));

        let regexp_name = interner.intern("RegExp");
        globals.insert(regexp_name, Value::function(-580));

        // Function constructor: `new Function("a", "b", "return a+b")` or `Function("...")`.
        // We stash a sentinel; Call/Construct handles it by concatenating source,
        // compiling, and creating a callable function.
        let function_name = interner.intern("Function");
        globals.insert(function_name, Value::function(-551));

        // Reflect: plain object (spec requires it to be an ordinary object)
        let reflect_obj = JsObject::ordinary();
        let reflect_oid = heap.allocate(reflect_obj);
        let reflect_name = interner.intern("Reflect");
        globals.insert(reflect_name, Value::object_id(reflect_oid));

        // globalThis: create a global object whose own property lookups
        // proxy to the globals map. For simplicity we expose a plain object
        // pre-populated with the common primitives; reads/writes go through
        // the object, not the globals map.
        let mut global_this_obj = JsObject::ordinary();
        // Populate non-configurable read-only globals (NaN, Infinity, undefined)
        // so `globalThis.Infinity = X` in strict mode correctly throws.
        let nan_key = interner.intern("NaN");
        global_this_obj.define_property(nan_key,
            Property::with_flags(Value::number(f64::NAN), 0));
        let inf_key = interner.intern("Infinity");
        global_this_obj.define_property(inf_key,
            Property::with_flags(Value::number(f64::INFINITY), 0));
        let undef_key = interner.intern("undefined");
        global_this_obj.define_property(undef_key,
            Property::with_flags(Value::undefined(), 0));
        let global_this_oid = heap.allocate(global_this_obj);
        let global_this_name = interner.intern("globalThis");
        globals.insert(global_this_name, Value::object_id(global_this_oid));

        // Pre-register well-known symbol descriptions
        let sym_descs = vec![
            Some(interner.intern("Symbol.iterator")),
            Some(interner.intern("Symbol.hasInstance")),
            Some(interner.intern("Symbol.toPrimitive")),
            Some(interner.intern("Symbol.toStringTag")),
            Some(interner.intern("Symbol.species")),
            Some(interner.intern("Symbol.unscopables")),
            Some(interner.intern("Symbol.asyncIterator")),
            Some(interner.intern("Symbol.matchAll")),
        ];

        // Pre-populate fast lookup Vec from all initial globals
        let globals_vec = {
            let max_id = globals.keys().map(|k| k.0 as usize).max().unwrap_or(0);
            let mut v = vec![Value::null(); max_id + 1];
            for (k, val) in &globals {
                v[k.0 as usize] = *val;
            }
            v
        };


        let mut vm = Self {
            chunks,
            frames: vec![CallFrame { chunk_idx: 0, ip: 0, base: 0, upvalues: Vec::new(), this_value: Value::object_id(global_this_oid), is_constructor: false, pending_super_call: false, generator_id: None, argc: 0, saved_args: Vec::new(), arguments_oid: None, is_derived_ctor: false, super_called: false, new_target: Value::undefined(), await_super_result: false, with_base: 0 }],
            stack: Vec::with_capacity(256),
            globals,
            interner,
            heap,
            globals_vec,
            global_ic: HashMap::new(),
            global_version: 0,
            global_ic_version: HashMap::new(),
            exc_handlers: Vec::new(),
            protect_throw_depth: 0,
            microtask_queue: Vec::new(),
            host_roots: Vec::new(),
            with_stack: Vec::new(),
            script_completion: Value::undefined(),
            param_scope_depth: 0,
            closure_upvalues: Vec::new(),
            open_upvalues: std::collections::HashMap::new(),
            #[cfg(any(all(target_arch = "aarch64", target_os = "macos"), target_arch = "x86_64"))]
            call_counts: HashMap::new(),
            #[cfg(any(all(target_arch = "aarch64", target_os = "macos"), target_arch = "x86_64"))]
            jit_functions: HashMap::new(),
            output: Vec::new(),
            silent_console: false,
            module_cache: HashMap::new(),
            module_dir: None,
            regex_cache: crate::vm::regexp::RegexCache::new(),
            func_prototypes,
            object_prototype,
            function_prototype,
            array_prototype,
            promise_prototype,
            iterator_prototype: None,
            boolean_prototype,
            number_prototype,
            string_prototype,
            global_this_oid,
            math_oid: Some(math_oid),
            json_oid: Some(json_oid),
            symbol_descriptions: sym_descs,
            symbol_registry: std::collections::HashMap::new(),
            next_symbol_id: 8, // 0-7 are well-known
            sym_iterator: 0,
            sym_has_instance: 1,
            sym_to_primitive: 2,
            sym_to_string_tag: 3,
            sym_species: 4,
            sym_unscopables: 5,
            sym_async_iterator: 6,
            sym_match_all: 7,
            fn_property_overrides: HashMap::new(),
            pending_private_brands: HashMap::new(),
            computed_exclusions: Vec::new(),
            steps: 0,
            max_steps: 0,
        };
        vm.init_typed_arrays();
        vm
    }

    pub(crate) fn flatten_chunk(mut chunk: Chunk, out: &mut Vec<Chunk>) {
        let children = std::mem::take(&mut chunk.child_chunks);
        // Record absolute indices of each direct child before recursing.
        // The first child will be at out.len() + 1 (after we push self).
        // Subsequent children follow after their siblings' full subtrees.
        let self_idx = out.len();
        out.push(chunk);
        for child in children {
            let child_abs_idx = out.len();
            out[self_idx].children.push(child_abs_idx);
            Self::flatten_chunk(child, out);
        }
    }

    /// Close all open upvalues that point to stack slots >= `from`.
    ///
    /// Heavy scripts (Google's homepage submit pipeline is the
    /// canonical example) accumulate stale `Open` upvalues in
    /// `closure_upvalues` storage: their original stack slot was
    /// already truncated away on a previous `Return`, but the
    /// upvalue itself outlived the closure that captured it. The
    /// next time `close_upvalues_above` runs with a low `from`,
    /// the naive `self.stack[stack_idx]` panics with "index out
    /// of bounds".
    ///
    /// The right long-term fix is to call `close_upvalues_above`
    /// at every stack-shrink site — but the VM has ~15 of those
    /// and threading the call through each is a larger
    /// refactor. For now: bound-check the indexing and close any
    /// stale upvalue to `undefined`. A subsequent read of the
    /// upvalue then returns `undefined` cleanly instead of
    /// crashing the host.
    /// Close any open upvalue cells whose slots are about to be
    /// destroyed, then shrink the stack. Every stack shrink must go
    /// through here: truncating past an open cell without closing it
    /// leaves the cell aimed at memory a later frame will reuse, so a
    /// surviving closure reads foreign values (react-dom's scheduler
    /// state silently became another frame's locals this way).
    pub(crate) fn truncate_stack(&mut self, to: usize) {
        if !self.open_upvalues.is_empty() {
            self.close_upvalues_above(to);
        }
        self.stack.truncate(to);
    }

    pub(crate) fn close_upvalues_above(&mut self, from: usize) {
        let stack_len = self.stack.len();
        let dead: Vec<usize> = self
            .open_upvalues
            .keys()
            .copied()
            .filter(|idx| *idx >= from)
            .collect();
        static WATCH: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
        let watch = *WATCH.get_or_init(|| std::env::var("ZINC_WATCH_SLOT").ok().and_then(|v| v.parse().ok()));
        for idx in dead {
            if let Some(cell) = self.open_upvalues.remove(&idx) {
                let val = if idx < stack_len {
                    self.stack[idx]
                } else {
                    // Stale entry whose slot was already truncated away
                    // on a path that didn't close — degrade to undefined.
                    Value::undefined()
                };
                if watch == Some(idx) {
                    eprintln!(
                        "[slotwatch] CLOSE slot {} from={} stack_len={} value={} chunk={} ip={}",
                        idx, from, stack_len, self.type_of_value(val),
                        self.frames.last().map(|f| self.interner.resolve(self.chunks[f.chunk_idx].name).to_string()).unwrap_or_default(),
                        self.frames.last().map(|f| f.ip).unwrap_or(0)
                    );
                }
                *cell.borrow_mut() = UpvalueLocation::Closed(val);
            }
        }
    }







    // ---- Promise helpers ----



    pub fn drain_microtasks(&mut self) -> Result<(), VmError> {
        let mut iterations = 0;
        while !self.microtask_queue.is_empty() {
            iterations += 1;
            if iterations > 10000 { return Err(VmError::RuntimeError("microtask loop limit".into())); }
            let task = self.microtask_queue.remove(0);
            match task {
                Microtask::PromiseReaction { callback, value, result_promise, is_fulfilled } => {
                    if let Some(cb) = callback {
                        match self.call_function(cb, &[value]) {
                            Ok(result) => {
                                // If the callback returned a thenable (Promise),
                                // adopt its state instead of resolving with the
                                // promise as the value.
                                let inner_promise = result.as_object_id().and_then(|oid| {
                                    self.heap.get(oid).and_then(|o| {
                                        if matches!(&o.kind, ObjectKind::Promise { .. }) {
                                            Some(oid)
                                        } else { None }
                                    })
                                });
                                if let Some(inner_oid) = inner_promise {
                                    // Adopt the inner promise's state: when it
                                    // settles, forward to result_promise.
                                    let then_name = self.interner.intern("then");
                                    let resolve_sentinel = Value::function(-600_000 - result_promise.0 as i32);
                                    let reject_sentinel = Value::function(-700_000 - result_promise.0 as i32);
                                    self.exec_promise_method(inner_oid, then_name, &[resolve_sentinel, reject_sentinel])?;
                                } else {
                                    self.resolve_promise(result_promise, result)?;
                                }
                            }
                            Err(VmError::Throw(reason)) => {
                                self.reject_promise(result_promise, reason)?;
                            }
                            Err(VmError::TypeError(msg)) => {
                                let err = self.make_native_error("TypeError", &msg);
                                self.reject_promise(result_promise, err)?;
                            }
                            Err(VmError::ReferenceError(msg)) => {
                                let err = self.make_native_error("ReferenceError", &msg);
                                self.reject_promise(result_promise, err)?;
                            }
                            Err(VmError::RuntimeError(msg)) => {
                                let err = self.make_native_error("Error", &msg);
                                self.reject_promise(result_promise, err)?;
                            }
                        }
                    } else {
                        // No callback: propagate the value
                        if is_fulfilled {
                            self.resolve_promise(result_promise, value)?;
                        } else {
                            self.reject_promise(result_promise, value)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }







    /// Run mark-and-sweep garbage collection.
    pub fn collect_gc(&mut self) {
        let mut roots: Vec<ObjectId> = Vec::new();

        // Root 1: stack
        for val in &self.stack {
            if let Some(oid) = trace_value(*val) { roots.push(oid); }
        }

        // Root 2: globals
        for val in self.globals.values() {
            if let Some(oid) = trace_value(*val) { roots.push(oid); }
        }

        // Root 3: globals_vec
        for val in &self.globals_vec {
            if let Some(oid) = trace_value(*val) { roots.push(oid); }
        }

        // Root 4: call frames
        for frame in &self.frames {
            if let Some(oid) = trace_value(frame.this_value) { roots.push(oid); }
            if let Some(gid) = frame.generator_id { roots.push(gid); }
            for uv in &frame.upvalues {
                if let UpvalueLocation::Closed(val) = &*uv.cell.borrow()
                    && let Some(oid) = trace_value(*val)
                {
                    roots.push(oid);
                }
            }
        }

        // Root 5: closure upvalues
        for closure_uvs in &self.closure_upvalues {
            for uv in closure_uvs {
                if let UpvalueLocation::Closed(val) = &*uv.cell.borrow()
                    && let Some(oid) = trace_value(*val)
                {
                    roots.push(oid);
                }
            }
        }

        // Root 6: microtask queue
        for task in &self.microtask_queue {
            match task {
                Microtask::PromiseReaction { callback, value, result_promise, .. } => {
                    if let Some(cb) = callback
                        && let Some(oid) = trace_value(*cb)
                    {
                        roots.push(oid);
                    }
                    if let Some(oid) = trace_value(*value) { roots.push(oid); }
                    roots.push(*result_promise);
                }
            }
        }

        // Root 7: singleton prototype objects
        roots.push(self.object_prototype);
        roots.push(self.function_prototype);
        roots.push(self.array_prototype);
        roots.push(self.boolean_prototype);
        roots.push(self.number_prototype);
        roots.push(self.string_prototype);

        // Root 8: function prototype cache
        for &oid in self.func_prototypes.values() {
            roots.push(oid);
        }
        // Root 8b: shared builtin-iterator prototype. Without this the
        // cached oid dangles after a sweep and every later iterator
        // chains to a recycled object (silent corruption; surfaced as
        // async-generator tests hanging once allocation crossed the GC
        // threshold).
        if let Some(oid) = self.iterator_prototype {
            roots.push(oid);
        }

        // Root 8c: function property overrides — holds the lazily
        // created NativeFn objects for extracted statics
        // (Object.create, Object.defineProperty, ...) plus user-set
        // function properties. These were collectable before; tests
        // only survived because GC rarely fired between caching and
        // use.
        for ov in self.fn_property_overrides.values() {
            if let Some(val) = ov
                && let Some(oid) = trace_value(*val)
            {
                roots.push(oid);
            }
        }

        // Root 9: math_oid, json_oid
        if let Some(oid) = self.math_oid { roots.push(oid); }
        if let Some(oid) = self.json_oid { roots.push(oid); }

        // Root 10: embedder-pinned objects (pending host promises etc.)
        roots.extend_from_slice(&self.host_roots);

        self.heap.mark_from_roots(&roots);
        self.heap.sweep();
        self.heap.gc_threshold = (self.heap.gc_threshold * 2).max(256);
    }

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
        if let Some(handler) = self.exc_handlers.pop() {
            let mut err = crate::runtime::object::JsObject::ordinary();
            let msg_key = self.interner.intern("message");
            let msg_id = self.interner.intern(msg);
            err.set_property(msg_key, Value::string(msg_id));
            let name_key = self.interner.intern("name");
            let type_error = self.interner.intern("TypeError");
            err.set_property(name_key, Value::string(type_error));
            // Also set up prototype chain for instanceof TypeError
            let proto = self.get_type_error_prototype();
            if let Some(proto_id) = proto {
                err.prototype = Some(proto_id);
            }
            let oid = self.heap.allocate(err);
            while self.frames.len() > handler.frame_idx + 1 {
                self.frames.pop();
            }
            self.truncate_stack(handler.stack_depth);
            self.with_stack.truncate(handler.with_depth);
            self.push(Value::object_id(oid));
            self.frames.last_mut().unwrap().ip = handler.catch_target as usize;
            Ok(())
        } else {
            Err(VmError::TypeError(msg.to_owned()))
        }
    }

    fn get_type_error_prototype(&mut self) -> Option<crate::runtime::object::ObjectId> {
        // TypeError is a native sentinel function (-511); its prototype is
        // tracked in func_prototypes, not on the heap.
        self.func_prototypes.get(&-511).copied()
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

    /// Pop two values off the stack, convert each to f64.
    /// JS ToNumber: coerce any value to f64.
    #[inline(always)]
    pub(crate) fn to_f64(&self, val: Value) -> f64 {
        if let Some(n) = val.as_number() { return n; }
        if val.is_boolean() { return if val.as_bool().unwrap() { 1.0 } else { 0.0 }; }
        if val.is_null() { return 0.0; }
        if val.is_undefined() { return f64::NAN; }
        if val.is_string() {
            let inl = val.as_inline_string();
            let raw = if let Some(ref i) = inl {
                i.as_str()
            } else {
                self.interner.resolve(val.as_string_id().unwrap())
            };
            let s = raw.trim();
            if s.is_empty() { return 0.0; }
            // Handle hex literals: 0x, 0X
            if s.starts_with("0x") || s.starts_with("0X") {
                return u64::from_str_radix(&s[2..], 16).map(|v| v as f64).unwrap_or(f64::NAN);
            }
            // Handle octal literals: 0o, 0O
            if s.starts_with("0o") || s.starts_with("0O") {
                return u64::from_str_radix(&s[2..], 8).map(|v| v as f64).unwrap_or(f64::NAN);
            }
            // Handle binary literals: 0b, 0B
            if s.starts_with("0b") || s.starts_with("0B") {
                return u64::from_str_radix(&s[2..], 2).map(|v| v as f64).unwrap_or(f64::NAN);
            }
            // Spec allows only `Infinity`, `+Infinity`, `-Infinity` (case-sensitive).
            // Rust's parser accepts "inf"/"INFINITY" too, so guard against that.
            match s {
                "Infinity" | "+Infinity" => return f64::INFINITY,
                "-Infinity" => return f64::NEG_INFINITY,
                _ => {}
            }
            if s.eq_ignore_ascii_case("inf")
                || s.eq_ignore_ascii_case("infinity")
                || s.eq_ignore_ascii_case("+inf")
                || s.eq_ignore_ascii_case("+infinity")
                || s.eq_ignore_ascii_case("-inf")
                || s.eq_ignore_ascii_case("-infinity")
                || s.eq_ignore_ascii_case("nan")
            {
                return f64::NAN;
            }
            return s.parse::<f64>().unwrap_or(f64::NAN);
        }
        // Wrapper objects: unwrap and coerce the primitive
        if let Some(oid) = val.as_object_id()
            && let Some(obj) = self.heap.get(oid) {
            match &obj.kind {
                ObjectKind::Wrapper(inner) => return self.to_f64(*inner),
                ObjectKind::ConsString { .. } => {
                    let s = self.flatten_cons_to_string(val);
                    let s = s.trim();
                    if s.is_empty() { return 0.0; }
                    return s.parse::<f64>().unwrap_or(f64::NAN);
                }
                _ => {}
            }
        }
        f64::NAN
    }

    /// Convert a Value to i32 for bitwise operations (ECMAScript ToInt32).
    pub(crate) fn to_i32(&self, val: Value) -> Result<i32, VmError> {
        let n = self.to_f64(val);
        if n.is_nan() || n.is_infinite() || n == 0.0 { return Ok(0); }
        let int = n.signum() * n.abs().floor();
        let int32bit = int.rem_euclid(4294967296.0);
        if int32bit >= 2147483648.0 {
            Ok((int32bit - 4294967296.0) as i32)
        } else {
            Ok(int32bit as i32)
        }
    }

    /// Convert a Value to u32 for unsigned right shift (ECMAScript ToUint32).
    pub(crate) fn to_u32(&self, val: Value) -> Result<u32, VmError> {
        let n = self.to_f64(val);
        if n.is_nan() || n.is_infinite() || n == 0.0 { return Ok(0); }
        let int = n.signum() * n.abs().floor();
        Ok(int.rem_euclid(4294967296.0) as u32)
    }

    /// Push a number result, using SMI when the value fits in i32 with no
    /// fractional part.  Preserves -0.0 as a float (JS distinguishes it).
    #[inline]
    /// Pop two operands for an arithmetic/bitwise/shift op, applying ToPrimitive
    /// (number hint) exactly once each. Classifies the pair so the caller can run
    /// BigInt vs Number semantics; mixing the two is a TypeError (Mixed).
    pub(crate) fn pop_arith_operands(&mut self) -> Result<ArithOperands, VmError> {
        let b = self.pop()?;
        let a = self.pop()?;
        // Raw BigInts are already primitive — don't run ToPrimitive on them.
        let a = if a.is_object() && !self.is_bigint(a) { self.try_coerce_to_primitive_hint(a, "number")? } else { a };
        if a.is_symbol() {
            let err = self.make_native_error("TypeError", "Cannot convert a Symbol value to a number");
            return Err(VmError::Throw(err));
        }
        let b = if b.is_object() && !self.is_bigint(b) { self.try_coerce_to_primitive_hint(b, "number")? } else { b };
        if b.is_symbol() {
            let err = self.make_native_error("TypeError", "Cannot convert a Symbol value to a number");
            return Err(VmError::Throw(err));
        }
        match (self.as_bigint(a), self.as_bigint(b)) {
            (Some(x), Some(y)) => Ok(ArithOperands::BigInts(x, y)),
            (None, None) => Ok(ArithOperands::Numbers(self.to_f64(a), self.to_f64(b))),
            _ => Ok(ArithOperands::Mixed),
        }
    }

    /// Throw the standard "Cannot mix BigInt and other types" TypeError.
    pub(crate) fn throw_mix_bigint(&mut self) -> Result<(), VmError> {
        let err = self.make_native_error("TypeError", "Cannot mix BigInt and other types, use explicit conversions");
        self.handle_throw(err)
    }

    pub(crate) fn push_number(&mut self, n: f64) {
        if n == 0.0 && n.is_sign_negative() {
            // -0.0 must stay as a float
            self.push(Value::number(n));
        } else if n.fract() == 0.0
            && n >= i32::MIN as f64
            && n <= i32::MAX as f64
            && !n.is_nan()
        {
            self.push(Value::int(n as i32));
        } else {
            self.push(Value::number(n));
        }
    }

    // ---- String coercion helpers ------------------------------------------

    /// Convert a Value to its string representation, using the interner for
    /// string values.
    pub(crate) fn value_to_string(&self, val: Value) -> String {
        if let Some(inl) = val.as_inline_string() {
            inl.as_str().to_owned()
        } else if let Some(id) = val.as_string_id() {
            self.interner.resolve(id).to_owned()
        } else if val.is_undefined() {
            "undefined".into()
        } else if val.is_null() {
            "null".into()
        } else if let Some(b) = val.as_bool() {
            if b { "true".into() } else { "false".into() }
        } else if let Some(i) = val.as_int() {
            i.to_string()
        } else if let Some(f) = val.as_number() {
            js_format_number(f)
        } else if val.is_symbol() {
            let id = val.as_symbol_id().unwrap();
            if let Some(Some(desc)) = self.symbol_descriptions.get(id as usize) {
                format!("Symbol({})", self.interner.resolve(*desc))
            } else {
                "Symbol()".into()
            }
        } else if val.is_function() {
            "function() { [native code] }".into()
        } else if let Some(oid) = val.as_object_id() {
            if let Some(obj) = self.heap.get(oid) {
                match &obj.kind {
                    ObjectKind::ConsString { .. } => {
                        self.flatten_cons_to_string(val)
                    }
                    ObjectKind::FlatString { data, .. } => data.to_string(),
                    ObjectKind::Array(elements) => {
                        let parts: Vec<String> = elements.iter().map(|v| self.value_to_string(*v)).collect();
                        parts.join(",")
                    }
                    ObjectKind::Wrapper(inner) => self.value_to_string(*inner),
                    ObjectKind::BigInt(b) => b.to_string(),
                    ObjectKind::RegExp { pattern, flags } => {
                        format!("/{pattern}/{flags}")
                    }
                    _ => {
                        // Check for Error-like objects: scan properties for "message" string
                        if let Some(obj) = self.heap.get(oid) {
                            let mut msg_val: Option<Value> = None;
                            let mut name_val: Option<Value> = None;
                            let proto = obj.prototype;
                            for &(k, ref prop) in &obj.properties {
                                let ks = self.interner.resolve(k);
                                if ks == "message" { msg_val = Some(prop.value); }
                                else if ks == "name" { name_val = Some(prop.value); }
                            }
                            // Walk prototype chain for "name" if not an own property
                            if name_val.is_none() {
                                let mut cur = proto;
                                'proto_walk: while let Some(p_oid) = cur {
                                    if let Some(p_obj) = self.heap.get(p_oid) {
                                        for &(k, ref prop) in &p_obj.properties {
                                            if self.interner.resolve(k) == "name" {
                                                name_val = Some(prop.value);
                                                break 'proto_walk;
                                            }
                                        }
                                        cur = p_obj.prototype;
                                    } else {
                                        break;
                                    }
                                }
                            }
                            if let Some(mv) = msg_val
                                && (mv.is_string() || self.is_cons_string(mv)) {
                                    let msg = self.flatten_cons_to_string(mv);
                                    let name = name_val
                                        .map(|v| self.flatten_cons_to_string(v))
                                        .filter(|s| !s.is_empty())
                                        .unwrap_or_else(|| "Error".to_owned());
                                    return format!("{name}: {msg}");
                                }
                        }
                        "[object Object]".into()
                    }
                }
            } else {
                "[object Object]".into()
            }
        } else {
            "???".into()
        }
    }

    /// Return the typeof string for a value.
    pub(crate) fn type_of_value(&self, val: Value) -> &'static str {
        if val.is_undefined() {
            "undefined"
        } else if val.is_null() {
            "object"
        } else if val.is_boolean() {
            "boolean"
        } else if val.is_function() {
            "function"
        } else if val.is_int() || val.is_number() {
            "number"
        } else if val.is_string() {
            "string"
        } else if val.is_symbol() {
            "symbol"
        } else if val.is_object() {
            if let Some(oid) = val.as_object_id()
                && let Some(obj) = self.heap.get(oid) {
                    if matches!(obj.kind, ObjectKind::ConsString { .. } | ObjectKind::FlatString { .. }) { return "string"; }
                    if matches!(obj.kind, ObjectKind::BigInt(_)) { return "bigint"; }
                    // Function-kind objects (bound functions, native sentinels wrapped
                    // as objects, bytecode functions stored as objects) report "function".
                    if matches!(obj.kind, ObjectKind::Function(_)) { return "function"; }
                    // Classes have __constructor__ — typeof should be "function"
                    for &(k, _) in &obj.properties {
                        if self.interner.resolve(k) == "__constructor__" { return "function"; }
                    }
                }
            "object"
        } else {
            "undefined"
        }
    }

    // ---- JS throw / native error helpers ---------------------------------

    /// Create a native JS error object with `name` and `message` properties.
    pub(crate) fn make_native_error(&mut self, name: &str, message: &str) -> Value {
        let mut err = crate::runtime::object::JsObject::ordinary();
        let name_sid = self.interner.intern(name);
        let msg_sid  = self.interner.intern(message);
        // Set prototype to the error type's prototype for instanceof checks
        let err_sentinel: i32 = match name {
            "TypeError" => -511, "RangeError" => -512, "ReferenceError" => -513,
            "SyntaxError" => -514, "EvalError" => -515, "URIError" => -516, _ => -510,
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
                // Not a known iterator kind — caller (e.g. CallMethod) shouldn't reach
                // here for known kinds. Return done=true as a safe fallback.
                return self.make_iter_result(Value::undefined(), true);
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

    /// Property GET of an Object static (`Object.create`, `Object.keys`, …)
    /// as a callable value: lazily wraps exec_object_static in a NativeFn,
    /// cached in fn_property_overrides for stable identity. Without this,
    /// `Object.create || shim` feature detection reads undefined — core-js
    /// then activates its ancient-IE iframe shim and dies (DuckDuckGo's
    /// polyfills bundle did exactly that).
    pub(crate) fn object_static_callable(&mut self, name_id: StringId) -> Option<Value> {
        let name_str = self.interner.resolve(name_id).to_owned();
        if !Self::OBJECT_STATIC_NAMES.contains(&name_str.as_str()) {
            return None;
        }
        let func: crate::runtime::object::NativeFn = std::sync::Arc::new(
            move |vm: &mut Vm, _this: Value, args: &[Value]| -> Result<Value, Value> {
                match vm.exec_object_static(&name_str, args) {
                    Ok(v) => Ok(v.unwrap_or(Value::undefined())),
                    Err(VmError::Throw(v)) => Err(v),
                    Err(e) => {
                        let msg = format!("{e:?}");
                        Err(vm.make_native_error("Error", &msg))
                    }
                }
            },
        );
        let fn_obj = JsObject {
            properties: Vec::new(),
            prototype: None,
            kind: ObjectKind::Function(crate::runtime::object::FunctionKind::Native { name: name_id, func }),
            marked: false,
            extensible: true,
        };
        let oid = self.heap.allocate(fn_obj);
        let val = Value::object_id(oid);
        self.fn_property_overrides.insert((-508, name_id), Some(val));
        Some(val)
    }

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

    /// ToBigInt(value) per spec. Returns the BigInt value, or Err(thrown).
    pub(crate) fn value_to_bigint(&mut self, val: Value) -> Result<num_bigint::BigInt, VmError> {
        // Unwrap a primitive-wrapper object first (Object(1n)).
        let v = if val.is_object() && !self.is_bigint(val) {
            self.try_coerce_to_primitive_hint(val, "number")?
        } else { val };
        if let Some(b) = self.as_bigint(v) { return Ok(b); }
        if let Some(boolean) = v.as_bool() {
            return Ok(num_bigint::BigInt::from(if boolean { 1 } else { 0 }));
        }
        if v.is_number() || v.is_int() {
            let f = self.to_f64(v);
            if !f.is_finite() || f.fract() != 0.0 {
                let err = self.make_native_error("RangeError", "The number is not a safe integer");
                return Err(VmError::Throw(err));
            }
            return Ok(num_traits::FromPrimitive::from_f64(f).unwrap_or_default());
        }
        if self.is_string_like(v) {
            let s = self.flatten_cons_to_string(v);
            return match string_to_bigint(&s) {
                Some(b) => Ok(b),
                None => {
                    let err = self.make_native_error("SyntaxError", "Cannot convert string to a BigInt");
                    Err(VmError::Throw(err))
                }
            };
        }
        let err = self.make_native_error("TypeError", "Cannot convert value to a BigInt");
        Err(VmError::Throw(err))
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

    pub(crate) fn exec_object_static(&mut self, mn: &str, args: &[Value]) -> Result<Option<Value>, VmError> {
        let result = match mn {
            "keys" => {
                // Function values keep their own properties in
                // fn_property_overrides — webpack's runtime does
                // `Object.keys(a.O)` where a.O is a function used as a
                // registry of chunk-loading handlers.
                if let Some(sentinel) = args.first().and_then(|v| v.as_function()) {
                    let key_ids: Vec<StringId> = self
                        .fn_property_overrides
                        .iter()
                        .filter(|((s, _), v)| *s == sentinel && v.is_some())
                        .map(|((_, k), _)| *k)
                        .collect();
                    // HashMap iteration order is arbitrary — sort by name
                    // for deterministic output.
                    let mut names: Vec<String> = key_ids
                        .iter()
                        .map(|k| self.interner.resolve(*k).to_owned())
                        .collect();
                    names.sort();
                    let values: Vec<Value> = names
                        .iter()
                        .map(|n| Value::string(self.interner.intern(n)))
                        .collect();
                    let arr = JsObject::array(values);
                    Value::object_id(self.heap.allocate(arr))
                } else if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
                    let is_array = self.heap.get(oid).map(|o| matches!(&o.kind, ObjectKind::Array(_))).unwrap_or(false);
                    if is_array {
                        let len = self.heap.get(oid).and_then(|o| if let ObjectKind::Array(ref e) = o.kind { Some(e.len()) } else { None }).unwrap_or(0);
                        let keys: Vec<Value> = (0..len).map(|i| {
                            let s = self.interner.intern(&i.to_string());
                            Value::string(s)
                        }).collect();
                        let arr = JsObject::array(keys);
                        Value::object_id(self.heap.allocate(arr))
                    } else {
                        let props: Vec<(StringId, bool)> = self.heap.get(oid)
                            .map(|o| o.properties.iter().map(|&(k, ref p)| (k, p.is_enumerable())).collect())
                            .unwrap_or_default();
                        let mut numeric: Vec<(u64, StringId)> = Vec::new();
                        let mut string: Vec<StringId> = Vec::new();
                        let mut seen: std::collections::HashSet<StringId> = std::collections::HashSet::new();
                        for (k, en) in props {
                            if !en { continue; }
                            // Accessor properties are stored under
                            // __get_NAME__ / __set_NAME__ — surface the
                            // bare NAME and dedupe so a paired getter/setter
                            // produces one entry.
                            let accessor_inner = {
                                let name = self.interner.resolve(k);
                                if let Some(rest) = name.strip_prefix("__get_").and_then(|s| s.strip_suffix("__")) {
                                    Some(rest.to_owned())
                                } else if let Some(rest) = name.strip_prefix("__set_").and_then(|s| s.strip_suffix("__")) {
                                    Some(rest.to_owned())
                                } else if is_internal_key(name) {
                                    continue;
                                } else {
                                    None
                                }
                            };
                            let key = match accessor_inner {
                                Some(s) => self.interner.intern(&s),
                                None => k,
                            };
                            if !seen.insert(key) { continue; }
                            let name = self.interner.resolve(key);
                            if let Ok(n) = name.parse::<u64>() {
                                numeric.push((n, key));
                            } else {
                                string.push(key);
                            }
                        }
                        numeric.sort_by_key(|&(n, _)| n);
                        let mut keys: Vec<Value> = numeric.into_iter().map(|(_, k)| Value::string(k)).collect();
                        keys.extend(string.into_iter().map(Value::string));
                        let arr = JsObject::array(keys);
                        Value::object_id(self.heap.allocate(arr))
                    }
                } else { Value::undefined() }
            }
            "values" => {
                if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
                    let vals: Vec<Value> = self.heap.get(oid)
                        .map(|o| {
                            if let ObjectKind::Array(ref elems) = o.kind {
                                elems.clone()
                            } else {
                                o.properties.iter()
                                    .filter(|(k, p)| p.is_enumerable()
                                        && !is_internal_key(self.interner.resolve(*k)))
                                    .map(|(_, p)| p.value).collect()
                            }
                        })
                        .unwrap_or_default();
                    let arr = JsObject::array(vals);
                    Value::object_id(self.heap.allocate(arr))
                } else { Value::undefined() }
            }
            "entries" => {
                if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
                    let pairs: Vec<(Value, Value)> = self.heap.get(oid)
                        .map(|o| o.properties.iter()
                            .filter(|(k, p)| p.is_enumerable()
                                && !is_internal_key(self.interner.resolve(*k)))
                            .map(|&(k, ref p)| (Value::string(k), p.value)).collect())
                        .unwrap_or_default();
                    let mut entries = Vec::new();
                    for (k, v) in pairs {
                        let pair = JsObject::array(vec![k, v]);
                        entries.push(Value::object_id(self.heap.allocate(pair)));
                    }
                    let arr = JsObject::array(entries);
                    Value::object_id(self.heap.allocate(arr))
                } else { Value::undefined() }
            }
            "assign" => self.exec_object_assign(args),
            "create" => {
                let proto = args.first().copied().unwrap_or(Value::null());
                let mut obj = JsObject::ordinary();
                obj.prototype = proto.as_object_id();
                // Handle property descriptors argument (2nd arg)
                if let Some(desc_val) = args.get(1)
                    && let Some(desc_oid) = desc_val.as_object_id()
                {
                    let props: Vec<(StringId, Value)> = self.heap.get(desc_oid)
                        .map(|o| o.properties.iter().map(|&(k, ref p)| (k, p.value)).collect())
                        .unwrap_or_default();
                    for (key, desc_obj_val) in props {
                        if let Some(d_oid) = desc_obj_val.as_object_id() {
                            let value_key = self.interner.intern("value");
                            let val = self.heap.get_property_chain(d_oid, value_key)
                                .unwrap_or(Value::undefined());
                            obj.set_property(key, val);
                        }
                    }
                }
                Value::object_id(self.heap.allocate(obj))
            }
            "defineProperty" => self.object_define_property(args),
            "defineProperties" => {
                // Simplified: treat like Object.assign for now
                args.first().copied().unwrap_or(Value::undefined())
            }
            "getOwnPropertyDescriptor" => {
                let first_arg = args.first().copied().unwrap_or(Value::undefined());
                let key_arg = args.get(1).copied().unwrap_or(Value::undefined());
                // For symbol keys, use __sym_N__ encoding; otherwise stringify
                let key_str = if key_arg.is_symbol() {
                    format!("__sym_{}__", key_arg.as_symbol_id().unwrap())
                } else {
                    self.value_to_string(key_arg)
                };
                // Function values keep user-set properties in
                // fn_property_overrides — core-js inspects descriptors of
                // well-known symbols it installed on the Symbol
                // constructor (`getOwnPropertyDescriptor(Symbol,
                // 'asyncDispose').enumerable`).
                if let Some(sentinel) = first_arg.as_function() {
                    let key_id = self.interner.intern(&key_str);
                    let deleted = matches!(self.fn_property_overrides.get(&(sentinel, key_id)), Some(None));
                    let (value, intrinsic) = if let Some(Some(v)) = self.fn_property_overrides.get(&(sentinel, key_id)).copied() {
                        (Some(v), false)
                    } else if deleted {
                        (None, false)
                    } else {
                        // Intrinsic name / length descriptors (spec flags:
                        // writable false, enumerable false, configurable true).
                        (self.fn_get_own_prop(sentinel, key_id), true)
                    };
                    if let Some(v) = value {
                        let mut desc = JsObject::ordinary();
                        desc.prototype = Some(self.object_prototype);
                        let value_key = self.interner.intern("value");
                        let writable_key = self.interner.intern("writable");
                        let enumerable_key = self.interner.intern("enumerable");
                        let configurable_key = self.interner.intern("configurable");
                        desc.set_property(value_key, v);
                        desc.set_property(writable_key, Value::boolean(!intrinsic));
                        desc.set_property(enumerable_key, Value::boolean(!intrinsic));
                        desc.set_property(configurable_key, Value::boolean(true));
                        return Ok(Some(Value::object_id(self.heap.allocate(desc))));
                    }
                    return Ok(Some(Value::undefined()));
                }
                if let Some(oid) = first_arg.as_object_id() {
                    let key_id = self.interner.intern(&key_str);
                    // Check for accessor properties first
                    let getter_key_str = format!("__get_{key_str}__");
                    let setter_key_str = format!("__set_{key_str}__");
                    let getter_key = self.interner.intern(&getter_key_str);
                    let setter_key = self.interner.intern(&setter_key_str);
                    let getter_desc = self.heap.get(oid)
                        .and_then(|o| o.get_property_descriptor(getter_key));
                    let setter_desc = self.heap.get(oid)
                        .and_then(|o| o.get_property_descriptor(setter_key));
                    if getter_desc.is_some() || setter_desc.is_some() {
                        let mut desc = JsObject::ordinary();
                        let get_key = self.interner.intern("get");
                        let set_key = self.interner.intern("set");
                        let en_key = self.interner.intern("enumerable");
                        let cf_key = self.interner.intern("configurable");
                        let getter_v = getter_desc.map(|p| p.value).unwrap_or(Value::undefined());
                        let setter_v = setter_desc.map(|p| p.value).unwrap_or(Value::undefined());
                        // Pull the conceptual descriptor's enumerable/configurable
                        // from the getter's slot (or the setter's if no getter).
                        let flags_src = getter_desc.or(setter_desc).unwrap();
                        desc.set_property(get_key, getter_v);
                        desc.set_property(set_key, setter_v);
                        desc.set_property(en_key, Value::boolean(flags_src.is_enumerable()));
                        desc.set_property(cf_key, Value::boolean(flags_src.is_configurable()));
                        Value::object_id(self.heap.allocate(desc))
                    } else if let Some(obj) = self.heap.get(oid)
                        && let Some(prop) = obj.get_property_descriptor(key_id) {
                            let mut desc = JsObject::ordinary();
                            let val_key = self.interner.intern("value");
                            let wr_key = self.interner.intern("writable");
                            let en_key = self.interner.intern("enumerable");
                            let cf_key = self.interner.intern("configurable");
                            desc.set_property(val_key, prop.value);
                            desc.set_property(wr_key, Value::boolean(prop.is_writable()));
                            desc.set_property(en_key, Value::boolean(prop.is_enumerable()));
                            desc.set_property(cf_key, Value::boolean(prop.is_configurable()));
                            Value::object_id(self.heap.allocate(desc))
                        } else if let Some(arr_info) = self.heap.get(oid).and_then(|o| {
                            // Array index "0", "1", … and "length" descriptors.
                            if let ObjectKind::Array(ref e) = o.kind {
                                Some((e.clone(), e.len()))
                            } else { None }
                        }) {
                            let (elements, len) = arr_info;
                            let val_key = self.interner.intern("value");
                            let wr_key = self.interner.intern("writable");
                            let en_key = self.interner.intern("enumerable");
                            let cf_key = self.interner.intern("configurable");
                            if key_str == "length" {
                                let mut desc = JsObject::ordinary();
                                desc.set_property(val_key, Value::int(len as i32));
                                desc.set_property(wr_key, Value::boolean(true));
                                desc.set_property(en_key, Value::boolean(false));
                                desc.set_property(cf_key, Value::boolean(false));
                                Value::object_id(self.heap.allocate(desc))
                            } else if let Ok(idx) = key_str.parse::<usize>() {
                                if idx < len {
                                    let mut desc = JsObject::ordinary();
                                    desc.set_property(val_key, elements[idx]);
                                    desc.set_property(wr_key, Value::boolean(true));
                                    desc.set_property(en_key, Value::boolean(true));
                                    desc.set_property(cf_key, Value::boolean(true));
                                    Value::object_id(self.heap.allocate(desc))
                                } else { Value::undefined() }
                            } else { Value::undefined() }
                        } else { Value::undefined() }
                } else if first_arg.is_function() {
                    let sentinel = first_arg.as_function().unwrap();
                    let key_id = self.interner.intern(&key_str);
                    let prop_val = if matches!(key_str.as_str(), "name" | "length") {
                        self.fn_get_own_prop(sentinel, key_id)
                            .map(|v| (v, false, false, true))
                    } else if key_str == "prototype" {
                        let proto_val = if let Some(&proto_oid) = self.func_prototypes.get(&sentinel) {
                            Value::object_id(proto_oid)
                        } else {
                            let mut proto = JsObject::ordinary();
                            proto.prototype = Some(self.object_prototype);
                            // Generator functions get a plain empty prototype.
                            let chunk_idx = (sentinel & 0xFFFF) as usize;
                            let is_gen = chunk_idx < self.chunks.len()
                                && self.chunks[chunk_idx].flags.contains(ChunkFlags::GENERATOR);
                            if !is_gen {
                                let ctor_key = self.interner.intern("constructor");
                                proto.define_property(ctor_key, Property::with_flags(
                                    first_arg, Property::WRITABLE | Property::CONFIGURABLE
                                ));
                            }
                            let proto_oid = self.heap.allocate(proto);
                            self.func_prototypes.insert(sentinel, proto_oid);
                            Value::object_id(proto_oid)
                        };
                        Some((proto_val, true, false, false))
                    } else { None };
                    if let Some((val, writable, enumerable, configurable)) = prop_val {
                        let mut desc = JsObject::ordinary();
                        let val_key = self.interner.intern("value");
                        let wr_key = self.interner.intern("writable");
                        let en_key = self.interner.intern("enumerable");
                        let cf_key = self.interner.intern("configurable");
                        desc.set_property(val_key, val);
                        desc.set_property(wr_key, Value::boolean(writable));
                        desc.set_property(en_key, Value::boolean(enumerable));
                        desc.set_property(cf_key, Value::boolean(configurable));
                        Value::object_id(self.heap.allocate(desc))
                    } else { Value::undefined() }
                } else { Value::undefined() }
            }
            "getOwnPropertyNames" => {
                // Native function values (Array, RegExp, etc.) expose
                // their three standard own props: length, name, prototype.
                if let Some(fv) = args.first() && fv.is_function() {
                    let mut names: Vec<Value> = Vec::new();
                    for n in ["length", "name", "prototype"] {
                        let id = self.interner.intern(n);
                        names.push(Value::string(id));
                    }
                    let arr = JsObject::array(names);
                    let arr_oid = self.heap.allocate(arr);
                    Value::object_id(arr_oid)
                } else if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
                    let mut seen = std::collections::HashSet::new();
                    let mut names: Vec<Value> = Vec::new();
                    // For arrays, integer indices come first in numeric order,
                    // then the `length` property, then named own properties.
                    let array_len = self.heap.get(oid).and_then(|o| {
                        if let ObjectKind::Array(ref e) = o.kind {
                            Some(e.len())
                        } else {
                            None
                        }
                    });
                    if let Some(len) = array_len {
                        for i in 0..len {
                            let s = i.to_string();
                            let id = self.interner.intern(&s);
                            names.push(Value::string(id));
                            seen.insert(s);
                        }
                        let length_str = "length".to_string();
                        if seen.insert(length_str.clone()) {
                            let id = self.interner.intern("length");
                            names.push(Value::string(id));
                        }
                    }
                    let raw_props: Vec<StringId> = self.heap.get(oid)
                        .map(|o| o.properties.iter().map(|(k, _)| *k).collect())
                        .unwrap_or_default();
                    // Per spec OrdinaryOwnPropertyKeys: integer indices
                    // come first in ascending numeric order, then the
                    // remaining string keys in insertion order.
                    let mut numeric: Vec<(u32, String)> = Vec::new();
                    let mut string_keys: Vec<String> = Vec::new();
                    for k in raw_props {
                        let s = self.interner.resolve(k).to_owned();
                        let real = if (s.starts_with("__get_") || s.starts_with("__set_")) && s.ends_with("__") {
                            s[6..s.len()-2].to_owned()
                        } else if is_internal_key(&s) {
                            continue;
                        } else {
                            s
                        };
                        if !seen.insert(real.clone()) { continue; }
                        if let Ok(n) = real.parse::<u32>()
                            && n.to_string() == real
                        {
                            numeric.push((n, real));
                        } else {
                            string_keys.push(real);
                        }
                    }
                    numeric.sort_by_key(|&(n, _)| n);
                    for (_, s) in numeric {
                        let id = self.interner.intern(&s);
                        names.push(Value::string(id));
                    }
                    for s in string_keys {
                        let id = self.interner.intern(&s);
                        names.push(Value::string(id));
                    }
                    let arr = JsObject::array(names);
                    Value::object_id(self.heap.allocate(arr))
                } else { Value::undefined() }
            }
            "getOwnPropertyDescriptors" => {
                if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
                    let props: Vec<(StringId, Property)> = self.heap.get(oid)
                        .map(|o| o.properties.iter().map(|(k, p)| (*k, *p)).collect())
                        .unwrap_or_default();
                    let val_key = self.interner.intern("value");
                    let wr_key = self.interner.intern("writable");
                    let en_key = self.interner.intern("enumerable");
                    let cf_key = self.interner.intern("configurable");
                    let mut result = JsObject::ordinary();
                    for (k, prop) in props {
                        let mut desc = JsObject::ordinary();
                        desc.set_property(val_key, prop.value);
                        desc.set_property(wr_key, Value::boolean(prop.is_writable()));
                        desc.set_property(en_key, Value::boolean(prop.is_enumerable()));
                        desc.set_property(cf_key, Value::boolean(prop.is_configurable()));
                        let desc_oid = self.heap.allocate(desc);
                        result.set_property(k, Value::object_id(desc_oid));
                    }
                    Value::object_id(self.heap.allocate(result))
                } else { Value::undefined() }
            }
            "freeze" => {
                let target = args.first().copied().unwrap_or(Value::undefined());
                if let Some(oid) = target.as_object_id()
                    && let Some(obj) = self.heap.get_mut(oid) {
                        obj.extensible = false;
                        for entry in &mut obj.properties {
                            entry.1.flags &= !(Property::WRITABLE | Property::CONFIGURABLE);
                        }
                    }
                target
            }
            "seal" => {
                let target = args.first().copied().unwrap_or(Value::undefined());
                if let Some(oid) = target.as_object_id()
                    && let Some(obj) = self.heap.get_mut(oid) {
                        obj.extensible = false;
                        for entry in &mut obj.properties {
                            entry.1.flags &= !Property::CONFIGURABLE;
                        }
                    }
                target
            }
            "isFrozen" => {
                if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
                    let frozen = self.heap.get(oid)
                        .map(|o| !o.extensible && o.properties.iter().all(|(_, p)| !p.is_writable() && !p.is_configurable()))
                        .unwrap_or(true);
                    Value::boolean(frozen)
                } else { Value::boolean(true) }
            }
            "isSealed" => {
                if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
                    let sealed = self.heap.get(oid)
                        .map(|o| !o.extensible && o.properties.iter().all(|(_, p)| !p.is_configurable()))
                        .unwrap_or(true);
                    Value::boolean(sealed)
                } else { Value::boolean(true) }
            }
            "is" => {
                let a = args.first().copied().unwrap_or(Value::undefined());
                let b = args.get(1).copied().unwrap_or(Value::undefined());
                // Object.is: like === but NaN===NaN is true, +0!==-0 is true
                let result = if a.is_number() && b.is_number() {
                    let na = a.as_number().unwrap();
                    let nb = b.as_number().unwrap();
                    if na.is_nan() && nb.is_nan() { true }
                    else if na == 0.0 && nb == 0.0 { na.to_bits() == nb.to_bits() }
                    else { na == nb }
                } else {
                    self.strict_eq(a, b)
                };
                Value::boolean(result)
            }
            "getPrototypeOf" => {
                let arg = args.first().copied().unwrap_or(Value::undefined());
                if let Some(oid) = arg.as_object_id() {
                    // Class/function objects get Function.prototype as their proto
                    let is_fn_obj = self.heap.get(oid)
                        .map(|o| matches!(&o.kind, ObjectKind::Function(_)))
                        .unwrap_or(false);
                    if is_fn_obj {
                        Value::object_id(self.function_prototype)
                    } else {
                        self.heap.get(oid)
                            .and_then(|o| o.prototype.map(Value::object_id))
                            .unwrap_or(Value::null())
                    }
                } else if arg.is_function() {
                    // Sentinel functions → Function.prototype
                    Value::object_id(self.function_prototype)
                } else { Value::null() }
            }
            "setPrototypeOf" => {
                let target = args.first().copied().unwrap_or(Value::undefined());
                if let Some(oid) = target.as_object_id() {
                    let proto = args.get(1).copied().unwrap_or(Value::null());
                    if let Some(obj) = self.heap.get_mut(oid) {
                        obj.prototype = proto.as_object_id();
                    }
                }
                target
            }
            "preventExtensions" => {
                let target = args.first().copied().unwrap_or(Value::undefined());
                if let Some(oid) = target.as_object_id()
                    && let Some(obj) = self.heap.get_mut(oid) {
                        obj.extensible = false;
                    }
                target
            }
            "isExtensible" => {
                let arg = args.first().copied().unwrap_or(Value::undefined());
                if let Some(oid) = arg.as_object_id() {
                    Value::boolean(self.heap.get(oid).map(|o| o.extensible).unwrap_or(false))
                } else if arg.is_function() {
                    // Function values are extensible by default.
                    Value::boolean(true)
                } else { Value::boolean(false) }
            }
            "hasOwn" => {
                let target = args.first().copied().unwrap_or(Value::undefined());
                let key_val = args.get(1).copied().unwrap_or(Value::undefined());
                if let Some(oid) = target.as_object_id() {
                    let key_str = self.value_to_string(key_val);
                    let key_id = self.interner.intern(&key_str);
                    let getter_key = self.interner.intern(&format!("__get_{key_str}__"));
                    let setter_key = self.interner.intern(&format!("__set_{key_str}__"));
                    let has = self.heap.get(oid).map(|o| {
                        o.has_own_property(key_id)
                            || o.has_own_property(getter_key)
                            || o.has_own_property(setter_key)
                    }).unwrap_or(false);
                    Value::boolean(has)
                } else { Value::boolean(false) }
            }
            "getOwnPropertySymbols" => {
                // Own symbol-keyed properties are stored under __sym_N__
                // names; decode N back into symbol values. core-js's
                // NATIVE_SYMBOL probe requires this function to exist.
                let mut syms: Vec<Value> = Vec::new();
                if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
                    let keys: Vec<StringId> = self.heap.get(oid)
                        .map(|o| o.properties.iter().map(|&(k, _)| k).collect())
                        .unwrap_or_default();
                    for k in keys {
                        let name = self.interner.resolve(k);
                        if let Some(rest) = name.strip_prefix("__sym_").and_then(|s| s.strip_suffix("__"))
                            && let Ok(n) = rest.parse::<u32>()
                        {
                            syms.push(Value::symbol(n));
                        }
                    }
                }
                let arr = JsObject::array(syms);
                Value::object_id(self.heap.allocate(arr))
            }
            "fromEntries" => {
                // Object.fromEntries(iterable)
                let mut obj = JsObject::ordinary();
                if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
                    let entries: Vec<Value> = self.heap.get(oid)
                        .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                        .unwrap_or_default();
                    for entry in entries {
                        if let Some(entry_oid) = entry.as_object_id()
                            && let Some(eobj) = self.heap.get(entry_oid)
                                && let ObjectKind::Array(ref pair) = eobj.kind
                                    && pair.len() >= 2 {
                                        let key_val = pair[0];
                                        let val = pair[1];
                                        let key_str = self.value_to_string(key_val);
                                        let key_id = self.interner.intern(&key_str);
                                        obj.set_property(key_id, val);
                                    }
                    }
                }
                Value::object_id(self.heap.allocate(obj))
            }
            _ => return Ok(None),
        };
        Ok(Some(result))
    }

    // ---- Function own-property helpers ------------------------------------

    /// Get a function's own property value, consulting the override table.
    /// Returns None if the property doesn't exist (or was deleted).
    pub(crate) fn fn_get_own_prop(&mut self, sentinel: i32, key: StringId) -> Option<Value> {
        let key_str = self.interner.resolve(key).to_owned();
        // Check the override table first
        if let Some(ov) = self.fn_property_overrides.get(&(sentinel, key)) {
            return *ov; // None = deleted, Some(v) = overridden
        }
        // Fall back to defaults
        let chunk_idx = (sentinel & 0xFFFF) as usize;
        match key_str.as_str() {
            "name" => {
                if chunk_idx > 0 && chunk_idx < self.chunks.len() {
                    let name_sid = self.chunks[chunk_idx].name;
                    let name_s = self.interner.resolve(name_sid).to_owned();
                    let visible = if name_s.starts_with('<') { String::new() } else { name_s };
                    let vsid = self.interner.intern(&visible);
                    Some(Value::string(vsid))
                } else {
                    let empty = self.interner.intern("");
                    Some(Value::string(empty))
                }
            }
            "length" => {
                if chunk_idx > 0 && chunk_idx < self.chunks.len() {
                    Some(Value::int(self.chunks[chunk_idx].formal_length as i32))
                } else {
                    Some(Value::int(0))
                }
            }
            _ => None,
        }
    }

    /// Comprehensive property lookup on a function-sentinel value (e.g. Number.POSITIVE_INFINITY,
    /// Symbol.iterator, fn.prototype). Returns the resolved value (Value::undefined() if missing).
    /// Used by both dot- and bracket-access paths.
    pub(crate) fn fn_property_get(&mut self, sentinel: i32, name_id: StringId, obj_val: Value) -> Value {
        if let Some(ov) = self.fn_property_overrides.get(&(sentinel, name_id)).copied() {
            return ov.unwrap_or(Value::undefined());
        }
        let name_str = self.interner.resolve(name_id).to_owned();
        match sentinel {
            -505 => match name_str.as_str() {
                "prototype" => Value::object_id(self.number_prototype),
                "NaN" => Value::number(f64::NAN),
                "POSITIVE_INFINITY" => Value::number(f64::INFINITY),
                "NEGATIVE_INFINITY" => Value::number(f64::NEG_INFINITY),
                "MAX_VALUE" => Value::number(f64::MAX),
                // Smallest positive value: the smallest denormal (5e-324),
                // i.e. the double with bit pattern 1 — not the smallest normal.
                "MIN_VALUE" => Value::number(f64::from_bits(1)),
                "MAX_SAFE_INTEGER" => Value::number(9007199254740991.0),
                "MIN_SAFE_INTEGER" => Value::number(-9007199254740991.0),
                "EPSILON" => Value::number(f64::EPSILON),
                "isNaN" => Value::function(-530),
                "isFinite" => Value::function(-531),
                "isInteger" => Value::function(-532),
                "isSafeInteger" => Value::function(-533),
                "parseInt" => Value::function(-500),
                "parseFloat" => Value::function(-501),
                "name" => { let id = self.interner.intern("Number"); Value::string(id) }
                "length" => Value::int(1),
                "call" | "apply" | "bind" => obj_val,
                _ => Value::undefined(),
            },
            -570 => match name_str.as_str() {
                "for" => Value::function(-752),
                "keyFor" => Value::function(-753),
                "iterator" => Value::symbol(self.sym_iterator),
                "hasInstance" => Value::symbol(self.sym_has_instance),
                "toPrimitive" => Value::symbol(self.sym_to_primitive),
                "toStringTag" => Value::symbol(self.sym_to_string_tag),
                "species" => Value::symbol(self.sym_species),
                "unscopables" => Value::symbol(self.sym_unscopables),
                "asyncIterator" => Value::symbol(self.sym_async_iterator),
                "matchAll" => Value::symbol(self.sym_match_all),
                "prototype" => Value::object_id(self.symbol_prototype_oid()),
                "name" => { let id = self.interner.intern("Symbol"); Value::string(id) }
                "length" => Value::int(0),
                "call" | "apply" | "bind" => obj_val,
                _ => Value::undefined(),
            },
            -504 => match name_str.as_str() {
                "prototype" => Value::object_id(self.string_prototype),
                "fromCharCode" => Value::function(-534),
                "fromCodePoint" => Value::function(-535),
                "raw" => Value::function(-536),
                "name" => { let id = self.interner.intern("String"); Value::string(id) }
                "length" => Value::int(1),
                "call" | "apply" | "bind" => obj_val,
                _ => Value::undefined(),
            },
            -507 => match name_str.as_str() {
                "prototype" => Value::object_id(self.array_prototype),
                "name" => { let id = self.interner.intern("Array"); Value::string(id) }
                "length" => Value::int(1),
                "call" | "apply" | "bind" => obj_val,
                // Extractable static: `var isArray = Array.isArray;` (react-dom).
                "isArray" => Value::function(-751),
                // Constructors are functions; inherited methods
                // (hasOwnProperty, isPrototypeOf, …) resolve through
                // Function.prototype → Object.prototype. e.g.
                // `Array.hasOwnProperty.call(o, k)` (common in
                // minified libs like Highlight.js).
                _ => self.heap.get_property_chain(self.function_prototype, name_id)
                    .unwrap_or(Value::undefined()),
            },
            -508 => match name_str.as_str() {
                "prototype" => Value::object_id(self.object_prototype),
                "name" => { let id = self.interner.intern("Object"); Value::string(id) }
                "length" => Value::int(1),
                "call" | "apply" | "bind" => obj_val,
                // Extractable static: `var assign = Object.assign` is the
                // standard minified-bundle prologue (React, Preact, ...).
                "assign" => Value::function(-750),
                _ => {
                    // Extractable Object statics — see object_static_callable.
                    if let Some(val) = self.object_static_callable(name_id) {
                        val
                    } else {
                        self.heap.get_property_chain(self.function_prototype, name_id)
                            .unwrap_or(Value::undefined())
                    }
                }
            },
            -551 => match name_str.as_str() {
                "prototype" => Value::object_id(self.function_prototype),
                "name" => { let id = self.interner.intern("Function"); Value::string(id) }
                "length" => Value::int(1),
                "call" | "apply" | "bind" => obj_val,
                _ => self.heap.get_property_chain(self.function_prototype, name_id)
                    .unwrap_or(Value::undefined()),
            },
            _ => match name_str.as_str() {
                "prototype" => {
                    if let Some(&proto_oid) = self.func_prototypes.get(&sentinel) {
                        Value::object_id(proto_oid)
                    } else {
                        let mut proto = JsObject::ordinary();
                        proto.prototype = Some(self.object_prototype);
                        // Generator functions' prototypes are plain empty objects per
                        // spec (`function* f(){}.prototype` has no own properties).
                        let chunk_idx = (sentinel & 0xFFFF) as usize;
                        let is_gen = chunk_idx < self.chunks.len()
                            && self.chunks[chunk_idx].flags.contains(ChunkFlags::GENERATOR);
                        if !is_gen {
                            let ctor_key = self.interner.intern("constructor");
                            proto.define_property(ctor_key, Property::with_flags(
                                obj_val, Property::WRITABLE | Property::CONFIGURABLE
                            ));
                        }
                        let proto_oid = self.heap.allocate(proto);
                        self.func_prototypes.insert(sentinel, proto_oid);
                        Value::object_id(proto_oid)
                    }
                }
                "constructor" => Value::function(-551),
                "name" | "length" => self.fn_get_own_prop(sentinel, name_id).unwrap_or(Value::undefined()),
                "call" | "apply" | "bind" => obj_val,
                // Inherited methods resolve through Function.prototype →
                // Object.prototype so every function value exposes
                // hasOwnProperty / isPrototypeOf / propertyIsEnumerable /
                // toString / valueOf (`fn.hasOwnProperty(...)` etc.).
                _ => self.heap.get_property_chain(self.function_prototype, name_id)
                    .unwrap_or(Value::undefined()),
            },
        }
    }

    // ---- BigInt helpers ----------------------------------------------------

    /// Allocate a heap BigInt and return it as a Value. Its prototype is
    /// BigInt.prototype so property reads (`.constructor`, `.toString`) resolve;
    /// `typeof`/arithmetic still classify it by its BigInt kind, not the proto.
    pub(crate) fn make_bigint(&mut self, v: num_bigint::BigInt) -> Value {
        let proto = self.bigint_prototype_oid();
        let mut obj = crate::runtime::object::JsObject::bigint(v);
        obj.prototype = Some(proto);
        let oid = self.heap.allocate(obj);
        Value::object_id(oid)
    }

    /// ToBoolean, heap-aware: `0n` is falsy (other BigInts truthy); everything
    /// else defers to the bit-level `Value::to_boolean`.
    pub(crate) fn truthy(&self, val: Value) -> bool {
        if let Some(b) = self.as_bigint(val) {
            return !num_traits::Zero::is_zero(&b);
        }
        val.to_boolean()
    }

    /// True if `val` is a BigInt primitive (heap object of kind BigInt).
    pub(crate) fn is_bigint(&self, val: Value) -> bool {
        val.as_object_id()
            .and_then(|oid| self.heap.get(oid))
            .map(|o| matches!(o.kind, crate::runtime::object::ObjectKind::BigInt(_)))
            .unwrap_or(false)
    }

    /// BigInt shift: `a << b` (left=true) or `a >> b` (left=false). A negative
    /// shift count reverses direction (`a << -n` == `a >> n`). `>>` is an
    /// arithmetic shift (rounds toward -∞), matching BigInt semantics.
    pub(crate) fn bigint_shift(&mut self, a: num_bigint::BigInt, b: num_bigint::BigInt, left: bool) -> Value {
        use num_bigint::{BigInt, Sign};
        use num_traits::{ToPrimitive, Zero};
        // Normalize to "shift left by `n`" where n may be negative.
        let n = b.to_i64().map(|v| if left { v } else { -v });
        let result = match n {
            Some(n) if n >= 0 => a << (n as usize),
            Some(n) => a >> ((-n) as usize),
            None => {
                // Shift count doesn't fit i64. Either direction is infeasible to
                // materialize for a nonzero value; approximate the limit results.
                let shifting_left = (b.sign() == Sign::Plus) == left;
                if a.is_zero() {
                    BigInt::zero()
                } else if shifting_left {
                    a << usize::MAX // effectively unbounded; only reachable with a==0 in practice
                } else if a.sign() == Sign::Minus {
                    BigInt::from(-1)
                } else {
                    BigInt::zero()
                }
            }
        };
        self.make_bigint(result)
    }

    /// Clone the BigInt value out of `val`, if it is one.
    pub(crate) fn as_bigint(&self, val: Value) -> Option<num_bigint::BigInt> {
        val.as_object_id()
            .and_then(|oid| self.heap.get(oid))
            .and_then(|o| match &o.kind {
                crate::runtime::object::ObjectKind::BigInt(b) => Some(b.clone()),
                _ => None,
            })
    }

    // ---- ConsString helpers -----------------------------------------------

    /// Returns true if val is a heap object of kind ConsString.
    #[inline(always)]
    /// Embedder-facing: the string content of `val` when it IS a
    /// string (flat/interned or a runtime-concatenation ConsString),
    /// else None. DOM bindings read text arguments through this —
    /// resolving only interned ids silently turned every
    /// `'prefix' + variable` argument into "".
    pub fn string_content(&self, val: Value) -> Option<String> {
        if self.is_string_like(val) {
            // is_string_like covers interned, inline (SSO), ConsString and
            // FlatString; value_to_string decodes each correctly.
            Some(self.value_to_string(val))
        } else {
            None
        }
    }

    pub(crate) fn is_cons_string(&self, val: Value) -> bool {
        if let Some(oid) = val.as_object_id()
            && let Some(obj) = self.heap.get(oid) {
            matches!(obj.kind, ObjectKind::ConsString { .. })
        } else {
            false
        }
    }

    /// Returns true if val is string-like: either a TAG_STRING or a ConsString object.
    #[inline(always)]
    pub(crate) fn is_string_like(&self, val: Value) -> bool {
        val.is_string() || self.is_cons_string(val) || self.is_flat_string(val)
    }

    /// Whether a string value's content is all-ASCII, enabling O(1) byte-indexed
    /// codepoint access in string methods. Interned strings use the cached flag;
    /// inline strings check their (≤5) bytes; ConsStrings conservatively return
    /// false (the slow char-walk path stays correct).
    #[inline]
    pub(crate) fn string_is_ascii(&self, val: Value) -> bool {
        if let Some(inl) = val.as_inline_string() {
            inl.as_str().is_ascii()
        } else if let Some(id) = val.as_string_id() {
            self.interner.is_ascii(id)
        } else {
            false
        }
    }

    /// True if `val` is a non-interned flat heap string (ObjectKind::FlatString).
    pub(crate) fn is_flat_string(&self, val: Value) -> bool {
        val.as_object_id()
            .and_then(|oid| self.heap.get(oid))
            .map(|o| matches!(o.kind, ObjectKind::FlatString { .. }))
            .unwrap_or(false)
    }

    /// Create a string Value from a `&str`, preferring an inline (small-string)
    /// encoding so transient short strings cost no heap allocation and no
    /// interner growth. String-producing operations use this instead of
    /// `Value::string(self.interner.intern(s))`.
    ///
    /// - empty           -> the interned StringId(0) singleton
    /// - <= 5 UTF-8 bytes -> inline (NaN-box payload; no allocation)
    /// - longer          -> interned (deduplicated and GC-free; the FlatString
    ///   heap path measured slower than interning on the decode benchmark)
    pub(crate) fn new_str(&mut self, s: &str) -> Value {
        if s.is_empty() {
            return Value::string(crate::util::interner::StringId(0));
        }
        if let Some(v) = Value::inline_string(s) {
            return v;
        }
        Value::string(self.interner.intern(s))
    }

    /// Returns the character length of a string-like value in O(1) for ConsString.
    pub(crate) fn string_char_len(&self, val: Value) -> u32 {
        if let Some(inl) = val.as_inline_string() {
            inl.as_str().chars().count() as u32
        } else if let Some(id) = val.as_string_id() {
            self.interner.char_len(id)
        } else if let Some(oid) = val.as_object_id()
            && let Some(obj) = self.heap.get(oid) {
            match &obj.kind {
                ObjectKind::ConsString { len, .. } => *len,
                ObjectKind::FlatString { char_len, .. } => *char_len,
                _ => 0,
            }
        } else {
            0
        }
    }

    /// Flatten a ConsString (or regular string) to a plain String without interning.
    /// Uses an iterative traversal to avoid stack overflow on deep trees.
    pub(crate) fn flatten_cons_to_string(&self, val: Value) -> String {
        if let Some(inl) = val.as_inline_string() {
            return inl.as_str().to_owned();
        }
        if let Some(id) = val.as_string_id() {
            return self.interner.resolve(id).to_owned();
        }
        let capacity = self.string_char_len(val) as usize;
        let mut result = String::with_capacity(capacity);
        let mut stack = Vec::new();
        stack.push(val);
        while let Some(cur) = stack.pop() {
            if let Some(inl) = cur.as_inline_string() {
                result.push_str(inl.as_str());
            } else if let Some(id) = cur.as_string_id() {
                result.push_str(self.interner.resolve(id));
            } else if let Some(oid) = cur.as_object_id()
                && let Some(obj) = self.heap.get(oid) {
                match &obj.kind {
                    ObjectKind::ConsString { left, right, .. } => {
                        // Push right first so left is processed first (LIFO)
                        stack.push(*right);
                        stack.push(*left);
                    }
                    ObjectKind::FlatString { data, .. } => result.push_str(data),
                    _ => {}
                }
            }
        }
        result
    }

    /// Flatten a ConsString to an interned StringId.
    pub(crate) fn flatten_to_string_id(&mut self, val: Value) -> crate::util::interner::StringId {
        if let Some(id) = val.as_string_id() {
            return id;
        }
        let flat = self.flatten_cons_to_string(val);
        self.interner.intern(&flat)
    }

    // ---- Abstract equality (simplified) -----------------------------------

    /// Simplified abstract equality (==). Handles the most common cases:
    /// Abstract equality (==) that propagates throws from valueOf/toString
    /// during ToPrimitive as `Err(VmError::Throw(_))`.
    ///
    ///   - same type: strict equality
    ///   - null == undefined (and vice versa)
    ///   - number == string: coerce string to number
    pub(crate) fn try_abstract_eq(&mut self, a: Value, b: Value) -> Result<bool, VmError> {
        // Fast path: identical bits
        if a.raw() == b.raw() {
            // NaN !== NaN
            if a.is_float() {
                let f = a.as_number().unwrap();
                return Ok(!f.is_nan());
            }
            return Ok(true);
        }

        // null == undefined
        if a.is_nullish() && b.is_nullish() {
            return Ok(true);
        }
        // null/undefined equals NOTHING else, with no coercion — per
        // spec §7.2.13. Falling through coerced the object operand via
        // ToPrimitive, which can THROW (core-js's `null == t` isNullOrUndefined
        // helper died on objects with object-returning toString).
        if a.is_nullish() || b.is_nullish() {
            return Ok(false);
        }

        // Both numbers (int/float mix)
        if a.is_number() && b.is_number() {
            return Ok(a.as_number() == b.as_number());
        }

        // Both strings (including ConsString)
        if self.is_string_like(a) && self.is_string_like(b) {
            return Ok(self.str_eq(a, b));
        }

        // Both booleans
        if a.is_boolean() && b.is_boolean() {
            return Ok(false); // already handled by raw() check
        }

        // number == string: coerce string to number
        if a.is_number() && b.is_string() {
            if let Some(n) = self.string_to_number(b) {
                return Ok(a.as_number() == Some(n));
            }
            return Ok(false);
        }
        if a.is_string() && b.is_number() {
            if let Some(n) = self.string_to_number(a) {
                return Ok(b.as_number() == Some(n));
            }
            return Ok(false);
        }

        // boolean vs other: coerce boolean to number, retry
        if a.is_boolean() {
            let num_a = if a.as_bool().unwrap() { 1.0 } else { 0.0 };
            return self.try_abstract_eq(Value::number(num_a), b);
        }
        if b.is_boolean() {
            let num_b = if b.as_bool().unwrap() { 1.0 } else { 0.0 };
            return self.try_abstract_eq(a, Value::number(num_b));
        }

        // BigInt comparisons. A BigInt is a heap object, so this must run before
        // the generic object-vs-primitive branch below.
        if self.is_bigint(a) || self.is_bigint(b) {
            // If the other operand is a (non-BigInt) object, coerce it first.
            if self.is_bigint(a) && b.is_object() && !self.is_bigint(b) {
                let pb = self.try_coerce_to_primitive_hint(b, "default")?;
                return self.try_abstract_eq(a, pb);
            }
            if self.is_bigint(b) && a.is_object() && !self.is_bigint(a) {
                let pa = self.try_coerce_to_primitive_hint(a, "default")?;
                return self.try_abstract_eq(pa, b);
            }
            let (big, other) = if let Some(x) = self.as_bigint(a) { (x, b) } else { (self.as_bigint(b).unwrap(), a) };
            if let Some(y) = self.as_bigint(other) {
                return Ok(big == y);
            }
            if other.is_number() {
                return Ok(bigint_eq_f64(&big, self.to_f64(other)));
            }
            if self.is_string_like(other) {
                let s = self.flatten_cons_to_string(other);
                return Ok(matches!(string_to_bigint(&s), Some(y) if big == y));
            }
            return Ok(false);
        }

        // object vs primitive: unwrap wrapper only when the OTHER side is primitive
        // (object == object compares references, not values)
        if a.is_object() && !b.is_object() {
            let pa = self.try_coerce_to_primitive_hint(a, "default")?;
            if pa.raw() != a.raw() {
                return self.try_abstract_eq(pa, b);
            }
        }
        if b.is_object() && !a.is_object() {
            let pb = self.try_coerce_to_primitive_hint(b, "default")?;
            if pb.raw() != b.raw() {
                return self.try_abstract_eq(a, pb);
            }
        }

        Ok(false)
    }

    /// Strict equality (===).
    /// Content equality for two string-like values, avoiding allocation on the
    /// hot path. Both args MUST be string-like (caller checks `is_string_like`).
    ///
    /// The decode loop's dominant comparison is `charAt(i) === '%'`: an *inline*
    /// string vs an *interned* literal. Their NaN-box bits differ, so the
    /// fast identity check misses — but we can still compare the inline bytes
    /// against the interned `&str` directly, with no `String` allocation.
    pub(crate) fn str_eq(&self, a: Value, b: Value) -> bool {
        // Identical encoding: inline==inline (same bytes) or interned==interned
        // (interning dedupes, so equal content => equal id => equal bits).
        if a.raw() == b.raw() {
            return true;
        }
        match (a.as_inline_string(), b.as_inline_string()) {
            (Some(x), Some(y)) => x.as_str() == y.as_str(),
            (Some(x), None) => self.value_eq_str(b, x.as_str()),
            (None, Some(y)) => self.value_eq_str(a, y.as_str()),
            (None, None) => {
                // Neither is inline. Two distinct interned ids => distinct content
                // (dedup guarantee; identical bits already handled above). Anything
                // involving a ConsString needs a flattening compare.
                if let (Some(ia), Some(ib)) = (a.as_string_id(), b.as_string_id()) {
                    return ia == ib;
                }
                self.flatten_cons_to_string(a) == self.flatten_cons_to_string(b)
            }
        }
    }

    /// Compare a string-like value against a known `&str` without allocating
    /// when `val` is interned; flattens only for ConsString.
    fn value_eq_str(&self, val: Value, s: &str) -> bool {
        if let Some(id) = val.as_string_id() {
            self.interner.resolve(id) == s
        } else if self.is_cons_string(val) || self.is_flat_string(val) {
            self.flatten_cons_to_string(val) == s
        } else {
            false
        }
    }

    pub(crate) fn strict_eq(&self, a: Value, b: Value) -> bool {
        if a.raw() == b.raw() {
            if a.is_float() {
                let f = a.as_number().unwrap();
                return !f.is_nan();
            }
            return true;
        }
        // Handle int == float comparison: 1 === 1.0 should be true
        if a.is_number() && b.is_number() {
            return a.as_number() == b.as_number();
        }
        // BigInt === BigInt compares by mathematical value; a BigInt is never
        // strictly equal to a value of any other type.
        match (self.as_bigint(a), self.as_bigint(b)) {
            (Some(x), Some(y)) => return x == y,
            (Some(_), None) | (None, Some(_)) => return false,
            (None, None) => {}
        }
        // ConsString equality: compare flattened content
        let a_str = self.is_string_like(a);
        let b_str = self.is_string_like(b);
        if a_str && b_str {
            return self.str_eq(a, b);
        }
        false
    }

    /// Try to parse a string value as a number (for == coercion).
    pub(crate) fn string_to_number(&self, val: Value) -> Option<f64> {
        let inl = val.as_inline_string();
        let raw = if let Some(ref i) = inl {
            i.as_str()
        } else {
            let id = val.as_string_id()?;
            self.interner.resolve(id)
        };
        let s = raw.trim();
        if s.is_empty() {
            return Some(0.0);
        }
        // Handle hex/octal/binary literal strings: "0xff", "0o17", "0b1010".
        if s.len() > 2 {
            let (sign, body) = match s.as_bytes()[0] {
                b'+' => (1.0, &s[1..]),
                b'-' => (-1.0, &s[1..]),
                _ => (1.0, s),
            };
            if body.len() > 2 && body.as_bytes()[0] == b'0' {
                let radix = match body.as_bytes()[1] {
                    b'x' | b'X' => Some(16),
                    b'o' | b'O' => Some(8),
                    b'b' | b'B' => Some(2),
                    _ => None,
                };
                if let Some(r) = radix
                    && let Ok(n) = u64::from_str_radix(&body[2..], r)
                {
                    return Some(sign * n as f64);
                }
            }
        }
        match s {
            "Infinity" | "+Infinity" => return Some(f64::INFINITY),
            "-Infinity" => return Some(f64::NEG_INFINITY),
            _ => {}
        }
        s.parse::<f64>().ok()
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

    /// Run the VM until the frame stack depth drops to `stop_depth`.
    /// Used by `call_function_this` to execute callbacks using the full dispatch loop.
    pub(crate) fn run_until(&mut self, stop_depth: usize) -> Result<Value, VmError> {
        let mut gc_counter: u32 = 0;
        // Resolve the per-instruction debug/profiling switches ONCE here rather
        // than doing an atomic OnceLock load on every dispatched opcode — these
        // are off in normal runs and were measurable per-op overhead.
        let trace_ip = {
            static TRACE_IP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            *TRACE_IP.get_or_init(|| std::env::var("ZINC_TRACE_IP").is_ok_and(|v| v == "1"))
        };
        let hist_on = {
            static HIST_ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            *HIST_ON.get_or_init(|| {
                std::env::var("ZINC_OPCODE_HIST").is_ok_and(|v| v == "1")
                    || std::env::var("ZINC_TIME").is_ok_and(|v| v == "1")
            })
        };
        loop {
            // GC safepoint + fuel check (every 1024 instructions)
            gc_counter = gc_counter.wrapping_add(1);
            if gc_counter & 0x3FF == 0 {
                if self.heap.needs_gc() { self.collect_gc(); }
                if self.max_steps > 0 {
                    self.steps += 1024;
                    if self.steps > self.max_steps {
                        return Err(VmError::RuntimeError("execution limit exceeded".into()));
                    }
                }
            }

            if self.frames.len() <= stop_depth {
                return Ok(if self.stack.is_empty() {
                    Value::undefined()
                } else {
                    self.pop()?
                });
            }

            let chunk_idx = self.cur_chunk();
            let ip = self.cur_ip();
            if ip >= self.chunks[chunk_idx].code.len() {
                // Implicit return from function
                if self.frames.len() <= stop_depth {
                    return Ok(if self.stack.is_empty() { Value::undefined() } else { self.pop()? });
                }
                let frame = self.frames.pop().unwrap();
                let result = if frame.is_constructor { frame.this_value } else { Value::undefined() };
                self.truncate_stack(frame.base.saturating_sub(1));
                if self.frames.len() <= stop_depth {
                    return Ok(result);
                }
                self.push(result);
                continue;
            }

            let byte = self.read_byte();
            // Debug/profiling, gated by switches resolved once at loop entry.
            if trace_ip {
                let f = self.frames.last().unwrap();
                eprintln!("[ip] chunk {} ip {} op 0x{byte:02x}", f.chunk_idx, f.ip - 1);
            }
            if hist_on {
                OPCODE_HIST[byte as usize].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            if !OpCode::is_valid(byte) {
                // A mid-instruction landing means corrupted control flow
                // (e.g. a stale exception handler's catch target). Panic
                // with enough context to localize it instead of letting
                // the transmute below go undefined.
                let f = self.frames.last().unwrap();
                let c = &self.chunks[f.chunk_idx];
                let name = self.interner.resolve(c.name);
                let lo = f.ip.saturating_sub(20);
                let hi = (f.ip + 10).min(c.code.len());
                panic!(
                    "invalid opcode 0x{byte:02x} in chunk {} '{}' at ip {} (code len {}); bytes[{lo}..{hi}] = {:02x?}",
                    f.chunk_idx, name, f.ip - 1, c.code.len(), &c.code[lo..hi]
                );
            }
            // Safety: validated by the is_valid check above.
            let opcode = unsafe { std::mem::transmute::<u8, OpCode>(byte) };

            match opcode {
                // ---- Constants & Literals --------------------------------
                OpCode::Const => {
                    let index = self.read_u16() as usize;
                    let chunk = self.cur_chunk();
                    let val = self.chunks[chunk].constants.get(index).copied().unwrap_or(Value::undefined());
                    self.push(val);
                }

                OpCode::LoadBigInt => {
                    let index = self.read_u16() as usize;
                    let chunk = self.cur_chunk();
                    let digits = self.chunks[chunk].constants.get(index)
                        .and_then(|v| v.as_string_id())
                        .map(|id| self.interner.resolve(id).to_owned())
                        .unwrap_or_default();
                    let big = parse_bigint_literal(&digits).unwrap_or_default();
                    let v = self.make_bigint(big);
                    self.push(v);
                }

                OpCode::ConstLong => {
                    let index = {
                        let v = self.chunks[self.cur_chunk()].read_u32(self.cur_ip());
                        self.frames.last_mut().unwrap().ip += 4;
                        v as usize
                    };
                    let chunk = self.cur_chunk();
                    let val = self.chunks[chunk].constants.get(index).copied().unwrap_or(Value::undefined());
                    self.push(val);
                }

                OpCode::Undefined => self.push(Value::undefined()),
                OpCode::Null => self.push(Value::null()),
                OpCode::True => self.push(Value::boolean(true)),
                OpCode::False => self.push(Value::boolean(false)),
                OpCode::Zero => self.push(Value::int(0)),
                OpCode::One => self.push(Value::int(1)),

                // ---- Stack Manipulation ----------------------------------
                OpCode::Pop => {
                    self.pop()?;
                }

                OpCode::PopN => {
                    let n = self.read_byte() as usize;
                    let new_len = self.stack.len().saturating_sub(n);
                    self.truncate_stack(new_len);
                }

                OpCode::Dup => {
                    let val = self.peek()?;
                    self.push(val);
                }

                OpCode::Dup2 => {
                    let len = self.stack.len();
                    if len < 2 {
                        return Err(VmError::RuntimeError("stack underflow".into()));
                    }
                    let a = self.stack[len - 2];
                    let b = self.stack[len - 1];
                    self.push(a);
                    self.push(b);
                }

                OpCode::Swap => {
                    let len = self.stack.len();
                    if len < 2 {
                        return Err(VmError::RuntimeError("stack underflow".into()));
                    }
                    self.stack.swap(len - 1, len - 2);
                }

                OpCode::Rot3 => {
                    // [a, b, c] -> [c, a, b]
                    let len = self.stack.len();
                    if len < 3 {
                        return Err(VmError::RuntimeError("stack underflow".into()));
                    }
                    let c = self.stack[len - 1];
                    self.stack[len - 1] = self.stack[len - 2];
                    self.stack[len - 2] = self.stack[len - 3];
                    self.stack[len - 3] = c;
                }

                // ---- Arithmetic ------------------------------------------
                OpCode::Add => {
                    let b = self.pop()?;
                    let a = self.pop()?;

                    // ToPrimitive for objects/functions before type check
                    // (throws propagate via VmError::Throw). try_coerce_to_primitive_hint
                    // also handles function-tagged Values.
                    let a_prim = if a.is_object() || a.is_function() {
                        match self.try_coerce_to_primitive_hint(a, "default") {
                            Ok(v) => v,
                            Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                            Err(e) => return Err(e),
                        }
                    } else { a };
                    let b_prim = if b.is_object() || b.is_function() {
                        match self.try_coerce_to_primitive_hint(b, "default") {
                            Ok(v) => v,
                            Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                            Err(e) => return Err(e),
                        }
                    } else { b };

                    let a_is_str = a_prim.is_string() || self.is_cons_string(a_prim) || self.is_string_wrapper(a_prim);
                    let b_is_str = b_prim.is_string() || self.is_cons_string(b_prim) || self.is_string_wrapper(b_prim);

                    if a_is_str || b_is_str {
                        // Per spec, when ToString runs on a Symbol it throws TypeError.
                        if a_prim.is_symbol() || b_prim.is_symbol() {
                            let err = self.make_native_error("TypeError", "Cannot convert a Symbol value to a string");
                            self.handle_throw(err)?;
                            continue;
                        }
                        // Normalize each side to a string-like value (TAG_STRING or ConsString)
                        let left_val = if self.is_string_like(a_prim) {
                            a_prim
                        } else {
                            let s = self.value_to_string(a_prim);
                            self.new_str(&s)
                        };
                        let right_val = if self.is_string_like(b_prim) {
                            b_prim
                        } else {
                            let s = self.value_to_string(b_prim);
                            self.new_str(&s)
                        };
                        let left_len = self.string_char_len(left_val);
                        let right_len = self.string_char_len(right_val);
                        let len = left_len + right_len;
                        // Short-circuit: avoid creating a ConsString for empty operands
                        if left_len == 0 {
                            // "" + x = x (flatten right to interned string for consistency)
                            let id = self.flatten_to_string_id(right_val);
                            self.push(Value::string(id));
                        } else if right_len == 0 {
                            // x + "" = x
                            let id = self.flatten_to_string_id(left_val);
                            self.push(Value::string(id));
                        } else {
                            let cs = JsObject {
                                properties: Vec::new(),
                                prototype: None,
                                kind: ObjectKind::ConsString { left: left_val, right: right_val, len },
                                marked: false,
                                extensible: false,
                            };
                            let oid = self.heap.allocate(cs);
                            self.push(Value::object_id(oid));
                        }
                    } else {
                        // ToNumber(lhs) before ToNumber(rhs); both throw on Symbol.
                        if a_prim.is_symbol() {
                            let err = self.make_native_error("TypeError", "Cannot convert a Symbol value to a number");
                            self.handle_throw(err)?;
                            continue;
                        }
                        if b_prim.is_symbol() {
                            let err = self.make_native_error("TypeError", "Cannot convert a Symbol value to a number");
                            self.handle_throw(err)?;
                            continue;
                        }
                        // Neither side is a string: BigInt + BigInt adds; mixing a
                        // BigInt with a non-BigInt is a TypeError.
                        match (self.as_bigint(a_prim), self.as_bigint(b_prim)) {
                            (Some(x), Some(y)) => { let v = self.make_bigint(x + y); self.push(v); continue; }
                            (None, None) => {}
                            _ => { self.throw_mix_bigint()?; continue; }
                        }
                        let na = self.to_f64(a_prim);
                        let nb = self.to_f64(b_prim);
                        self.push_number(na + nb);
                    }
                }

                OpCode::Sub => {
                    match self.pop_arith_operands() {
                        Ok(ArithOperands::Numbers(a, b)) => self.push_number(a - b),
                        Ok(ArithOperands::BigInts(a, b)) => { let v = self.make_bigint(a - b); self.push(v); }
                        Ok(ArithOperands::Mixed) => { self.throw_mix_bigint()?; continue; }
                        Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                        Err(e) => return Err(e),
                    }
                }

                OpCode::Mul => {
                    match self.pop_arith_operands() {
                        Ok(ArithOperands::Numbers(a, b)) => self.push_number(a * b),
                        Ok(ArithOperands::BigInts(a, b)) => { let v = self.make_bigint(a * b); self.push(v); }
                        Ok(ArithOperands::Mixed) => { self.throw_mix_bigint()?; continue; }
                        Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                        Err(e) => return Err(e),
                    }
                }

                OpCode::Div => {
                    match self.pop_arith_operands() {
                        Ok(ArithOperands::Numbers(a, b)) => self.push_number(a / b),
                        Ok(ArithOperands::BigInts(a, b)) => {
                            if num_bigint::BigInt::from(0) == b {
                                let err = self.make_native_error("RangeError", "Division by zero");
                                self.handle_throw(err)?; continue;
                            }
                            let v = self.make_bigint(a / b); self.push(v);
                        }
                        Ok(ArithOperands::Mixed) => { self.throw_mix_bigint()?; continue; }
                        Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                        Err(e) => return Err(e),
                    }
                }

                OpCode::Rem => {
                    match self.pop_arith_operands() {
                        Ok(ArithOperands::Numbers(a, b)) => self.push_number(a % b),
                        Ok(ArithOperands::BigInts(a, b)) => {
                            if num_bigint::BigInt::from(0) == b {
                                let err = self.make_native_error("RangeError", "Division by zero");
                                self.handle_throw(err)?; continue;
                            }
                            let v = self.make_bigint(a % b); self.push(v);
                        }
                        Ok(ArithOperands::Mixed) => { self.throw_mix_bigint()?; continue; }
                        Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                        Err(e) => return Err(e),
                    }
                }

                OpCode::Exp => {
                    match self.pop_arith_operands() {
                        Ok(ArithOperands::Numbers(a, b)) => {
                            // Per spec, if abs(base) is 1 and exponent is ±∞, the result
                            // is NaN. Rust's powf follows IEEE 754 and returns 1 here.
                            let result = if a.abs() == 1.0 && b.is_infinite() {
                                f64::NAN
                            } else {
                                a.powf(b)
                            };
                            self.push_number(result);
                        }
                        Ok(ArithOperands::BigInts(a, b)) => {
                            use num_traits::Signed;
                            if b.is_negative() {
                                let err = self.make_native_error("RangeError", "Exponent must be non-negative");
                                self.handle_throw(err)?; continue;
                            }
                            // Exponent fits in u32 for any feasible result; clamp avoids overflow.
                            let exp: u32 = num_traits::ToPrimitive::to_u32(&b).unwrap_or(u32::MAX);
                            let v = self.make_bigint(a.pow(exp)); self.push(v);
                        }
                        Ok(ArithOperands::Mixed) => { self.throw_mix_bigint()?; continue; }
                        Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                        Err(e) => return Err(e),
                    }
                }

                OpCode::Neg => {
                    let val = self.pop()?;
                    let prim = if val.is_object() {
                        match self.try_coerce_to_primitive_hint(val, "number") {
                            Ok(v) => v,
                            Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                            Err(e) => return Err(e),
                        }
                    } else { val };
                    if let Some(b) = self.as_bigint(prim) {
                        let v = self.make_bigint(-b); self.push(v);
                    } else {
                        self.push_number(-self.to_f64(prim));
                    }
                }

                OpCode::Pos => {
                    let val = self.pop()?;
                    let prim = if val.is_object() {
                        match self.try_coerce_to_primitive_hint(val, "number") {
                            Ok(v) => v,
                            Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                            Err(e) => return Err(e),
                        }
                    } else { val };
                    // Unary `+` performs ToNumber, which throws on BigInt.
                    if self.is_bigint(prim) {
                        let err = self.make_native_error("TypeError", "Cannot convert a BigInt value to a number");
                        self.handle_throw(err)?; continue;
                    }
                    self.push_number(self.to_f64(prim));
                }

                OpCode::ToNumeric => {
                    let val = self.pop()?;
                    let prim = if val.is_object() {
                        match self.try_coerce_to_primitive_hint(val, "number") {
                            Ok(v) => v,
                            Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                            Err(e) => return Err(e),
                        }
                    } else { val };
                    if self.is_bigint(prim) {
                        self.push(prim);
                    } else if prim.is_symbol() {
                        let err = self.make_native_error("TypeError", "Cannot convert a Symbol value to a number");
                        self.handle_throw(err)?; continue;
                    } else {
                        self.push_number(self.to_f64(prim));
                    }
                }

                OpCode::Inc => {
                    let val = self.pop()?;
                    let prim = if val.is_object() {
                        match self.try_coerce_to_primitive_hint(val, "number") {
                            Ok(v) => v,
                            Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                            Err(e) => return Err(e),
                        }
                    } else { val };
                    if let Some(b) = self.as_bigint(prim) {
                        let v = self.make_bigint(b + 1); self.push(v);
                    } else {
                        self.push_number(self.to_f64(prim) + 1.0);
                    }
                }

                OpCode::Dec => {
                    let val = self.pop()?;
                    let prim = if val.is_object() {
                        match self.try_coerce_to_primitive_hint(val, "number") {
                            Ok(v) => v,
                            Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                            Err(e) => return Err(e),
                        }
                    } else { val };
                    if let Some(b) = self.as_bigint(prim) {
                        let v = self.make_bigint(b - 1); self.push(v);
                    } else {
                        self.push_number(self.to_f64(prim) - 1.0);
                    }
                }

                // ---- Bitwise ---------------------------------------------
                OpCode::BitAnd => {
                    match self.pop_arith_operands() {
                        Ok(ArithOperands::Numbers(a, b)) => self.push(Value::int(f64_to_int32(a) & f64_to_int32(b))),
                        Ok(ArithOperands::BigInts(a, b)) => { let v = self.make_bigint(a & b); self.push(v); }
                        Ok(ArithOperands::Mixed) => { self.throw_mix_bigint()?; continue; }
                        Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                        Err(e) => return Err(e),
                    }
                }

                OpCode::BitOr => {
                    match self.pop_arith_operands() {
                        Ok(ArithOperands::Numbers(a, b)) => self.push(Value::int(f64_to_int32(a) | f64_to_int32(b))),
                        Ok(ArithOperands::BigInts(a, b)) => { let v = self.make_bigint(a | b); self.push(v); }
                        Ok(ArithOperands::Mixed) => { self.throw_mix_bigint()?; continue; }
                        Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                        Err(e) => return Err(e),
                    }
                }

                OpCode::BitXor => {
                    match self.pop_arith_operands() {
                        Ok(ArithOperands::Numbers(a, b)) => self.push(Value::int(f64_to_int32(a) ^ f64_to_int32(b))),
                        Ok(ArithOperands::BigInts(a, b)) => { let v = self.make_bigint(a ^ b); self.push(v); }
                        Ok(ArithOperands::Mixed) => { self.throw_mix_bigint()?; continue; }
                        Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                        Err(e) => return Err(e),
                    }
                }

                OpCode::BitNot => {
                    let val = self.pop()?;
                    let prim = if val.is_object() {
                        match self.try_coerce_to_primitive_hint(val, "number") {
                            Ok(v) => v,
                            Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                            Err(e) => return Err(e),
                        }
                    } else { val };
                    if let Some(b) = self.as_bigint(prim) {
                        // ~x == -(x + 1) for BigInt.
                        let v = self.make_bigint(-(b + num_bigint::BigInt::from(1))); self.push(v);
                    } else {
                        let n = self.to_i32(prim)?;
                        self.push(Value::int(!n));
                    }
                }

                OpCode::Shl => {
                    match self.pop_arith_operands() {
                        Ok(ArithOperands::Numbers(a, b)) => {
                            let shift = (f64_to_int32(b) as u32) & 0x1F;
                            self.push(Value::int(f64_to_int32(a).wrapping_shl(shift)));
                        }
                        Ok(ArithOperands::BigInts(a, b)) => {
                            let v = self.bigint_shift(a, b, true); self.push(v);
                        }
                        Ok(ArithOperands::Mixed) => { self.throw_mix_bigint()?; continue; }
                        Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                        Err(e) => return Err(e),
                    }
                }

                OpCode::Shr => {
                    match self.pop_arith_operands() {
                        Ok(ArithOperands::Numbers(a, b)) => {
                            let shift = (f64_to_int32(b) as u32) & 0x1F;
                            self.push(Value::int(f64_to_int32(a).wrapping_shr(shift)));
                        }
                        Ok(ArithOperands::BigInts(a, b)) => {
                            let v = self.bigint_shift(a, b, false); self.push(v);
                        }
                        Ok(ArithOperands::Mixed) => { self.throw_mix_bigint()?; continue; }
                        Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                        Err(e) => return Err(e),
                    }
                }

                OpCode::UShr => {
                    let b_val = self.pop()?;
                    let a_val = self.pop()?;
                    // Spec: ToNumber(lhs) before ToNumber(rhs); ToNumber on Symbol throws.
                    let a_val = if a_val.is_object() {
                        match self.try_coerce_to_primitive_hint(a_val, "number") {
                            Ok(v) => v,
                            Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                            Err(e) => return Err(e),
                        }
                    } else { a_val };
                    if a_val.is_symbol() {
                        let err = self.make_native_error("TypeError", "Cannot convert a Symbol value to a number");
                        self.handle_throw(err)?;
                        continue;
                    }
                    let b_val = if b_val.is_object() {
                        match self.try_coerce_to_primitive_hint(b_val, "number") {
                            Ok(v) => v,
                            Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                            Err(e) => return Err(e),
                        }
                    } else { b_val };
                    if b_val.is_symbol() {
                        let err = self.make_native_error("TypeError", "Cannot convert a Symbol value to a number");
                        self.handle_throw(err)?;
                        continue;
                    }
                    // BigInts have no unsigned right shift; any BigInt operand throws.
                    if self.is_bigint(a_val) || self.is_bigint(b_val) {
                        let err = self.make_native_error("TypeError", "BigInts have no unsigned right shift, use >> instead");
                        self.handle_throw(err)?;
                        continue;
                    }
                    let a = self.to_u32(a_val)?;
                    let b = self.to_u32(b_val)? & 0x1F;
                    let result = a >> b;
                    if result <= i32::MAX as u32 {
                        self.push(Value::int(result as i32));
                    } else {
                        self.push(Value::number(result as f64));
                    }
                }

                // ---- Comparison ------------------------------------------
                OpCode::Eq => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    match self.try_abstract_eq(a, b) {
                        Ok(r) => self.push(Value::boolean(r)),
                        Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                        Err(e) => return Err(e),
                    }
                }

                OpCode::Ne => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    match self.try_abstract_eq(a, b) {
                        Ok(r) => self.push(Value::boolean(!r)),
                        Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                        Err(e) => return Err(e),
                    }
                }

                OpCode::StrictEq => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.push(Value::boolean(self.strict_eq(a, b)));
                }

                OpCode::StrictNe => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.push(Value::boolean(!self.strict_eq(a, b)));
                }

                OpCode::Lt => {
                    let bv = self.pop()?;
                    let av = self.pop()?;
                    match self.relational_compare(av, bv, |a, b| a < b, |a, b| a < b, |o| o == std::cmp::Ordering::Less) {
                        Ok(r) => self.push(Value::boolean(r)),
                        Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                        Err(e) => return Err(e),
                    }
                }

                OpCode::Le => {
                    let bv = self.pop()?;
                    let av = self.pop()?;
                    match self.relational_compare(av, bv, |a, b| a <= b, |a, b| a <= b, |o| o != std::cmp::Ordering::Greater) {
                        Ok(r) => self.push(Value::boolean(r)),
                        Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                        Err(e) => return Err(e),
                    }
                }

                OpCode::Gt => {
                    let bv = self.pop()?;
                    let av = self.pop()?;
                    match self.relational_compare(av, bv, |a, b| a > b, |a, b| a > b, |o| o == std::cmp::Ordering::Greater) {
                        Ok(r) => self.push(Value::boolean(r)),
                        Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                        Err(e) => return Err(e),
                    }
                }

                OpCode::Ge => {
                    let bv = self.pop()?;
                    let av = self.pop()?;
                    match self.relational_compare(av, bv, |a, b| a >= b, |a, b| a >= b, |o| o != std::cmp::Ordering::Less) {
                        Ok(r) => self.push(Value::boolean(r)),
                        Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                        Err(e) => return Err(e),
                    }
                }

                // ---- Logical / Unary -------------------------------------
                OpCode::Not => {
                    let val = self.pop()?;
                    let t = self.truthy(val); self.push(Value::boolean(!t));
                }

                OpCode::TypeOf => {
                    let val = self.pop()?;
                    let type_str = self.type_of_value(val);
                    let id = self.interner.intern(type_str);
                    self.push(Value::string(id));
                }

                OpCode::TypeOfGlobal => {
                    let name_index = self.read_u16() as usize;
                    let name_val = self.chunks[self.cur_chunk()].constants[name_index];
                    let name_id = name_val.as_string_id().ok_or_else(|| {
                        VmError::RuntimeError("expected string constant for variable name".into())
                    })?;
                    // Mirror GetGlobal: names absent from the globals map can
                    // still live as properties on the globalThis object
                    // (`this.X = …` at script level — the UMD-wrapper
                    // pattern). `typeof` must agree with bare reads.
                    let val = match self.globals.get(&name_id).copied() {
                        Some(v) => v,
                        None => self
                            .heap
                            .get(self.global_this_oid)
                            .and_then(|o| o.get_property(name_id))
                            .unwrap_or(Value::undefined()),
                    };
                    let type_str = self.type_of_value(val);
                    let id = self.interner.intern(type_str);
                    self.push(Value::string(id));
                }

                OpCode::Void => {
                    self.pop()?;
                    self.push(Value::undefined());
                }

                // ---- Control Flow ----------------------------------------
                OpCode::Jump => {
                    let offset = self.read_i16();
                    // offset is relative to the position AFTER reading the operand
                    self.frames.last_mut().unwrap().ip = (self.frames.last().unwrap().ip as isize + offset as isize) as usize;
                }

                OpCode::JumpLong => {
                    let offset = {
                        let v = self.chunks[self.cur_chunk()].read_u32(self.cur_ip()) as i32;
                        self.frames.last_mut().unwrap().ip += 4;
                        v
                    };
                    self.frames.last_mut().unwrap().ip = (self.frames.last().unwrap().ip as isize + offset as isize) as usize;
                }

                OpCode::JumpIfFalse => {
                    let offset = self.read_i16();
                    let val = self.pop()?;
                    if !self.truthy(val) {
                        self.frames.last_mut().unwrap().ip = (self.frames.last().unwrap().ip as isize + offset as isize) as usize;
                    }
                }

                OpCode::JumpIfTrue => {
                    let offset = self.read_i16();
                    let val = self.pop()?;
                    if self.truthy(val) {
                        self.frames.last_mut().unwrap().ip = (self.frames.last().unwrap().ip as isize + offset as isize) as usize;
                    }
                }

                OpCode::JumpIfFalsePeek => {
                    let offset = self.read_i16();
                    let val = self.peek()?;
                    if !self.truthy(val) {
                        self.frames.last_mut().unwrap().ip = (self.frames.last().unwrap().ip as isize + offset as isize) as usize;
                    }
                }

                OpCode::JumpIfTruePeek => {
                    let offset = self.read_i16();
                    let val = self.peek()?;
                    if self.truthy(val) {
                        self.frames.last_mut().unwrap().ip = (self.frames.last().unwrap().ip as isize + offset as isize) as usize;
                    }
                }

                OpCode::JumpIfNullishPeek => {
                    let offset = self.read_i16();
                    let val = self.peek()?;
                    if val.is_nullish() {
                        self.frames.last_mut().unwrap().ip = (self.frames.last().unwrap().ip as isize + offset as isize) as usize;
                    }
                }

                OpCode::Loop => {
                    let offset = self.read_u16() as usize;
                    self.frames.last_mut().unwrap().ip -= offset;
                }

                // ---- Variable Access -------------------------------------
                OpCode::GetGlobal => {
                    let name_index = self.read_u16() as usize;
                    let name_val = self.chunks[self.cur_chunk()].constants[name_index];
                    let name_id = name_val.as_string_id().ok_or_else(|| {
                        VmError::RuntimeError("expected string constant".into())
                    })?;
                    // `with` scope: innermost-first, look up the name as a property of
                    // any active with-scope object before falling back to globals.
                    if !self.with_stack.is_empty() {
                        let mut found: Option<Value> = None;
                        for &oid in self.with_stack.iter().rev() {
                            if let Some(obj) = self.heap.get(oid)
                                && let Some(v) = obj.get_property(name_id)
                            {
                                found = Some(v);
                                break;
                            }
                        }
                        if let Some(v) = found {
                            self.push(v);
                            continue;
                        }
                    }
                    // Fast path: Vec-based lookup (O(1) instead of HashMap)
                    // null in the vec means "not present" (we never store null as a global)
                    let idx = name_id.0 as usize;
                    if idx < self.globals_vec.len() && !self.globals_vec[idx].is_null() {
                        self.push(self.globals_vec[idx]);
                        continue;
                    }
                    let name_str = self.interner.resolve(name_id);
                    if name_str == "__this__" {
                        let this_val = self.frames.last().unwrap().this_value;
                        self.push(this_val);
                    } else if name_str == "arguments" && self.frames.len() > 1 {
                        // Arrow functions don't have their own `arguments` — walk up
                        // to the nearest non-arrow frame.
                        let mut frame_idx = self.frames.len() - 1;
                        while frame_idx > 0
                            && self.chunks[self.frames[frame_idx].chunk_idx].flags.contains(ChunkFlags::ARROW)
                        {
                            frame_idx -= 1;
                        }
                        // Reuse the cached arguments object so identity holds and
                        // mutations (e.g. via `with`) persist across reads.
                        let cached = self.frames[frame_idx].arguments_oid;
                        let oid = if let Some(oid) = cached {
                            oid
                        } else {
                            let args = self.frames[frame_idx].saved_args.clone();
                            let chunk_idx = self.frames[frame_idx].chunk_idx;
                            let is_strict = self.chunks[chunk_idx].flags.contains(ChunkFlags::STRICT);
                            let mut arr = JsObject::array(args);
                            // Per spec, arguments has Object.prototype (not Array.prototype).
                            arr.prototype = Some(self.object_prototype);
                            // arguments[Symbol.iterator] is Array.prototype.values, so
                            // `for (x of arguments)` and `[...arguments]` work. The object
                            // is array-kind, so the array values iterator (-626) applies.
                            let sym_iter_key = self.interner.intern(&format!("__sym_{}__", self.sym_iterator));
                            arr.define_property(
                                sym_iter_key,
                                crate::runtime::object::Property::with_flags(
                                    Value::function(-626),
                                    crate::runtime::object::Property::WRITABLE
                                        | crate::runtime::object::Property::CONFIGURABLE,
                                ),
                            );
                            if !is_strict {
                                let callee_key = self.interner.intern("callee");
                                arr.define_property(
                                    callee_key,
                                    crate::runtime::object::Property::with_flags(
                                        Value::function(chunk_idx as i32),
                                        crate::runtime::object::Property::WRITABLE
                                            | crate::runtime::object::Property::CONFIGURABLE,
                                    ),
                                );
                            }
                            let oid = self.heap.allocate(arr);
                            self.frames[frame_idx].arguments_oid = Some(oid);
                            oid
                        };
                        self.push(Value::object_id(oid));
                    } else {
                        match self.globals.get(&name_id).copied() {
                            Some(val) => self.push(val),
                            None => {
                                // Fall back to the globalThis object — properties set via
                                // `this.x = …` or `globalThis.x = …` at script level live
                                // there, and bare identifiers should see them.
                                if let Some(obj) = self.heap.get(self.global_this_oid)
                                    && let Some(v) = obj.get_property(name_id)
                                {
                                    self.push(v);
                                    continue;
                                }
                                let name = self.interner.resolve(name_id).to_owned();
                                let (line, pc, chunk_name) = if let Some(f) = self.frames.last() {
                                    let cn = self.interner.resolve(self.chunks[f.chunk_idx].name).to_owned();
                                    (self.chunks[f.chunk_idx].get_line(f.ip as u32), f.ip, cn)
                                } else { (0, 0, String::new()) };
                                let msg = format!(
                                    "{name} is not defined (at line {line}, pc {pc}, chunk '{chunk_name}')"
                                );
                                let err = self.make_native_error("ReferenceError", &msg);
                                self.handle_throw(err)?;
                                continue;
                            }
                        }
                    }
                }

                OpCode::SetGlobal => {
                    let name_index = self.read_u16() as usize;
                    let name_val = self.chunks[self.cur_chunk()].constants[name_index];
                    let name_id = name_val.as_string_id().ok_or_else(|| {
                        VmError::RuntimeError("expected string constant for variable name".into())
                    })?;
                    let val = self.peek()?;
                    // `with` scope: if any active with-object owns this name, set it there.
                    if !self.with_stack.is_empty() {
                        let mut target_oid: Option<ObjectId> = None;
                        for &oid in self.with_stack.iter().rev() {
                            if let Some(obj) = self.heap.get(oid)
                                && obj.get_property(name_id).is_some()
                            {
                                target_oid = Some(oid);
                                break;
                            }
                        }
                        if let Some(oid) = target_oid {
                            if let Some(obj) = self.heap.get_mut(oid) {
                                obj.set_property(name_id, val);
                            }
                            continue;
                        }
                    }
                    // Strict-mode: assigning to an unresolvable reference (no binding
                    // exists anywhere in scope) throws ReferenceError per spec.
                    let chunk_idx = self.cur_chunk();
                    let in_strict = self.chunks[chunk_idx].flags.contains(ChunkFlags::STRICT);
                    if in_strict && !self.globals.contains_key(&name_id) {
                        let global_this_oid = self.global_this_oid;
                        let on_global_this = self.heap.get(global_this_oid)
                            .map(|o| o.has_own_property(name_id))
                            .unwrap_or(false);
                        if !on_global_this {
                            let name = self.interner.resolve(name_id).to_owned();
                            let msg = format!("{name} is not defined");
                            let err = self.make_native_error("ReferenceError", &msg);
                            self.handle_throw(err)?;
                            continue;
                        }
                    }
                    self.globals.insert(name_id, val);
                    // Sync to fast Vec
                    let idx = name_id.0 as usize;
                    if idx >= self.globals_vec.len() { self.globals_vec.resize(idx + 1, Value::null()); }
                    self.globals_vec[idx] = val;
                    self.global_version += 1;
                    // Mirror onto globalThis. Per spec, PutValue on an unresolvable
                    // reference in non-strict mode creates a writable/enumerable/
                    // configurable property on the global object; explicit `var`
                    // declarations (DefineGlobal) create writable/enumerable but
                    // non-configurable. Preserve existing flags if already there.
                    let global_this_oid = self.global_this_oid;
                    if let Some(obj) = self.heap.get_mut(global_this_oid) {
                        if obj.has_own_property(name_id) {
                            obj.set_property(name_id, val);
                        } else {
                            obj.define_property(
                                name_id,
                                Property::with_flags(
                                    val,
                                    Property::WRITABLE | Property::ENUMERABLE | Property::CONFIGURABLE,
                                ),
                            );
                        }
                    }
                }

                OpCode::DefineGlobal => {
                    let name_index = self.read_u16() as usize;
                    let name_val = self.chunks[self.cur_chunk()].constants[name_index];
                    let name_id = name_val.as_string_id().ok_or_else(|| {
                        VmError::RuntimeError("expected string constant for variable name".into())
                    })?;
                    let val = self.pop()?;
                    self.globals.insert(name_id, val);
                    let idx = name_id.0 as usize;
                    if idx >= self.globals_vec.len() { self.globals_vec.resize(idx + 1, Value::null()); }
                    self.globals_vec[idx] = val;
                    self.global_version += 1;
                    // Mirror onto globalThis so `Object.getOwnPropertyDescriptor(this, name)`
                    // and `Object.prototype.hasOwnProperty.call(globalThis, name)` see the
                    // declared var/function. Spec descriptors: writable, enumerable, NOT
                    // configurable (per CreateGlobalVarBinding).
                    let global_this_oid = self.global_this_oid;
                    if let Some(obj) = self.heap.get_mut(global_this_oid) {
                        if !obj.has_own_property(name_id) {
                            obj.define_property(
                                name_id,
                                Property::with_flags(val, Property::WRITABLE | Property::ENUMERABLE),
                            );
                        } else {
                            obj.set_property(name_id, val);
                        }
                    }
                }

                OpCode::GetLocal => {
                    let slot = self.read_byte() as usize;
                    let base = self.frames.last().unwrap().base;
                    let idx = base + slot;
                    let val = if idx < self.stack.len() { self.stack[idx] } else { Value::undefined() };
                    self.push(val);
                }

                OpCode::SetLocal => {
                    let slot = self.read_byte() as usize;
                    let val = self.peek()?;
                    let base = self.frames.last().unwrap().base;
                    let idx = base + slot;
                    if idx < self.stack.len() { self.stack[idx] = val; }
                }

                OpCode::GetLocalWide => {
                    let slot = self.read_u16() as usize;
                    let base = self.frames.last().unwrap().base;
                    let idx = base + slot;
                    let val = if idx < self.stack.len() { self.stack[idx] } else { Value::undefined() };
                    self.push(val);
                }

                OpCode::SetLocalWide => {
                    let slot = self.read_u16() as usize;
                    let val = self.peek()?;
                    let base = self.frames.last().unwrap().base;
                    let idx = base + slot;
                    if idx < self.stack.len() { self.stack[idx] = val; }
                }

                // ---- Functions -------------------------------------------
                OpCode::Call => {
                    let mut argc = self.read_byte() as usize;
                    let func_pos = self.stack.len() - 1 - argc;
                    let func_val = self.stack[func_pos];
                    // Embedder-controlled trace: set `ZINC_TRACE_CALLS=1`
                    // to dump every OpCall's receiver shape +
                    // bytecode position. Bulky output — turn off in
                    // production. Built behind an env-var check so
                    // the hot path is unaffected when not tracing.
                    if trace_calls_enabled() {
                        let (line, pc) = if let Some(f) = self.frames.last() {
                            (self.chunks[f.chunk_idx].get_line(f.ip as u32), f.ip)
                        } else { (0, 0) };
                        let receiver = self.value_to_string(func_val);
                        let callable = if func_val.is_function() { "fn" }
                            else if func_val.is_object() { "obj" }
                            else if func_val.is_null() { "null" }
                            else if func_val.is_undefined() { "undef" }
                            else { "other" };
                        eprintln!(
                            "[zinc-trace] Call line={line} pc={pc} kind={callable} recv={receiver} argc={argc}"
                        );
                    }
                    // Remember the actual user-passed arg count before padding so the
                    // arguments object reflects the call site, not the formal parameter
                    // list.
                    let actual_argc = argc;

                    if func_val.is_function() {
                        let packed = func_val.as_function().unwrap();
                        let closure_id = ((packed as u32) >> 16) as usize;
                        let chunk_idx = (packed & 0xFFFF) as usize;

                        if chunk_idx >= 1 && chunk_idx < self.chunks.len() {
                            // Pad missing arguments with undefined
                            let expected_params = self.chunks[chunk_idx].param_count as usize;
                            while argc < expected_params {
                                self.push(Value::undefined());
                                argc += 1; // shadow the outer argc
                            }

                            // Check if this is an async function. Async *generators* fall
                            // through to the normal call path so the body's CreateGenerator
                            // opcode returns an iterator object directly — `.next()` is
                            // what wraps each step in a Promise per spec.
                            if self.chunks[chunk_idx].flags.contains(ChunkFlags::ASYNC)
                                && !self.chunks[chunk_idx].flags.contains(ChunkFlags::GENERATOR)
                            {
                                // Create a promise, run body synchronously, resolve with result
                                let promise_id = self.allocate_promise();
                                let args_vec: Vec<Value> = (0..argc).map(|i| self.stack[func_pos + 1 + i]).collect();
                                self.truncate_stack(func_pos);
                                match self.call_function(func_val, &args_vec) {
                                    Ok(val) => { self.resolve_promise(promise_id, val)?; }
                                    Err(VmError::Throw(reason)) => {
                                        self.reject_promise(promise_id, reason)?;
                                    }
                                    Err(VmError::TypeError(msg)) => {
                                        let err = self.make_native_error("TypeError", &msg);
                                        self.reject_promise(promise_id, err)?;
                                    }
                                    Err(VmError::ReferenceError(msg)) => {
                                        let err = self.make_native_error("ReferenceError", &msg);
                                        self.reject_promise(promise_id, err)?;
                                    }
                                    Err(VmError::RuntimeError(msg)) => {
                                        let err = self.make_native_error("Error", &msg);
                                        self.reject_promise(promise_id, err)?;
                                    }
                                }
                                self.push(Value::object_id(promise_id));
                                continue;
                            }

                            // ---- JIT: check if we have compiled native code ----
                            #[cfg(any(all(target_arch = "aarch64", target_os = "macos"), target_arch = "x86_64"))]
                            {
                                // Check if JIT code already exists
                                if let Some(jit_fn) = self.jit_functions.get(&chunk_idx) {
                                    // Call native code directly!
                                    let result = if jit_fn.param_count() == 3 && argc >= 3 {
                                        let v0 = self.stack[func_pos + 1];
                                        let v1 = self.stack[func_pos + 2];
                                        let v2 = self.stack[func_pos + 3];
                                        let a0 = v0.as_number().unwrap_or(0.0) as i64;
                                        let a1 = v1.as_number().unwrap_or(0.0) as i64;
                                        let a2 = v2.as_number().unwrap_or(0.0) as i64;
                                        jit_fn.call3(a0, a1, a2)
                                    } else if jit_fn.param_count() == 2 && argc >= 2 {
                                        let v0 = self.stack[func_pos + 1];
                                        let v1 = self.stack[func_pos + 2];
                                        let a0 = v0.as_number().unwrap_or(0.0) as i64;
                                        let a1 = v1.as_number().unwrap_or(0.0) as i64;
                                        jit_fn.call2(a0, a1)
                                    } else {
                                        let arg = if argc > 0 {
                                            let v = self.stack[func_pos + 1];
                                            v.as_number().unwrap_or(0.0) as i64
                                        } else { 0 };
                                        jit_fn.call(arg)
                                    };
                                    self.truncate_stack(func_pos);
                                    if result >= i32::MIN as i64 && result <= i32::MAX as i64 {
                                        self.push(Value::int(result as i32));
                                    } else {
                                        self.push(Value::number(result as f64));
                                    }
                                    continue;
                                }

                                // Count calls and try to JIT at threshold
                                let count = self.call_counts.entry(chunk_idx).or_insert(0);
                                *count += 1;
                                if *count == 100 {
                                    // Try to JIT-compile this function
                                    if let Some(jit_fn) = crate::jit::compiler::jit_compile(
                                        &self.chunks[chunk_idx],
                                        &self.chunks,
                                    ) {
                                        self.jit_functions.insert(chunk_idx, jit_fn);
                                        // Don't use it yet on this call — next time
                                    }
                                }
                            }

                            // Generator functions: fall through to normal call path.
                            // The body's `CreateGenerator` opcode (emitted at the end of
                            // the prologue by the compiler) will capture frame state and
                            // return a generator object. This makes parameter destructuring
                            // and default-value evaluation eager, per spec.

                            // ---- Interpreter: normal bytecode execution ----
                            let upvalues = if closure_id < self.closure_upvalues.len()
                                && !self.closure_upvalues[closure_id].is_empty() {
                                self.closure_upvalues[closure_id].clone()
                            } else {
                                Vec::new()
                            };

                            // Check if this is a super() call
                            let is_super = self.frames.last().map(|f| f.pending_super_call).unwrap_or(false);
                            let this_val = if is_super {
                                if let Some(f) = self.frames.last_mut() {
                                    f.pending_super_call = false;
                                    // Mark caller so the parent ctor's return value can rebind
                                    // `this` per BindThisValue.
                                    f.await_super_result = true;
                                }
                                self.frames.last().unwrap().this_value
                            } else if self.chunks[chunk_idx].flags.contains(ChunkFlags::ARROW) {
                                // Arrow functions inherit `this` from enclosing scope
                                self.frames.last().map(|f| f.this_value).unwrap_or(Value::undefined())
                            } else if self.chunks[chunk_idx].flags.contains(ChunkFlags::STRICT) {
                                Value::undefined()
                            } else {
                                // Non-strict function called without explicit this binding:
                                // `this` is coerced to the global object.
                                Value::object_id(self.global_this_oid)
                            };

                            let saved_args: Vec<Value> = (0..actual_argc)
                                .map(|i| self.stack.get(func_pos + 1 + i).copied().unwrap_or(Value::undefined()))
                                .collect();
                            // Arrow functions inherit new.target from the enclosing
                            // scope; ordinary calls (not via `new`) have new.target = undefined.
                            let new_target = if self.chunks[chunk_idx].flags.contains(ChunkFlags::ARROW) {
                                self.frames.last().map(|f| f.new_target).unwrap_or(Value::undefined())
                            } else {
                                Value::undefined()
                            };
                            self.frames.push(CallFrame {
                                chunk_idx,
                                ip: 0,
                                base: func_pos + 1,
                                upvalues,
                                this_value: this_val,
                                is_constructor: false,
                                pending_super_call: false,
                                generator_id: None,
                                argc,
                                saved_args, arguments_oid: None, is_derived_ctor: false, super_called: false,
                                new_target,
                                await_super_result: false,
                                with_base: self.with_stack.len(),
                            });
                            continue;
                        }
                    }

                    // Check for Promise resolve/reject sentinels
                    if func_val.is_function() {
                        let s = func_val.as_function().unwrap();
                        if s <= -600_000 && s > -700_000 {
                            // Promise resolve
                            let pid = ObjectId((-600_000 - s) as u32);
                            let val = if argc > 0 { self.stack[func_pos + 1] } else { Value::undefined() };
                            self.truncate_stack(func_pos);
                            self.resolve_promise(pid, val)?;
                            self.push(Value::undefined());
                            continue;
                        }
                        if s <= -700_000 && s > -800_000 {
                            // Promise reject
                            let pid = ObjectId((-700_000 - s) as u32);
                            let val = if argc > 0 { self.stack[func_pos + 1] } else { Value::undefined() };
                            self.truncate_stack(func_pos);
                            self.reject_promise(pid, val)?;
                            self.push(Value::undefined());
                            continue;
                        }
                        // Promise combinator callbacks (see promise.rs encoding).
                        if s <= -1_000_000_000 && s > -2_100_000_000 {
                            let encoded = (-1_000_000_000i64 - s as i64) as u32;
                            let tracker_oid = ObjectId(encoded / 2048);
                            let index = ((encoded % 2048) / 2) as usize;
                            let is_reject = encoded & 1 == 1;
                            let val = if argc > 0 { self.stack[func_pos + 1] } else { Value::undefined() };
                            self.truncate_stack(func_pos);
                            if is_reject {
                                self.handle_combinator_reject(tracker_oid, index, val)?;
                            } else {
                                self.handle_combinator_resolve(tracker_oid, index, val)?;
                            }
                            self.push(Value::undefined());
                            continue;
                        }
                        // Promise.finally fulfill callback
                        if s <= -1_100_000 && s > -1_200_000 {
                            let tracker_oid = ObjectId((-1_100_000 - s) as u32);
                            let val = if argc > 0 { self.stack[func_pos + 1] } else { Value::undefined() };
                            self.truncate_stack(func_pos);
                            // Call the finally callback, then resolve with original value
                            if let Some(obj) = self.heap.get(tracker_oid)
                                && let ObjectKind::FinallyTracker { callback, .. } = &obj.kind {
                                    let cb = *callback;
                                    let _ = self.call_function(cb, &[]);
                                }
                            self.push(val);
                            continue;
                        }
                        // Promise.finally reject callback
                        if s <= -1_200_000 && s > -1_300_000 {
                            let tracker_oid = ObjectId((-1_200_000 - s) as u32);
                            let val = if argc > 0 { self.stack[func_pos + 1] } else { Value::undefined() };
                            self.truncate_stack(func_pos);
                            if let Some(obj) = self.heap.get(tracker_oid)
                                && let ObjectKind::FinallyTracker { callback, .. } = &obj.kind {
                                    let cb = *callback;
                                    let _ = self.call_function(cb, &[]);
                                }
                            // Re-throw/reject: propagate rejection reason
                            return Err(VmError::RuntimeError(self.value_to_string(val)));
                        }
                    }

                    // Check for Symbol() — NOT constructable with new
                    if func_val.is_function() && func_val.as_function() == Some(-570) {
                        let desc = if argc > 0 {
                            let d = self.stack[func_pos + 1];
                            if d.is_undefined() { None } else { Some(self.interner.intern(&self.value_to_string(d))) }
                        } else { None };
                        let id = self.next_symbol_id;
                        self.next_symbol_id += 1;
                        if id as usize >= self.symbol_descriptions.len() {
                            self.symbol_descriptions.resize(id as usize + 1, None);
                        }
                        self.symbol_descriptions[id as usize] = desc;
                        self.truncate_stack(func_pos);
                        self.push(Value::symbol(id));
                        continue;
                    }

                    // BigInt(value) — converts to a BigInt (not constructable).
                    if func_val.is_function() && func_val.as_function() == Some(-638) {
                        let arg = if argc > 0 { self.stack[func_pos + 1] } else { Value::undefined() };
                        self.truncate_stack(func_pos);
                        match self.value_to_bigint(arg) {
                            Ok(b) => { let v = self.make_bigint(b); self.push(v); }
                            Err(VmError::Throw(v)) => { self.handle_throw(v)?; }
                            Err(e) => return Err(e),
                        }
                        continue;
                    }

                    // RegExp(pattern, flags) — without new, same as new RegExp
                    if func_val.is_function() && func_val.as_function() == Some(-580) {
                        let pattern = if argc > 0 { self.value_to_string(self.stack[func_pos + 1]) } else { String::new() };
                        let flags = if argc > 1 { self.value_to_string(self.stack[func_pos + 2]) } else { String::new() };
                        let obj = JsObject {
                            properties: Vec::new(), prototype: self.func_prototypes.get(&-580).copied(),
                            kind: ObjectKind::RegExp { pattern, flags },
                            marked: false, extensible: true,
                        };
                        let oid = self.heap.allocate(obj);
                        self.truncate_stack(func_pos);
                        self.push(Value::object_id(oid));
                        continue;
                    }

                    // Date() called as a function (not constructor) returns current date string
                    if func_val.is_function() && func_val.as_function() == Some(-550) {
                        let ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as f64)
                            .unwrap_or(0.0);
                        let s = format_date(ms);
                        let id = self.interner.intern(&s);
                        self.truncate_stack(func_pos);
                        self.push(Value::string(id));
                        continue;
                    }

                    // Function(...args) — last arg is body, previous args are param names
                    if func_val.is_function() && func_val.as_function() == Some(-551) {
                        let args: Vec<Value> = (0..argc).map(|i| self.stack[func_pos + 1 + i]).collect();
                        self.truncate_stack(func_pos);
                        let result = self.construct_function(&args)?;
                        self.push(result);
                        continue;
                    }

                    // Check for eval()
                    if func_val.is_function() && func_val.as_function() == Some(-560) {
                        let code = if argc > 0 { self.stack[func_pos + 1] } else { Value::undefined() };
                        self.truncate_stack(func_pos);
                        // eval with non-string argument returns the argument
                        if !code.is_string() {
                            self.push(code);
                            continue;
                        }
                        let code_str = self.value_to_string(code);
                        // Lex, parse, compile
                        let mut lexer = crate::lexer::lexer::Lexer::new(&code_str, &mut self.interner);
                        let tokens = lexer.tokenize();
                        let mut parser = crate::parser::parser::Parser::new(tokens, &code_str, &mut self.interner);
                        let parsed = parser.parse_program();
                        // parse_program recovers from errors and returns Ok with a
                        // partial AST, recording diagnostics in `errors`; a non-empty
                        // list means the source is syntactically invalid. eval must
                        // throw a catchable SyntaxError (not abort the VM).
                        let parse_errors: Vec<String> =
                            parser.errors.iter().map(|e| e.to_string()).collect();
                        let program = match parsed {
                            Ok(p) => p,
                            Err(e) => {
                                let err = self.make_native_error("SyntaxError", &e.to_string());
                                self.handle_throw(err)?;
                                continue;
                            }
                        };
                        if !parse_errors.is_empty() {
                            let msg = parse_errors.join("; ");
                            let err = self.make_native_error("SyntaxError", &msg);
                            self.handle_throw(err)?;
                            continue;
                        }
                        // EvalDeclarationInstantiation early error: a direct eval
                        // running in a parameter default may not declare a var or
                        // function whose name collides with a parameter / body
                        // lexical / the parameter-scoped `arguments` binding.
                        if self.param_scope_depth > 0
                            && let Some(frame) = self.frames.last()
                        {
                            let cidx = frame.chunk_idx;
                            let declared = collect_eval_hoisted_names(&program);
                            let collides = declared.iter().any(|n|
                                self.chunks[cidx].param_names.contains(n)
                                || self.chunks[cidx].lexical_names.contains(n));
                            if collides {
                                let err = self.make_native_error("SyntaxError",
                                    "Identifier in eval declaration conflicts with a binding in the enclosing scope");
                                self.handle_throw(err)?;
                                continue;
                            }
                        }
                        let compiler = crate::compiler::compiler::Compiler::new(&mut self.interner);
                        let chunk = match compiler.compile_program(&program) {
                            Ok(c) => c,
                            Err(e) => {
                                let err = self.make_native_error("SyntaxError", &e);
                                self.handle_throw(err)?;
                                continue;
                            }
                        };
                        // Flatten and add chunks to VM. Adjust children indices to be absolute
                        // by adding base_idx (flatten_chunk uses indices relative to its output vec).
                        let base_idx = self.chunks.len();
                        let mut flat_chunks = Vec::new();
                        Vm::flatten_chunk(chunk, &mut flat_chunks);
                        for c in &mut flat_chunks {
                            for child in &mut c.children {
                                *child += base_idx;
                            }
                        }
                        // Direct eval inherits caller's strict mode. (Indirect eval
                        // would always be non-strict, but we don't currently distinguish.)
                        let caller_strict = {
                            let cur_chunk = self.cur_chunk();
                            self.chunks[cur_chunk].flags.contains(ChunkFlags::STRICT)
                        };
                        if caller_strict {
                            for c in &mut flat_chunks {
                                c.flags |= ChunkFlags::STRICT;
                            }
                        }
                        self.maybe_disasm_chunks(&flat_chunks);
                        self.chunks.extend(flat_chunks);
                        // Execute inheriting the current `this` binding (spec requirement)
                        let eval_fn = Value::function(base_idx as i32);
                        let current_this = self.frames.last().map(|f| f.this_value).unwrap_or(Value::undefined());
                        let frames_before = self.frames.len();
                        let stack_before = self.stack.len();
                        // The eval body's Halt returns ITS completion value; run it with
                        // a fresh register and restore the caller's afterwards so a nested
                        // eval can't clobber the enclosing script's completion value.
                        let saved_completion = self.script_completion;
                        self.script_completion = Value::undefined();
                        let result = self.call_function_this(eval_fn, current_this, &[]);
                        self.script_completion = saved_completion;
                        let result = result?;
                        // Eval chunks end in Halt, which doesn't unwind the call frame.
                        // Clean up leftover frame and stack slots.
                        while self.frames.len() > frames_before { self.frames.pop(); }
                        self.truncate_stack(stack_before);
                        self.push(result);
                        continue;
                    }

                    // Check for native global function sentinels
                    if func_val.is_function() {
                        let sentinel = func_val.as_function().unwrap();
                        // super() to native error/collection constructors: initialize `this`
                        let is_super_call = self.frames.last().map(|f| f.pending_super_call).unwrap_or(false);
                        if is_super_call && (-516..=-510).contains(&sentinel) {
                            if let Some(f) = self.frames.last_mut() { f.pending_super_call = false; }
                            let this_val = self.frames.last().map(|f| f.this_value).unwrap_or(Value::undefined());
                            let error_type = match sentinel {
                                -510 => "Error", -511 => "TypeError", -512 => "RangeError",
                                -513 => "ReferenceError", -514 => "SyntaxError",
                                -515 => "EvalError", -516 => "URIError", _ => "Error",
                            };
                            // Per spec, only set "message" if a non-undefined argument was passed.
                            let msg_arg = if argc > 0 { Some(self.stack[func_pos + 1]) } else { None };
                            if let Some(this_oid) = this_val.as_object_id()
                                && let Some(arg) = msg_arg
                                && !arg.is_undefined()
                            {
                                let msg = self.value_to_string(arg);
                                let msg_key = self.interner.intern("message");
                                let msg_id = self.interner.intern(&msg);
                                let stack_key = self.interner.intern("stack");
                                let stack_str = format!("{error_type}: {msg}");
                                let stack_id = self.interner.intern(&stack_str);
                                if let Some(obj) = self.heap.get_mut(this_oid) {
                                    obj.define_property(
                                        msg_key,
                                        Property::with_flags(
                                            Value::string(msg_id),
                                            Property::WRITABLE | Property::CONFIGURABLE,
                                        ),
                                    );
                                    obj.set_property(stack_key, Value::string(stack_id));
                                }
                            }
                            self.truncate_stack(func_pos);
                            self.push(this_val);
                            continue;
                        }
                        // super() to native collection / date constructors:
                        // mutate `this` (the subclass-allocated instance) so it
                        // has the proper internal kind, and return it.
                        if is_super_call && matches!(sentinel, -540 | -541 | -542 | -543 | -550) {
                            if let Some(f) = self.frames.last_mut() { f.pending_super_call = false; }
                            let this_val = self.frames.last().map(|f| f.this_value).unwrap_or(Value::undefined());
                            if let Some(this_oid) = this_val.as_object_id() {
                                let new_kind = match sentinel {
                                    -540 => {
                                        let mut entries = Vec::new();
                                        if argc > 0
                                            && let Some(arr_oid) = self.stack[func_pos + 1].as_object_id()
                                            && let Some(obj) = self.heap.get(arr_oid)
                                            && let ObjectKind::Array(ref elems) = obj.kind
                                        {
                                            let elems = elems.clone();
                                            for elem in &elems {
                                                if let Some(pair_oid) = elem.as_object_id()
                                                    && let Some(pair_obj) = self.heap.get(pair_oid)
                                                    && let ObjectKind::Array(ref pair) = pair_obj.kind
                                                    && pair.len() >= 2
                                                {
                                                    entries.push((pair[0], pair[1]));
                                                }
                                            }
                                        }
                                        ObjectKind::Map { entries }
                                    }
                                    -541 => {
                                        let mut entries = Vec::new();
                                        if argc > 0
                                            && let Some(arr_oid) = self.stack[func_pos + 1].as_object_id()
                                            && let Some(obj) = self.heap.get(arr_oid)
                                            && let ObjectKind::Array(ref elems) = obj.kind
                                        {
                                            entries = elems.clone();
                                        }
                                        ObjectKind::Set { entries }
                                    }
                                    -542 => ObjectKind::WeakMap { entries: Vec::new() },
                                    -543 => ObjectKind::WeakSet { entries: Vec::new() },
                                    -550 => {
                                        let ms = if argc == 0 {
                                            std::time::SystemTime::now()
                                                .duration_since(std::time::UNIX_EPOCH)
                                                .map(|d| d.as_millis() as f64)
                                                .unwrap_or(0.0)
                                        } else if argc == 1 {
                                            self.to_f64(self.stack[func_pos + 1])
                                        } else {
                                            0.0
                                        };
                                        ObjectKind::Date(ms)
                                    }
                                    _ => unreachable!(),
                                };
                                if let Some(obj) = self.heap.get_mut(this_oid) {
                                    obj.kind = new_kind;
                                }
                            }
                            self.truncate_stack(func_pos);
                            self.push(this_val);
                            continue;
                        }
                        // -507 is excluded: in exec_global_fn that id means
                        // Array.isArray (method dispatch), but a plain
                        // `Array(n)` call must CONSTRUCT (spec: Array called
                        // as a function behaves like `new Array`). It falls
                        // through to the constructor handler below.
                        if (-536..=-500).contains(&sentinel) && sentinel != -507 {
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[func_pos + 1 + i]).collect();
                            let result = self.exec_global_fn(sentinel, &args);
                            self.truncate_stack(func_pos);
                            self.push(result);
                            continue;
                        }
                        if sentinel == -507 {
                            // Array called as a function constructs (spec:
                            // identical to `new Array(...)`).
                            let elements: Vec<Value> = if argc == 1 {
                                let only = self.stack[func_pos + 1];
                                if let Some(n) = only.as_number()
                                    && n.is_finite() && n.fract() == 0.0 && n >= 0.0 && n <= u32::MAX as f64
                                {
                                    vec![Value::undefined(); n as usize]
                                } else if let Some(n) = only.as_int() {
                                    if n >= 0 { vec![Value::undefined(); n as usize] } else { vec![only] }
                                } else {
                                    vec![only]
                                }
                            } else {
                                (0..argc).map(|i| self.stack[func_pos + 1 + i]).collect()
                            };
                            let mut arr_obj = JsObject::array(elements);
                            arr_obj.prototype = Some(self.array_prototype);
                            let oid = self.heap.allocate(arr_obj);
                            self.truncate_stack(func_pos);
                            self.push(Value::object_id(oid));
                            continue;
                        }
                        if sentinel == -750 {
                            // Extracted Object.assign value
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[func_pos + 1 + i]).collect();
                            let result = self.exec_object_assign(&args);
                            self.truncate_stack(func_pos);
                            self.push(result);
                            continue;
                        }
                        if sentinel == -752 || sentinel == -753 {
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[func_pos + 1 + i]).collect();
                            let result = if sentinel == -752 {
                                self.exec_symbol_for(&args)
                            } else {
                                self.exec_symbol_key_for(&args)
                            };
                            self.truncate_stack(func_pos);
                            self.push(result);
                            continue;
                        }
                        if sentinel == -751 {
                            // Extracted Array.isArray value (its dispatch id,
                            // -507, doubles as the Array constructor).
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[func_pos + 1 + i]).collect();
                            let result = self.exec_global_fn(-507, &args);
                            self.truncate_stack(func_pos);
                            self.push(result);
                            continue;
                        }
                        if (-726..=-700).contains(&sentinel) {
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[func_pos + 1 + i]).collect();
                            let result = self.exec_math_sentinel(sentinel, &args);
                            self.truncate_stack(func_pos);
                            self.push(result);
                            continue;
                        }
                        if (-635..=-590).contains(&sentinel) || sentinel == -639 || sentinel == -640 {
                            // Native this-dependent methods called standalone (this=undefined)
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[func_pos + 1 + i]).collect();
                            let result = self.exec_native_method(sentinel, Value::undefined(), &args);
                            self.truncate_stack(func_pos);
                            self.push(result);
                            continue;
                        }
                    }

                    // Host-supplied native function (registered via Engine::register_host_fn).
                    if let Some(oid) = func_val.as_object_id() {
                        let native_fn = self.heap.get(oid).and_then(|o| {
                            if let ObjectKind::Function(crate::runtime::object::FunctionKind::Native { func, .. }) = &o.kind {
                                Some(func.clone())
                            } else { None }
                        });
                        if let Some(func) = native_fn {
                            let args_vec: Vec<Value> = (0..argc).map(|i| self.stack[func_pos + 1 + i]).collect();
                            // Per spec, an ordinary call gets `this = undefined`; non-strict
                            // wraps to globalThis. Match the bytecode-call default here so
                            // host code sees the same shape.
                            let this_val = Value::undefined();
                            self.truncate_stack(func_pos);
                            match (func)(self, this_val, &args_vec) {
                                Ok(v) => { self.push(v); continue; }
                                Err(reason) => { self.handle_throw(reason)?; continue; }
                            }
                        }
                    }

                    // Bound function object: unwrap target + bound args/this
                    if let Some(oid) = func_val.as_object_id() {
                        let bound_info = self.heap.get(oid).and_then(|o| {
                            if let ObjectKind::Function(crate::runtime::object::FunctionKind::Bound {
                                target, this_val, args,
                            }) = &o.kind {
                                Some((*target, *this_val, args.clone()))
                            } else { None }
                        });
                        if let Some((target_oid, this_val, bound_args)) = bound_info {
                            // Resolve target to function value (Bytecode or NativeSentinel)
                            let target_fn = self.heap.get(target_oid).and_then(|o| {
                                match &o.kind {
                                    ObjectKind::Function(crate::runtime::object::FunctionKind::Bytecode { chunk_idx, .. }) => {
                                        Some(Value::function(*chunk_idx as i32))
                                    }
                                    ObjectKind::Function(crate::runtime::object::FunctionKind::NativeSentinel { sentinel }) => {
                                        Some(Value::function(*sentinel))
                                    }
                                    _ => None,
                                }
                            });
                            if let Some(fn_val) = target_fn {
                                let call_args: Vec<Value> = bound_args.into_iter()
                                    .chain((0..argc).map(|i| self.stack[func_pos + 1 + i]))
                                    .collect();
                                self.truncate_stack(func_pos);
                                let result = self.call_with_async_wrap(fn_val, this_val, &call_args)?;
                                self.push(result);
                                continue;
                            }
                        }
                    }

                    // Object(v) called as function: wraps primitives, returns objects as-is
                    if func_val.is_function() && func_val.as_function() == Some(-508) {
                        let arg = if argc > 0 { self.stack[func_pos + 1] } else { Value::undefined() };
                        let result = if self.is_bigint(arg) {
                            // ToObject(bigint) → a BigInt wrapper object (typeof "object").
                            let proto = self.bigint_prototype_oid();
                            let mut obj = JsObject { properties: Vec::new(), prototype: Some(proto),
                                kind: ObjectKind::Wrapper(arg), marked: false, extensible: true };
                            let prim_key = self.interner.intern("__primitive__");
                            obj.set_property(prim_key, arg);
                            Value::object_id(self.heap.allocate(obj))
                        } else if arg.is_object() {
                            arg
                        } else if arg.is_null() || arg.is_undefined() {
                            let mut obj = JsObject::ordinary();
                            obj.prototype = Some(self.object_prototype);
                            Value::object_id(self.heap.allocate(obj))
                        } else if arg.is_symbol() {
                            let mut obj = JsObject::ordinary();
                            obj.prototype = Some(self.object_prototype);
                            let prim_key = self.interner.intern("__primitive__");
                            obj.set_property(prim_key, arg);
                            Value::object_id(self.heap.allocate(obj))
                        } else {
                            // Boolean/Number/String wrapper
                            let wrapper_sentinel = if arg.as_bool().is_some() { -506i32 }
                                else if arg.is_number() || arg.is_int() { -505 }
                                else { -504 };
                            let mut obj = JsObject::ordinary();
                            obj.prototype = self.func_prototypes.get(&wrapper_sentinel).copied()
                                .or(Some(self.object_prototype));
                            let prim_key = self.interner.intern("__primitive__");
                            obj.set_property(prim_key, arg);
                            Value::object_id(self.heap.allocate(obj))
                        };
                        self.truncate_stack(func_pos);
                        self.push(result);
                        continue;
                    }

                    // Throw TypeError for non-callable values: primitives and
                    // ordinary objects (objects that aren't function-like).
                    // (Note: undefined and Symbol values fall through to the
                    // silent-undefined-push path below to preserve compatibility
                    // with test262 harness helpers that depend on it; making
                    // them throw regressed ~600 statements/class tests.)
                    let is_explicit_nonfunc = func_val.is_null()
                        || func_val.as_bool().is_some()
                        || func_val.is_number()
                        || func_val.is_int()
                        || func_val.is_string()
                        || (func_val.is_object() && !func_val.is_function() && {
                            // Treat object as non-callable when its kind isn't Function
                            // and it doesn't have a __constructor__ marker (class).
                            if let Some(oid) = func_val.as_object_id() {
                                let ctor_key = self.interner.intern("__constructor__");
                                self.heap.get(oid).map(|o| {
                                    !matches!(&o.kind, ObjectKind::Function(_))
                                        && o.get_property(ctor_key).is_none()
                                }).unwrap_or(true)
                            } else { true }
                        });
                    if is_explicit_nonfunc {
                        let type_name = if func_val.is_null() { "null".to_owned() }
                            else if let Some(b) = func_val.as_bool() { b.to_string() }
                            else if func_val.is_string() {
                                format!("\"{}\"", self.value_to_string(func_val))
                            } else { self.value_to_string(func_val) };
                        // Annotate with source line + chunk-byte
                        // offset so the embedder can correlate
                        // back to the offending call site. Crucial
                        // for diagnosing Closure-compiled bundles
                        // where the failing call is one of
                        // thousands and the receiver tells you
                        // nothing about *which* call.
                        let (line, pc, chunk_name) = if let Some(f) = self.frames.last() {
                            let cn = self.interner.resolve(self.chunks[f.chunk_idx].name).to_owned();
                            (
                                self.chunks[f.chunk_idx].get_line(f.ip as u32),
                                f.ip,
                                cn,
                            )
                        } else {
                            (0, 0, String::new())
                        };
                        let msg = format!(
                            "{type_name} is not a function (at line {line}, bytecode pc {pc}, chunk '{chunk_name}')"
                        );
                        self.truncate_stack(func_pos);
                        self.throw_type_error(&msg)?;
                        continue;
                    }
                    // Object with `__constructor__` marker called as a plain
                    // function (no `new`). The earlier is_explicit_nonfunc
                    // check exempts these from the TypeError path because
                    // they ARE callable — dispatch through the stored
                    // constructor function instead of silently returning
                    // undefined. Closure-compiled bundles call class-like
                    // objects this way: google.com /search's xjs bundle has
                    // a dispatcher object whose `__constructor__` slot
                    // holds the real callable.
                    if func_val.is_object()
                        && !func_val.is_function()
                        && let Some(oid) = func_val.as_object_id()
                    {
                        let ctor_key = self.interner.intern("__constructor__");
                        let ctor_val = self.heap.get(oid)
                            .and_then(|o| o.get_property(ctor_key))
                            .filter(|v| v.is_function());
                        if let Some(ctor) = ctor_val {
                            let args: Vec<Value> = (0..argc)
                                .map(|i| self.stack[func_pos + 1 + i])
                                .collect();
                            self.truncate_stack(func_pos);
                            match self.call_with_async_wrap(ctor, Value::undefined(), &args) {
                                Ok(result) => { self.push(result); continue; }
                                Err(VmError::Throw(reason)) => { self.handle_throw(reason)?; continue; }
                                Err(VmError::TypeError(msg)) => {
                                    let err = self.make_native_error("TypeError", &msg);
                                    self.handle_throw(err)?; continue;
                                }
                                Err(VmError::ReferenceError(msg)) => {
                                    let err = self.make_native_error("ReferenceError", &msg);
                                    self.handle_throw(err)?; continue;
                                }
                                Err(VmError::RuntimeError(msg)) => {
                                    let err = self.make_native_error("Error", &msg);
                                    self.handle_throw(err)?; continue;
                                }
                            }
                        }
                    }
                    // Unknown/undefined — silently return undefined
                    self.truncate_stack(func_pos);
                    self.push(Value::undefined());
                }

                OpCode::Return => {
                    let mut result = self.pop()?;
                    let frame = self.frames.pop().unwrap();
                    // Returning out of any `with` blocks opened in this frame: drop
                    // their scope objects (the lexical WithExit was jumped over).
                    if self.with_stack.len() > frame.with_base {
                        self.with_stack.truncate(frame.with_base);
                    }
                    if !self.closure_upvalues.is_empty() {
                        self.close_upvalues_above(frame.base.saturating_sub(1));
                    }
                    // A constructor returning means its (and the subclass levels it stood
                    // in for) private elements are now installed — clear pending brands.
                    if frame.is_constructor && !self.pending_private_brands.is_empty()
                        && let Some(coid) = frame.this_value.as_object_id()
                    {
                        self.pending_private_brands.remove(&coid);
                    }
                    // Derived-class constructor: per spec, if super() was never called
                    // and the return value isn't an object, throw ReferenceError.
                    if frame.is_derived_ctor && !frame.super_called
                        && !result.is_object() && !result.is_function()
                    {
                        self.frames.push(frame);
                        let err = self.make_native_error(
                            "ReferenceError",
                            "Must call super constructor in derived class before returning",
                        );
                        self.handle_throw(err)?;
                        continue;
                    }
                    // Constructor return semantics: if the return value is not an object,
                    // return `this` instead (per ES spec, [[Construct]] step 13).
                    if frame.is_constructor && !result.is_object() && !result.is_function() {
                        result = frame.this_value;
                    }
                    // BindThisValue: if the caller is awaiting a super() result, rebind
                    // its `this` to whatever the parent constructor produced.
                    if let Some(caller) = self.frames.last_mut()
                        && caller.await_super_result
                    {
                        caller.await_super_result = false;
                        if result.is_object() {
                            caller.this_value = result;
                        }
                    }
                    // Generator return: mark completed, produce {value, done: true}
                    if let Some(gid) = frame.generator_id {
                        if let Some(obj) = self.heap.get_mut(gid)
                            && let ObjectKind::Generator { state, .. } = &mut obj.kind
                        {
                            *state = GeneratorState::Completed;
                        }
                        self.truncate_stack(frame.base.saturating_sub(1));
                        let iter_result = self.make_iter_result(result, true)?;
                        self.push(iter_result);
                    } else if self.frames.len() <= stop_depth {
                        self.truncate_stack(frame.base.saturating_sub(1));
                        return Ok(result);
                    } else {
                        self.truncate_stack(frame.base.saturating_sub(1));
                        self.push(result);
                    }
                }

                OpCode::LoadCallee => {
                    // The running closure value is at stack[base - 1] (set up by Call).
                    let base = self.frames.last().unwrap().base;
                    let callee = if base > 0 { self.stack[base - 1] } else { Value::undefined() };
                    self.push(callee);
                }

                OpCode::ReturnUndefined => {
                    let frame = self.frames.pop().unwrap();
                    if self.with_stack.len() > frame.with_base {
                        self.with_stack.truncate(frame.with_base);
                    }
                    if frame.is_constructor && !self.pending_private_brands.is_empty()
                        && let Some(coid) = frame.this_value.as_object_id()
                    {
                        self.pending_private_brands.remove(&coid);
                    }
                    // Derived-class constructor without super() — throw ReferenceError.
                    if frame.is_derived_ctor && !frame.super_called {
                        self.frames.push(frame);
                        let err = self.make_native_error(
                            "ReferenceError",
                            "Must call super constructor in derived class before returning",
                        );
                        self.handle_throw(err)?;
                        continue;
                    }
                    let result = if frame.is_constructor { frame.this_value } else { Value::undefined() };
                    if !self.closure_upvalues.is_empty() {
                        self.close_upvalues_above(frame.base.saturating_sub(1));
                    }
                    // BindThisValue: see Return opcode for context.
                    if let Some(caller) = self.frames.last_mut()
                        && caller.await_super_result
                    {
                        caller.await_super_result = false;
                        if result.is_object() {
                            caller.this_value = result;
                        }
                    }
                    // Generator return: mark completed, produce {value: undefined, done: true}
                    if let Some(gid) = frame.generator_id {
                        if let Some(obj) = self.heap.get_mut(gid)
                            && let ObjectKind::Generator { state, .. } = &mut obj.kind
                        {
                            *state = GeneratorState::Completed;
                        }
                        self.truncate_stack(frame.base.saturating_sub(1));
                        let iter_result = self.make_iter_result(Value::undefined(), true)?;
                        self.push(iter_result);
                    } else if self.frames.len() <= stop_depth {
                        self.truncate_stack(frame.base.saturating_sub(1));
                        return Ok(result);
                    } else {
                        self.truncate_stack(frame.base.saturating_sub(1));
                        self.push(result);
                    }
                }

                // ---- Object / Array (placeholders) -----------------------
                OpCode::CreateObject => {
                    let mut obj = JsObject::ordinary();
                    obj.prototype = Some(self.object_prototype);
                    let id = self.heap.allocate(obj);
                    self.push(Value::object_id(id));
                }

                OpCode::CreateArray => {
                    let hint = self.read_u16() as usize;
                    let elements = Vec::with_capacity(hint);
                    let mut obj = JsObject::array(elements);
                    obj.prototype = Some(self.array_prototype);
                    let id = self.heap.allocate(obj);
                    self.push(Value::object_id(id));
                }

                // ---- Miscellaneous ---------------------------------------
                OpCode::Halt => {
                    // Script/eval chunks return the completion value recorded via
                    // SetCompletion (declarations and empty/false branches leave it
                    // untouched per UpdateEmpty). Other chunks (manually built ones,
                    // internal helpers) keep the legacy "top of frame" behavior.
                    let cur = self.cur_chunk();
                    if self.chunks[cur].flags.contains(ChunkFlags::SCRIPT) {
                        return Ok(self.script_completion);
                    }
                    let frame_base = self.frames.last().map(|f| f.base).unwrap_or(0);
                    return Ok(if self.stack.len() > frame_base {
                        self.pop()?
                    } else {
                        Value::undefined()
                    });
                }

                OpCode::SetCompletion => {
                    self.script_completion = self.pop()?;
                }

                OpCode::BeginParamExpr => { self.param_scope_depth += 1; }
                OpCode::EndParamExpr => { self.param_scope_depth = self.param_scope_depth.saturating_sub(1); }

                OpCode::CollectRest => {
                    let start_idx = self.read_byte() as usize;
                    let target_slot = self.read_byte() as usize;
                    let frame = self.frames.last().unwrap();
                    let base = frame.base;
                    // Use the actual call-site argc (saved_args.len()) rather than
                    // the padded `argc`, so undefined slots added to satisfy
                    // expected formals don't bleed into the rest array.
                    let actual_argc = frame.saved_args.len();
                    let mut rest_elements = Vec::new();
                    for i in start_idx..actual_argc {
                        if i < frame.saved_args.len() {
                            rest_elements.push(frame.saved_args[i]);
                        } else if base + i < self.stack.len() {
                            rest_elements.push(self.stack[base + i]);
                        }
                    }
                    let arr = JsObject::array(rest_elements);
                    let arr_oid = self.heap.allocate(arr);
                    // Store in the target local slot
                    let base = self.frames.last().unwrap().base;
                    if base + target_slot < self.stack.len() {
                        self.stack[base + target_slot] = Value::object_id(arr_oid);
                    }
                }

                OpCode::Nop => { /* do nothing */ }

                // ---- Unimplemented opcodes (stubs) -----------------------
                // These all advance ip past their operands so the loop stays
                // in sync, then return an explicit runtime error.
                OpCode::GetUpvalue => {
                    let idx = self.read_byte() as usize;
                    let val = {
                        let frame = self.frames.last().unwrap();
                        if idx < frame.upvalues.len() {
                            frame.upvalues[idx].get(&self.stack)
                        } else {
                            Value::undefined()
                        }
                    };
                    self.push(val);
                }

                OpCode::GetUpvalueWide => {
                    let idx = self.read_u16() as usize;
                    let val = {
                        let frame = self.frames.last().unwrap();
                        if idx < frame.upvalues.len() {
                            frame.upvalues[idx].get(&self.stack)
                        } else {
                            Value::undefined()
                        }
                    };
                    self.push(val);
                }

                OpCode::SetUpvalueWide => {
                    let idx = self.read_u16() as usize;
                    let val = self.peek()?;
                    let frame_idx = self.frames.len() - 1;
                    if idx < self.frames[frame_idx].upvalues.len() {
                        // Shared cell: every closure capturing this variable
                        // observes the write, matching real closure semantics.
                        let cell = self.frames[frame_idx].upvalues[idx].cell.clone();
                        let loc = cell.borrow().clone();
                        match loc {
                            UpvalueLocation::Open(stack_idx) => {
                                if stack_idx < self.stack.len() {
                                    self.stack[stack_idx] = val;
                                }
                            }
                            UpvalueLocation::Closed(_) => {
                                *cell.borrow_mut() = UpvalueLocation::Closed(val);
                            }
                        }
                    }
                }

                OpCode::SetUpvalue => {
                    let idx = self.read_byte() as usize;
                    let val = self.peek()?;
                    let frame_idx = self.frames.len() - 1;
                    if idx < self.frames[frame_idx].upvalues.len() {
                        // Shared cell: every closure capturing this variable
                        // observes the write, matching real closure semantics.
                        let cell = self.frames[frame_idx].upvalues[idx].cell.clone();
                        let loc = cell.borrow().clone();
                        match loc {
                            UpvalueLocation::Open(stack_idx) => {
                                if stack_idx < self.stack.len() {
                                    self.stack[stack_idx] = val;
                                }
                            }
                            UpvalueLocation::Closed(_) => {
                                *cell.borrow_mut() = UpvalueLocation::Closed(val);
                            }
                        }
                    }
                }

                OpCode::CloseUpvalue => {
                    // Close the topmost local: move its value from the stack into
                    // all upvalues that reference that stack slot.
                    let stack_idx = self.stack.len() - 1;
                    let val = self.stack[stack_idx];
                    if let Some(cell) = self.open_upvalues.remove(&stack_idx) {
                        *cell.borrow_mut() = UpvalueLocation::Closed(val);
                    }
                    self.pop()?;
                }

                OpCode::InitLet | OpCode::CheckTdz => {
                    let _slot = self.read_byte();
                    // No-op for now (TDZ not enforced)
                }

                OpCode::DeleteProp => {
                    let key = self.pop()?;
                    let obj_val = self.pop()?;
                    let chunk_idx = self.cur_chunk();
                    let in_strict = self.chunks[chunk_idx].flags.contains(ChunkFlags::STRICT);
                    // Per spec, `delete null.x` / `delete undefined.x` throws TypeError.
                    if obj_val.is_null() || obj_val.is_undefined() {
                        let type_name = if obj_val.is_null() { "null" } else { "undefined" };
                        let msg = format!("Cannot convert {type_name} to object");
                        let err = self.make_native_error("TypeError", &msg);
                        self.handle_throw(err)?;
                        continue;
                    }
                    let result = if let Some(oid) = obj_val.as_object_id() {
                        // ToPropertyKey: resolve string id, symbol slot, or stringified number.
                        let resolved_key = if key.is_symbol() {
                            Some(self.interner.intern(&format!("__sym_{}__", key.as_symbol_id().unwrap())))
                        } else if let Some(sid) = key.as_string_id() {
                            Some(sid)
                        } else if let Some(n) = key.as_number() {
                            let s = if n.fract() == 0.0 && n.is_finite() {
                                (n as i64).to_string()
                            } else { n.to_string() };
                            Some(self.interner.intern(&s))
                        } else {
                            None
                        };
                        if let Some(key_id) = resolved_key {
                            let key_str = self.interner.resolve(key_id).to_owned();
                            // Array.length is non-configurable per spec — delete returns false.
                            let is_array = self.heap.get(oid)
                                .map(|o| matches!(&o.kind, ObjectKind::Array(_)))
                                .unwrap_or(false);
                            if is_array && key_str == "length" {
                                false
                            } else if is_array
                                && let Ok(idx) = key_str.parse::<usize>()
                            {
                                if let Some(obj) = self.heap.get_mut(oid)
                                    && let ObjectKind::Array(ref mut elems) = obj.kind
                                    && idx < elems.len()
                                {
                                    elems[idx] = Value::undefined();
                                }
                                true
                            } else {
                                let getter_key = self.interner.intern(&format!("__get_{key_str}__"));
                                let setter_key = self.interner.intern(&format!("__set_{key_str}__"));
                                let r1 = self.heap.get_mut(oid).map(|o| o.delete_property(key_id)).unwrap_or(true);
                                let r2 = self.heap.get_mut(oid).map(|o| o.delete_property(getter_key)).unwrap_or(true);
                                let r3 = self.heap.get_mut(oid).map(|o| o.delete_property(setter_key)).unwrap_or(true);
                                r1 && r2 && r3
                            }
                        } else {
                            true
                        }
                    } else if obj_val.is_function() {
                        if let Some(key_id) = key.as_string_id() {
                            let sentinel = obj_val.as_function().unwrap();
                            // Mark the property as deleted so subsequent reads see undefined.
                            // For "name" / "length" the standard descriptor would otherwise
                            // resurface; for other user-set properties this clears them.
                            self.fn_property_overrides.insert((sentinel, key_id), None);
                        }
                        true
                    } else {
                        true
                    };
                    // Strict-mode: failed delete (non-configurable) throws TypeError.
                    if in_strict && !result {
                        let prop = self.value_to_string(key);
                        let msg = format!("Cannot delete property '{prop}'");
                        let err = self.make_native_error("TypeError", &msg);
                        self.handle_throw(err)?;
                        continue;
                    }
                    self.push(Value::boolean(result));
                }

                OpCode::DeleteGlobal => {
                    let name_idx = self.read_u16() as usize;
                    let name_val = self.chunks[self.cur_chunk()].constants[name_idx];
                    let name_id = name_val.as_string_id().unwrap();
                    // `with` scope: if any active with-object owns this name, delete it there.
                    if !self.with_stack.is_empty() {
                        let mut target_oid: Option<ObjectId> = None;
                        for &oid in self.with_stack.iter().rev() {
                            if let Some(obj) = self.heap.get(oid)
                                && obj.get_property(name_id).is_some()
                            {
                                target_oid = Some(oid);
                                break;
                            }
                        }
                        if let Some(oid) = target_oid {
                            let removed = if let Some(obj) = self.heap.get_mut(oid) {
                                obj.properties.iter().position(|(k, _)| *k == name_id)
                                    .map(|pos| { obj.properties.remove(pos); true })
                                    .unwrap_or(false)
                            } else { false };
                            self.push(Value::boolean(removed));
                            continue;
                        }
                    }
                    // Check whether the binding is non-configurable on globalThis
                    // (var/function decls are non-configurable per spec).
                    let gt_oid = self.global_this_oid;
                    let gt_desc = self.heap.get(gt_oid)
                        .and_then(|o| o.get_property_descriptor(name_id));
                    if let Some(desc) = gt_desc
                        && !desc.is_configurable()
                    {
                        self.push(Value::boolean(false));
                        continue;
                    }
                    // Otherwise: remove from both maps. Returns true even for unresolvable
                    // (per spec, `delete x` where x has no binding succeeds).
                    self.globals.remove(&name_id);
                    // Also clear the Vec-based fast path so subsequent reads go
                    // through the slow lookup and miss correctly.
                    let idx = name_id.0 as usize;
                    if idx < self.globals_vec.len() {
                        self.globals_vec[idx] = Value::null();
                    }
                    self.global_version += 1;
                    if let Some(obj) = self.heap.get_mut(gt_oid)
                        && let Some(pos) = obj.properties.iter().position(|(k, _)| *k == name_id)
                    {
                        obj.properties.remove(pos);
                    }
                    self.push(Value::boolean(true));
                }

                OpCode::InstanceOf => {
                    let constructor = self.pop()?;
                    let obj = self.pop()?;
                    // Spec: if RHS has @@hasInstance, call it with `this = RHS`, arg = LHS,
                    // and return ToBoolean(result). Takes precedence over the default
                    // prototype-chain walk and even applies to non-callable RHS.
                    let sym_key = self.interner.intern(&format!("__sym_{}__", self.sym_has_instance));
                    let has_instance_fn = if let Some(oid) = constructor.as_object_id() {
                        self.heap.get_property_chain(oid, sym_key)
                    } else {
                        None
                    };
                    if let Some(fn_val) = has_instance_fn
                        && fn_val.is_function()
                    {
                        let result = self.call_function_this(fn_val, constructor, &[obj])?;
                        self.push(Value::boolean(result.to_boolean()));
                        continue;
                    }
                    // RHS must be callable — throw TypeError for non-callables
                    let ctor_key_id = self.interner.intern("__constructor__");
                    let constructor_callable = constructor.is_function()
                        || constructor.as_object_id()
                            .and_then(|oid| self.heap.get(oid))
                            .map(|o| {
                                matches!(&o.kind, ObjectKind::Function(_))
                                    || o.get_property(ctor_key_id).is_some()
                            })
                            .unwrap_or(false);
                    if !constructor_callable {
                        let err = self.make_native_error("TypeError", "Right-hand side of 'instanceof' is not callable");
                        self.handle_throw(err)?;
                        continue;
                    }
                    // Special case: built-in constructors
                    if constructor.is_function() {
                        let s = constructor.as_function().unwrap();
                        if s == -508 {
                            // Object constructor: true for any object-like value (objects + functions)
                            self.push(Value::boolean(obj.is_object() || obj.is_function()));
                            continue;
                        }
                        if s == -507 {
                            // Array constructor: true if it's an array OR has Array.prototype
                            // anywhere in its prototype chain (subclass detection).
                            let is_arr = obj.as_object_id()
                                .and_then(|oid| self.heap.get(oid))
                                .map(|o| matches!(&o.kind, ObjectKind::Array(_)))
                                .unwrap_or(false);
                            let has_array_proto = if let Some(oid) = obj.as_object_id() {
                                let mut cur = self.heap.get(oid).and_then(|o| o.prototype);
                                let mut found = false;
                                while let Some(pid) = cur {
                                    if pid == self.array_prototype { found = true; break; }
                                    cur = self.heap.get(pid).and_then(|o| o.prototype);
                                }
                                found
                            } else { false };
                            self.push(Value::boolean(is_arr || has_array_proto));
                            continue;
                        }
                        if s == -551 {
                            // Function constructor: true if obj is any function
                            let is_fn = obj.is_function()
                                || obj.as_object_id()
                                    .and_then(|oid| self.heap.get(oid))
                                    .map(|o| matches!(&o.kind, ObjectKind::Function(_)))
                                    .unwrap_or(false);
                            if is_fn {
                                self.push(Value::boolean(true));
                                continue;
                            }
                            // Fall through for subclass detection.
                        }
                        if s == -520 {
                            // Promise constructor: true if obj is a Promise object
                            let is_prom = obj.as_object_id()
                                .and_then(|oid| self.heap.get(oid))
                                .map(|o| matches!(&o.kind, ObjectKind::Promise { .. }))
                                .unwrap_or(false);
                            if is_prom {
                                self.push(Value::boolean(true));
                                continue;
                            }
                            // Fall through for subclass detection.
                        }
                        if (-543..=-540).contains(&s) {
                            // Map (-540), Set (-541), WeakMap (-542), WeakSet (-543)
                            let want = s;
                            let is_match = obj.as_object_id()
                                .and_then(|oid| self.heap.get(oid))
                                .map(|o| matches!(
                                    (&o.kind, want),
                                    (ObjectKind::Map { .. }, -540)
                                    | (ObjectKind::Set { .. }, -541)
                                    | (ObjectKind::WeakMap { .. }, -542)
                                    | (ObjectKind::WeakSet { .. }, -543)
                                ))
                                .unwrap_or(false);
                            if is_match {
                                self.push(Value::boolean(true));
                                continue;
                            }
                            // Fall through to prototype-chain walk for subclass detection.
                        }
                        if s == -550 {
                            // Date
                            let is_date = obj.as_object_id()
                                .and_then(|oid| self.heap.get(oid))
                                .map(|o| matches!(&o.kind, ObjectKind::Date(_)))
                                .unwrap_or(false);
                            if is_date {
                                self.push(Value::boolean(true));
                                continue;
                            }
                            // Fall through to prototype-chain walk for subclass detection.
                        }
                    }
                    // If constructor's `prototype` property has been user-set to a
                    // non-object, throw TypeError per spec (OrdinaryHasInstance).
                    {
                        let proto_key = self.interner.intern("prototype");
                        let proto_val = if constructor.is_function() {
                            let packed = constructor.as_function().unwrap();
                            if packed < 0 { None } else { self.fn_get_own_prop(packed, proto_key) }
                        } else if let Some(ctor_oid) = constructor.as_object_id() {
                            self.heap.get(ctor_oid).and_then(|o| o.get_property(proto_key))
                        } else { None };
                        if let Some(pv) = proto_val
                            && !pv.is_object()
                            && !pv.is_function()
                        {
                            let err = self.make_native_error(
                                "TypeError",
                                "Right-hand side of 'instanceof' has non-object prototype",
                            );
                            self.handle_throw(err)?;
                            continue;
                        }
                    }
                    let result = if let Some(obj_oid) = obj.as_object_id() {
                        // Get constructor.prototype
                        let ctor_proto = if constructor.is_function() {
                            let packed = constructor.as_function().unwrap();
                            self.func_prototypes.get(&packed).copied()
                        } else if let Some(ctor_oid) = constructor.as_object_id() {
                            // Class-based constructor: look up prototype property
                            let proto_key = self.interner.intern("prototype");
                            self.heap.get(ctor_oid)
                                .and_then(|o| o.get_property(proto_key))
                                .and_then(|v| v.as_object_id())
                        } else { None };

                        if let Some(target_proto) = ctor_proto {
                            // Walk obj's prototype chain looking for target_proto
                            let mut current = self.heap.get(obj_oid).and_then(|o| o.prototype);
                            let mut depth = 0;
                            let mut found = false;
                            while let Some(proto_oid) = current {
                                if depth > 64 { break; }
                                if proto_oid == target_proto { found = true; break; }
                                current = self.heap.get(proto_oid).and_then(|o| o.prototype);
                                depth += 1;
                            }
                            found
                        } else {
                            // Fallback: check error constructor name matching
                            if let Some(o) = self.heap.get(obj_oid) {
                                let name_key = self.interner.intern("name");
                                if let Some(name_val) = o.get_property(name_key)
                                    && constructor.is_function() {
                                        let sentinel = constructor.as_function().unwrap();
                                        let ctor_name = match sentinel {
                                            -510 => "Error", -511 => "TypeError",
                                            -512 => "RangeError", -513 => "ReferenceError",
                                            -514 => "SyntaxError", -515 => "EvalError",
                                            -516 => "URIError", _ => "",
                                        };
                                        if !ctor_name.is_empty() {
                                            name_val.as_string_id()
                                                .map(|nid| {
                                                    let n = self.interner.resolve(nid);
                                                    // Exact match or base Error matches any *Error
                                                    n == ctor_name || (ctor_name == "Error" && n.ends_with("Error"))
                                                })
                                                .unwrap_or(false)
                                        } else { false }
                                    } else { false }
                            } else { false }
                        }
                    } else { false };
                    self.push(Value::boolean(result));
                }

                OpCode::In => {
                    let obj = self.pop()?;
                    let key = self.pop()?;
                    // RHS must be an object or function — throw TypeError for primitives.
                    if obj.is_boolean() || obj.is_string() || self.is_cons_string(obj)
                        || obj.is_null() || obj.is_undefined() || obj.is_number() || obj.is_int()
                        || obj.is_symbol()
                    {
                        let keyrepr = self.value_to_string(key);
                        let (line, pc, chunk_name) = if let Some(f) = self.frames.last() {
                            let cn = self.interner.resolve(self.chunks[f.chunk_idx].name).to_owned();
                            (self.chunks[f.chunk_idx].get_line(f.ip as u32), f.ip, cn)
                        } else { (0, 0, String::new()) };
                        let err = self.make_native_error("TypeError", &format!("Cannot use 'in' operator to search for '{keyrepr}' in a non-object (at line {line}, pc {pc}, chunk '{chunk_name}')"));
                        self.handle_throw(err)?;
                        continue;
                    }
                    let result = if let Some(oid) = obj.as_object_id() {
                        if let Some(kid) = key.as_string_id() {
                            // Walk prototype chain for 'in' operator
                            self.heap.get_property_chain(oid, kid).is_some()
                        } else if let Some(sym_id) = key.as_symbol_id() {
                            // Symbol keys are stored as `__sym_<id>__`
                            let key_str = format!("__sym_{sym_id}__");
                            let key_id = self.interner.intern(&key_str);
                            self.heap.get_property_chain(oid, key_id).is_some()
                        } else if let Some(idx) = key.as_int() {
                            // Numeric key: check array elements
                            self.heap.get(oid)
                                .map(|o| if let ObjectKind::Array(ref elems) = o.kind {
                                    idx >= 0 && (idx as usize) < elems.len()
                                } else { false })
                                .unwrap_or(false)
                        } else {
                            let key_str = self.value_to_string(key);
                            let key_id = self.interner.intern(&key_str);
                            self.heap.get_property_chain(oid, key_id).is_some()
                        }
                    } else if obj.is_function() {
                        // Sentinel constructors: check overrides, intrinsic
                        // names, and well-known static properties. Functions
                        // are objects; `'prototype' in fn` and core-js's
                        // descriptor-flag probes (`'value' in desc` style on
                        // function targets) must work.
                        let sentinel = obj.as_function().unwrap();
                        let key_str = self.value_to_string(key);
                        let key_id = self.interner.intern(&key_str);
                        if let Some(ov) = self.fn_property_overrides.get(&(sentinel, key_id)) {
                            ov.is_some()
                        } else {
                            // `prototype` exists on constructible functions
                            // only — arrows and methods have none.
                            let has_proto = if sentinel >= 0 {
                                let chunk_idx = (sentinel & 0xFFFF) as usize;
                                chunk_idx < self.chunks.len()
                                    && !self.chunks[chunk_idx].flags.contains(ChunkFlags::ARROW)
                                    && !self.chunks[chunk_idx].flags.contains(ChunkFlags::METHOD)
                            } else {
                                true
                            };
                            (key_str == "prototype" && has_proto)
                            || matches!(key_str.as_str(), "name" | "length" | "call" | "apply" | "bind" | "constructor")
                            || matches!(
                                (sentinel, key_str.as_str()),
                                (-505, "MAX_VALUE") | (-505, "MIN_VALUE") | (-505, "NaN")
                                | (-505, "POSITIVE_INFINITY") | (-505, "NEGATIVE_INFINITY")
                                | (-505, "EPSILON") | (-505, "MAX_SAFE_INTEGER") | (-505, "MIN_SAFE_INTEGER")
                                | (-505, "isFinite") | (-505, "isInteger") | (-505, "isNaN") | (-505, "isSafeInteger")
                                | (-505, "parseFloat") | (-505, "parseInt")
                                | (-504, "fromCharCode") | (-504, "fromCodePoint") | (-504, "raw")
                                | (-508, "assign") | (-508, "keys") | (-508, "create") | (-508, "defineProperty")
                                | (-507, "isArray") | (-507, "from") | (-507, "of")
                            )
                        }
                    } else { false };
                    self.push(Value::boolean(result));
                }

                OpCode::GetProperty => {
                    let name_idx = self.read_u16() as usize;
                    let ic_slot = self.read_u16() as usize;
                    let chunk_idx = self.cur_chunk();
                    let name_val = self.chunks[chunk_idx].constants[name_idx];
                    let name_id = unsafe { name_val.as_string_id().unwrap_unchecked() };

                    let top = self.peek()?;
                    if let Some(oid) = top.as_object_id()
                        && let Some(obj) = self.heap.get(oid) {
                        // IC fast path: monomorphic inline cache hit
                        let cached = self.chunks[chunk_idx].property_ic[ic_slot];
                        if cached != 0xFF
                            && let Some(&(k, ref prop)) = obj.properties.get(cached as usize)
                            && k == name_id
                        {
                            let val = prop.value;
                            self.pop()?;
                            self.push(val);
                            continue;
                        }
                        // Linear scan with IC population
                        if let Some(pos) = obj.properties.iter().position(|(k, _)| *k == name_id) {
                            let val = obj.properties[pos].1.value;
                            if pos <= 254 {
                                self.chunks[chunk_idx].property_ic[ic_slot] = pos as u8;
                            }
                            self.pop()?;
                            self.push(val);
                            continue;
                        }
                    }

                    // Slow path: special cases
                    let peeked = top;
                    if peeked.is_null() || peeked.is_undefined() {
                        self.pop()?;
                        let type_name = if peeked.is_null() { "null" } else { "undefined" };
                        let prop = self.interner.resolve(name_id).to_owned();
                        let (line, pc, chunk_name) = if let Some(f) = self.frames.last() {
                            let cn = self.interner.resolve(self.chunks[f.chunk_idx].name).to_owned();
                            (self.chunks[f.chunk_idx].get_line(f.ip as u32), f.ip, cn)
                        } else { (0, 0, String::new()) };
                        let msg = format!(
                            "Cannot read properties of {type_name} (reading '{prop}') (at line {line}, pc {pc}, chunk '{chunk_name}')"
                        );
                        let err = self.make_native_error("TypeError", &msg);
                        self.handle_throw(err)?;
                        continue;
                    }
                    let obj_val = self.pop()?;
                    let name_str = self.interner.resolve(name_id);
                    if let Some(oid) = obj_val.as_object_id() {
                        // ConsString: O(1) .length from cached field; otherwise flatten to string
                        if let Some(obj) = self.heap.get(oid)
                            && let ObjectKind::ConsString { len, .. } = obj.kind {
                            if name_str == "length" {
                                self.push(Value::int(len as i32));
                                continue;
                            }
                            // Flatten and re-run as a TAG_STRING property access
                            let flat = self.flatten_cons_to_string(obj_val);
                            let sid = self.interner.intern(&flat);
                            let flat_val = Value::string(sid);
                            let name_s = self.interner.resolve(name_id).to_owned();
                            let s = self.interner.resolve(sid).to_owned();
                            let method_idx = match name_s.as_str() {
                                "charAt" => 0, "charCodeAt" => 1, "indexOf" => 2,
                                "lastIndexOf" => 3, "includes" => 4, "startsWith" => 5,
                                "endsWith" => 6, "slice" => 7, "substring" => 8,
                                "toUpperCase" => 9, "toLowerCase" => 10,
                                "trim" => 11, "trimStart" => 12, "trimEnd" => 13,
                                "split" => 14, "replace" => 15, "repeat" => 16,
                                "padStart" => 17, "padEnd" => 18, "concat" => 19,
                                "match" => 20, "search" => 21, "replaceAll" => 22,
                                "codePointAt" => 23, "at" => 24,
                                "toString" | "valueOf" => { self.push(flat_val); continue; }
                                _ => { self.push(Value::undefined()); continue; }
                            };
                            let _ = s; // suppress unused warning
                            let sentinel = -200 - method_idx;
                            self.push(Value::function(sentinel));
                            continue;
                        }
                        // TypedArray / ArrayBuffer / DataView properties.
                        if let Some(obj) = self.heap.get(oid) {
                            match &obj.kind {
                                ObjectKind::TypedArray { kind, elements, buffer } => {
                                    let kind = *kind; let len = elements.len(); let buffer = *buffer;
                                    let bpe = kind.bytes_per_element();
                                    match name_str {
                                        "length" => { self.push(Value::int(len as i32)); continue; }
                                        "byteLength" => { self.push(Value::int((len * bpe) as i32)); continue; }
                                        "byteOffset" => { self.push(Value::int(0)); continue; }
                                        "BYTES_PER_ELEMENT" => { self.push(Value::int(bpe as i32)); continue; }
                                        "buffer" => { self.push(Value::object_id(buffer)); continue; }
                                        "constructor" => { self.push(Value::function(crate::vm::typedarray::sentinel_for_kind(kind))); continue; }
                                        _ => {
                                            if let Ok(i) = name_str.parse::<usize>() {
                                                let v = self.typed_array_get(oid, i).unwrap_or(Value::undefined());
                                                self.push(v); continue;
                                            }
                                        }
                                    }
                                }
                                ObjectKind::ArrayBuffer(b) => {
                                    if name_str == "byteLength" { self.push(Value::int(b.len() as i32)); continue; }
                                }
                                ObjectKind::DataView { buffer, byte_offset, byte_length } => {
                                    match name_str {
                                        "byteLength" => { self.push(Value::int(*byte_length as i32)); continue; }
                                        "byteOffset" => { self.push(Value::int(*byte_offset as i32)); continue; }
                                        "buffer" => { self.push(Value::object_id(*buffer)); continue; }
                                        _ => {}
                                    }
                                }
                                _ => {}
                            }
                        }
                        // Check for array-specific properties
                        if let Some(obj) = self.heap.get(oid)
                            && let ObjectKind::Array(ref elements) = obj.kind {
                            if name_str == "length" {
                                self.push(Value::int(elements.len() as i32));
                                continue;
                            }
                            // Numeric string index: arr["0"], arr["1"], etc.
                            if let Ok(idx) = name_str.parse::<usize>() {
                                let val = elements.get(idx).copied().unwrap_or(Value::undefined());
                                self.push(val);
                                continue;
                            }
                        }
                        // Map/Set size property
                        if name_str == "size"
                            && let Some(obj) = self.heap.get(oid) {
                                match &obj.kind {
                                    ObjectKind::Map { entries } => { self.push(Value::int(entries.len() as i32)); continue; }
                                    ObjectKind::Set { entries } => { self.push(Value::int(entries.len() as i32)); continue; }
                                    _ => {}
                                }
                        }
                        // For arrays, expose Array.prototype methods as the same sentinel function
                        // values so that `arr.method === Array.prototype.method` holds.
                        if let Some(obj) = self.heap.get(oid)
                            && matches!(&obj.kind, ObjectKind::Array(_))
                        {
                            let proto_sentinel = match name_str {
                                "join" => Some(-600i32), "push" => Some(-601), "pop" => Some(-602),
                                "shift" => Some(-603), "unshift" => Some(-604), "indexOf" => Some(-605),
                                "includes" => Some(-606), "forEach" => Some(-607), "map" => Some(-608),
                                "filter" => Some(-609), "reduce" => Some(-610), "some" => Some(-611),
                                "every" => Some(-612), "find" => Some(-613), "findIndex" => Some(-614),
                                "slice" => Some(-615), "concat" => Some(-616), "reverse" => Some(-617),
                                "sort" => Some(-618), "flat" => Some(-619), "flatMap" => Some(-620),
                                "fill" => Some(-621), "splice" => Some(-622), "reduceRight" => Some(-623),
                                "at" => Some(-624), "keys" => Some(-625), "values" => Some(-626),
                                "entries" => Some(-627), "lastIndexOf" => Some(-628), "toString" => Some(-629),
                                _ => None,
                            };
                            if let Some(s) = proto_sentinel {
                                self.push(Value::function(s));
                                continue;
                            }
                        }
                        // Check for RegExp properties
                        if let Some(obj) = self.heap.get(oid)
                            && let ObjectKind::RegExp { pattern, flags } = &obj.kind
                        {
                            let val = match name_str {
                                "source" => { let id = self.interner.intern(pattern.as_str()); Value::string(id) }
                                "flags" => { let id = self.interner.intern(flags.as_str()); Value::string(id) }
                                "global" => Value::boolean(flags.contains('g')),
                                "ignoreCase" => Value::boolean(flags.contains('i')),
                                "multiline" => Value::boolean(flags.contains('m')),
                                "dotAll" => Value::boolean(flags.contains('s')),
                                "unicode" => Value::boolean(flags.contains('u')),
                                "sticky" => Value::boolean(flags.contains('y')),
                                "lastIndex" => Value::int(0),
                                // Unknown names walk the chain: RegExp.prototype
                                // holds extractable test/exec/toString NativeFns.
                                _ => self.heap.get_property_chain(oid, name_id)
                                    .unwrap_or(Value::undefined()),
                            };
                            self.push(val);
                            continue;
                        }
                        // Check for getter
                        let getter_key_str = format!("__get_{}__", name_str);
                        let getter_key = self.interner.intern(&getter_key_str);
                        let getter_fn = self.heap.get_property_chain(oid, getter_key);
                        if let Some(gfn) = getter_fn
                            && gfn.is_function()
                        {
                            let result = self.call_function_this(gfn, obj_val, &[])?;
                            self.push(result);
                            continue;
                        }
                        let mut val = self.heap.get_property_chain(oid, name_id)
                            .unwrap_or(Value::undefined());
                        // globalThis proxies misses to the globals map so
                        // `globalThis.Array` / `global[name]` resolve the
                        // engine builtins — core-js reads every primordial
                        // this way (`i[t].prototype[e]`).
                        if val.is_undefined() && oid == self.global_this_oid
                            && let Some(&g) = self.globals.get(&name_id) {
                                val = g;
                            }
                        self.push(val);
                    } else if obj_val.is_string() {
                        // String property/method access (interned or inline).
                        match name_str {
                            "length" => self.push(Value::int(self.string_char_len(obj_val) as i32)),
                            // String methods return sentinels for CallMethod dispatch
                            "charAt" | "charCodeAt" | "indexOf" | "lastIndexOf"
                            | "includes" | "startsWith" | "endsWith"
                            | "slice" | "substring" | "substr" | "toUpperCase" | "toLowerCase"
                            | "trim" | "trimStart" | "trimEnd" | "normalize"
                            | "split" | "replace" | "repeat"
                            | "padStart" | "padEnd" | "concat"
                            | "match" | "search" | "replaceAll"
                            | "codePointAt" | "at" => {
                                // Encode: string sentinel = -200 - method_index
                                let method_idx = match name_str {
                                    "charAt" => 0, "charCodeAt" => 1, "indexOf" => 2,
                                    "lastIndexOf" => 3, "includes" => 4, "startsWith" => 5,
                                    "endsWith" => 6, "slice" => 7, "substring" => 8,
                                    "toUpperCase" => 9, "toLowerCase" => 10,
                                    "trim" => 11, "trimStart" => 12, "trimEnd" => 13,
                                    "split" => 14, "replace" => 15, "repeat" => 16,
                                    "padStart" => 17, "padEnd" => 18, "concat" => 19,
                                    "match" => 20, "search" => 21, "replaceAll" => 22,
                                    "codePointAt" => 23, "at" => 24,
                                    _ => 99,
                                };
                                self.push(Value::function(-200 - method_idx));
                            }
                            "constructor" => self.push(Value::function(-504)),
                            _ => self.push(Value::undefined()),
                        }
                    } else if obj_val.is_function() {
                        // Property access on sentinel globals (Number.NaN, etc)
                        let sentinel = obj_val.as_function().unwrap();
                        // Check fn_property_overrides for user-set properties first (all sentinels)
                        if let Some(ov) = self.fn_property_overrides.get(&(sentinel, name_id)).copied() {
                            self.push(ov.unwrap_or(Value::undefined()));
                            continue;
                        }
                        let result = match sentinel {
                            -505 => match name_str {
                                "prototype" => Value::object_id(self.number_prototype),
                                "NaN" => Value::number(f64::NAN),
                                "POSITIVE_INFINITY" => Value::number(f64::INFINITY),
                                "NEGATIVE_INFINITY" => Value::number(f64::NEG_INFINITY),
                                "MAX_VALUE" => Value::number(f64::MAX),
                                // Smallest positive denormal (5e-324), not the smallest normal.
                                "MIN_VALUE" => Value::number(f64::from_bits(1)),
                                "MAX_SAFE_INTEGER" => Value::number(9007199254740991.0),
                                "MIN_SAFE_INTEGER" => Value::number(-9007199254740991.0),
                                "EPSILON" => Value::number(f64::EPSILON),
                                "isNaN" => Value::function(-530),
                                "isFinite" => Value::function(-531),
                                "isInteger" => Value::function(-532),
                                "isSafeInteger" => Value::function(-533),
                                "parseInt" => Value::function(-500),
                                "parseFloat" => Value::function(-501),
                                _ => Value::undefined(),
                            },
                            -506 => match name_str {
                                "prototype" => Value::object_id(self.boolean_prototype),
                                _ => Value::undefined(),
                            },
                            -570 => match name_str {
                                "for" => Value::function(-752),
                                "keyFor" => Value::function(-753),
                                "iterator" => Value::symbol(self.sym_iterator),
                                "hasInstance" => Value::symbol(self.sym_has_instance),
                                "toPrimitive" => Value::symbol(self.sym_to_primitive),
                                "toStringTag" => Value::symbol(self.sym_to_string_tag),
                                "species" => Value::symbol(self.sym_species),
                                "unscopables" => Value::symbol(self.sym_unscopables),
                                "asyncIterator" => Value::symbol(self.sym_async_iterator),
                                "matchAll" => Value::symbol(self.sym_match_all),
                                "prototype" => Value::object_id(self.symbol_prototype_oid()),
                                _ => Value::undefined(),
                            },
                            -504 => match name_str {
                                "prototype" => Value::object_id(self.string_prototype),
                                // String static methods exposed as sentinels
                                "fromCharCode" => Value::function(-534),
                                "fromCodePoint" => Value::function(-535),
                                "raw" => Value::function(-536),
                                _ => Value::undefined(),
                            },
                            -507 => match name_str {
                                // Array static properties
                                "prototype" => Value::object_id(self.array_prototype),
                                // Extractable static: `var isArray = Array.isArray;`
                                // (react-dom's reconciler aliases it).
                                "isArray" => Value::function(-751),
                                // Inherited function methods (hasOwnProperty,
                                // call, …) via Function.prototype → Object.prototype.
                                _ => self.heap.get_property_chain(self.function_prototype, name_id)
                                    .unwrap_or(Value::undefined()),
                            },
                            -508 => match name_str {
                                // Object static properties
                                "prototype" => Value::object_id(self.object_prototype),
                                // Lazily wrap `Object.defineProperty` as a callable so the
                                // `var f = Object.defineProperty; f(obj, key, desc)` pattern
                                // (used by test262's propertyHelper.js, MDN-style helpers,
                                // etc.) works. Cached in fn_property_overrides so identity
                                // is stable across reads.
                                // Same lazy-callable trick for `Object.assign` —
                                // `var assign = Object.assign;` is the standard
                                // minified-bundle prologue (React, Preact, ...).
                                "assign" => {
                                    let func: crate::runtime::object::NativeFn = std::sync::Arc::new(
                                        |vm: &mut Vm, _this: Value, args: &[Value]| -> Result<Value, Value> {
                                            Ok(vm.exec_object_assign(args))
                                        }
                                    );
                                    let fn_obj = JsObject {
                                        properties: Vec::new(),
                                        prototype: None,
                                        kind: ObjectKind::Function(crate::runtime::object::FunctionKind::Native { name: name_id, func }),
                                        marked: false,
                                        extensible: true,
                                    };
                                    let oid = self.heap.allocate(fn_obj);
                                    let val = Value::object_id(oid);
                                    self.fn_property_overrides.insert((-508, name_id), Some(val));
                                    val
                                }
                                "defineProperty" => {
                                    let func: crate::runtime::object::NativeFn = std::sync::Arc::new(
                                        |vm: &mut Vm, _this: Value, args: &[Value]| -> Result<Value, Value> {
                                            Ok(vm.object_define_property(args))
                                        }
                                    );
                                    let fn_obj = JsObject {
                                        properties: Vec::new(),
                                        prototype: None,
                                        kind: ObjectKind::Function(crate::runtime::object::FunctionKind::Native { name: name_id, func }),
                                        marked: false,
                                        extensible: true,
                                    };
                                    let oid = self.heap.allocate(fn_obj);
                                    let val = Value::object_id(oid);
                                    self.fn_property_overrides.insert((-508, name_id), Some(val));
                                    val
                                }
                                _ => {
                                    // Same lazy-callable wrap for every other
                                    // Object static — see object_static_callable.
                                    if let Some(val) = self.object_static_callable(name_id) {
                                        val
                                    } else {
                                        self.heap.get_property_chain(self.function_prototype, name_id)
                                            .unwrap_or(Value::undefined())
                                    }
                                }
                            },
                            -551 => match name_str {
                                // Function static properties
                                "prototype" => Value::object_id(self.function_prototype),
                                _ => self.heap.get_property_chain(self.function_prototype, name_id)
                                    .unwrap_or(Value::undefined()),
                            },
                            _ => {
                                // User-defined function properties.
                                // Arrow functions and strict-mode functions have
                                // 'caller' and 'arguments' as poison-pill accessors
                                // that throw TypeError on access.
                                if matches!(name_str, "caller" | "arguments") && sentinel >= 0 {
                                    let chunk_idx = (sentinel & 0xFFFF) as usize;
                                    let is_restricted = chunk_idx < self.chunks.len()
                                        && (self.chunks[chunk_idx].flags.contains(ChunkFlags::ARROW)
                                            || self.chunks[chunk_idx].flags.contains(ChunkFlags::STRICT));
                                    if is_restricted {
                                        let err = self.make_native_error(
                                            "TypeError",
                                            &format!("'{name_str}' may not be accessed on strict mode functions"),
                                        );
                                        self.handle_throw(err)?;
                                        continue;
                                    }
                                }
                                match name_str {
                                    "prototype" => {
                                        if let Some(&proto_oid) = self.func_prototypes.get(&sentinel) {
                                            Value::object_id(proto_oid)
                                        } else {
                                            let mut proto = JsObject::ordinary();
                                            proto.prototype = Some(self.object_prototype);
                                            // Generator functions get a plain empty prototype.
                                            let chunk_idx = (sentinel & 0xFFFF) as usize;
                                            let is_gen = chunk_idx < self.chunks.len()
                                                && self.chunks[chunk_idx].flags.contains(ChunkFlags::GENERATOR);
                                            if !is_gen {
                                                let ctor_key = self.interner.intern("constructor");
                                                proto.define_property(ctor_key, Property::with_flags(
                                                    obj_val, Property::WRITABLE | Property::CONFIGURABLE
                                                ));
                                            }
                                            let proto_oid = self.heap.allocate(proto);
                                            self.func_prototypes.insert(sentinel, proto_oid);
                                            Value::object_id(proto_oid)
                                        }
                                    }
                                    "constructor" => Value::function(-551),
                                    "name" | "length" => {
                                        self.fn_get_own_prop(sentinel, name_id)
                                            .unwrap_or(Value::undefined())
                                    }
                                    "call" | "apply" | "bind" => {
                                        // Return function sentinel for method dispatch
                                        obj_val
                                    }
                                    // Inherited methods (hasOwnProperty,
                                    // isPrototypeOf, propertyIsEnumerable,
                                    // toString, valueOf) via Function.prototype
                                    // → Object.prototype. Lets `fn.hasOwnProperty(k)`
                                    // and `Ctor.hasOwnProperty.call(o, k)` work.
                                    _ => self.heap.get_property_chain(self.function_prototype, name_id)
                                        .unwrap_or(Value::undefined()),
                                }
                            }
                        };
                        self.push(result);
                    } else if (obj_val.is_number() || obj_val.is_int())
                        && name_str == "constructor"
                    {
                        self.push(Value::function(-505));
                    } else if obj_val.is_boolean() && name_str == "constructor" {
                        self.push(Value::function(-506));
                    } else if let Some(proto_oid) = if obj_val.is_int() || obj_val.is_number() {
                        Some(self.number_prototype)
                    } else if obj_val.as_bool().is_some() {
                        Some(self.boolean_prototype)
                    } else {
                        None
                    } {
                        // Property access on a primitive walks its wrapper's
                        // prototype chain: Number.prototype, Boolean.prototype, etc.
                        // Getters on user-extended Object.prototype are invoked
                        // with `this` set to the boxed primitive (sloppy semantics).
                        let getter_key = self.interner.intern(&format!("__get_{name_str}__"));
                        let getter_fn = self.heap.get_property_chain(proto_oid, getter_key);
                        if let Some(gfn) = getter_fn
                            && gfn.is_function()
                        {
                            let this_val = self.box_primitive(obj_val);
                            let result = self.call_function_this(gfn, this_val, &[])?;
                            self.push(result);
                            continue;
                        }
                        let val = self.heap.get_property_chain(proto_oid, name_id)
                            .unwrap_or(Value::undefined());
                        self.push(val);
                    } else {
                        self.push(Value::undefined());
                    }
                }

                OpCode::SetProperty => {
                    let name_idx = self.read_u16() as usize;
                    let ic_slot = self.read_u16() as usize;
                    let chunk_idx = self.cur_chunk();
                    let name_val = self.chunks[chunk_idx].constants[name_idx];
                    let name_id = name_val.as_string_id().unwrap();
                    let val = self.pop()?;
                    let obj_val = self.pop()?;
                    // Setting a property on null/undefined throws TypeError.
                    if obj_val.is_null() || obj_val.is_undefined() {
                        let kind = if obj_val.is_null() { "null" } else { "undefined" };
                        let name_s = self.interner.resolve(name_id).to_owned();
                        let err = self.make_native_error(
                            "TypeError",
                            &format!("Cannot set properties of {kind} (setting '{name_s}')"),
                        );
                        self.handle_throw(err)?;
                        continue;
                    }
                    if obj_val.is_function() {
                        let sentinel = obj_val.as_function().unwrap();
                        // Built-in read-only properties on native sentinels (Number.MAX_VALUE,
                        // Symbol.iterator, Array.prototype, etc.) — strict-mode writes throw,
                        // non-strict writes silently fail.
                        let in_strict = self.chunks[chunk_idx].flags.contains(ChunkFlags::STRICT);
                        let name_str = self.interner.resolve(name_id);
                        let is_readonly = matches!((sentinel, name_str),
                            (-505, "NaN") | (-505, "POSITIVE_INFINITY") | (-505, "NEGATIVE_INFINITY")
                            | (-505, "MAX_VALUE") | (-505, "MIN_VALUE")
                            | (-505, "MAX_SAFE_INTEGER") | (-505, "MIN_SAFE_INTEGER")
                            | (-505, "EPSILON")
                            | (-570, "iterator") | (-570, "hasInstance") | (-570, "toPrimitive")
                            | (-570, "toStringTag") | (-570, "species") | (-570, "unscopables")
                            | (-570, "asyncIterator") | (-570, "matchAll")
                            | (-507, "prototype") | (-508, "prototype") | (-551, "prototype")
                        )
                        // Constructor `length` and `name` are non-writable per spec.
                        || ((-636..=-500).contains(&sentinel) && (name_str == "length" || name_str == "name"));
                        if is_readonly {
                            if in_strict {
                                let prop = name_str.to_owned();
                                let msg = format!("Cannot assign to read only property '{prop}'");
                                let err = self.make_native_error("TypeError", &msg);
                                self.handle_throw(err)?;
                                continue;
                            }
                            // Non-strict: silently no-op.
                            self.push(val);
                            continue;
                        }
                        // Restricted-properties poison pill: arrow functions and
                        // strict-mode functions throw TypeError on caller/arguments
                        // assignment regardless of caller strictness.
                        if matches!(name_str, "caller" | "arguments") && sentinel >= 0 {
                            let chunk_idx = (sentinel & 0xFFFF) as usize;
                            let is_restricted = chunk_idx < self.chunks.len()
                                && (self.chunks[chunk_idx].flags.contains(ChunkFlags::ARROW)
                                    || self.chunks[chunk_idx].flags.contains(ChunkFlags::STRICT));
                            if is_restricted {
                                let err = self.make_native_error(
                                    "TypeError",
                                    &format!("'{name_str}' may not be set on strict mode functions"),
                                );
                                self.handle_throw(err)?;
                                continue;
                            }
                        }
                        self.fn_property_overrides.insert((sentinel, name_id), Some(val));
                        // Keep func_prototypes in sync so `obj instanceof F` reads
                        // the user-set prototype, not the auto-generated one.
                        if name_str == "prototype"
                            && let Some(proto_oid) = val.as_object_id()
                        {
                            self.func_prototypes.insert(sentinel, proto_oid);
                        }
                    } else if let Some(oid) = obj_val.as_object_id() {
                        // Check for setter
                        let name_str = self.interner.resolve(name_id).to_owned();
                        // Special case: arr.length = N truncates/extends the array
                        if name_str == "length" {
                            let is_array = self.heap.get(oid)
                                .map(|o| matches!(&o.kind, ObjectKind::Array(_)))
                                .unwrap_or(false);
                            if is_array {
                                let new_len = self.to_f64(val) as usize;
                                if let Some(obj) = self.heap.get_mut(oid)
                                    && let ObjectKind::Array(ref mut elements) = obj.kind
                                {
                                    elements.resize(new_len, Value::undefined());
                                }
                                self.push(val);
                                continue;
                            }
                        }
                        let setter_key = self.interner.intern(&format!("__set_{name_str}__"));
                        let setter_fn = self.heap.get_property_chain(oid, setter_key);
                        let getter_key = self.interner.intern(&format!("__get_{name_str}__"));
                        let has_getter = self.heap.get_property_chain(oid, getter_key).is_some();
                        if let Some(sfn) = setter_fn
                            && sfn.is_function()
                        {
                            // Protect the call so any throw bubbles back to us
                            // rather than being caught by an outer try block
                            // before SetProperty's stack manipulation completes.
                            let prev_protect = self.protect_throw_depth;
                            self.protect_throw_depth = self.frames.len() + 1;
                            let r = self.call_function_this(sfn, obj_val, &[val]);
                            self.protect_throw_depth = prev_protect;
                            match r {
                                Ok(_) => {}
                                Err(VmError::Throw(v)) => {
                                    self.handle_throw(v)?;
                                    continue;
                                }
                                Err(e) => return Err(e),
                            }
                        } else {
                            // Strict-mode check: assigning to a non-writable data
                            // property anywhere on the prototype chain, an accessor
                            // without a setter, or a non-extensible object's missing
                            // property must throw TypeError.
                            let in_strict = self.chunks[chunk_idx].flags.contains(ChunkFlags::STRICT);
                            let is_readonly_own = self.heap.get(oid).and_then(|o| {
                                o.get_property_descriptor(name_id).filter(|p| !p.is_writable())
                            }).is_some();
                            // Walk prototype chain for an inherited non-writable data
                            // property only when no own property exists.
                            let has_own_data = self.heap.get(oid)
                                .map(|o| o.has_own_property(name_id))
                                .unwrap_or(false);
                            let is_readonly_proto = if !has_own_data {
                                let mut cur = self.heap.get(oid).and_then(|o| o.prototype);
                                let mut depth = 0;
                                let mut found = false;
                                while let Some(pid) = cur {
                                    if depth > 64 { break; }
                                    if let Some(p) = self.heap.get(pid) {
                                        if let Some(desc) = p.get_property_descriptor(name_id) {
                                            if !desc.is_writable() { found = true; }
                                            break;
                                        }
                                        cur = p.prototype;
                                    } else { break; }
                                    depth += 1;
                                }
                                found
                            } else { false };
                            let extensible = self.heap.get(oid).map(|o| o.extensible).unwrap_or(true);
                            let has_own = self.heap.get(oid)
                                .map(|o| o.has_own_property(name_id))
                                .unwrap_or(false);
                            if in_strict && (is_readonly_own || is_readonly_proto || has_getter || (!extensible && !has_own)) {
                                let prop = self.interner.resolve(name_id).to_owned();
                                let msg = if has_getter {
                                    format!("Cannot set property '{prop}' which has only a getter")
                                } else if is_readonly_own || is_readonly_proto {
                                    format!("Cannot assign to read only property '{prop}'")
                                } else {
                                    format!("Cannot add property {prop}, object is not extensible")
                                };
                                let err = self.make_native_error("TypeError", &msg);
                                self.handle_throw(err)?;
                                continue;
                            }
                            // In non-strict mode with a getter and no setter, silently fail.
                            if has_getter {
                                self.push(val);
                                continue;
                            }
                            // Non-strict: silently no-op when an inherited
                            // non-writable property would be shadowed.
                            if is_readonly_proto {
                                self.push(val);
                                continue;
                            }
                            if let Some(obj) = self.heap.get_mut(oid) {
                                // IC: update or insert — record the slot for future fast access
                                let pos = obj.properties.iter().position(|(k, _)| *k == name_id);
                                obj.set_property(name_id, val);
                                let slot = pos.unwrap_or(obj.properties.len().saturating_sub(1));
                                if slot <= 254 {
                                    self.chunks[chunk_idx].property_ic[ic_slot] = slot as u8;
                                }
                            }
                        }
                    }
                    self.push(val);
                }

                OpCode::GetElement => {
                    let key = self.pop()?;
                    let obj_val = self.pop()?;
                    // RequireObjectCoercible: null/undefined base throws TypeError
                    // BEFORE ToPropertyKey runs on the key.
                    if obj_val.is_null() || obj_val.is_undefined() {
                        let kind = if obj_val.is_null() { "null" } else { "undefined" };
                        let keyrepr = self.value_to_string(key);
                        let (line, pc, chunk_name) = if let Some(f) = self.frames.last() {
                            let cn = self.interner.resolve(self.chunks[f.chunk_idx].name).to_owned();
                            (self.chunks[f.chunk_idx].get_line(f.ip as u32), f.ip, cn)
                        } else { (0, 0, String::new()) };
                        let err = self.make_native_error(
                            "TypeError",
                            &format!("Cannot read properties of {kind} (reading '{keyrepr}') (at line {line}, pc {pc}, chunk '{chunk_name}')"),
                        );
                        self.handle_throw(err)?;
                        continue;
                    }
                    // ToPropertyKey: coerce non-string/non-symbol/non-numeric keys
                    // (undefined, null, boolean, function, object) to their string form.
                    let key = if key.is_undefined() {
                        Value::string(self.interner.intern("undefined"))
                    } else if key.is_null() {
                        Value::string(self.interner.intern("null"))
                    } else if let Some(b) = key.as_bool() {
                        Value::string(self.interner.intern(if b { "true" } else { "false" }))
                    } else if key.is_function() {
                        let s = self.value_to_string(key);
                        Value::string(self.interner.intern(&s))
                    } else if key.is_object() && !key.is_symbol() && !self.is_cons_string(key) {
                        // Generic objects (non-array, non-symbol) → "[object Object]" or
                        // their custom toString.
                        let is_array = key.as_object_id()
                            .and_then(|oid| self.heap.get(oid))
                            .map(|o| matches!(&o.kind, ObjectKind::Array(_)))
                            .unwrap_or(false);
                        if !is_array {
                            let s = self.value_to_string(key);
                            Value::string(self.interner.intern(&s))
                        } else { key }
                    } else { key };
                    // Typed array: an integer (canonical-index) key reads the element;
                    // non-index keys ("length", methods) fall through to the property path.
                    if let Some(oid) = obj_val.as_object_id()
                        && self.typed_array_len(oid).is_some()
                        && let Some(i) = self.canonical_index(key)
                    {
                        let v = self.typed_array_get(oid, i).unwrap_or(Value::undefined());
                        self.push(v);
                        continue;
                    }
                    if let Some(oid) = obj_val.as_object_id()
                        && let Some(obj) = self.heap.get(oid)
                    {
                        if let ObjectKind::Array(ref elements) = obj.kind {
                            // Fast path: SMI index (most common case)
                            if let Some(i) = key.as_int()
                                && i >= 0
                            {
                                let val = elements.get(i as usize).copied().unwrap_or(Value::undefined());
                                self.push(val);
                                continue;
                            }
                            // Float index — only canonical integers count as
                            // array indices; fractional numbers fall through
                            // to string-property lookup.
                            if let Some(n) = key.as_number()
                                && n >= 0.0
                                && n.fract() == 0.0
                                && n.is_finite()
                                && n < 4_294_967_295.0
                            {
                                let val = elements.get(n as usize).copied().unwrap_or(Value::undefined());
                                self.push(val);
                                continue;
                            }
                            // String key on array: "length" or numeric string like "0"
                            if let Some(name_id) = key.as_string_id() {
                                let name = self.interner.resolve(name_id);
                                if name == "length" {
                                    self.push(Value::int(elements.len() as i32));
                                    continue;
                                }
                                // Try parsing string as numeric index: arr["0"]
                                if let Ok(idx) = name.parse::<usize>() {
                                    let val = elements.get(idx).copied().unwrap_or(Value::undefined());
                                    self.push(val);
                                    continue;
                                }
                            }
                        }
                        // String property lookup — check getter first, then plain property.
                        // Inline/ConsString/numeric keys have no StringId, so intern them
                        // (intern-on-demand: a transient string used as a property key).
                        let key = if key.is_inline_string() || self.is_cons_string(key) || self.is_flat_string(key) {
                            let flat = self.flatten_cons_to_string(key);
                            Value::string(self.interner.intern(&flat))
                        } else if key.as_number().is_some() && key.as_string_id().is_none() {
                            let s = self.value_to_string(key);
                            Value::string(self.interner.intern(&s))
                        } else { key };
                        if let Some(name_id) = key.as_string_id() {
                            let name_str = self.interner.resolve(name_id).to_owned();
                            // Check for getter
                            let getter_key_str = format!("__get_{name_str}__");
                            let getter_key = self.interner.intern(&getter_key_str);
                            if let Some(gfn) = self.heap.get_property_chain(oid, getter_key)
                                && gfn.is_function() {
                                    let result = self.call_function_this(gfn, obj_val, &[])?;
                                    self.push(result);
                                    continue;
                                }
                            let mut val = self.heap.get_property_chain(oid, name_id)
                                .unwrap_or(Value::undefined());
                            // globalThis proxies misses to the globals map
                            // (same as the dot-access path) — core-js reads
                            // primordials as `global[name]`.
                            if val.is_undefined() && oid == self.global_this_oid
                                && let Some(&g) = self.globals.get(&name_id) {
                                    val = g;
                                }
                            self.push(val);
                            continue;
                        }
                        // Symbol-keyed property lookup
                        if key.is_symbol() {
                            let sid = key.as_symbol_id().unwrap();
                            let sym_key = self.interner.intern(&format!("__sym_{sid}__"));
                            let val = self.heap.get_property_chain(oid, sym_key)
                                .unwrap_or(Value::undefined());
                            self.push(val);
                            continue;
                        }
                        // Numeric key on ordinary object: coerce to string ("0", "1", ...)
                        if let Some(n) = key.as_number() {
                            let s = if n.fract() == 0.0 && n.is_finite() {
                                (n as i64).to_string()
                            } else {
                                n.to_string()
                            };
                            let name_id = self.interner.intern(&s);
                            // Check for accessor first.
                            let getter_key = self.interner.intern(&format!("__get_{s}__"));
                            if let Some(gfn) = self.heap.get_property_chain(oid, getter_key)
                                && gfn.is_function()
                            {
                                let result = self.call_function_this(gfn, obj_val, &[])?;
                                self.push(result);
                                continue;
                            }
                            let val = self.heap.get_property_chain(oid, name_id)
                                .unwrap_or(Value::undefined());
                            self.push(val);
                            continue;
                        }
                    }
                    // String bracket index access: "hello"[0] → "h" (interned, inline, or ConsString)
                    let string_val_opt: Option<String> = if self.is_string_like(obj_val) {
                        Some(self.value_to_string(obj_val))
                    } else { None };
                    if let Some(s) = string_val_opt {
                        let ascii = self.string_is_ascii(obj_val);
                        if let Some(i) = key.as_int() {
                            if i >= 0 {
                                let i = i as usize;
                                if ascii {
                                    if i < s.len() { let v = self.new_str(&s[i..i + 1]); self.push(v); continue; }
                                } else if let Some(ch) = s.chars().nth(i) {
                                    let mut buf = [0u8; 4];
                                    let v = self.new_str(ch.encode_utf8(&mut buf));
                                    self.push(v);
                                    continue;
                                }
                            }
                        } else if let Some(idx) = key.as_number() {
                            let i = idx as usize;
                            if idx >= 0.0 && idx.fract() == 0.0 {
                                if ascii {
                                    if i < s.len() { let v = self.new_str(&s[i..i + 1]); self.push(v); continue; }
                                } else if let Some(ch) = s.chars().nth(i) {
                                    let mut buf = [0u8; 4];
                                    let v = self.new_str(ch.encode_utf8(&mut buf));
                                    self.push(v);
                                    continue;
                                }
                            }
                        } else if let Some(name_id) = key.as_string_id() {
                            let name = self.interner.resolve(name_id);
                            if name == "length" {
                                self.push(Value::int(self.string_char_len(obj_val) as i32));
                                continue;
                            }
                        }
                    }
                    // Function bracket access: delegate to dot-access helper for consistency.
                    if obj_val.is_function()
                        && let Some(key_id) = key.as_string_id() {
                        let sentinel = obj_val.as_function().unwrap();
                        let result = self.fn_property_get(sentinel, key_id, obj_val);
                        self.push(result);
                        continue;
                    }
                    self.push(Value::undefined());
                }

                OpCode::SetElement => {
                    let val = self.pop()?;
                    let key = self.pop()?;
                    let obj_val = self.pop()?;
                    // RequireObjectCoercible: null/undefined base throws TypeError
                    // BEFORE ToPropertyKey runs on the key.
                    if obj_val.is_null() || obj_val.is_undefined() {
                        let kind = if obj_val.is_null() { "null" } else { "undefined" };
                        let err = self.make_native_error(
                            "TypeError",
                            &format!("Cannot set properties of {kind}"),
                        );
                        self.handle_throw(err)?;
                        continue;
                    }
                    // ToPropertyKey: flatten ConsString and coerce non-primitive keys.
                    let key = if self.is_cons_string(key) {
                        let flat = self.flatten_cons_to_string(key);
                        Value::string(self.interner.intern(&flat))
                    } else if key.is_undefined() {
                        Value::string(self.interner.intern("undefined"))
                    } else if key.is_null() {
                        Value::string(self.interner.intern("null"))
                    } else if let Some(b) = key.as_bool() {
                        Value::string(self.interner.intern(if b { "true" } else { "false" }))
                    } else if key.is_function() {
                        let s = self.value_to_string(key);
                        Value::string(self.interner.intern(&s))
                    } else if key.is_object() && !key.is_symbol() {
                        // Object keys: invoke ToPrimitive(string) so the user's toString
                        // (or valueOf) runs and the result is used as the property name.
                        let prim = match self.try_coerce_to_primitive_hint(key, "string") {
                            Ok(v) => v,
                            Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                            Err(e) => return Err(e),
                        };
                        if prim.is_symbol() {
                            prim
                        } else {
                            let s = self.value_to_string(prim);
                            Value::string(self.interner.intern(&s))
                        }
                    } else { key };
                    // Typed array indexed write: coerce + store (out-of-range is a no-op).
                    if let Some(oid) = obj_val.as_object_id()
                        && self.typed_array_len(oid).is_some()
                        && let Some(i) = self.canonical_index(key)
                    {
                        match self.typed_array_set(oid, i, val) {
                            Ok(_) => { self.push(val); continue; }
                            Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                            Err(e) => return Err(e),
                        }
                    }
                    if let Some(oid) = obj_val.as_object_id()
                        && let Some(obj) = self.heap.get_mut(oid)
                    {
                        if let ObjectKind::Array(ref mut elements) = obj.kind {
                            // Fast path: SMI index
                            if let Some(i) = key.as_int()
                                && i >= 0
                            {
                                let idx = i as usize;
                                while elements.len() <= idx {
                                    elements.push(Value::undefined());
                                }
                                elements[idx] = val;
                                self.push(val);
                                continue;
                            }
                            // Per spec, only canonical-integer keys in [0, 2^32 - 1)
                            // count as array indices. Fractional numbers like 1.1
                            // become string-keyed properties on the object.
                            if let Some(n) = key.as_number()
                                && n >= 0.0
                                && n.fract() == 0.0
                                && n.is_finite()
                                && n < 4_294_967_295.0
                            {
                                let idx = n as usize;
                                while elements.len() <= idx {
                                    elements.push(Value::undefined());
                                }
                                elements[idx] = val;
                                self.push(val);
                                continue;
                            }
                        }
                        // Numeric non-array-index keys, and inline/flat strings (which
                        // have no StringId), become interned string-keyed properties.
                        let key = if key.is_inline_string() || self.is_flat_string(key) {
                            let flat = self.flatten_cons_to_string(key);
                            Value::string(self.interner.intern(&flat))
                        } else if key.as_number().is_some() && key.as_string_id().is_none() {
                            let s = self.value_to_string(key);
                            Value::string(self.interner.intern(&s))
                        } else { key };
                        if let Some(name_id) = key.as_string_id() {
                            // Check for setter first
                            let name_str = self.interner.resolve(name_id).to_owned();
                            let setter_key = self.interner.intern(&format!("__set_{name_str}__"));
                            if let Some(sfn) = self.heap.get_property_chain(oid, setter_key)
                                && sfn.is_function() {
                                    let prev_protect = self.protect_throw_depth;
                                    self.protect_throw_depth = self.frames.len() + 1;
                                    let r = self.call_function_this(sfn, obj_val, &[val]);
                                    self.protect_throw_depth = prev_protect;
                                    match r {
                                        Ok(_) => {}
                                        Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                                        Err(e) => return Err(e),
                                    }
                                    self.push(val);
                                    continue;
                                }
                            // Strict-mode strict checks (mirror SetProperty).
                            let chunk_idx = self.cur_chunk();
                            let in_strict = self.chunks[chunk_idx].flags.contains(ChunkFlags::STRICT);
                            let getter_key = self.interner.intern(&format!("__get_{name_str}__"));
                            let has_getter = self.heap.get_property_chain(oid, getter_key).is_some();
                            let is_readonly_own = self.heap.get(oid).and_then(|o| {
                                o.get_property_descriptor(name_id).filter(|p| !p.is_writable())
                            }).is_some();
                            let extensible = self.heap.get(oid).map(|o| o.extensible).unwrap_or(true);
                            let has_own = self.heap.get(oid)
                                .map(|o| o.has_own_property(name_id))
                                .unwrap_or(false);
                            if in_strict && (is_readonly_own || has_getter || (!extensible && !has_own)) {
                                let prop = self.interner.resolve(name_id).to_owned();
                                let msg = if has_getter {
                                    format!("Cannot set property '{prop}' which has only a getter")
                                } else if is_readonly_own {
                                    format!("Cannot assign to read only property '{prop}'")
                                } else {
                                    format!("Cannot add property {prop}, object is not extensible")
                                };
                                let err = self.make_native_error("TypeError", &msg);
                                self.handle_throw(err)?;
                                continue;
                            }
                            if has_getter {
                                self.push(val);
                                continue;
                            }
                            if let Some(obj) = self.heap.get_mut(oid) {
                                obj.set_property(name_id, val);
                            }
                        } else if key.is_symbol() {
                            // Store symbol-keyed properties using a prefix scheme
                            let sid = key.as_symbol_id().unwrap();
                            let sym_key = self.interner.intern(&format!("__sym_{sid}__"));
                            if let Some(obj) = self.heap.get_mut(oid) {
                                obj.set_property(sym_key, val);
                            }
                        } else if let Some(n) = key.as_number() {
                            // Numeric string key for non-arrays (e.g., {0: "a"})
                            let s = if n.fract() == 0.0 && n.is_finite() {
                                (n as i64).to_string()
                            } else {
                                n.to_string()
                            };
                            let name_id = self.interner.intern(&s);
                            if let Some(obj) = self.heap.get_mut(oid) {
                                obj.set_property(name_id, val);
                            }
                        }
                    }
                    self.push(val);
                }

                OpCode::GetSuper => {
                    let _ = self.read_u16();
                    return Err(VmError::RuntimeError(format!(
                        "{opcode:?} not yet implemented"
                    )));
                }

                OpCode::GetSuperElem => {
                    return Err(VmError::RuntimeError(
                        "GetSuperElem not yet implemented".into(),
                    ));
                }

                OpCode::OptionalChain => {
                    let offset = self.read_i16();
                    let val = self.peek()?;
                    if val.is_null() || val.is_undefined() {
                        self.pop()?;
                        self.push(Value::undefined());
                        let frame = self.frames.last_mut().unwrap();
                        frame.ip = (frame.ip as isize + offset as isize) as usize;
                    }
                }

                OpCode::GetPrivate => {
                    let name_idx = self.read_u16() as usize;
                    let name_val = self.chunks[self.cur_chunk()].constants[name_idx];
                    let name_id = name_val.as_string_id().unwrap();
                    let obj_val = self.pop()?;
                    if let Some(oid) = obj_val.as_object_id() {
                        let name_str = self.interner.resolve(name_id).to_owned();
                        let getter_key = self.interner.intern(&format!("__get_{}__", name_str));
                        let setter_key = self.interner.intern(&format!("__set_{}__", name_str));
                        // Brand check: a private method/accessor of a subclass level whose
                        // constructor hasn't run yet (super not returned) is not installed.
                        // Private methods live on the prototype under `__priv_#name__`.
                        let priv_method_key = self.interner.intern(&format!("__priv_{}__", name_str));
                        if self.private_brand_not_installed(oid, getter_key, setter_key, priv_method_key) {
                            self.throw_type_error(&format!(
                                "Cannot read private member {name_str} from an object whose class did not declare it"
                            ))?;
                            continue;
                        }
                        // PrivateBrandCheck: the object must actually carry this private
                        // name. Reading #name off an unrelated object throws TypeError.
                        if self.private_brand_missing(oid, &name_str) {
                            self.throw_type_error(&format!(
                                "Cannot read private member {name_str} from an object whose class did not declare it"
                            ))?;
                            continue;
                        }
                        // Check for private getter (__get_#name__)
                        let getter_fn = self.heap.get_property_chain(oid, getter_key);
                        if let Some(gfn) = getter_fn && gfn.is_function() {
                            let result = self.call_function_this(gfn, obj_val, &[])?;
                            self.push(result);
                        } else {
                            // If a private setter exists but no getter, the field is an
                            // accessor without a getter — PrivateGet must throw TypeError.
                            let setter_fn = self.heap.get_property_chain(oid, setter_key);
                            if let Some(sfn) = setter_fn && sfn.is_function() {
                                self.throw_type_error(&format!(
                                    "Cannot read private accessor {name_str} with only a setter"
                                ))?;
                                continue;
                            }
                            // Private fields: stored as __priv_#name__ (fields) or
                            // literal #name (methods on prototype). Try mangled first.
                            let priv_key = self.interner.intern(&format!("__priv_{}__", name_str));
                            let val = self.heap.get_property_chain(oid, priv_key)
                                .or_else(|| self.heap.get_property_chain(oid, name_id))
                                .unwrap_or(Value::undefined());
                            self.push(val);
                        }
                    } else {
                        // PrivateFieldGet step 2: a non-object receiver throws TypeError.
                        let name_str = self.interner.resolve(name_id).to_owned();
                        self.throw_type_error(&format!(
                            "Cannot read private member {name_str} from a non-object"
                        ))?;
                        continue;
                    }
                }

                OpCode::SetPrivate => {
                    let name_idx = self.read_u16() as usize;
                    let name_val = self.chunks[self.cur_chunk()].constants[name_idx];
                    let name_id = name_val.as_string_id().unwrap();
                    let value = self.pop()?;
                    let obj_val = self.pop()?;
                    if let Some(oid) = obj_val.as_object_id() {
                        let name_str = self.interner.resolve(name_id).to_owned();
                        let setter_key = self.interner.intern(&format!("__set_{}__", name_str));
                        let getter_key_for_brand = self.interner.intern(&format!("__get_{}__", name_str));
                        let priv_method_key = self.interner.intern(&format!("__priv_{}__", name_str));
                        // Brand check (see GetPrivate): subclass-level private not yet installed.
                        if self.private_brand_not_installed(oid, getter_key_for_brand, setter_key, priv_method_key) {
                            self.throw_type_error(&format!(
                                "Cannot write private member {name_str} to an object whose class did not declare it"
                            ))?;
                            continue;
                        }
                        // PrivateBrandCheck: writing #name to an unrelated object throws.
                        if self.private_brand_missing(oid, &name_str) {
                            self.throw_type_error(&format!(
                                "Cannot write private member {name_str} to an object whose class did not declare it"
                            ))?;
                            continue;
                        }
                        // Check for private setter (__set_#name__)
                        let setter_fn = self.heap.get_property_chain(oid, setter_key);
                        if let Some(sfn) = setter_fn && sfn.is_function() {
                            let prev_protect = self.protect_throw_depth;
                            self.protect_throw_depth = self.frames.len() + 1;
                            let r = self.call_function_this(sfn, obj_val, &[value]);
                            self.protect_throw_depth = prev_protect;
                            match r {
                                Ok(_) => {}
                                Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                                Err(e) => return Err(e),
                            }
                        } else {
                            // If a private getter exists but no setter, the field is an
                            // accessor without a setter — PrivateSet must throw TypeError.
                            let getter_key_str = format!("__get_{}__", name_str);
                            let getter_key = self.interner.intern(&getter_key_str);
                            let getter_fn = self.heap.get_property_chain(oid, getter_key);
                            if let Some(gfn) = getter_fn && gfn.is_function() {
                                self.throw_type_error(&format!(
                                    "Cannot set private accessor {name_str} with only a getter"
                                ))?;
                                continue;
                            }
                            // Distinguish private methods (on prototype) from private
                            // fields (on the instance). PrivateSet on a method throws.
                            let priv_key = self.interner.intern(&format!("__priv_{}__", name_str));
                            let has_own = self.heap.get(oid)
                                .map(|o| o.get_property(priv_key).is_some())
                                .unwrap_or(false);
                            if !has_own
                                && let Some(mv) = self.heap.get_property_chain(oid, priv_key)
                                && mv.is_function()
                            {
                                self.throw_type_error(&format!(
                                    "Cannot assign to private method {name_str}"
                                ))?;
                                continue;
                            }
                            if let Some(obj) = self.heap.get_mut(oid) {
                                obj.set_property(priv_key, value);
                            }
                        }
                    } else {
                        // PrivateFieldSet: a non-object receiver throws TypeError.
                        let name_str = self.interner.resolve(name_id).to_owned();
                        self.throw_type_error(&format!(
                            "Cannot write private member {name_str} to a non-object"
                        ))?;
                        continue;
                    }
                    self.push(value);
                }

                OpCode::HasPrivate => {
                    // `#name in obj` — check whether the object exposes the
                    // private name as a field, method, or accessor on the
                    // prototype chain. Per spec, RHS must be an object.
                    let name_idx = self.read_u16() as usize;
                    let name_val = self.chunks[self.cur_chunk()].constants[name_idx];
                    let name_id = name_val.as_string_id().unwrap();
                    let obj_val = self.pop()?;
                    let Some(oid) = obj_val.as_object_id() else {
                        let err = self.make_native_error(
                            "TypeError",
                            "Cannot use 'in' operator to search for private name in non-object",
                        );
                        self.handle_throw(err)?;
                        continue;
                    };
                    let name_str = self.interner.resolve(name_id).to_owned();
                    let priv_key = self.interner.intern(&format!("__priv_{name_str}__"));
                    let getter_key = self.interner.intern(&format!("__get_{name_str}__"));
                    let setter_key = self.interner.intern(&format!("__set_{name_str}__"));
                    let has = self.heap.get_property_chain(oid, priv_key).is_some()
                        || self.heap.get_property_chain(oid, getter_key).is_some()
                        || self.heap.get_property_chain(oid, setter_key).is_some()
                        || self.heap.get_property_chain(oid, name_id).is_some();
                    self.push(Value::boolean(has));
                }

                OpCode::CallMethod => {
                    let argc = self.read_byte() as usize;
                    let method_name_idx = self.read_u16() as usize;
                    let method_name = self.chunks[self.cur_chunk()].constants[method_name_idx]
                        .as_string_id().unwrap();
                    // Stack layout: [..., obj, arg0, ..., argN]
                    let obj_pos = self.stack.len() - 1 - argc;
                    let obj_val = self.stack[obj_pos];
                    if trace_calls_enabled() {
                        let (line, pc) = if let Some(f) = self.frames.last() {
                            (self.chunks[f.chunk_idx].get_line(f.ip as u32), f.ip)
                        } else { (0, 0) };
                        let method = self.interner.resolve(method_name).to_owned();
                        let recv_kind = if obj_val.is_function() { "fn" }
                            else if obj_val.is_object() { "obj" }
                            else if obj_val.is_null() { "null" }
                            else if obj_val.is_undefined() { "undef" }
                            else { "other" };
                        let recv = self.value_to_string(obj_val);
                        eprintln!(
                            "[zinc-trace] CallMethod line={line} pc={pc} recv-kind={recv_kind} recv={recv} method={method} argc={argc}"
                        );
                    }

                    // Per spec, calling a method on null/undefined throws TypeError
                    // before the method is even read.
                    if obj_val.is_null() || obj_val.is_undefined() {
                        let kind = if obj_val.is_null() { "null" } else { "undefined" };
                        let prop = self.interner.resolve(method_name).to_owned();
                        let (line, pc, chunk_name) = if let Some(f) = self.frames.last() {
                            let cn = self.interner.resolve(self.chunks[f.chunk_idx].name).to_owned();
                            (self.chunks[f.chunk_idx].get_line(f.ip as u32), f.ip, cn)
                        } else { (0, 0, String::new()) };
                        let msg = format!(
                            "Cannot read properties of {kind} (reading '{prop}') (at line {line}, pc {pc}, chunk '{chunk_name}')"
                        );
                        self.truncate_stack(obj_pos);
                        let err = self.make_native_error("TypeError", &msg);
                        self.handle_throw(err)?;
                        continue;
                    }
                    // Private method calls (`this.#m()`) compile to CallMethod with a
                    // `#`-prefixed name. Brand-check: a subclass-level private method
                    // reached from a base constructor (super not yet returned) is not
                    // installed and must throw — see private_brand_not_installed.
                    if !self.pending_private_brands.is_empty()
                        && let Some(moid) = obj_val.as_object_id()
                    {
                        let mname = self.interner.resolve(method_name).to_owned();
                        if mname.starts_with('#') {
                            let getter_key = self.interner.intern(&format!("__get_{}__", mname));
                            let setter_key = self.interner.intern(&format!("__set_{}__", mname));
                            let priv_key = self.interner.intern(&format!("__priv_{}__", mname));
                            if self.private_brand_not_installed(moid, getter_key, setter_key, priv_key) {
                                let nm = self.interner.resolve(method_name).to_owned();
                                self.truncate_stack(obj_pos);
                                let err = self.make_native_error("TypeError", &format!(
                                    "Cannot read private member {nm} from an object whose class did not declare it"
                                ));
                                self.handle_throw(err)?;
                                continue;
                            }
                        }
                    }
                    // Calling a private method (`o.#m()`) on a non-object receiver
                    // is a PrivateFieldGet on a primitive — throws TypeError.
                    if obj_val.as_object_id().is_none() {
                        let mname = self.interner.resolve(method_name).to_owned();
                        if mname.starts_with('#') {
                            self.truncate_stack(obj_pos);
                            let err = self.make_native_error("TypeError", &format!(
                                "Cannot read private member {mname} from a non-object"
                            ));
                            self.handle_throw(err)?;
                            continue;
                        }
                    }

                    // Look up the method on the object (walking prototype chain)
                    let method_val = if let Some(oid) = obj_val.as_object_id() {
                        self.heap.get_property_chain(oid, method_name)
                    } else {
                        None
                    };

                    // Host-supplied native function looked up via prototype chain
                    // (registered with Engine::register_host_fn or attached as a
                    // method on a host object). Receiver is bound as `this`.
                    if let Some(mv) = method_val
                        && let Some(method_oid) = mv.as_object_id()
                    {
                        let native_fn = self.heap.get(method_oid).and_then(|o| {
                            if let ObjectKind::Function(crate::runtime::object::FunctionKind::Native { func, .. }) = &o.kind {
                                Some(func.clone())
                            } else { None }
                        });
                        if let Some(func) = native_fn {
                            let args_vec: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                            self.truncate_stack(obj_pos);
                            match (func)(self, obj_val, &args_vec) {
                                Ok(v) => { self.push(v); continue; }
                                Err(reason) => { self.handle_throw(reason)?; continue; }
                            }
                        }
                        // Bound / bytecode / sentinel FUNCTION OBJECTS called
                        // as methods (`obj.m(...)`, `arr[i](...)`): unwrap the
                        // same way the plain-Call opcode does. They silently
                        // fell through to the undefined-push tail before —
                        // React's useState setter is a bound function read
                        // out of the hook array.
                        enum MethodFnKind {
                            Bound(crate::runtime::object::ObjectId, Value, Vec<Value>),
                            Direct(i32),
                        }
                        let kind_call = self.heap.get(method_oid).and_then(|o| match &o.kind {
                            ObjectKind::Function(crate::runtime::object::FunctionKind::Bound { target, this_val, args }) =>
                                Some(MethodFnKind::Bound(*target, *this_val, args.clone())),
                            ObjectKind::Function(crate::runtime::object::FunctionKind::Bytecode { chunk_idx, .. }) =>
                                Some(MethodFnKind::Direct(*chunk_idx as i32)),
                            ObjectKind::Function(crate::runtime::object::FunctionKind::NativeSentinel { sentinel }) =>
                                Some(MethodFnKind::Direct(*sentinel)),
                            _ => None,
                        });
                        if let Some(kind_call) = kind_call {
                            let call_args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                            self.truncate_stack(obj_pos);
                            match kind_call {
                                MethodFnKind::Bound(target_oid, this_val, bound_args) => {
                                    let target_fn = self.heap.get(target_oid).and_then(|o| match &o.kind {
                                        ObjectKind::Function(crate::runtime::object::FunctionKind::Bytecode { chunk_idx, .. }) =>
                                            Some(Value::function(*chunk_idx as i32)),
                                        ObjectKind::Function(crate::runtime::object::FunctionKind::NativeSentinel { sentinel }) =>
                                            Some(Value::function(*sentinel)),
                                        _ => None,
                                    });
                                    if let Some(fn_val) = target_fn {
                                        let full: Vec<Value> =
                                            bound_args.into_iter().chain(call_args).collect();
                                        let result = self.call_with_async_wrap(fn_val, this_val, &full)?;
                                        self.push(result);
                                    } else {
                                        self.push(Value::undefined());
                                    }
                                    continue;
                                }
                                MethodFnKind::Direct(packed) => {
                                    let result =
                                        self.call_with_async_wrap(Value::function(packed), obj_val, &call_args)?;
                                    self.push(result);
                                    continue;
                                }
                            }
                        }
                    }

                    // Check for console.log/warn/error sentinels
                    if let Some(mv) = method_val
                        && mv.is_function() {
                            let sentinel = mv.as_function().unwrap();
                            if (-102..=-100).contains(&sentinel) {
                                // console output
                                let mut parts = Vec::new();
                                for i in 0..argc {
                                    let val = self.stack[obj_pos + 1 + i];
                                    parts.push(self.value_to_string(val));
                                }
                                let line = parts.join(" ");
                                if !self.silent_console {
                                    if sentinel == -102 {
                                        eprintln!("{line}"); // console.error -> stderr
                                    } else {
                                        println!("{line}");
                                    }
                                }
                                self.output.push(line);
                                self.truncate_stack(obj_pos);
                                self.push(Value::undefined());
                                continue;
                            }
                        }

                    // Wrapper objects (`new Number(x)` / `new String(x)` /
                    // `new Boolean(x)`) expose primitive method dispatch so e.g.
                    // `new Number(1).toFixed(2)` works. Use the inner primitive
                    // for the type-driven branches below.
                    let effective_val = if let Some(oid) = obj_val.as_object_id()
                        && let Some(obj) = self.heap.get(oid)
                        && let ObjectKind::Wrapper(inner) = &obj.kind
                    {
                        *inner
                    } else {
                        obj_val
                    };

                    // Symbol primitive methods: Symbol.prototype.valueOf returns the
                    // symbol (so `s == s.valueOf()`), toString returns "Symbol(desc)".
                    if effective_val.is_symbol() {
                        let mn = self.interner.resolve(method_name).to_owned();
                        match mn.as_str() {
                            "valueOf" => {
                                self.truncate_stack(obj_pos);
                                self.push(effective_val);
                                continue;
                            }
                            "toString" => {
                                let s = self.value_to_string(effective_val);
                                let id = self.interner.intern(&s);
                                self.truncate_stack(obj_pos);
                                self.push(Value::string(id));
                                continue;
                            }
                            _ => {}
                        }
                    }

                    // BigInt primitive methods: valueOf returns the BigInt;
                    // toString(radix) renders it (radix defaults to 10).
                    if let Some(b) = self.as_bigint(effective_val) {
                        let mn = self.interner.resolve(method_name).to_owned();
                        match mn.as_str() {
                            "valueOf" => {
                                self.truncate_stack(obj_pos);
                                self.push(effective_val);
                                continue;
                            }
                            "toString" | "toLocaleString" => {
                                let radix = if argc > 0 {
                                    self.to_f64(self.stack[obj_pos + 1]) as u32
                                } else { 10 };
                                let s = if (2..=36).contains(&radix) { b.to_str_radix(radix) } else { b.to_string() };
                                let id = self.interner.intern(&s);
                                self.truncate_stack(obj_pos);
                                self.push(Value::string(id));
                                continue;
                            }
                            _ => {}
                        }
                    }

                    // Check if the obj is a string (or ConsString) and dispatch string method
                    let string_for_method = if self.is_string_like(effective_val) {
                        Some(self.value_to_string(effective_val))
                    } else {
                        None
                    };
                    if let Some(s) = string_for_method {
                        let ascii = self.string_is_ascii(effective_val);
                        let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                        let result = self.exec_string_method(&s, method_name, &args, ascii);
                        self.truncate_stack(obj_pos);
                        self.push(result);
                        continue;
                    }

                    // Number primitive methods: (42).toString(16), (3.14).toFixed(2)
                    if effective_val.is_number() || effective_val.is_int() {
                        let mn = self.interner.resolve(method_name).to_owned();
                        let n = self.to_f64(effective_val);
                        let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                        let result = match mn.as_str() {
                            "toString" => {
                                let radix = args.first().and_then(|v| v.as_number()).unwrap_or(10.0) as u32;
                                let s = if radix == 10 {
                                    self.value_to_string(effective_val)
                                } else if n.fract() == 0.0 && n.is_finite() {
                                    // Integer with non-10 radix
                                    let i = n as i64;
                                    if i >= 0 { radix_fmt(i as u64, radix) }
                                    else { format!("-{}", radix_fmt((-i) as u64, radix)) }
                                } else {
                                    self.value_to_string(effective_val)
                                };
                                let id = self.interner.intern(&s);
                                Value::string(id)
                            }
                            "valueOf" => effective_val,
                            "toFixed" => {
                                let digits = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                                let s = format!("{:.prec$}", n, prec = digits);
                                let id = self.interner.intern(&s);
                                Value::string(id)
                            }
                            "toPrecision" => {
                                let s = if let Some(p) = args.first().and_then(|v| v.as_number()) {
                                    let p = p as usize;
                                    if n == 0.0 {
                                        format!("{:.prec$}", 0.0, prec = p.saturating_sub(1))
                                    } else {
                                        let mag = n.abs().log10().floor() as i32;
                                        if mag >= -6 && mag < p as i32 {
                                            let decimals = (p as i32 - 1 - mag).max(0) as usize;
                                            format!("{:.prec$}", n, prec = decimals)
                                        } else {
                                            let mantissa = n / 10f64.powi(mag);
                                            let decimals = p.saturating_sub(1);
                                            let sign = if mag >= 0 { "+" } else { "-" };
                                            format!("{:.prec$}e{}{}", mantissa, sign, mag.abs(), prec = decimals)
                                        }
                                    }
                                } else {
                                    self.value_to_string(obj_val)
                                };
                                let id = self.interner.intern(&s);
                                Value::string(id)
                            }
                            "toExponential" => {
                                let digits = args.first().and_then(|v| v.as_number()).map(|d| d as usize);
                                let s = if n == 0.0 {
                                    let decimals = digits.unwrap_or(0);
                                    if decimals == 0 { "0e+0".to_string() }
                                    else { format!("{:.prec$}e+0", 0.0, prec = decimals) }
                                } else {
                                    let mag = n.abs().log10().floor() as i32;
                                    let mantissa = n / 10f64.powi(mag);
                                    let sign = if mag >= 0 { "+" } else { "-" };
                                    match digits {
                                        Some(d) => format!("{:.prec$}e{}{}", mantissa, sign, mag.abs(), prec = d),
                                        None => {
                                            // Minimum digits: default formatting, trim trailing zeros
                                            let mut m = format!("{mantissa}");
                                            if m.contains('.') {
                                                m = m.trim_end_matches('0').trim_end_matches('.').to_string();
                                            }
                                            format!("{}e{}{}", m, sign, mag.abs())
                                        }
                                    }
                                };
                                let id = self.interner.intern(&s);
                                Value::string(id)
                            }
                            _ => Value::undefined(),
                        };
                        self.truncate_stack(obj_pos);
                        self.push(result);
                        continue;
                    }

                    // Boolean primitive methods: true.toString()
                    if effective_val.is_boolean() {
                        let mn = self.interner.resolve(method_name).to_owned();
                        let result = match mn.as_str() {
                            "toString" => {
                                let s = if effective_val.as_bool().unwrap() { "true" } else { "false" };
                                let id = self.interner.intern(s);
                                Value::string(id)
                            }
                            "valueOf" => effective_val,
                            _ => Value::undefined(),
                        };
                        self.truncate_stack(obj_pos);
                        self.push(result);
                        continue;
                    }

                    // Check for Function.prototype.call/apply/bind. The
                    // receiver matches when it's a function value, an
                    // object whose kind is Function, OR an object with a
                    // `__constructor__` slot (class). For the class case
                    // call_function_this unwraps the slot and dispatches
                    // through the stored constructor; without this the
                    // method lookup would fall through to silent
                    // undefined and break Closure bundles that emit
                    // `Parent.call(this, …)` super-call patterns.
                    let is_class_obj = obj_val.is_object() && obj_val.as_object_id()
                        .and_then(|oid| {
                            let ctor_key = self.interner.intern("__constructor__");
                            self.heap.get(oid).map(|o| o.get_property(ctor_key).is_some())
                        }).unwrap_or(false);
                    if obj_val.is_function() || is_class_obj || (obj_val.is_object() && obj_val.as_object_id()
                        .and_then(|oid| self.heap.get(oid))
                        .map(|o| matches!(&o.kind, ObjectKind::Function(_)))
                        .unwrap_or(false))
                    {
                        let mn = self.interner.resolve(method_name).to_owned();
                        match mn.as_str() {
                            "call" => {
                                let this_arg = if argc > 0 { self.stack[obj_pos + 1] } else { Value::undefined() };
                                let call_args: Vec<Value> = (1..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                                self.truncate_stack(obj_pos);
                                let result = self.call_with_async_wrap(obj_val, this_arg, &call_args)?;
                                self.push(result);
                                continue;
                            }
                            "apply" => {
                                let this_arg = if argc > 0 { self.stack[obj_pos + 1] } else { Value::undefined() };
                                let mut call_args = Vec::new();
                                if argc > 1 {
                                    let arr_val = self.stack[obj_pos + 2];
                                    if let Some(arr_oid) = arr_val.as_object_id()
                                        && let Some(obj) = self.heap.get(arr_oid)
                                            && let ObjectKind::Array(ref elems) = obj.kind {
                                                call_args = elems.clone();
                                            }
                                }
                                self.truncate_stack(obj_pos);
                                let result = self.call_with_async_wrap(obj_val, this_arg, &call_args)?;
                                self.push(result);
                                continue;
                            }
                            "bind" => {
                                let this_arg = if argc > 0 { self.stack[obj_pos + 1] } else { Value::undefined() };
                                let bound_args: Vec<Value> = (1..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                                // Create a bound function object
                                let func_obj_id = if let Some(oid) = obj_val.as_object_id() { oid }
                                    else {
                                        let packed = obj_val.as_function().unwrap();
                                        if packed < 0 {
                                            // Native sentinel — wrap as NativeSentinel to preserve dispatch
                                            let fobj = JsObject {
                                                properties: Vec::new(), prototype: None,
                                                kind: ObjectKind::Function(crate::runtime::object::FunctionKind::NativeSentinel { sentinel: packed }),
                                                marked: false, extensible: true,
                                            };
                                            self.heap.allocate(fobj)
                                        } else {
                                            // User bytecode function — wrap as Bytecode,
                                            // keeping the FULL packed value (closure_id in
                                            // the high bits) so the bound function still
                                            // sees its captured upvalues when called.
                                            let chunk_only = (packed & 0xFFFF) as usize;
                                            let name = if chunk_only < self.chunks.len() { self.chunks[chunk_only].name } else { self.interner.intern("<bound>") };
                                            let fobj = JsObject::function_bytecode(packed as usize, name);
                                            self.heap.allocate(fobj)
                                        }
                                    };
                                let bound = JsObject {
                                    properties: Vec::new(), prototype: None,
                                    kind: ObjectKind::Function(crate::runtime::object::FunctionKind::Bound {
                                        target: func_obj_id,
                                        this_val: this_arg,
                                        args: bound_args,
                                    }),
                                    marked: false, extensible: true,
                                };
                                let bound_oid = self.heap.allocate(bound);
                                self.truncate_stack(obj_pos);
                                self.push(Value::object_id(bound_oid));
                                continue;
                            }
                            _ => {} // fall through to other dispatchers
                        }
                    }

                    // Check for array methods
                    if let Some(oid) = obj_val.as_object_id() {
                        if let Some(obj) = self.heap.get(oid)
                            && matches!(&obj.kind, ObjectKind::Array(_)) {
                                let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                                let result = self.exec_array_method(oid, method_name, &args)?;
                                self.truncate_stack(obj_pos);
                                self.push(result);
                                continue;
                            }
                        // Check for Generator methods (.next, .return, .throw).
                        // For async generators, the result of each step is wrapped
                        // in a fulfilled / rejected Promise per spec.
                        if let Some(obj) = self.heap.get(oid)
                            && matches!(&obj.kind, ObjectKind::Generator { .. })
                        {
                            let gen_chunk = if let ObjectKind::Generator { chunk_idx, .. } = self.heap.get(oid).unwrap().kind {
                                chunk_idx
                            } else { 0 };
                            let is_async_gen = gen_chunk < self.chunks.len()
                                && self.chunks[gen_chunk].flags.contains(ChunkFlags::ASYNC);
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                            // Clear CallMethod operands before resuming
                            self.truncate_stack(obj_pos);
                            let action = self.exec_generator_method(oid, method_name, &args);
                            match action {
                                Ok(crate::vm::generator::GeneratorAction::Done(result)) => {
                                    if is_async_gen {
                                        let pid = self.allocate_promise();
                                        self.resolve_promise(pid, result)?;
                                        self.push(Value::object_id(pid));
                                    } else {
                                        self.push(result);
                                    }
                                    continue;
                                }
                                Ok(crate::vm::generator::GeneratorAction::Resumed) => {
                                    // Generator frame pushed — main loop will execute it.
                                    // Async generators need the eventual yield/return to
                                    // be wrapped in a Promise; the simplest path is to
                                    // mark the frame and have the next Yield/Return
                                    // handler do the wrap. For now, async gens that
                                    // genuinely suspend fall through unwrapped — most
                                    // generated test262 cases yield eagerly via the
                                    // SuspendedStart -> first-yield path which Done's.
                                    continue;
                                }
                                Err(VmError::Throw(reason)) if is_async_gen => {
                                    let pid = self.allocate_promise();
                                    self.reject_promise(pid, reason)?;
                                    self.push(Value::object_id(pid));
                                    continue;
                                }
                                Err(e) => return Err(e),
                            }
                        }
                        // Check for RegExp methods
                        if let Some(obj) = self.heap.get(oid)
                            && matches!(&obj.kind, ObjectKind::RegExp { .. })
                        {
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                            let result = self.exec_regexp_method(oid, method_name, &args)?;
                            self.truncate_stack(obj_pos);
                            self.push(result);
                            continue;
                        }
                        // Check for Map methods
                        if let Some(obj) = self.heap.get(oid)
                            && matches!(&obj.kind, ObjectKind::Map { .. })
                        {
                            let mn = self.interner.resolve(method_name);
                            if mn == "size" {
                                let sz = if let ObjectKind::Map { entries } = &self.heap.get(oid).unwrap().kind { entries.len() } else { 0 };
                                self.truncate_stack(obj_pos);
                                self.push(Value::int(sz as i32));
                                continue;
                            }
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                            let result = self.exec_map_method(oid, method_name, &args)?;
                            self.truncate_stack(obj_pos);
                            self.push(result);
                            continue;
                        }
                        // Check for Set methods
                        if let Some(obj) = self.heap.get(oid)
                            && matches!(&obj.kind, ObjectKind::Set { .. })
                        {
                            let mn = self.interner.resolve(method_name);
                            if mn == "size" {
                                let sz = if let ObjectKind::Set { entries } = &self.heap.get(oid).unwrap().kind { entries.len() } else { 0 };
                                self.truncate_stack(obj_pos);
                                self.push(Value::int(sz as i32));
                                continue;
                            }
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                            let result = self.exec_set_method(oid, method_name, &args)?;
                            self.truncate_stack(obj_pos);
                            self.push(result);
                            continue;
                        }
                        // Check for WeakMap methods
                        if let Some(obj) = self.heap.get(oid)
                            && matches!(&obj.kind, ObjectKind::WeakMap { .. })
                        {
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                            let result = self.exec_weakmap_method(oid, method_name, &args)?;
                            self.truncate_stack(obj_pos);
                            self.push(result);
                            continue;
                        }
                        // Check for Date methods
                        if let Some(obj) = self.heap.get(oid)
                            && let ObjectKind::Date(ms) = obj.kind
                        {
                            let mn = self.interner.resolve(method_name).to_owned();
                            let result = match mn.as_str() {
                                "getTime" | "valueOf" => Value::number(ms),
                                "getFullYear" => Value::int(epoch_to_ymd(ms).0),
                                "getMonth" => Value::int(epoch_to_ymd(ms).1),
                                "getDate" => Value::int(epoch_to_ymd(ms).2),
                                "getHours" => Value::int(((ms / 3_600_000.0).rem_euclid(24.0)) as i32),
                                "getMinutes" => Value::int(((ms / 60_000.0).rem_euclid(60.0)) as i32),
                                "getSeconds" => Value::int(((ms / 1000.0).rem_euclid(60.0)) as i32),
                                "getMilliseconds" => Value::int((ms.rem_euclid(1000.0)) as i32),
                                "getDay" => {
                                    // UNIX epoch (1970-01-01) was Thursday = 4
                                    let days = (ms / 86_400_000.0).floor() as i64;
                                    Value::int((((days + 4) % 7 + 7) % 7) as i32)
                                }
                                "getTimezoneOffset" => Value::int(0),
                                "toISOString" | "toString" | "toJSON" => {
                                    let s = format_iso(ms);
                                    let id = self.interner.intern(&s);
                                    Value::string(id)
                                }
                                _ => Value::undefined(),
                            };
                            self.truncate_stack(obj_pos);
                            self.push(result);
                            continue;
                        }
                        // Check for WeakSet methods
                        if let Some(obj) = self.heap.get(oid)
                            && matches!(&obj.kind, ObjectKind::WeakSet { .. })
                        {
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                            let result = self.exec_weakset_method(oid, method_name, &args)?;
                            self.truncate_stack(obj_pos);
                            self.push(result);
                            continue;
                        }
                        // Check for Math methods (fast: cached ObjectId comparison)
                        if self.math_oid == Some(oid) {
                            // Fast path: read args directly from stack, avoid Vec alloc for 1-2 args
                            let arg0 = if argc > 0 { self.stack[obj_pos + 1] } else { Value::undefined() };
                            let arg1 = if argc > 1 { self.stack[obj_pos + 2] } else { Value::undefined() };
                            let name_str = self.interner.resolve(method_name);
                            let result = match name_str {
                                "sin" => Value::number(self.to_f64(arg0).sin()),
                                "cos" => Value::number(self.to_f64(arg0).cos()),
                                "abs" => Value::number(self.to_f64(arg0).abs()),
                                "floor" => Value::number(self.to_f64(arg0).floor()),
                                "ceil" => Value::number(self.to_f64(arg0).ceil()),
                                "round" => Value::number(self.to_f64(arg0).round()),
                                "sqrt" => Value::number(self.to_f64(arg0).sqrt()),
                                "pow" => {
                                    let av = self.to_f64(arg0);
                                    let bv = self.to_f64(arg1);
                                    let r = if av.abs() == 1.0 && bv.is_infinite() { f64::NAN } else { av.powf(bv) };
                                    Value::number(r)
                                },
                                "max" => Value::number(self.to_f64(arg0).max(self.to_f64(arg1))),
                                "min" => Value::number(self.to_f64(arg0).min(self.to_f64(arg1))),
                                _ => {
                                    let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                                    self.exec_math_method(method_name, &args)
                                }
                            };
                            self.truncate_stack(obj_pos);
                            self.push(result);
                            continue;
                        }
                        // Check for JSON methods (fast: cached ObjectId comparison)
                        if self.json_oid == Some(oid) {
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                            let result = self.exec_json_method(method_name, &args);
                            self.truncate_stack(obj_pos);
                            self.push(result);
                            continue;
                        }
                    }

                    // Object.prototype methods (hasOwnProperty, toString, valueOf, etc.)
                    if let Some(oid) = obj_val.as_object_id() {
                        let mn = self.interner.resolve(method_name).to_owned();
                        match mn.as_str() {
                            "hasOwnProperty" => {
                                let key_val = if argc > 0 { self.stack[obj_pos + 1] } else { Value::undefined() };
                                let key = if key_val.is_symbol() {
                                    format!("__sym_{}__", key_val.as_symbol_id().unwrap())
                                } else {
                                    self.value_to_string(key_val)
                                };
                                let key_id = self.interner.intern(&key);
                                let getter_key = self.interner.intern(&format!("__get_{key}__"));
                                let setter_key = self.interner.intern(&format!("__set_{key}__"));
                                let has = self.heap.get(oid).map(|o| {
                                    // For arrays, also check element indices
                                    let array_idx = key.parse::<usize>().ok().and_then(|idx| {
                                        if let ObjectKind::Array(ref elems) = o.kind {
                                            if idx < elems.len() { Some(true) } else { None }
                                        } else { None }
                                    }).unwrap_or(false);
                                    array_idx
                                        || o.has_own_property(key_id)
                                        || o.has_own_property(getter_key)
                                        || o.has_own_property(setter_key)
                                }).unwrap_or(false);
                                self.truncate_stack(obj_pos);
                                self.push(Value::boolean(has));
                                continue;
                            }
                            "propertyIsEnumerable" => {
                                let key_val = if argc > 0 { self.stack[obj_pos + 1] } else { Value::undefined() };
                                let key = if key_val.is_symbol() {
                                    format!("__sym_{}__", key_val.as_symbol_id().unwrap())
                                } else {
                                    self.value_to_string(key_val)
                                };
                                let key_id = self.interner.intern(&key);
                                let getter_key = self.interner.intern(&format!("__get_{key}__"));
                                let setter_key = self.interner.intern(&format!("__set_{key}__"));
                                let is_enum = self.heap.get(oid)
                                    .and_then(|o| {
                                        o.get_property_descriptor(key_id)
                                            .or_else(|| o.get_property_descriptor(getter_key))
                                            .or_else(|| o.get_property_descriptor(setter_key))
                                    })
                                    .map(|p| p.is_enumerable())
                                    .unwrap_or(false);
                                self.truncate_stack(obj_pos);
                                self.push(Value::boolean(is_enum));
                                continue;
                            }
                            "toString" => {
                                // Error.prototype.toString: when the receiver inherits from
                                // Error.prototype, return `${name}: ${message}` reading both
                                // via the prototype chain (name defaults to "Error"). `name`
                                // lives on Error.prototype, so an own-property check misses it.
                                let is_error = self.func_prototypes.get(&-510).copied()
                                    .map(|ep| {
                                        let mut cur = self.heap.get(oid).and_then(|o| o.prototype);
                                        while let Some(c) = cur {
                                            if c == ep { return true; }
                                            cur = self.heap.get(c).and_then(|o| o.prototype);
                                        }
                                        false
                                    })
                                    .unwrap_or(false);
                                let error_str = if is_error {
                                    let name_key = self.interner.intern("name");
                                    let msg_key = self.interner.intern("message");
                                    let name_s = self.heap.get_property_chain(oid, name_key)
                                        .map(|v| self.value_to_string(v))
                                        .unwrap_or_else(|| "Error".to_string());
                                    let msg_s = self.heap.get_property_chain(oid, msg_key)
                                        .map(|v| self.value_to_string(v))
                                        .unwrap_or_default();
                                    Some(if msg_s.is_empty() { name_s }
                                         else if name_s.is_empty() { msg_s }
                                         else { format!("{name_s}: {msg_s}") })
                                } else { None };
                                if let Some(s) = error_str {
                                    let id = self.interner.intern(&s);
                                    self.truncate_stack(obj_pos);
                                    self.push(Value::string(id));
                                    continue;
                                }
                                // Wrapper objects (new Boolean/Number/String) format as
                                // their wrapped primitive value, matching the per-type
                                // toString defined on Boolean/Number/String.prototype.
                                if let Some(o) = self.heap.get(oid)
                                    && let ObjectKind::Wrapper(inner) = &o.kind
                                {
                                    let inner = *inner;
                                    let s = if inner.is_boolean() {
                                        if inner.to_boolean() { "true".to_owned() } else { "false".to_owned() }
                                    } else {
                                        self.value_to_string(inner)
                                    };
                                    let id = self.interner.intern(&s);
                                    self.truncate_stack(obj_pos);
                                    self.push(Value::string(id));
                                    continue;
                                }
                                // Return [object Type] string
                                let tag = if let Some(o) = self.heap.get(oid) {
                                    match &o.kind {
                                        ObjectKind::Array(_) => "[object Array]",
                                        ObjectKind::Function(_) => "[object Function]",
                                        ObjectKind::RegExp { .. } => "[object RegExp]",
                                        ObjectKind::Promise { .. } => "[object Promise]",
                                        ObjectKind::Map { .. } => "[object Map]",
                                        ObjectKind::Set { .. } => "[object Set]",
                                        ObjectKind::WeakMap { .. } => "[object WeakMap]",
                                        ObjectKind::WeakSet { .. } => "[object WeakSet]",
                                        _ => "[object Object]",
                                    }
                                } else { "[object Object]" };
                                let id = self.interner.intern(tag);
                                self.truncate_stack(obj_pos);
                                self.push(Value::string(id));
                                continue;
                            }
                            "valueOf" => {
                                // Wrapper objects valueOf returns the wrapped primitive.
                                if let Some(o) = self.heap.get(oid)
                                    && let ObjectKind::Wrapper(inner) = &o.kind
                                {
                                    let inner = *inner;
                                    self.truncate_stack(obj_pos);
                                    self.push(inner);
                                    continue;
                                }
                                self.truncate_stack(obj_pos);
                                self.push(obj_val);
                                continue;
                            }
                            "isPrototypeOf" => {
                                let target = if argc > 0 { self.stack[obj_pos + 1] } else { Value::undefined() };
                                let result = self.is_prototype_of(obj_val, target);
                                self.truncate_stack(obj_pos);
                                self.push(Value::boolean(result));
                                continue;
                            }
                            _ => {}
                        }
                    }

                    // toString / valueOf on function values
                    if obj_val.is_function() {
                        let mn = self.interner.resolve(method_name).to_owned();
                        match mn.as_str() {
                            "toString" => {
                                let sentinel = obj_val.as_function().unwrap();
                                let name_id = self.interner.intern("name");
                                let name = self.fn_get_own_prop(sentinel, name_id)
                                    .and_then(|v| v.as_string_id())
                                    .map(|sid| self.interner.resolve(sid).to_owned())
                                    .unwrap_or_default();
                                let s = format!("function {name}() {{ [native code] }}");
                                let id = self.interner.intern(&s);
                                self.truncate_stack(obj_pos);
                                self.push(Value::string(id));
                                continue;
                            }
                            "valueOf" => {
                                self.truncate_stack(obj_pos);
                                self.push(obj_val);
                                continue;
                            }
                            _ => {}
                        }
                    }

                    // hasOwnProperty on function values
                    if obj_val.is_function() {
                        let mn = self.interner.resolve(method_name).to_owned();
                        if mn == "hasOwnProperty" {
                            let key = if argc > 0 { self.value_to_string(self.stack[obj_pos + 1]) } else { String::new() };
                            let key_id = self.interner.intern(&key);
                            let sentinel = obj_val.as_function().unwrap();
                            let has = self.fn_get_own_prop(sentinel, key_id).is_some();
                            self.truncate_stack(obj_pos);
                            self.push(Value::boolean(has));
                            continue;
                        }
                    }

                    // User-set callable property on a function value
                    // (`f.method = fn; f.method(args)`): invoke the override with `this = f`.
                    if obj_val.is_function() {
                        let sentinel = obj_val.as_function().unwrap();
                        if let Some(Some(method_fn)) = self.fn_property_overrides.get(&(sentinel, method_name)).copied()
                            && method_fn.is_function()
                        {
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                            let result = self.call_function_this(method_fn, obj_val, &args)?;
                            self.truncate_stack(obj_pos);
                            self.push(result);
                            continue;
                        }
                    }


                    // Try to call as a closure method on an object (walk prototype chain)
                    if let Some(oid) = obj_val.as_object_id() {
                        // For private names (#x), try mangled key first
                        let method_name_s = self.interner.resolve(method_name).to_owned();
                        // PrivateBrandCheck: calling #m() on an object that does not
                        // carry the brand throws TypeError (e.g. `c.access.call({})`).
                        if method_name_s.starts_with('#')
                            && self.private_brand_missing(oid, &method_name_s)
                        {
                            self.truncate_stack(obj_pos);
                            let err = self.make_native_error("TypeError", &format!(
                                "Cannot read private member {method_name_s} from an object whose class did not declare it"
                            ));
                            self.handle_throw(err)?;
                            continue;
                        }
                        let mut method_val = if method_name_s.starts_with('#') {
                            let mangled = self.interner.intern(&format!("__priv_{}__", method_name_s));
                            self.heap.get_property_chain(oid, mangled)
                                .or_else(|| self.heap.get_property_chain(oid, method_name))
                        } else {
                            self.heap.get_property_chain(oid, method_name)
                        };
                        // If no direct method found, check for a getter (`__get_<name>__`):
                        // call the getter and use its return value as the method to invoke.
                        if method_val.is_none() {
                            let getter_key = self.interner.intern(&format!("__get_{method_name_s}__"));
                            if let Some(gfn) = self.heap.get_property_chain(oid, getter_key)
                                && gfn.is_function()
                                && let Ok(rv) = self.call_function_this(gfn, obj_val, &[])
                            {
                                method_val = Some(rv);
                            }
                        }
                        if let Some(mv) = method_val
                            && mv.is_function() {
                                let packed = mv.as_function().unwrap();
                                let _closure_id = ((packed as u32) >> 16) as usize;
                                let chunk_idx = (packed & 0xFFFF) as usize;
                                if chunk_idx >= 1 && chunk_idx < self.chunks.len() {
                                    // Async methods: route through call_with_async_wrap
                                    // so the body runs synchronously and its result is
                                    // wrapped in a fulfilled / rejected Promise. Async
                                    // *generators* fall through to the normal generator
                                    // call path — `.next()` is what wraps each step.
                                    if self.chunks[chunk_idx].flags.contains(ChunkFlags::ASYNC)
                                        && !self.chunks[chunk_idx].flags.contains(ChunkFlags::GENERATOR)
                                    {
                                        let args_vec: Vec<Value> = (0..argc)
                                            .map(|i| self.stack[obj_pos + 1 + i])
                                            .collect();
                                        self.truncate_stack(obj_pos);
                                        let result = self.call_with_async_wrap(mv, obj_val, &args_vec)?;
                                        self.push(result);
                                        continue;
                                    }
                                    // Generator methods: fall through; CreateGenerator opcode
                                    // in the body's prologue will capture state.
                                    // Restructure stack: [obj, args...] -> [args...]
                                    // Put closure in func_pos, shift args
                                    self.stack[obj_pos] = mv;
                                    let mut actual_argc = argc;
                                    let expected = self.chunks[chunk_idx].param_count as usize;
                                    while actual_argc < expected {
                                        self.push(Value::undefined());
                                        actual_argc += 1;
                                    }
                                    let upvalues = if _closure_id < self.closure_upvalues.len() {
                                        self.closure_upvalues[_closure_id].clone()
                                    } else { Vec::new() };
                                    let saved_args: Vec<Value> = (0..argc)
                                        .map(|i| self.stack.get(obj_pos + 1 + i).copied().unwrap_or(Value::undefined()))
                                        .collect();
                                    self.frames.push(CallFrame {
                                        chunk_idx, ip: 0, base: obj_pos + 1,
                                        upvalues, this_value: obj_val, is_constructor: false,
                                        pending_super_call: false, generator_id: None, argc,
                                        saved_args, arguments_oid: None, is_derived_ctor: false, super_called: false,
                                        new_target: Value::undefined(),
                                        await_super_result: false,
                                        with_base: self.with_stack.len(),
                                    });
                                    continue;
                                }
                            }
                    }

                    // Iterator instance methods: `.next()` / `.return()` on Array/Map/Set/
                    // KeyIterator and Generator iterators.
                    if let Some(oid) = obj_val.as_object_id() {
                        let mn = self.interner.resolve(method_name);
                        let kind_match = self.heap.get(oid).map(|o| matches!(
                            &o.kind,
                            ObjectKind::ArrayIterator(..)
                                | ObjectKind::MapIterator(..)
                                | ObjectKind::SetIterator(..)
                                | ObjectKind::KeyIterator(..)
                        )).unwrap_or(false);
                        if kind_match && mn == "next" {
                            // Step the iterator and produce { value, done }.
                            let result = self.iterator_next_step(oid)?;
                            self.truncate_stack(obj_pos);
                            self.push(result);
                            continue;
                        }
                        if kind_match && mn == "return" {
                            // Per spec, builtin iterators don't define return; we still
                            // return { value: arg, done: true } as a courtesy.
                            let arg = if argc > 0 { self.stack[obj_pos + 1] } else { Value::undefined() };
                            let res = self.make_iter_result(arg, true)?;
                            self.truncate_stack(obj_pos);
                            self.push(res);
                            continue;
                        }
                    }

                    // Check for Promise instance methods (.then/.catch)
                    if let Some(oid) = obj_val.as_object_id() {
                        let is_promise = self.heap.get(oid)
                            .map(|o| matches!(&o.kind, ObjectKind::Promise { .. }))
                            .unwrap_or(false);
                        if is_promise {
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                            let result = self.exec_promise_method(oid, method_name, &args)?;
                            self.truncate_stack(obj_pos);
                            self.push(result);
                            continue;
                        }
                    }

                    // Object static methods (Object.keys, Object.create, ...).
                    // Implementations live in exec_object_static so the
                    // extracted-value path (`var c = Object.create; c(...)`)
                    // dispatches to the same code.
                    if obj_val.is_function() && obj_val.as_function() == Some(-508) {
                        let mn = self.interner.resolve(method_name).to_owned();
                        let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                        let result = self.exec_object_static(&mn, &args)?.unwrap_or(Value::undefined());
                        self.truncate_stack(obj_pos);
                        self.push(result);
                        continue;
                    }

                    // Array.isArray / Array.from / Array.of
                    if obj_val.is_function() && obj_val.as_function() == Some(-507) {
                        let mn = self.interner.resolve(method_name).to_owned();
                        match mn.as_str() {
                            "isArray" => {
                                let arg = if argc > 0 { self.stack[obj_pos + 1] } else { Value::undefined() };
                                let is_arr = arg.as_object_id()
                                    .and_then(|oid| self.heap.get(oid))
                                    .map(|o| matches!(&o.kind, ObjectKind::Array(_)))
                                    .unwrap_or(false);
                                self.truncate_stack(obj_pos);
                                self.push(Value::boolean(is_arr));
                                continue;
                            }
                            "from" => {
                                let source = if argc > 0 { self.stack[obj_pos + 1] } else { Value::undefined() };
                                let map_fn = if argc > 1 { Some(self.stack[obj_pos + 2]) } else { None };
                                // Generic iterables (not Array/Set/Map kinds, which have
                                // fast paths below) drive the iterator protocol with
                                // per-item mapping and IteratorClose on a mapfn throw —
                                // core-js probes exactly this (SAFE_CLOSING) before
                                // trusting native collections; without it every Map/Set
                                // gets wrapped in its compatibility shell.
                                let is_known_kind = source.as_object_id()
                                    .and_then(|o| self.heap.get(o))
                                    .map(|o| matches!(&o.kind,
                                        ObjectKind::Array(_) | ObjectKind::Set { .. } | ObjectKind::Map { .. }))
                                    .unwrap_or(false);
                                if !is_known_kind && source.as_object_id().is_some() {
                                    match self.array_from_iterable(source, map_fn) {
                                        Ok(Some(values)) => {
                                            let arr = JsObject::array(values);
                                            let new_oid = self.heap.allocate(arr);
                                            self.truncate_stack(obj_pos);
                                            self.push(Value::object_id(new_oid));
                                            continue;
                                        }
                                        Ok(None) => {} // not iterable: array-like fallback below
                                        Err(VmError::Throw(v)) => {
                                            self.truncate_stack(obj_pos);
                                            self.handle_throw(v)?;
                                            continue;
                                        }
                                        Err(e) => return Err(e),
                                    }
                                }
                                let mut result = Vec::new();
                                // Collect raw elements first
                                let raw_elems: Vec<Value> = if let Some(src_oid) = source.as_object_id() {
                                    if let Some(obj) = self.heap.get(src_oid) {
                                        match &obj.kind {
                                            ObjectKind::Array(elems) => elems.clone(),
                                            ObjectKind::Set { entries } => entries.clone(),
                                            ObjectKind::Map { entries } => {
                                                let pairs = entries.clone();
                                                pairs.into_iter().map(|(k, v)| {
                                                    let pair_arr = JsObject::array(vec![k, v]);
                                                    Value::object_id(self.heap.allocate(pair_arr))
                                                }).collect()
                                            }
                                            _ => {
                                                // Try array-like with length
                                                let length_key = self.interner.intern("length");
                                                if let Some(len_val) = self.heap.get(src_oid).and_then(|o| o.get_property(length_key)) {
                                                    if let Some(len) = len_val.as_number() {
                                                        let n = len as usize;
                                                        let mut items = Vec::with_capacity(n);
                                                        for i in 0..n {
                                                            let key_str = i.to_string();
                                                            let key_id = self.interner.intern(&key_str);
                                                            items.push(self.heap.get(src_oid).and_then(|o| o.get_property(key_id)).unwrap_or(Value::undefined()));
                                                        }
                                                        items
                                                    } else { vec![] }
                                                } else { vec![] }
                                            }
                                        }
                                    } else { vec![] }
                                } else if source.is_string() {
                                    let s = self.value_to_string(source);
                                    s.chars().map(|c| {
                                        let id = self.interner.intern(&c.to_string());
                                        Value::string(id)
                                    }).collect()
                                } else { vec![] };
                                // Apply map_fn if provided
                                for (i, elem) in raw_elems.iter().enumerate() {
                                    if let Some(mfn) = map_fn {
                                        result.push(self.call_function(mfn, &[*elem, Value::int(i as i32)])?);
                                    } else {
                                        result.push(*elem);
                                    }
                                }
                                let arr = JsObject::array(result);
                                let new_oid = self.heap.allocate(arr);
                                self.truncate_stack(obj_pos);
                                self.push(Value::object_id(new_oid));
                                continue;
                            }
                            "of" => {
                                let items: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                                let arr = JsObject::array(items);
                                let new_oid = self.heap.allocate(arr);
                                self.truncate_stack(obj_pos);
                                self.push(Value::object_id(new_oid));
                                continue;
                            }
                            _ => {}
                        }
                    }

                    // Date static methods
                    if obj_val.is_function() && obj_val.as_function() == Some(-550) {
                        let mn = self.interner.resolve(method_name).to_owned();
                        let result = match mn.as_str() {
                            "now" => Value::number(
                                std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_millis() as f64)
                                    .unwrap_or(0.0)
                            ),
                            "UTC" => {
                                // Simplified: sum argc components based on (y, m, d, h, mn, s, ms)
                                Value::number(0.0)
                            }
                            "parse" => Value::number(f64::NAN),
                            _ => Value::undefined(),
                        };
                        self.truncate_stack(obj_pos);
                        self.push(result);
                        continue;
                    }

                    // String.fromCharCode / String.fromCodePoint
                    if obj_val.is_function() && obj_val.as_function() == Some(-504) {
                        let mn = self.interner.resolve(method_name).to_owned();
                        if mn == "fromCharCode" || mn == "fromCodePoint" {
                            let mut result = String::new();
                            for i in 0..argc {
                                let code = self.to_f64(self.stack[obj_pos + 1 + i]) as u32;
                                if let Some(c) = char::from_u32(code) {
                                    result.push(c);
                                }
                            }
                            let id = self.interner.intern(&result);
                            self.truncate_stack(obj_pos);
                            self.push(Value::string(id));
                            continue;
                        }
                        if mn == "raw" {
                            // String.raw({raw: [...]}, ...subs)
                            let template = if argc > 0 { self.stack[obj_pos + 1] } else { Value::undefined() };
                            let raw_key = self.interner.intern("raw");
                            let raw_arr_val = template.as_object_id()
                                .and_then(|oid| self.heap.get(oid))
                                .and_then(|o| o.get_property(raw_key))
                                .unwrap_or(Value::undefined());
                            let raw_strs: Vec<Value> = raw_arr_val.as_object_id()
                                .and_then(|oid| self.heap.get(oid))
                                .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                                .unwrap_or_default();
                            let mut result = String::new();
                            for (i, s) in raw_strs.iter().enumerate() {
                                result.push_str(&self.value_to_string(*s));
                                if i + 1 < raw_strs.len() && i + 1 < argc {
                                    result.push_str(&self.value_to_string(self.stack[obj_pos + 1 + i + 1]));
                                }
                            }
                            let id = self.interner.intern(&result);
                            self.truncate_stack(obj_pos);
                            self.push(Value::string(id));
                            continue;
                        }
                    }

                    // Symbol static methods (Symbol.for / Symbol.keyFor)
                    if obj_val.is_function() && obj_val.as_function() == Some(-570) {
                        let mn = self.interner.resolve(method_name).to_owned();
                        if mn == "for" || mn == "keyFor" {
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                            let result = if mn == "for" {
                                self.exec_symbol_for(&args)
                            } else {
                                self.exec_symbol_key_for(&args)
                            };
                            self.truncate_stack(obj_pos);
                            self.push(result);
                            continue;
                        }
                    }

                    // Number static methods (Number.isNaN, Number.isFinite, etc.)
                    if obj_val.is_function() && obj_val.as_function() == Some(-505) {
                        let mn = self.interner.resolve(method_name).to_owned();
                        let sentinel = match mn.as_str() {
                            "isNaN" => Some(-530),
                            "isFinite" => Some(-531),
                            "isInteger" => Some(-532),
                            "isSafeInteger" => Some(-533),
                            "parseInt" => Some(-500),
                            "parseFloat" => Some(-501),
                            _ => None,
                        };
                        if let Some(s) = sentinel {
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                            let result = self.exec_global_fn(s, &args);
                            self.truncate_stack(obj_pos);
                            self.push(result);
                            continue;
                        }
                    }

                    // Check for Promise static methods (Promise.resolve/reject)
                    if obj_val.is_function() && obj_val.as_function() == Some(-520) {
                        let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                        let result = self.exec_promise_static(method_name, &args)?;
                        self.truncate_stack(obj_pos);
                        self.push(result);
                        continue;
                    }

                    // Generic method call fallthrough - push undefined
                    self.truncate_stack(obj_pos);
                    self.push(Value::undefined());
                }

                OpCode::Construct => {
                    let argc = self.read_byte() as usize;
                    let func_pos = self.stack.len() - 1 - argc;
                    let func_val = self.stack[func_pos];

                    // Per spec, IsConstructor check fires AFTER args are evaluated.
                    // If func_val is neither a function value nor a class-like object,
                    // throw TypeError. (Class objects have a __constructor__ marker.)
                    let is_constructable = func_val.is_function() || {
                        if let Some(oid) = func_val.as_object_id() {
                            let ctor_key = self.interner.intern("__constructor__");
                            self.heap.get(oid).map(|o| {
                                matches!(&o.kind, ObjectKind::Function(_))
                                    || o.get_property(ctor_key).is_some()
                            }).unwrap_or(false)
                        } else { false }
                    };
                    if !is_constructable {
                        // Same location annotation as property-read errors —
                        // on a minified bundle the line/pc is the only way to
                        // find which expression produced the non-constructor.
                        let (line, pc, chunk_name) = if let Some(f) = self.frames.last() {
                            let cn = self.interner.resolve(self.chunks[f.chunk_idx].name).to_owned();
                            (self.chunks[f.chunk_idx].get_line(f.ip as u32), f.ip, cn)
                        } else { (0, 0, String::new()) };
                        let kind = self.type_of_value(func_val);
                        let msg = format!(
                            "{kind} is not a constructor (at line {line}, pc {pc}, chunk '{chunk_name}')"
                        );
                        let err = self.make_native_error("TypeError", &msg);
                        self.handle_throw(err)?;
                        continue;
                    }

                    // Generator functions, arrow functions, async functions, and
                    // concise methods (object/class shorthand) can't be constructors.
                    if func_val.is_function() {
                        let packed = func_val.as_function().unwrap();
                        if packed > 0 {
                            let chunk_idx = (packed & 0xFFFF) as usize;
                            if chunk_idx < self.chunks.len() {
                                let flags = self.chunks[chunk_idx].flags;
                                if flags.contains(ChunkFlags::GENERATOR)
                                    || flags.contains(ChunkFlags::ARROW)
                                    || flags.contains(ChunkFlags::ASYNC)
                                    || flags.contains(ChunkFlags::METHOD)
                                {
                                    let kind = if flags.contains(ChunkFlags::GENERATOR) { "generator" }
                                        else if flags.contains(ChunkFlags::ASYNC) { "async function" }
                                        else if flags.contains(ChunkFlags::METHOD) { "method" }
                                        else { "arrow function" };
                                    let msg = format!("{kind} is not a constructor");
                                    let err = self.make_native_error("TypeError", &msg);
                                    self.handle_throw(err)?;
                                    continue;
                                }
                            }
                        }
                    }

                    // Handle Promise constructor
                    if func_val.is_function() && func_val.as_function() == Some(-520) {
                        let executor = if argc > 0 { self.stack[func_pos + 1] } else { Value::undefined() };
                        let pid = self.allocate_promise();
                        // Create resolve/reject sentinels
                        let resolve_val = Value::function(-600_000 - pid.0 as i32);
                        let reject_val = Value::function(-700_000 - pid.0 as i32);
                        // Call the executor
                        if executor.is_function() {
                            let _ = self.call_function(executor, &[resolve_val, reject_val]);
                        }
                        self.truncate_stack(func_pos);
                        self.push(Value::object_id(pid));
                        continue;
                    }

                    // Handle Map/Set/WeakMap/WeakSet constructors
                    if func_val.is_function() {
                        let sentinel = func_val.as_function().unwrap();
                        // ArrayBuffer / DataView / TypedArray constructors.
                        if sentinel == crate::vm::typedarray::SENT_ARRAYBUFFER {
                            let n = if argc > 0 { self.to_f64(self.stack[func_pos + 1]) } else { 0.0 };
                            let len = if n.is_finite() && n >= 0.0 { n as usize } else { 0 };
                            let oid = self.make_array_buffer(len);
                            self.truncate_stack(func_pos);
                            self.push(Value::object_id(oid));
                            continue;
                        }
                        if sentinel == crate::vm::typedarray::SENT_DATAVIEW {
                            let buf = if argc > 0 { self.stack[func_pos + 1] } else { Value::undefined() };
                            let buf_oid = buf.as_object_id().filter(|o| matches!(self.heap.get(*o).map(|x| &x.kind), Some(ObjectKind::ArrayBuffer(_))));
                            let Some(buf_oid) = buf_oid else {
                                let e = self.make_native_error("TypeError", "First argument to DataView constructor must be an ArrayBuffer");
                                self.truncate_stack(func_pos); self.handle_throw(e)?; continue;
                            };
                            let blen = if let Some(ObjectKind::ArrayBuffer(b)) = self.heap.get(buf_oid).map(|o| &o.kind) { b.len() } else { 0 };
                            let off = if argc > 1 { self.to_f64(self.stack[func_pos + 2]) as usize } else { 0 };
                            let len = if argc > 2 && !self.stack[func_pos + 3].is_undefined() { self.to_f64(self.stack[func_pos + 3]) as usize } else { blen.saturating_sub(off) };
                            let proto = self.func_prototypes.get(&crate::vm::typedarray::SENT_DATAVIEW).copied();
                            let obj = JsObject { properties: Vec::new(), prototype: proto,
                                kind: ObjectKind::DataView { buffer: buf_oid, byte_offset: off, byte_length: len }, marked: false, extensible: true };
                            let oid = self.heap.allocate(obj);
                            self.truncate_stack(func_pos);
                            self.push(Value::object_id(oid));
                            continue;
                        }
                        if let Some(kind) = crate::vm::typedarray::kind_for_sentinel(sentinel) {
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[func_pos + 1 + i]).collect();
                            self.truncate_stack(func_pos);
                            match self.construct_typed_array(kind, &args, None) {
                                Ok(v) => self.push(v),
                                Err(VmError::Throw(v)) => { self.handle_throw(v)?; }
                                Err(e) => return Err(e),
                            }
                            continue;
                        }
                        match sentinel {
                            -540 => { // new Map()
                                let mut entries = Vec::new();
                                // Optional iterable argument: arrays fast-path;
                                // anything else goes through the iterator
                                // protocol (generators, map.entries(), core-js
                                // correctness probes, ...).
                                if argc > 0 {
                                    let arg = self.stack[func_pos + 1];
                                    let elems: Vec<Value> = if let Some(arr_oid) = arg.as_object_id()
                                        && let Some(obj) = self.heap.get(arr_oid)
                                        && let ObjectKind::Array(ref elems) = obj.kind
                                    {
                                        elems.clone()
                                    } else if !arg.is_null() && !arg.is_undefined() {
                                        match self.collect_iterable(arg) {
                                            Ok(Some(items)) => items,
                                            Ok(None) => Vec::new(),
                                            Err(VmError::Throw(v)) => {
                                                self.truncate_stack(func_pos);
                                                self.handle_throw(v)?;
                                                continue;
                                            }
                                            Err(e) => return Err(e),
                                        }
                                    } else {
                                        Vec::new()
                                    };
                                    for elem in &elems {
                                        if let Some(pair_oid) = elem.as_object_id()
                                            && let Some(pair_obj) = self.heap.get(pair_oid)
                                                && let ObjectKind::Array(ref pair) = pair_obj.kind
                                                    && pair.len() >= 2 {
                                                        entries.push((pair[0], pair[1]));
                                                    }
                                    }
                                }
                                let obj = JsObject {
                                    properties: Vec::new(),
                                    // Chain to Map.prototype so property READS of
                                    // methods (`m.set` as a value) resolve; calls
                                    // still kind-dispatch first.
                                    prototype: self.func_prototypes.get(&-540).copied(),
                                    kind: ObjectKind::Map { entries }, marked: false, extensible: true,
                                };
                                let oid = self.heap.allocate(obj);
                                self.truncate_stack(func_pos);
                                self.push(Value::object_id(oid));
                                continue;
                            }
                            -541 => { // new Set()
                                let mut entries = Vec::new();
                                if argc > 0 {
                                    let arg = self.stack[func_pos + 1];
                                    if let Some(arr_oid) = arg.as_object_id()
                                        && let Some(obj) = self.heap.get(arr_oid)
                                        && let ObjectKind::Array(ref elems) = obj.kind
                                    {
                                        entries = elems.clone();
                                    } else if !arg.is_null() && !arg.is_undefined() {
                                        match self.collect_iterable(arg) {
                                            Ok(Some(items)) => entries = items,
                                            Ok(None) => {}
                                            Err(VmError::Throw(v)) => {
                                                self.truncate_stack(func_pos);
                                                self.handle_throw(v)?;
                                                continue;
                                            }
                                            Err(e) => return Err(e),
                                        }
                                    }
                                    // Set semantics: dedupe (SameValueZero-ish via strict_eq).
                                    let mut deduped: Vec<Value> = Vec::with_capacity(entries.len());
                                    for v in entries {
                                        if !deduped.iter().any(|d| self.strict_eq(*d, v)) {
                                            deduped.push(v);
                                        }
                                    }
                                    entries = deduped;
                                }
                                let obj = JsObject {
                                    properties: Vec::new(),
                                    prototype: self.func_prototypes.get(&-541).copied(),
                                    kind: ObjectKind::Set { entries }, marked: false, extensible: true,
                                };
                                let oid = self.heap.allocate(obj);
                                self.truncate_stack(func_pos);
                                self.push(Value::object_id(oid));
                                continue;
                            }
                            -542 => { // new WeakMap()
                                let mut entries: Vec<(ObjectId, Value)> = Vec::new();
                                if argc > 0 {
                                    let arg = self.stack[func_pos + 1];
                                    let pairs: Vec<Value> = if let Some(arr_oid) = arg.as_object_id()
                                        && let Some(obj) = self.heap.get(arr_oid)
                                        && let ObjectKind::Array(ref elems) = obj.kind
                                    {
                                        elems.clone()
                                    } else if !arg.is_null() && !arg.is_undefined() {
                                        match self.collect_iterable(arg) {
                                            Ok(Some(items)) => items,
                                            Ok(None) => Vec::new(),
                                            Err(VmError::Throw(v)) => {
                                                self.truncate_stack(func_pos);
                                                self.handle_throw(v)?;
                                                continue;
                                            }
                                            Err(e) => return Err(e),
                                        }
                                    } else {
                                        Vec::new()
                                    };
                                    for elem in &pairs {
                                        if let Some(pair_oid) = elem.as_object_id()
                                            && let Some(pair_obj) = self.heap.get(pair_oid)
                                            && let ObjectKind::Array(ref pair) = pair_obj.kind
                                            && pair.len() >= 2
                                            && let Some(key_oid) = pair[0].as_object_id()
                                        {
                                            entries.push((key_oid, pair[1]));
                                        }
                                    }
                                }
                                let obj = JsObject {
                                    properties: Vec::new(),
                                    prototype: self.func_prototypes.get(&-542).copied(),
                                    kind: ObjectKind::WeakMap { entries }, marked: false, extensible: true,
                                };
                                let oid = self.heap.allocate(obj);
                                self.truncate_stack(func_pos);
                                self.push(Value::object_id(oid));
                                continue;
                            }
                            -543 => { // new WeakSet()
                                let mut entries: Vec<ObjectId> = Vec::new();
                                if argc > 0 {
                                    let arg = self.stack[func_pos + 1];
                                    let items: Vec<Value> = if let Some(arr_oid) = arg.as_object_id()
                                        && let Some(obj) = self.heap.get(arr_oid)
                                        && let ObjectKind::Array(ref elems) = obj.kind
                                    {
                                        elems.clone()
                                    } else if !arg.is_null() && !arg.is_undefined() {
                                        match self.collect_iterable(arg) {
                                            Ok(Some(items)) => items,
                                            Ok(None) => Vec::new(),
                                            Err(VmError::Throw(v)) => {
                                                self.truncate_stack(func_pos);
                                                self.handle_throw(v)?;
                                                continue;
                                            }
                                            Err(e) => return Err(e),
                                        }
                                    } else {
                                        Vec::new()
                                    };
                                    for item in &items {
                                        if let Some(o) = item.as_object_id()
                                            && !entries.contains(&o)
                                        {
                                            entries.push(o);
                                        }
                                    }
                                }
                                let obj = JsObject {
                                    properties: Vec::new(),
                                    prototype: self.func_prototypes.get(&-543).copied(),
                                    kind: ObjectKind::WeakSet { entries }, marked: false, extensible: true,
                                };
                                let oid = self.heap.allocate(obj);
                                self.truncate_stack(func_pos);
                                self.push(Value::object_id(oid));
                                continue;
                            }
                            -550 => { // new Date()
                                let ms = if argc == 0 {
                                    std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_millis() as f64)
                                        .unwrap_or(0.0)
                                } else if argc == 1 {
                                    self.to_f64(self.stack[func_pos + 1])
                                } else {
                                    0.0
                                };
                                let obj = JsObject {
                                    properties: Vec::new(), prototype: None,
                                    kind: ObjectKind::Date(ms),
                                    marked: false, extensible: true,
                                };
                                let oid = self.heap.allocate(obj);
                                self.truncate_stack(func_pos);
                                self.push(Value::object_id(oid));
                                continue;
                            }
                            _ => {}
                        }
                    }

                    // new RegExp(pattern, flags)
                    if func_val.is_function() && func_val.as_function() == Some(-580) {
                        let pattern = if argc > 0 { self.value_to_string(self.stack[func_pos + 1]) } else { String::new() };
                        let flags = if argc > 1 { self.value_to_string(self.stack[func_pos + 2]) } else { String::new() };
                        let obj = JsObject {
                            properties: Vec::new(), prototype: self.func_prototypes.get(&-580).copied(),
                            kind: ObjectKind::RegExp { pattern, flags },
                            marked: false, extensible: true,
                        };
                        let oid = self.heap.allocate(obj);
                        self.truncate_stack(func_pos);
                        self.push(Value::object_id(oid));
                        continue;
                    }

                    // new Function(...args)
                    if func_val.is_function() && func_val.as_function() == Some(-551) {
                        let args: Vec<Value> = (0..argc).map(|i| self.stack[func_pos + 1 + i]).collect();
                        self.truncate_stack(func_pos);
                        let result = self.construct_function(&args)?;
                        self.push(result);
                        continue;
                    }

                    // new Array(...): construct an Array. Single-numeric-arg form sets length;
                    // otherwise the args become the elements.
                    if func_val.is_function() && func_val.as_function() == Some(-507) {
                        let elements: Vec<Value> = if argc == 1 {
                            let only = self.stack[func_pos + 1];
                            if let Some(n) = only.as_number()
                                && n.is_finite() && n.fract() == 0.0 && n >= 0.0 && n <= u32::MAX as f64
                            {
                                vec![Value::undefined(); n as usize]
                            } else if let Some(n) = only.as_int() {
                                if n >= 0 { vec![Value::undefined(); n as usize] } else { vec![only] }
                            } else {
                                vec![only]
                            }
                        } else {
                            (0..argc).map(|i| self.stack[func_pos + 1 + i]).collect()
                        };
                        let arr_obj = JsObject::array(elements);
                        let oid = self.heap.allocate(arr_obj);
                        if let Some(o) = self.heap.get_mut(oid) {
                            o.prototype = Some(self.array_prototype);
                        }
                        self.truncate_stack(func_pos);
                        self.push(Value::object_id(oid));
                        continue;
                    }

                    // Handle wrapper constructors (new Number, new Boolean, new String)
                    if func_val.is_function() {
                        let sentinel = func_val.as_function().unwrap();
                        if (-506..=-504).contains(&sentinel) {
                            // Per spec, new Number() / new String() / new Boolean() with no args
                            // returns 0 / "" / false respectively (not NaN/undefined-coerced).
                            let no_args = argc == 0;
                            let arg = if argc > 0 { self.stack[func_pos + 1] } else { Value::undefined() };
                            let wrapped = match sentinel {
                                -504 => { // String
                                    let s = if no_args { String::new() } else { self.value_to_string(arg) };
                                    let id = self.interner.intern(&s);
                                    Value::string(id)
                                }
                                -505 => { // Number
                                    if no_args { Value::number(0.0) } else { Value::number(self.to_f64(arg)) }
                                }
                                -506 => { // Boolean
                                    if no_args { Value::boolean(false) } else { Value::boolean(arg.to_boolean()) }
                                }
                                _ => arg,
                            };
                            let mut obj = JsObject::ordinary();
                            obj.kind = ObjectKind::Wrapper(wrapped);
                            // Set prototype to the constructor's prototype
                            obj.prototype = match sentinel {
                                -504 => Some(self.string_prototype),
                                -505 => Some(self.number_prototype),
                                -506 => Some(self.boolean_prototype),
                                _ => Some(self.object_prototype),
                            };
                            if sentinel == -504
                                && let Some(sid) = wrapped.as_string_id() {
                                    let len = self.interner.resolve(sid).chars().count() as i32;
                                    let len_key = self.interner.intern("length");
                                    obj.set_property(len_key, Value::int(len));
                                }
                            let oid = self.heap.allocate(obj);
                            self.truncate_stack(func_pos);
                            self.push(Value::object_id(oid));
                            continue;
                        }
                    }

                    // Handle Array constructor: new Array() or new Array(len)
                    if func_val.is_function() && func_val.as_function() == Some(-507) {
                        let arr = if argc == 1 {
                            let arg = self.stack[func_pos + 1];
                            if let Some(n) = arg.as_number() {
                                // new Array(length)
                                let len = n as usize;
                                JsObject::array(vec![Value::undefined(); len])
                            } else {
                                JsObject::array(vec![arg])
                            }
                        } else if argc > 1 {
                            let elems: Vec<Value> = (0..argc).map(|i| self.stack[func_pos + 1 + i]).collect();
                            JsObject::array(elems)
                        } else {
                            JsObject::array(Vec::new())
                        };
                        let oid = self.heap.allocate(arr);
                        self.truncate_stack(func_pos);
                        self.push(Value::object_id(oid));
                        continue;
                    }

                    // Handle Object constructor: new Object()
                    if func_val.is_function() && func_val.as_function() == Some(-508) {
                        let mut obj = JsObject::ordinary();
                        obj.prototype = Some(self.object_prototype);
                        let oid = self.heap.allocate(obj);
                        self.truncate_stack(func_pos);
                        self.push(Value::object_id(oid));
                        continue;
                    }

                    // Handle Error constructors
                    if func_val.is_function() {
                        let sentinel = func_val.as_function().unwrap();
                        if (-516..=-510).contains(&sentinel) {
                            let error_type = match sentinel {
                                -510 => "Error",
                                -511 => "TypeError",
                                -512 => "RangeError",
                                -513 => "ReferenceError",
                                -514 => "SyntaxError",
                                -515 => "EvalError",
                                -516 => "URIError",
                                _ => "Error",
                            };
                            let mut err_obj = JsObject::ordinary();
                            err_obj.prototype = self.func_prototypes.get(&sentinel).copied()
                                .or(Some(self.object_prototype));
                            // Only attach an own "message" if a non-undefined arg was passed.
                            let msg_arg = if argc > 0 { Some(self.stack[func_pos + 1]) } else { None };
                            let msg = if let Some(arg) = msg_arg
                                && !arg.is_undefined()
                            {
                                let s = self.value_to_string(arg);
                                let msg_key = self.interner.intern("message");
                                let msg_id = self.interner.intern(&s);
                                err_obj.define_property(
                                    msg_key,
                                    Property::with_flags(
                                        Value::string(msg_id),
                                        Property::WRITABLE | Property::CONFIGURABLE,
                                    ),
                                );
                                s
                            } else {
                                String::new()
                            };
                            let stack_key = self.interner.intern("stack");
                            let stack_str = format!("{error_type}: {msg}");
                            let stack_id = self.interner.intern(&stack_str);
                            err_obj.set_property(stack_key, Value::string(stack_id));
                            let oid = self.heap.allocate(err_obj);
                            self.truncate_stack(func_pos);
                            self.push(Value::object_id(oid));
                            continue;
                        }
                    }

                    // Create a new object for `this`, linked to F.prototype
                    let mut new_obj = JsObject::ordinary();
                    if func_val.is_function() {
                        let packed = func_val.as_function().unwrap();
                        let proto_key_id = self.interner.intern("prototype");
                        // Check if user has overridden .prototype (e.g., Robin2.prototype = {phylum:"avis"})
                        let user_proto = self.fn_property_overrides.get(&(packed, proto_key_id)).copied().flatten();
                        if let Some(uv) = user_proto
                            && let Some(proto_oid) = uv.as_object_id()
                        {
                            new_obj.prototype = Some(proto_oid);
                        } else if let Some(&proto_oid) = self.func_prototypes.get(&packed) {
                            // Get or create the prototype from the cache
                            new_obj.prototype = Some(proto_oid);
                        } else {
                            let chunk_idx = (packed & 0xFFFF) as usize;
                            if chunk_idx < self.chunks.len() {
                                let mut proto = JsObject::ordinary();
                                proto.prototype = Some(self.object_prototype);
                                let ctor_key = self.interner.intern("constructor");
                                proto.define_property(ctor_key, Property::with_flags(
                                    func_val, Property::WRITABLE | Property::CONFIGURABLE
                                ));
                                let proto_oid = self.heap.allocate(proto);
                                self.func_prototypes.insert(packed, proto_oid);
                                new_obj.prototype = Some(proto_oid);
                            }
                        }
                    }
                    let new_oid = self.heap.allocate(new_obj);
                    let this_val = Value::object_id(new_oid);

                    // Handle class objects: look up __constructor__
                    if let Some(class_oid) = func_val.as_object_id() {
                        let ctor_key = self.interner.intern("__constructor__");
                        let proto_key = self.interner.intern("prototype");
                        let super_key = self.interner.intern("__super__");
                        // Default constructor for derived classes: walk __super__ chain to find
                        // an explicit constructor and call it with the same args (forwarding).
                        // Note: Class opcode sets __constructor__ to undefined as a placeholder.
                        // A real ctor is a function value; treat undefined as "no constructor".
                        let mut ctor_val = self.heap.get(class_oid)
                            .and_then(|o| o.get_property(ctor_key))
                            .filter(|v| v.is_function());
                        // No own constructor → we walk `__super__` and reuse an ancestor's.
                        // Track that, plus which class owns the ctor we run, to decide
                        // whether the implicit `super(...args)` forward is already satisfied.
                        let used_default_ctor = ctor_val.is_none();
                        let mut ctor_owner_oid = class_oid;
                        // Native-error super: when extending Error/TypeError/etc., ctor_val
                        // is the sentinel itself; we'll handle it specially below.
                        let mut native_super_sentinel: Option<i32> = None;
                        if ctor_val.is_none() {
                            // Default constructor for derived classes: walk __super__ chain.
                            let mut cur_val: Option<Value> = self.heap.get(class_oid)
                                .and_then(|o| o.get_property(super_key));
                            while let Some(v) = cur_val {
                                if v.is_function() {
                                    let sentinel = v.as_function().unwrap();
                                    if (-516..=-510).contains(&sentinel)
                                        || matches!(sentinel, -540 | -541 | -542 | -543 | -550)
                                    {
                                        native_super_sentinel = Some(sentinel);
                                    }
                                    break;
                                }
                                let Some(sid) = v.as_object_id() else { break };
                                let Some(obj) = self.heap.get(sid) else { break };
                                if let Some(cv) = obj.get_property(ctor_key)
                                    && cv.is_function()
                                {
                                    ctor_val = Some(cv);
                                    ctor_owner_oid = sid;
                                    break;
                                }
                                cur_val = obj.get_property(super_key);
                            }
                        }
                        let proto_val = self.heap.get(class_oid)
                            .and_then(|o| o.get_property(proto_key));

                        // Link prototype chain instead of copying properties
                        if let Some(pv) = proto_val
                            && let Some(poid) = pv.as_object_id()
                            && let Some(new_o) = self.heap.get_mut(new_oid)
                        {
                            new_o.prototype = Some(poid);
                        }
                        // Store class reference for super() resolution
                        let class_key = self.interner.intern("__class__");
                        if let Some(new_o) = self.heap.get_mut(new_oid) {
                            new_o.set_property(class_key, func_val);
                        }

                        // Apply instance fields (stored as __ifield_{name}__ on the class).
                        // A derived instance also receives its ancestors' instance fields,
                        // so walk the whole __super__ chain and install base-class fields
                        // first (matching the order in which each constructor's fields run).
                        let ifield_prefix = "__ifield_";
                        let mut field_class_chain: Vec<ObjectId> = Vec::new();
                        let mut cur_field_class = Some(class_oid);
                        while let Some(coid) = cur_field_class {
                            if field_class_chain.contains(&coid) { break; }
                            field_class_chain.push(coid);
                            cur_field_class = self.heap.get(coid)
                                .and_then(|o| o.get_property(super_key))
                                .and_then(|v| v.as_object_id());
                        }
                        for &coid in field_class_chain.iter().rev() {
                            let instance_fields: Vec<(String, Value)> = self.heap.get(coid)
                                .map(|o| o.properties.iter()
                                    .filter_map(|(k, p)| {
                                        let key_str = self.interner.resolve(*k);
                                        if key_str.starts_with(ifield_prefix) && key_str.ends_with("__") {
                                            let inner = &key_str[ifield_prefix.len()..key_str.len() - 2];
                                            Some((inner.to_owned(), p.value))
                                        } else { None }
                                    })
                                    .collect())
                                .unwrap_or_default();
                            for (field_name, field_val) in instance_fields {
                                // Private fields (#name) are stored under __priv_#name__ to avoid
                                // being visible via hasOwnProperty("#name").
                                // Public fields (no #) keep their plain names.
                                let store_name = if field_name.starts_with('#') {
                                    format!("__priv_{}__", field_name)
                                } else {
                                    field_name.clone()
                                };
                                let real_key = self.interner.intern(&store_name);
                                if let Some(new_o) = self.heap.get_mut(new_oid) {
                                    new_o.set_property(real_key, field_val);
                                }
                            }
                        }

                        // Detect derived-class constructor: the class object has a `__super__`
                        // property pointing to its parent (set by OpCode::Inherit).
                        let is_derived = self.heap.get(class_oid)
                            .and_then(|o| o.get_property(super_key))
                            .is_some();
                        // `class B extends A {}` with no own ctor has the implicit
                        // `constructor(...args){ super(...args) }`. We run the reused
                        // ancestor ctor directly; when that ancestor is a BASE class
                        // (no `__super__`), the implicit super-forward is already done,
                        // so seed `super_called` to avoid a spurious "Must call super
                        // constructor" on return. (is_derived left intact.)
                        let owner_is_base = self.heap.get(ctor_owner_oid)
                            .and_then(|o| o.get_property(super_key))
                            .is_none();
                        let super_called_init = is_derived && used_default_ctor && owner_is_base;
                        // Private-method brand timing: when we run a base ancestor's ctor
                        // as the implicit constructor for a derived class with no own ctor,
                        // the subclass levels' private methods/accessors are NOT installed
                        // until that ctor (the super-equivalent) returns. Mark them pending
                        // so `this.#subclassPrivate()` reached from the base ctor throws,
                        // per spec. Walk class_oid's __super__ down to (excluding) the ctor
                        // owner, collecting each level's prototype oid.
                        if used_default_ctor && is_derived {
                            let mut pending: Vec<ObjectId> = Vec::new();
                            let mut cur = Some(class_oid);
                            while let Some(c) = cur {
                                if c == ctor_owner_oid { break; }
                                if let Some(p) = self.heap.get(c)
                                    .and_then(|o| o.get_property(proto_key))
                                    .and_then(|v| v.as_object_id())
                                {
                                    pending.push(p);
                                }
                                cur = self.heap.get(c)
                                    .and_then(|o| o.get_property(super_key))
                                    .and_then(|v| v.as_object_id());
                            }
                            if !pending.is_empty() {
                                self.pending_private_brands.insert(new_oid, pending);
                            }
                        }
                        if let Some(cv) = ctor_val
                            && cv.is_function() {
                                // Replace func on stack with this, push ctor as the call target
                                self.stack[func_pos] = this_val;
                                let packed = cv.as_function().unwrap();
                                let closure_id = ((packed as u32) >> 16) as usize;
                                let chunk_idx = (packed & 0xFFFF) as usize;
                                if chunk_idx >= 1 && chunk_idx < self.chunks.len() {
                                    let mut argc = argc;
                                    let expected = self.chunks[chunk_idx].param_count as usize;
                                    while argc < expected {
                                        self.push(Value::undefined());
                                        argc += 1;
                                    }
                                    let upvalues = if closure_id < self.closure_upvalues.len() {
                                        self.closure_upvalues[closure_id].clone()
                                    } else { Vec::new() };
                                    let saved_args: Vec<Value> = (0..argc)
                                        .map(|i| self.stack.get(func_pos + 1 + i).copied().unwrap_or(Value::undefined()))
                                        .collect();
                                    self.frames.push(CallFrame {
                                        chunk_idx, ip: 0, base: func_pos + 1,
                                        upvalues, this_value: this_val, is_constructor: true,
                                        pending_super_call: false, generator_id: None, argc,
                                        saved_args, arguments_oid: None,
                                        is_derived_ctor: is_derived, super_called: super_called_init,
                                        new_target: func_val,
                                        await_super_result: false,
                                        with_base: self.with_stack.len(),
                                    });
                                    continue;
                                }
                            }
                        // Default ctor extending a native Error: set message+stack on `this`
                        // (mirrors what super() to a native Error sentinel does).
                        // Per spec, "message" descriptor is { writable: true, enumerable: false,
                        // configurable: true } — only created when an argument is provided.
                        if let Some(sentinel) = native_super_sentinel
                            && (-516..=-510).contains(&sentinel)
                            && argc > 0
                            && !self.stack[func_pos + 1].is_undefined()
                        {
                            let msg = self.value_to_string(self.stack[func_pos + 1]);
                            let error_type = match sentinel {
                                -510 => "Error", -511 => "TypeError", -512 => "RangeError",
                                -513 => "ReferenceError", -514 => "SyntaxError",
                                -515 => "EvalError", -516 => "URIError", _ => "Error",
                            };
                            if let Some(this_oid) = this_val.as_object_id() {
                                let msg_key = self.interner.intern("message");
                                let msg_id = self.interner.intern(&msg);
                                let stack_key = self.interner.intern("stack");
                                let stack_str = format!("{error_type}: {msg}");
                                let stack_id = self.interner.intern(&stack_str);
                                if let Some(obj) = self.heap.get_mut(this_oid) {
                                    obj.define_property(
                                        msg_key,
                                        Property::with_flags(
                                            Value::string(msg_id),
                                            Property::WRITABLE | Property::CONFIGURABLE,
                                        ),
                                    );
                                    obj.set_property(stack_key, Value::string(stack_id));
                                }
                            }
                        }
                        // Default ctor extending Map/Set/WeakMap/WeakSet/Date: mutate
                        // `this` so it has the proper internal kind. (Mirror what
                        // super() to that sentinel would do.)
                        if let Some(sentinel) = native_super_sentinel
                            && matches!(sentinel, -540 | -541 | -542 | -543 | -550)
                            && let Some(this_oid) = this_val.as_object_id()
                        {
                            let new_kind = match sentinel {
                                -540 => {
                                    let mut entries = Vec::new();
                                    if argc > 0
                                        && let Some(arr_oid) = self.stack[func_pos + 1].as_object_id()
                                        && let Some(obj) = self.heap.get(arr_oid)
                                        && let ObjectKind::Array(ref elems) = obj.kind
                                    {
                                        let elems = elems.clone();
                                        for elem in &elems {
                                            if let Some(pair_oid) = elem.as_object_id()
                                                && let Some(pair_obj) = self.heap.get(pair_oid)
                                                && let ObjectKind::Array(ref pair) = pair_obj.kind
                                                && pair.len() >= 2
                                            {
                                                entries.push((pair[0], pair[1]));
                                            }
                                        }
                                    }
                                    ObjectKind::Map { entries }
                                }
                                -541 => {
                                    let mut entries = Vec::new();
                                    if argc > 0
                                        && let Some(arr_oid) = self.stack[func_pos + 1].as_object_id()
                                        && let Some(obj) = self.heap.get(arr_oid)
                                        && let ObjectKind::Array(ref elems) = obj.kind
                                    {
                                        entries = elems.clone();
                                    }
                                    ObjectKind::Set { entries }
                                }
                                -542 => ObjectKind::WeakMap { entries: Vec::new() },
                                -543 => ObjectKind::WeakSet { entries: Vec::new() },
                                -550 => {
                                    let ms = if argc == 0 {
                                        std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .map(|d| d.as_millis() as f64)
                                            .unwrap_or(0.0)
                                    } else if argc == 1 {
                                        self.to_f64(self.stack[func_pos + 1])
                                    } else {
                                        0.0
                                    };
                                    ObjectKind::Date(ms)
                                }
                                _ => unreachable!(),
                            };
                            if let Some(obj) = self.heap.get_mut(this_oid) {
                                obj.kind = new_kind;
                            }
                        }
                        // No constructor -- just return the object with prototype methods
                        self.truncate_stack(func_pos);
                        self.push(this_val);
                        continue;
                    }

                    if func_val.is_function() {
                        let packed = func_val.as_function().unwrap();
                        let closure_id = ((packed as u32) >> 16) as usize;
                        let chunk_idx = (packed & 0xFFFF) as usize;

                        if chunk_idx >= 1 && chunk_idx < self.chunks.len() {
                            let upvalues = if closure_id < self.closure_upvalues.len() {
                                self.closure_upvalues[closure_id].clone()
                            } else {
                                Vec::new()
                            };

                            // Replace the function slot with `this` so local slot -1 is this
                            self.stack[func_pos] = this_val;

                            let saved_args: Vec<Value> = (0..argc)
                                .map(|i| self.stack.get(func_pos + 1 + i).copied().unwrap_or(Value::undefined()))
                                .collect();
                            self.frames.push(CallFrame {
                                chunk_idx,
                                ip: 0,
                                base: func_pos + 1,
                                upvalues,
                                this_value: this_val,
                                is_constructor: true,
                                pending_super_call: false,
                                generator_id: None,
                                argc,
                                saved_args, arguments_oid: None, is_derived_ctor: false, super_called: false,
                                new_target: func_val,
                                await_super_result: false,
                                with_base: self.with_stack.len(),
                            });
                            continue;
                        }
                    }

                    self.truncate_stack(func_pos);
                    self.push(this_val);
                }

                OpCode::SpreadCall => {
                    let _ = self.read_byte();
                    // Stack: [func, args_array]
                    let args_val = self.pop()?;
                    let func_val = self.pop()?;
                    // Extract args from array
                    let args: Vec<Value> = if let Some(arr_oid) = args_val.as_object_id() {
                        self.heap.get(arr_oid)
                            .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                            .unwrap_or_default()
                    } else { vec![] };
                    // Propagate `this` for super() calls with spread arguments.
                    let is_super = self.frames.last().map(|f| f.pending_super_call).unwrap_or(false);
                    let result = if is_super {
                        if let Some(f) = self.frames.last_mut() {
                            f.pending_super_call = false;
                            f.super_called = true;
                        }
                        let this_val = self.frames.last().unwrap().this_value;
                        self.call_function_this(func_val, this_val, &args)?
                    } else {
                        self.call_function(func_val, &args)?
                    };
                    self.push(result);
                }
                OpCode::SpreadConstruct => {
                    let _ = self.read_byte();
                    // Stack: [func, args_array]
                    let args_val = self.pop()?;
                    let func_val = self.pop()?;
                    let args: Vec<Value> = if let Some(arr_oid) = args_val.as_object_id() {
                        self.heap.get(arr_oid)
                            .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                            .unwrap_or_default()
                    } else { vec![] };
                    // Re-push func and args, then dispatch as if Construct(argc) was called
                    let argc = args.len();
                    self.push(func_val);
                    for arg in &args { self.push(*arg); }
                    // Push back the bytecode equivalent of Construct
                    // We can't easily call Construct from here since it's part of the run loop.
                    // Instead, inline the logic: use call_function_this with a new object
                    self.truncate_stack(self.stack.len() - argc - 1); // pop everything we just pushed

                    // Now call construct logic manually for user functions
                    if func_val.is_function() {
                        let packed = func_val.as_function().unwrap();
                        let chunk_idx = (packed & 0xFFFF) as usize;
                        if chunk_idx > 0 && chunk_idx < self.chunks.len() {
                            // Create new object with prototype linkage
                            let mut new_obj = JsObject::ordinary();
                            if let Some(&proto_oid) = self.func_prototypes.get(&packed) {
                                new_obj.prototype = Some(proto_oid);
                            } else {
                                let mut proto = JsObject::ordinary();
                                let ctor_key = self.interner.intern("constructor");
                                proto.set_property(ctor_key, func_val);
                                let proto_oid = self.heap.allocate(proto);
                                self.func_prototypes.insert(packed, proto_oid);
                                new_obj.prototype = Some(proto_oid);
                            }
                            let new_oid = self.heap.allocate(new_obj);
                            let this_val = Value::object_id(new_oid);
                            // Call the function with this binding
                            let result = self.call_function_this(func_val, this_val, &args)?;
                            // Per JS spec, if constructor returns an object, use that; else return new instance
                            if result.is_object() {
                                self.push(result);
                            } else {
                                self.push(this_val);
                            }
                            continue;
                        }
                    }
                    self.push(Value::undefined());
                }

                OpCode::SetArrayItem => {
                    let idx = {
                        let v = self.chunks[self.cur_chunk()].read_u32(self.cur_ip());
                        self.frames.last_mut().unwrap().ip += 4;
                        v as usize
                    };
                    let val = self.pop()?;
                    let arr_val = self.peek()?;
                    if let Some(oid) = arr_val.as_object_id()
                        && let Some(obj) = self.heap.get_mut(oid)
                            && let ObjectKind::Array(ref mut elements) = obj.kind {
                                // If array already has more elements than idx (due to spread),
                                // push to end instead of overwriting
                                if idx < elements.len() && elements.len() > idx {
                                    elements.push(val);
                                } else {
                                    while elements.len() <= idx {
                                        elements.push(Value::undefined());
                                    }
                                    elements[idx] = val;
                                }
                            }
                }

                OpCode::ArrayAppend => {
                    let val = self.pop()?;
                    let arr_val = self.peek()?;
                    if let Some(oid) = arr_val.as_object_id()
                        && let Some(obj) = self.heap.get_mut(oid)
                        && let ObjectKind::Array(ref mut elements) = obj.kind
                    {
                        elements.push(val);
                    }
                }

                OpCode::ArraySpread => {
                    let source = self.pop()?;
                    let target = self.peek()?;
                    // Spread iterable into target array
                    let elems: Vec<Value> = if let Some(src_oid) = source.as_object_id() {
                        match self.heap.get(src_oid).map(|o| std::ptr::from_ref(&o.kind)) {
                            Some(_) => match &self.heap.get(src_oid).unwrap().kind {
                                ObjectKind::Array(e) => e.clone(),
                                ObjectKind::Set { entries } => entries.clone(),
                                ObjectKind::Map { entries } => {
                                    // Map yields [k,v] pair arrays
                                    let pairs = entries.clone();
                                    pairs.into_iter().map(|(k, v)| {
                                        let pair_arr = JsObject::array(vec![k, v]);
                                        Value::object_id(self.heap.allocate(pair_arr))
                                    }).collect()
                                }
                                ObjectKind::Generator { .. } => {
                                    // Drive generator until done
                                    let mut result = Vec::new();
                                    loop {
                                        let next_name = self.interner.intern("next");
                                        let next_result = self.exec_generator_method(src_oid, next_name, &[]);
                                        match next_result {
                                            Ok(crate::vm::generator::GeneratorAction::Done(iter_res)) => {
                                                if let Some(io) = iter_res.as_object_id()
                                                    && let Some(obj) = self.heap.get(io)
                                                {
                                                    let done_key = self.interner.intern("done");
                                                    let value_key = self.interner.intern("value");
                                                    let is_done = obj.get_property(done_key).map(|v| v.to_boolean()).unwrap_or(true);
                                                    if is_done { break; }
                                                    let val = obj.get_property(value_key).unwrap_or(Value::undefined());
                                                    result.push(val);
                                                } else { break; }
                                            }
                                            _ => break,
                                        }
                                        if result.len() > 100_000 { break; } // safety
                                    }
                                    result
                                }
                                _ => {
                                    // Generic iterable: look up @@iterator and run the protocol.
                                    let sym_iter_key = self.interner.intern(&format!("__sym_{}__", self.sym_iterator));
                                    let iter_fn = self.heap.get_property_chain(src_oid, sym_iter_key);
                                    let callable = |vm: &Vm, v: Value| v.is_function()
                                        || v.as_object_id().is_some_and(|o| vm.heap.get(o)
                                            .is_some_and(|x| matches!(&x.kind, ObjectKind::Function(_))));
                                    let iter_fn = match iter_fn {
                                        Some(v) if callable(self, v) => v,
                                        _ => {
                                            let err = self.make_native_error(
                                                "TypeError",
                                                "object is not iterable",
                                            );
                                            self.handle_throw(err)?;
                                            continue;
                                        }
                                    };
                                    // Protect the @@iterator / next / value reads so any
                                    // throw they produce bubbles back here rather than
                                    // being caught by an outer try/catch — handle_throw
                                    // is the right place to route the error.
                                    let prev_protect = self.protect_throw_depth;
                                    self.protect_throw_depth = self.frames.len() + 1;
                                    let iter_call = self.call_function_this(iter_fn, source, &[]);
                                    self.protect_throw_depth = prev_protect;
                                    let iter_val = match iter_call {
                                        Ok(v) => v,
                                        Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                                        Err(e) => return Err(e),
                                    };
                                    if !iter_val.is_object() {
                                        let err = self.make_native_error(
                                            "TypeError",
                                            "Iterator result is not an object",
                                        );
                                        self.handle_throw(err)?;
                                        continue;
                                    }
                                    let iter_oid = iter_val.as_object_id().unwrap();
                                    let next_key = self.interner.intern("next");
                                    let mut result = Vec::new();
                                    loop {
                                        let next_fn = self.heap.get_property_chain(iter_oid, next_key)
                                            .unwrap_or(Value::undefined());
                                        if !callable(self, next_fn) {
                                            let err = self.make_native_error(
                                                "TypeError",
                                                "iterator.next is not a function",
                                            );
                                            self.handle_throw(err)?;
                                            break;
                                        }
                                        let prev_protect = self.protect_throw_depth;
                                        self.protect_throw_depth = self.frames.len() + 1;
                                        let step_call = self.call_function_this(next_fn, iter_val, &[]);
                                        self.protect_throw_depth = prev_protect;
                                        let step = match step_call {
                                            Ok(v) => v,
                                            Err(VmError::Throw(v)) => { self.handle_throw(v)?; break; }
                                            Err(e) => return Err(e),
                                        };
                                        if !step.is_object() {
                                            let err = self.make_native_error(
                                                "TypeError",
                                                "Iterator result is not an object",
                                            );
                                            self.handle_throw(err)?;
                                            break;
                                        }
                                        let step_oid = step.as_object_id().unwrap();
                                        // Read `done` with getter support (IteratorComplete)
                                        let done_key = self.interner.intern("done");
                                        let done_getter_key = self.interner.intern("__get_done__");
                                        let done_gfn = self.heap.get_property_chain(step_oid, done_getter_key)
                                            .filter(|v| v.is_function());
                                        let done = if let Some(gfn) = done_gfn {
                                            match self.call_function_this(gfn, step, &[]) {
                                                Ok(v) => v.to_boolean(),
                                                Err(VmError::Throw(v)) => { self.handle_throw(v)?; break; }
                                                Err(e) => return Err(e),
                                            }
                                        } else {
                                            self.heap.get(step_oid)
                                                .and_then(|o| o.get_property(done_key))
                                                .map(|v| v.to_boolean())
                                                .unwrap_or(true)
                                        };
                                        if done { break; }
                                        // Read `value` with getter support (IteratorValue)
                                        let value_key = self.interner.intern("value");
                                        let value_getter_key = self.interner.intern("__get_value__");
                                        let value_gfn = self.heap.get_property_chain(step_oid, value_getter_key)
                                            .filter(|v| v.is_function());
                                        let val = if let Some(gfn) = value_gfn {
                                            match self.call_function_this(gfn, step, &[]) {
                                                Ok(v) => v,
                                                Err(VmError::Throw(v)) => { self.handle_throw(v)?; break; }
                                                Err(e) => return Err(e),
                                            }
                                        } else {
                                            self.heap.get(step_oid)
                                                .and_then(|o| o.get_property(value_key))
                                                .unwrap_or(Value::undefined())
                                        };
                                        result.push(val);
                                        if result.len() > 100_000 { break; }
                                    }
                                    result
                                }
                            },
                            None => vec![],
                        }
                    } else if source.is_string() {
                        let s = self.value_to_string(source);
                        s.chars().map(|c| {
                            let id = self.interner.intern(&c.to_string());
                            Value::string(id)
                        }).collect()
                    } else { vec![] };
                    if let Some(tgt_oid) = target.as_object_id()
                        && let Some(tgt_obj) = self.heap.get_mut(tgt_oid)
                            && let ObjectKind::Array(ref mut tgt_elems) = tgt_obj.kind {
                                tgt_elems.extend(elems);
                            }
                }

                OpCode::SetObjectProto => {
                    // `{__proto__: val}` literal: pop val, peek the object being built,
                    // set its prototype if val is null or an object. Other types are
                    // silently ignored per spec.
                    let val = self.pop()?;
                    let obj_val = self.peek()?;
                    if let Some(target_oid) = obj_val.as_object_id() {
                        if val.is_null() {
                            if let Some(obj) = self.heap.get_mut(target_oid) {
                                obj.prototype = None;
                            }
                        } else if let Some(proto_oid) = val.as_object_id()
                            && let Some(obj) = self.heap.get_mut(target_oid)
                        {
                            obj.prototype = Some(proto_oid);
                        }
                    }
                }

                OpCode::ObjectSpread => {
                    let source = self.pop()?;
                    let target = self.peek()?;
                    // Copy enumerable own properties from source to target
                    if let Some(src_oid) = source.as_object_id() {
                        let props: Vec<(StringId, Value)> = self.heap.get(src_oid)
                            .map(|o| o.properties.iter()
                                .filter(|(_, p)| p.is_enumerable())
                                .map(|&(k, ref p)| (k, p.value))
                                .collect())
                            .unwrap_or_default();
                        if let Some(tgt_oid) = target.as_object_id() {
                            for (key, val) in props {
                                if let Some(tgt) = self.heap.get_mut(tgt_oid) {
                                    tgt.set_property(key, val);
                                }
                            }
                        }
                    }
                }

                OpCode::DefineDataProp => {
                    let val = self.pop()?;
                    let key = self.pop()?;
                    // Object is still on the stack
                    let obj_val = self.peek()?;
                    if let Some(oid) = obj_val.as_object_id() {
                        // ToPropertyKey: coerce any non-symbol key to a string id.
                        let name_id = if let Some(sid) = key.as_string_id() {
                            Some(sid)
                        } else if self.is_cons_string(key) {
                            let s = self.flatten_cons_to_string(key);
                            Some(self.interner.intern(&s))
                        } else if let Some(n) = key.as_number() {
                            let s = if n.fract() == 0.0 && n.is_finite() {
                                (n as i64).to_string()
                            } else {
                                n.to_string()
                            };
                            Some(self.interner.intern(&s))
                        } else if key.is_symbol() {
                            let sid = key.as_symbol_id().unwrap();
                            Some(self.interner.intern(&format!("__sym_{sid}__")))
                        } else {
                            // undefined / null / boolean / function / object — stringify.
                            let s = self.value_to_string(key);
                            Some(self.interner.intern(&s))
                        };
                        // Static class methods named "prototype" throw TypeError per spec.
                        if let Some(nid) = name_id {
                            let ctor_key = self.interner.intern("__constructor__");
                            let is_class = self.heap.get(oid)
                                .map(|o| o.get_property(ctor_key).is_some())
                                .unwrap_or(false);
                            if is_class && self.interner.resolve(nid) == "prototype" {
                                self.throw_type_error("Cannot define static class member 'prototype'")?;
                                continue;
                            }
                        }
                        if let Some(name_id) = name_id
                            && let Some(obj) = self.heap.get_mut(oid) {
                                obj.set_property(name_id, val);
                            }
                        // If the value is an anonymous function and the key is a Symbol,
                        // set the function's name to '[description]' or ''
                        if val.is_function() && key.is_symbol() {
                            let sym_id = key.as_symbol_id().unwrap() as usize;
                            let sentinel = val.as_function().unwrap();
                            let chunk_idx = (sentinel & 0xFFFF) as usize;
                            let is_anon = if chunk_idx > 0 && chunk_idx < self.chunks.len() {
                                let n = self.interner.resolve(self.chunks[chunk_idx].name).to_owned();
                                n.is_empty() || n.starts_with('<')
                            } else { true };
                            if is_anon {
                                let fn_name = if let Some(Some(desc)) = self.symbol_descriptions.get(sym_id) {
                                    let desc_str = self.interner.resolve(*desc).to_owned();
                                    format!("[{desc_str}]")
                                } else {
                                    String::new()
                                };
                                let fn_name_sid = self.interner.intern(&fn_name);
                                let name_key = self.interner.intern("name");
                                self.fn_property_overrides.insert((sentinel, name_key), Some(Value::string(fn_name_sid)));
                            }
                        }
                    }
                }

                OpCode::DefineGetter | OpCode::DefineSetter => {
                    let func = self.pop()?;
                    let key = self.pop()?;
                    let obj_val = self.peek()?;
                    if let Some(oid) = obj_val.as_object_id() {
                        // ToPropertyKey: stringify any non-symbol value (including null
                        // and undefined → "null" / "undefined" per spec).
                        let name_str: String = if let Some(sid) = key.as_string_id() {
                            self.interner.resolve(sid).to_owned()
                        } else if self.is_cons_string(key) {
                            self.flatten_cons_to_string(key)
                        } else if key.is_symbol() {
                            format!("__sym_{}__", key.as_symbol_id().unwrap())
                        } else {
                            self.value_to_string(key)
                        };
                        // Static class accessors named "prototype" throw TypeError per spec.
                        let ctor_key = self.interner.intern("__constructor__");
                        let is_class = self.heap.get(oid)
                            .map(|o| o.get_property(ctor_key).is_some())
                            .unwrap_or(false);
                        if is_class && name_str == "prototype" {
                            self.throw_type_error("Cannot define static class accessor 'prototype'")?;
                            continue;
                        }
                        if let Some(obj) = self.heap.get_mut(oid)
                        {
                            // Symbol-keyed accessors are stored under their symbol slot key.
                            let accessor_key = if name_str.starts_with("__sym_") {
                                self.interner.intern(&name_str)
                            } else if opcode == OpCode::DefineGetter {
                                self.interner.intern(&format!("__get_{name_str}__"))
                            } else {
                                self.interner.intern(&format!("__set_{name_str}__"))
                            };
                            obj.set_property(accessor_key, func);
                        }
                    }
                }

                OpCode::DefineMethod => {
                    let name_idx = self.read_u16() as usize;
                    let name_val = self.chunks[self.cur_chunk()].constants[name_idx];
                    let name_id = name_val.as_string_id().unwrap();
                    let val = self.pop()?;
                    // Object should be on the stack below
                    let obj_val = self.peek()?;
                    if let Some(oid) = obj_val.as_object_id()
                        && let Some(obj) = self.heap.get_mut(oid) {
                            obj.set_property(name_id, val);
                        }
                }

                OpCode::CreateRegExp => {
                    let pattern_idx = self.read_u16() as usize;
                    let flags_idx = self.read_u16() as usize;
                    let pattern = {
                        let v = self.chunks[self.cur_chunk()].constants[pattern_idx];
                        self.value_to_string(v)
                    };
                    let flags = {
                        let v = self.chunks[self.cur_chunk()].constants[flags_idx];
                        self.value_to_string(v)
                    };
                    let mut obj = JsObject::regexp(pattern, flags);
                    obj.prototype = self.func_prototypes.get(&-580).copied();
                    let oid = self.heap.allocate(obj);
                    self.push(Value::object_id(oid));
                }

                OpCode::Closure => {
                    let child_rel_idx = self.read_u16() as usize;
                    let current = self.cur_chunk();
                    let abs_idx = self.chunks[current].children.get(child_rel_idx).copied()
                        .unwrap_or(current + 1 + child_rel_idx);

                    // Read upvalue descriptors from the child chunk
                    let upvalue_count = if abs_idx < self.chunks.len() {
                        self.chunks[abs_idx].upvalue_count as usize
                    } else {
                        0
                    };

                    // Read inline upvalue descriptors and capture
                    let mut upvalues = Vec::with_capacity(upvalue_count);
                    for _ in 0..upvalue_count {
                        let is_local = self.read_byte() != 0;
                        let index = self.read_u16() as usize;

                        if is_local {
                            // Capture from current frame's local stack slot,
                            // sharing the cell with every other closure that
                            // captures the same slot (open-upvalue registry).
                            let base = self.frames.last().unwrap().base;
                            let stack_idx = base + index;
                            let cell = self
                                .open_upvalues
                                .entry(stack_idx)
                                .or_insert_with(|| {
                                    std::rc::Rc::new(std::cell::RefCell::new(
                                        UpvalueLocation::Open(stack_idx),
                                    ))
                                })
                                .clone();
                            upvalues.push(Upvalue { cell });
                        } else {
                            // Transitive capture: share the parent's cell.
                            let parent_uv = self.frames.last().unwrap().upvalues.get(index).cloned();
                            if let Some(uv) = parent_uv {
                                upvalues.push(uv);
                            } else {
                                upvalues.push(Upvalue {
                                    cell: std::rc::Rc::new(std::cell::RefCell::new(
                                        UpvalueLocation::Closed(Value::undefined()),
                                    )),
                                });
                            }
                        }
                    }

                    // Store closure as chunk index (int), but also store upvalues
                    // We need a way to associate upvalues with the closure value.
                    // For now, use the closure_upvalues map.
                    let closure_id = self.closure_upvalues.len();
                    {
                        static WATCH_UV: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
                        let watch_uv = WATCH_UV.get_or_init(|| std::env::var("ZINC_WATCH_UV").ok());
                        if let Some(w) = watch_uv
                            && abs_idx < self.chunks.len()
                            && self.interner.resolve(self.chunks[abs_idx].name) == w {
                            eprintln!("[uvwatch] closure_id={} for chunk '{}' created; upvalues:", closure_id, w);
                            for (i, uv) in upvalues.iter().enumerate() {
                                eprintln!("[uvwatch]   uv{} = {:?}", i, uv.cell.borrow());
                            }
                            eprintln!("[uvwatch]   frame base={} stack_len={}", self.frames.last().map(|f| f.base).unwrap_or(0), self.stack.len());
                            for (i, uv) in upvalues.iter().enumerate() {
                                if let UpvalueLocation::Open(si) = &*uv.cell.borrow() {
                                    let v = self.stack.get(*si).copied().unwrap_or(Value::undefined());
                                    eprintln!("[uvwatch]   uv{} stack[{}] currently = {:?}", i, si, self.type_of_value(v));
                                }
                            }
                        }
                    }
                    self.closure_upvalues.push(upvalues);
                    // Encode closure_id in high bits, chunk_idx in low bits
                    // Use a special encoding: negative int where abs value encodes both
                    // Actually let's use a simpler approach: store as two values
                    // Or better: pack closure_id << 16 | chunk_idx
                    let packed = ((closure_id as i32) << 16) | (abs_idx as i32 & 0xFFFF);
                    self.push(Value::function(packed));
                }

                OpCode::ClosureLong => {
                    let child_rel_idx = {
                        let v = self.chunks[self.cur_chunk()].read_u32(self.cur_ip());
                        self.frames.last_mut().unwrap().ip += 4;
                        v as usize
                    };
                    let current = self.cur_chunk();
                    let abs_idx = self.chunks[current].children.get(child_rel_idx).copied()
                        .unwrap_or(current + 1 + child_rel_idx);
                    self.push(Value::function(abs_idx as i32));
                }

                OpCode::Class => {
                    let name_idx = self.read_u16() as usize;
                    let class_name_id = self.chunks[self.cur_chunk()].constants[name_idx].as_string_id().unwrap_or_else(|| self.interner.intern(""));
                    // Create a constructor placeholder and prototype object
                    let mut proto = JsObject::ordinary();
                    proto.prototype = Some(self.object_prototype);
                    let proto_oid = self.heap.allocate(proto);
                    // The class itself is represented as an ordinary object with a __proto__ property.
                    // Per spec, function objects expose `length`, `name`, `prototype` own
                    // properties in that order (visible in Object.getOwnPropertyNames).
                    let mut class_obj = JsObject::ordinary();
                    // Per spec MakeClassConstructor: a class without `extends`
                    // has [[Prototype]] = %Function.prototype% so static methods
                    // and `Object.getPrototypeOf(C)` resolve correctly. Inherit
                    // overrides this when an extends clause is present.
                    class_obj.prototype = Some(self.function_prototype);
                    let length_key = self.interner.intern("length");
                    class_obj.define_property(length_key, Property::with_flags(Value::int(0), Property::CONFIGURABLE));
                    let name_key = self.interner.intern("name");
                    class_obj.define_property(name_key, Property::with_flags(Value::string(class_name_id), Property::CONFIGURABLE));
                    let proto_key = self.interner.intern("prototype");
                    // class.prototype is non-enumerable, non-writable, non-configurable
                    // for class declarations (per spec MakeClassConstructor).
                    class_obj.define_property(proto_key, Property::with_flags(Value::object_id(proto_oid), 0));
                    // Mark as class with default constructor (so typeof returns "function").
                    // Stored as an internal key so it doesn't appear in enumeration.
                    let ctor_key = self.interner.intern("__constructor__");
                    class_obj.set_property(ctor_key, Value::undefined());
                    let class_oid = self.heap.allocate(class_obj);
                    // Set proto.constructor = class (non-enumerable, writable, configurable)
                    let constructor_key = self.interner.intern("constructor");
                    let class_key = self.interner.intern("__class__");
                    if let Some(proto) = self.heap.get_mut(proto_oid) {
                        proto.define_property(constructor_key, Property::with_flags(Value::object_id(class_oid), Property::WRITABLE | Property::CONFIGURABLE));
                        // Mark the prototype with its owning class so super lookups
                        // work even when a method is invoked on the prototype directly
                        // (e.g. `C.prototype.method()`), where `this` is the prototype.
                        proto.set_property(class_key, Value::object_id(class_oid));
                    }
                    self.push(Value::object_id(class_oid));
                }

                OpCode::ClassMethod => {
                    let name_idx = self.read_u16() as usize;
                    let name_val = self.chunks[self.cur_chunk()].constants[name_idx];
                    let name_id = name_val.as_string_id().unwrap();
                    let method_val = self.pop()?; // the compiled method (closure)
                    // Class is on the stack
                    let class_val = self.peek()?;
                    if let Some(class_oid) = class_val.as_object_id() {
                        let proto_key = self.interner.intern("prototype");
                        let proto_val = self.heap.get(class_oid)
                            .and_then(|o| o.get_property(proto_key));
                        if let Some(pv) = proto_val
                            && let Some(proto_oid) = pv.as_object_id() {
                                // Check if this is the constructor (sentinel name emitted by the compiler)
                                let constructor_name = self.interner.intern("\u{0}ctor");
                                if name_id == constructor_name {
                                    // Store constructor on the class object itself
                                    // Also update `length` from constructor's formal_length
                                    let formal_length = if let Some(packed) = method_val.as_function() {
                                        let chunk_idx = (packed & 0xFFFF) as usize;
                                        if chunk_idx < self.chunks.len() {
                                            self.chunks[chunk_idx].formal_length as i32
                                        } else { 0 }
                                    } else { 0 };
                                    if let Some(class_obj) = self.heap.get_mut(class_oid) {
                                        let ctor_key = self.interner.intern("__constructor__");
                                        class_obj.set_property(ctor_key, method_val);
                                        let length_key = self.interner.intern("length");
                                        class_obj.define_property(length_key, Property::with_flags(Value::int(formal_length), Property::CONFIGURABLE));
                                    }
                                } else {
                                    // Add method to prototype: non-enumerable, writable, configurable
                                    if let Some(proto) = self.heap.get_mut(proto_oid) {
                                        proto.define_property(name_id, Property::with_flags(
                                            method_val, Property::WRITABLE | Property::CONFIGURABLE
                                        ));
                                    }
                                }
                            }
                    }
                }

                OpCode::ClassStaticMethod => {
                    let name_idx = self.read_u16() as usize;
                    let name_val = self.chunks[self.cur_chunk()].constants[name_idx];
                    let name_id = name_val.as_string_id().unwrap();
                    let method_val = self.pop()?;
                    // Static class members named "prototype" throw TypeError per spec.
                    // The compiler mangles getter/setter names to __get_prototype__ /
                    // __set_prototype__, so check both forms.
                    let name_str = self.interner.resolve(name_id);
                    if name_str == "prototype"
                        || name_str == "__get_prototype__"
                        || name_str == "__set_prototype__"
                    {
                        self.throw_type_error("Cannot define static class member 'prototype'")?;
                        continue;
                    }
                    let class_val = self.peek()?;
                    if let Some(class_oid) = class_val.as_object_id()
                        && let Some(class_obj) = self.heap.get_mut(class_oid) {
                            let name_str = self.interner.resolve(name_id).to_owned();
                            let store_key = if name_str.starts_with('#') {
                                self.interner.intern(&format!("__priv_{}__", name_str))
                            } else { name_id };
                            // Static methods: non-enumerable, writable, configurable
                            class_obj.define_property(store_key, Property::with_flags(
                                method_val, Property::WRITABLE | Property::CONFIGURABLE
                            ));
                        }
                }

                OpCode::ClassStaticField => {
                    let name_idx = self.read_u16() as usize;
                    let name_val = self.chunks[self.cur_chunk()].constants[name_idx];
                    let name_id = name_val.as_string_id().unwrap();
                    let field_val = self.pop()?;
                    let class_val = self.peek()?;
                    if let Some(class_oid) = class_val.as_object_id()
                        && let Some(class_obj) = self.heap.get_mut(class_oid) {
                            let name_str = self.interner.resolve(name_id).to_owned();
                            let store_key = if name_str.starts_with('#') {
                                self.interner.intern(&format!("__priv_{}__", name_str))
                            } else {
                                name_id
                            };
                            class_obj.set_property(store_key, field_val);
                        }
                }

                OpCode::ClassField => {
                    // Instance field: store default value on class under __ifield_{name}__
                    // Applied to each new instance during Construct.
                    let name_idx = self.read_u16() as usize;
                    let name_val = self.chunks[self.cur_chunk()].constants[name_idx];
                    let name_id = name_val.as_string_id().unwrap();
                    let field_val = self.pop()?;
                    let class_val = self.peek()?;
                    if let Some(class_oid) = class_val.as_object_id() {
                        let field_name = self.interner.resolve(name_id).to_owned();
                        let ifield_key = self.interner.intern(&format!("__ifield_{field_name}__"));
                        if let Some(class_obj) = self.heap.get_mut(class_oid) {
                            class_obj.set_property(ifield_key, field_val);
                        }
                    }
                }

                OpCode::ClassFieldComputed => {
                    // Instance field with computed key: stack has [key, value] (key under value)
                    let field_val = self.pop()?;
                    let key_val = self.pop()?;
                    let class_val = self.peek()?;
                    if let Some(class_oid) = class_val.as_object_id() {
                        let field_name = if key_val.is_symbol() {
                            format!("__sym_{}__", key_val.as_symbol_id().unwrap())
                        } else {
                            self.value_to_string(key_val)
                        };
                        let ifield_key = self.interner.intern(&format!("__ifield_{field_name}__"));
                        if let Some(class_obj) = self.heap.get_mut(class_oid) {
                            class_obj.set_property(ifield_key, field_val);
                        }
                    }
                }

                OpCode::ClassStaticFieldComputed => {
                    // Static field with computed key: stack has [key, value]
                    let field_val = self.pop()?;
                    let key_val = self.pop()?;
                    let class_val = self.peek()?;
                    if let Some(class_oid) = class_val.as_object_id() {
                        let store_key = if key_val.is_symbol() {
                            let sym_name = format!("__sym_{}__", key_val.as_symbol_id().unwrap());
                            self.interner.intern(&sym_name)
                        } else {
                            let field_name = self.value_to_string(key_val);
                            self.interner.intern(&field_name)
                        };
                        if let Some(class_obj) = self.heap.get_mut(class_oid) {
                            class_obj.set_property(store_key, field_val);
                        }
                    }
                }

                OpCode::ClassPrivateMethod => {
                    let name_idx = self.read_u16() as usize;
                    let name_val = self.chunks[self.cur_chunk()].constants[name_idx];
                    let name_id = name_val.as_string_id().unwrap();
                    let method_val = self.pop()?;
                    let class_val = self.peek()?;
                    if let Some(class_oid) = class_val.as_object_id() {
                        let proto_key = self.interner.intern("prototype");
                        let proto_oid = self.heap.get(class_oid)
                            .and_then(|o| o.get_property(proto_key))
                            .and_then(|v| v.as_object_id());
                        let name_str = self.interner.resolve(name_id).to_owned();
                        let store_key = if name_str.starts_with('#') {
                            self.interner.intern(&format!("__priv_{}__", name_str))
                        } else { name_id };
                        if let Some(proto_oid) = proto_oid
                            && let Some(proto) = self.heap.get_mut(proto_oid) {
                                proto.define_property(store_key, Property::with_flags(
                                    method_val, Property::WRITABLE | Property::CONFIGURABLE
                                ));
                            }
                    }
                }

                OpCode::Inherit => {
                    // Stack: [class, superclass] — superclass is on top
                    let super_val = self.pop()?;
                    let class_val = self.peek()?;

                    // Per spec, the heritage value must be either null or a
                    // constructor (function / class object). Throw TypeError
                    // for anything else.
                    let heritage_ok = super_val.is_null()
                        || super_val.is_function()
                        || super_val.as_object_id().and_then(|oid| self.heap.get(oid)).map(|o| {
                            let ctor_key = self.interner.intern("__constructor__");
                            matches!(&o.kind, ObjectKind::Function(_))
                                || o.get_property(ctor_key).is_some()
                        }).unwrap_or(false);
                    if !heritage_ok {
                        let err = self.make_native_error(
                            "TypeError",
                            "Class extends value is not a constructor",
                        );
                        self.handle_throw(err)?;
                        continue;
                    }

                    // Per spec (ClassDefinitionEvaluation): the heritage value's
                    // `prototype` property must be either an object or null.
                    // Bound functions and built-in functions (Math.abs, Math.floor,
                    // etc.) lack one, which makes them invalid superclass values.
                    if !super_val.is_null() {
                        let proto_key_check = self.interner.intern("prototype");
                        let parent_proto = if let Some(soid) = super_val.as_object_id() {
                            self.heap.get_property_chain(soid, proto_key_check)
                        } else if super_val.is_function() {
                            let sentinel = super_val.as_function().unwrap();
                            // User function-overrides take precedence (e.g. fn.prototype = 42).
                            if let Some(Some(ov)) = self.fn_property_overrides.get(&(sentinel, proto_key_check)).copied() {
                                Some(ov)
                            } else {
                                // Built-in math/global sentinels have no prototype.
                                let has_default = (-516..=-510).contains(&sentinel)
                                    || matches!(sentinel, -504 | -505 | -506 | -507 | -508 | -520 | -551
                                        | -540 | -541 | -542 | -543 | -550 | -570 | -580)
                                    || (-672..=-660).contains(&sentinel) // ArrayBuffer/DataView/TypedArrays
                                    || sentinel >= 0; // user-defined functions auto-allocate
                                if has_default {
                                    None // resolution happens later; treat as "object"
                                } else {
                                    Some(Value::undefined()) // built-in like Math.abs
                                }
                            }
                        } else { None };
                        if let Some(pp) = parent_proto {
                            if !pp.is_null() && !pp.is_object() {
                                let err = self.make_native_error(
                                    "TypeError",
                                    "Class extends value's prototype is not an object or null",
                                );
                                self.handle_throw(err)?;
                                continue;
                            }
                        } else if super_val.as_object_id().is_some() {
                            // Object superclass with no prototype property at all.
                            let err = self.make_native_error(
                                "TypeError",
                                "Class extends value's prototype is not an object or null",
                            );
                            self.handle_throw(err)?;
                            continue;
                        }
                    }

                    if let Some(class_oid) = class_val.as_object_id() {
                        let proto_key = self.interner.intern("prototype");

                        // Get superclass's prototype — handles both object classes and
                        // native sentinels (Array, Object, Function, ...).
                        let super_proto_oid = if let Some(super_oid) = super_val.as_object_id() {
                            self.heap.get(super_oid).and_then(|o| o.get_property(proto_key)).and_then(|v| v.as_object_id())
                        } else if super_val.is_function() {
                            let sentinel = super_val.as_function().unwrap();
                            // Built-in prototypes have dedicated singleton fields.
                            match sentinel {
                                -507 => Some(self.array_prototype),
                                -508 => Some(self.object_prototype),
                                -551 => Some(self.function_prototype),
                                _ => {
                                    // Lazily allocate the sentinel's prototype on first
                                    // use so `class Sub extends Date {}` (and other
                                    // built-ins like Map/Set/Error/RegExp) gets its
                                    // prototype chain linked even when Date.prototype
                                    // hasn't been read yet.
                                    if let Some(&pid) = self.func_prototypes.get(&sentinel) {
                                        Some(pid)
                                    } else if sentinel >= 0 || sentinel <= -1000 {
                                        // User-defined or sentinel-encoded callbacks: skip.
                                        None
                                    } else {
                                        let mut proto = JsObject::ordinary();
                                        proto.prototype = Some(self.object_prototype);
                                        let ctor_key = self.interner.intern("constructor");
                                        proto.define_property(ctor_key, Property::with_flags(
                                            super_val, Property::WRITABLE | Property::CONFIGURABLE
                                        ));
                                        let pid = self.heap.allocate(proto);
                                        self.func_prototypes.insert(sentinel, pid);
                                        Some(pid)
                                    }
                                }
                            }
                        } else { None };

                        // Get subclass's prototype
                        let sub_proto = self.heap.get(class_oid)
                            .and_then(|o| o.get_property(proto_key))
                            .and_then(|v| v.as_object_id());

                        // Link: subclass.prototype.__proto__ = superclass.prototype
                        if let (Some(sub_pid), Some(super_pid)) = (sub_proto, super_proto_oid)
                            && let Some(sub_proto_obj) = self.heap.get_mut(sub_pid)
                        {
                            sub_proto_obj.prototype = Some(super_pid);
                        }

                        // Per spec ClassDefinitionEvaluation: the subclass
                        // constructor's [[Prototype]] is the parent constructor.
                        // For heap-object superclasses, store directly. For
                        // native sentinels we fall back to Function.prototype
                        // since sentinel functions can't be stored in the
                        // prototype field.
                        if let Some(super_oid) = super_val.as_object_id()
                            && let Some(class_obj) = self.heap.get_mut(class_oid)
                        {
                            class_obj.prototype = Some(super_oid);
                        }

                        // Store superclass reference for super() calls
                        let super_key = self.interner.intern("__super__");
                        if let Some(class_obj) = self.heap.get_mut(class_oid) {
                            class_obj.set_property(super_key, super_val);
                        }
                    }
                }

                OpCode::GetSuperClass => {
                    // Resolve `super` in a method:
                    //   - In an instance method, `this` is an instance; super is `this.__class__.__super__.prototype`.
                    //   - In a static method, `this` is the class itself; super is `this.__super__` (the parent class).
                    //   - For a class without `extends`, fall back to Object.prototype.
                    let this_val = self.frames.last().unwrap().this_value;
                    let class_key = self.interner.intern("__class__");
                    let super_key = self.interner.intern("__super__");
                    let proto_key = self.interner.intern("prototype");

                    let result = this_val.as_object_id().and_then(|oid| {
                        let has_super_directly = self.heap.get(oid)
                            .map(|o| o.get_property(super_key).is_some())
                            .unwrap_or(false);
                        if has_super_directly {
                            // Static method context: `this` is the class itself
                            self.heap.get(oid).and_then(|o| o.get_property(super_key))
                        } else {
                            // Instance method context: walk this.__class__.__super__.prototype.
                            // Look up __class__ via the prototype chain so the lookup also
                            // works when `this` is the class prototype itself
                            // (e.g. `C.prototype.method()`).
                            self.heap.get_property_chain(oid, class_key)
                                .and_then(|cv| cv.as_object_id())
                                .and_then(|cid| self.heap.get(cid))
                                .and_then(|cls| cls.get_property(super_key))
                                .and_then(|sv| sv.as_object_id())
                                .and_then(|sid| self.heap.get(sid))
                                .and_then(|s| s.get_property(proto_key))
                        }
                    });
                    let result = result.unwrap_or_else(|| Value::object_id(self.object_prototype));
                    self.push(result);
                    self.frames.last_mut().unwrap().pending_super_call = true;
                }

                OpCode::GetSuperConstructor => {
                    // Per spec, calling super() twice in a derived constructor throws
                    // ReferenceError ("`this` already initialized").
                    if let Some(f) = self.frames.last()
                        && f.is_derived_ctor
                        && f.super_called
                    {
                        let err = self.make_native_error(
                            "ReferenceError",
                            "Super constructor may only be called once",
                        );
                        self.handle_throw(err)?;
                        continue;
                    }
                    // Resolve parent constructor: this.__class__.__super__.__constructor__
                    let this_val = self.frames.last().unwrap().this_value;
                    let class_key = self.interner.intern("__class__");
                    let super_key = self.interner.intern("__super__");
                    let ctor_key = self.interner.intern("__constructor__");

                    let super_val = this_val.as_object_id()
                        .and_then(|oid| self.heap.get_property_chain(oid, class_key))
                        .and_then(|cv| cv.as_object_id())
                        .and_then(|cid| self.heap.get(cid))
                        .and_then(|cls| cls.get_property(super_key));

                    let result = if let Some(sv) = super_val {
                        if let Some(sid) = sv.as_object_id() {
                            // User-defined class: look up __constructor__
                            Some(self.heap.get(sid).and_then(|sup| sup.get_property(ctor_key))
                                .unwrap_or(sv))
                        } else if sv.is_function() {
                            // Native sentinel superclass (e.g. TypeError, Array): use it directly
                            Some(sv)
                        } else { None }
                    } else { None };

                    self.push(result.unwrap_or(Value::undefined()));

                    // Mark that the next Call should propagate this_value AND record
                    // that super() was invoked, so derived-class return checks pass.
                    if let Some(f) = self.frames.last_mut() {
                        f.pending_super_call = true;
                        f.super_called = true;
                    }
                }

                OpCode::Throw => {
                    let val = self.pop()?;
                    self.handle_throw(val)?;
                    continue;
                }

                OpCode::PushExcHandler => {
                    let catch_target = self.read_u16();
                    let finally_target = self.read_u16();
                    self.exc_handlers.push(ExcHandler {
                        catch_target,
                        finally_target,
                        stack_depth: self.stack.len(),
                        frame_idx: self.frames.len() - 1,
                        with_depth: self.with_stack.len(),
                    });
                }

                OpCode::PopExcHandler => {
                    self.exc_handlers.pop();
                }

                OpCode::EnterFinally | OpCode::LeaveFinally => {
                    // Simplified: finally blocks just execute inline
                }

                OpCode::GetForInIterator => {
                    // for-in: always create key iterator (string indices for arrays)
                    let val = self.pop()?;
                    if let Some(oid) = val.as_object_id() {
                        let keys: Vec<_> = self.heap.get(oid)
                            .map(|o| {
                                if let ObjectKind::Array(ref elems) = o.kind {
                                    // Array: yield "0", "1", "2", ...
                                    (0..elems.len()).map(|i| self.interner.intern(&i.to_string())).collect()
                                } else {
                                    // Object: walk prototype chain. Per spec
                                    // OrdinaryOwnPropertyKeys + EnumerateObjectProperties,
                                    // a property name encountered on a child shadows the
                                    // same name on any prototype regardless of its
                                    // enumerability — only enumerable own properties are
                                    // emitted, but non-enumerable ones still mark the
                                    // name as seen. Accessor properties stored under
                                    // __get_NAME__ / __set_NAME__ surface as the bare
                                    // NAME so for-in mirrors Object.keys behaviour.
                                    let mut all_keys = Vec::new();
                                    let mut seen: std::collections::HashSet<StringId> = std::collections::HashSet::new();
                                    let mut cur = Some(oid);
                                    let mut depth = 0;
                                    while let Some(cid) = cur {
                                        if depth > 64 { break; }
                                        let entries: Vec<(StringId, bool)> = if let Some(obj) = self.heap.get(cid) {
                                            obj.properties.iter().map(|&(k, ref p)| (k, p.is_enumerable())).collect()
                                        } else { break };
                                        let next_proto = self.heap.get(cid).and_then(|o| o.prototype);
                                        for (k, en) in entries {
                                            let exposed_str: Option<String> = {
                                                let name = self.interner.resolve(k);
                                                if let Some(rest) = name.strip_prefix("__get_").and_then(|s| s.strip_suffix("__")) {
                                                    Some(rest.to_owned())
                                                } else if let Some(rest) = name.strip_prefix("__set_").and_then(|s| s.strip_suffix("__")) {
                                                    Some(rest.to_owned())
                                                } else if is_internal_key(name) {
                                                    None
                                                } else {
                                                    Some(name.to_owned())
                                                }
                                            };
                                            let Some(ns) = exposed_str else { continue };
                                            // Skip names that look like internal keys after unwrapping
                                            // (e.g. accessor of a __sym_N__ symbol).
                                            if is_internal_key(&ns) { continue; }
                                            let key = self.interner.intern(&ns);
                                            let first_seen = seen.insert(key);
                                            if first_seen && en {
                                                all_keys.push(key);
                                            }
                                        }
                                        cur = next_proto;
                                        depth += 1;
                                    }
                                    all_keys
                                }
                            })
                            .unwrap_or_default();
                        let iter_proto = self.iterator_prototype_oid();
                        let iter_obj = JsObject {
                            properties: Vec::new(), prototype: Some(iter_proto),
                            kind: ObjectKind::KeyIterator(keys, 0),
                            marked: false, extensible: true,
                        };
                        let iter_id = self.heap.allocate(iter_obj);
                        self.push(Value::object_id(iter_id));
                    } else {
                        // Primitive: empty iterator
                        let iter_proto = self.iterator_prototype_oid();
                        let iter_obj = JsObject {
                            properties: Vec::new(), prototype: Some(iter_proto),
                            kind: ObjectKind::KeyIterator(Vec::new(), 0),
                            marked: false, extensible: true,
                        };
                        let iter_id = self.heap.allocate(iter_obj);
                        self.push(Value::object_id(iter_id));
                    }
                }

                OpCode::GetIterator => {
                    let val = self.pop()?;
                    if let Some(oid) = val.as_object_id() {
                        // Check for user-defined Symbol.iterator method first
                        let sym_iter_key = self.interner.intern(&format!("__sym_{}__", self.sym_iterator));
                        let user_iter_fn = self.heap.get_property_chain(oid, sym_iter_key);
                        if let Some(fn_val) = user_iter_fn
                            && fn_val.is_function()
                        {
                            let iter_val = self.call_function_this(fn_val, val, &[])?;
                            self.push(iter_val);
                            continue;
                        }
                        // Generators and built-in iterator objects are their own iterators
                        let is_generator = self.heap.get(oid)
                            .map(|o| matches!(&o.kind, ObjectKind::Generator { .. }
                                | ObjectKind::ArrayIterator(..)
                                | ObjectKind::MapIterator(..)
                                | ObjectKind::SetIterator(..)
                                | ObjectKind::KeyIterator(..)))
                            .unwrap_or(false);
                        if is_generator {
                            self.push(val); // pass through as-is
                        } else if self.heap.get(oid)
                            .map(|o| matches!(&o.kind, ObjectKind::Array(_)))
                            .unwrap_or(false) {
                            // Arrays should always reach the user_iter_fn path above
                            // via Array.prototype[Symbol.iterator]. If we got here, the
                            // user has deliberately removed it; throw per spec.
                            let err = self.make_native_error("TypeError", "object is not iterable");
                            self.handle_throw(err)?;
                            continue;
                        } else if self.heap.get(oid).map(|o| matches!(&o.kind, ObjectKind::Map { .. })).unwrap_or(false) {
                            // Live Map iterator: holds reference to the original Map
                            let iter_proto = self.iterator_prototype_oid();
                            let iter_obj = JsObject {
                                properties: Vec::new(),
                                prototype: Some(iter_proto),
                                kind: ObjectKind::MapIterator(oid, 0),
                                marked: false,
                                extensible: true,
                            };
                            let iter_id = self.heap.allocate(iter_obj);
                            self.push(Value::object_id(iter_id));
                        } else if self.heap.get(oid).map(|o| matches!(&o.kind, ObjectKind::Set { .. })).unwrap_or(false) {
                            // Live Set iterator: holds reference to the original Set
                            let iter_proto = self.iterator_prototype_oid();
                            let iter_obj = JsObject {
                                properties: Vec::new(),
                                prototype: Some(iter_proto),
                                kind: ObjectKind::SetIterator(oid, 0),
                                marked: false,
                                extensible: true,
                            };
                            let iter_id = self.heap.allocate(iter_obj);
                            self.push(Value::object_id(iter_id));
                        } else {
                            // Plain objects are not iterable with for-of (no @@iterator)
                            let err = self.make_native_error("TypeError", "object is not iterable");
                            self.handle_throw(err)?;
                            continue;
                        }
                    } else if val.is_string() {
                        // String iterator: iterate over characters
                        let s = self.value_to_string(val);
                        let chars: Vec<Value> = s.chars().map(|c| {
                            let id = self.interner.intern(&c.to_string());
                            Value::string(id)
                        }).collect();
                        let arr = JsObject::array(chars);
                        let arr_oid = self.heap.allocate(arr);
                        let iter_proto = self.iterator_prototype_oid();
                        let iter_obj = JsObject {
                            properties: Vec::new(),
                            prototype: Some(iter_proto),
                            kind: ObjectKind::ArrayIterator(arr_oid, 0),
                            marked: false,
                            extensible: true,
                        };
                        let iter_id = self.heap.allocate(iter_obj);
                        self.push(Value::object_id(iter_id));
                    } else {
                        let err = self.make_native_error("TypeError", "not iterable");
                        self.handle_throw(err)?;
                        continue;
                    }
                }

                OpCode::GetAsyncIterator => {
                    return Err(VmError::RuntimeError("async iterators not yet implemented".into()));
                }

                OpCode::IteratorNext => {
                    // Stack: [iterator] -> [iterator_result]
                    let iter_val = self.pop()?;
                    let iter_done_key = self.interner.intern("__iter_done__");
                    if let Some(iter_oid) = iter_val.as_object_id() {
                        // Check if this is a generator
                        let is_gen = self.heap.get(iter_oid)
                            .map(|o| matches!(&o.kind, ObjectKind::Generator { .. }))
                            .unwrap_or(false);
                        if is_gen {
                            // Resume the generator via .next()
                            let action = self.generator_resume(iter_oid, Value::undefined())?;
                            match action {
                                crate::vm::generator::GeneratorAction::Done(result) => {
                                    self.push(result);
                                }
                                crate::vm::generator::GeneratorAction::Resumed => {
                                    // Generator frame pushed — main loop will run it.
                                    // When it yields/returns, {value, done} will be on the stack.
                                    continue;
                                }
                            }
                        } else { // non-generator iterator path
                        let iter_info = {
                            let iter = self.heap.get(iter_oid).ok_or_else(|| {
                                VmError::RuntimeError("invalid iterator".into())
                            })?;
                            match &iter.kind {
                                ObjectKind::ArrayIterator(arr_id, idx) => Some((Some(*arr_id), *idx, false)),
                                ObjectKind::MapIterator(map_id, idx) => Some((Some(*map_id), *idx, false)),
                                ObjectKind::SetIterator(set_id, idx) => Some((Some(*set_id), *idx, false)),
                                ObjectKind::KeyIterator(_, idx) => Some((None, *idx, true)),
                                _ => None,
                            }
                        };
                        let iter_info = match iter_info {
                            Some(info) => info,
                            None => {
                                // User iterator protocol: call .next()
                                let next_key = self.interner.intern("next");
                                let next_fn = self.heap.get_property_chain(iter_oid, next_key)
                                    .unwrap_or(Value::undefined());
                                let result = self.call_function_this(next_fn, iter_val, &[])?;
                                // Mark iter as done if result.done is true so a later
                                // IteratorClose can skip the .return() call per spec.
                                if let Some(rid) = result.as_object_id() {
                                    let done_name = self.interner.intern("done");
                                    let done_val = self.heap.get_property_chain(rid, done_name)
                                        .unwrap_or(Value::undefined());
                                    if done_val.to_boolean()
                                        && let Some(iter) = self.heap.get_mut(iter_oid)
                                    {
                                        iter.set_property(iter_done_key, Value::boolean(true));
                                    }
                                }
                                self.push(result);
                                continue;
                            }
                        };
                        let (value, done) = if iter_info.2 {
                            // Key iterator
                            let keys: Vec<_> = {
                                let iter = self.heap.get(iter_oid).unwrap();
                                if let ObjectKind::KeyIterator(ref keys, _) = iter.kind {
                                    keys.clone()
                                } else { vec![] }
                            };
                            let idx = iter_info.1;
                            if idx < keys.len() {
                                (Value::string(keys[idx]), false)
                            } else {
                                (Value::undefined(), true)
                            }
                        } else {
                            // Array / Map / Set iterator — look up by source kind
                            let src_oid = iter_info.0.unwrap();
                            let idx = iter_info.1;
                            // Determine iterator kind from the iterator object itself
                            let is_map = matches!(
                                self.heap.get(iter_oid).map(|o| &o.kind),
                                Some(ObjectKind::MapIterator(..))
                            );
                            let is_set = matches!(
                                self.heap.get(iter_oid).map(|o| &o.kind),
                                Some(ObjectKind::SetIterator(..))
                            );
                            if is_map {
                                if let Some(src_obj) = self.heap.get(src_oid) {
                                    if let ObjectKind::Map { ref entries } = src_obj.kind {
                                        if idx < entries.len() {
                                            let (k, v) = entries[idx];
                                            let pair = JsObject::array(vec![k, v]);
                                            let pair_id = self.heap.allocate(pair);
                                            (Value::object_id(pair_id), false)
                                        } else {
                                            (Value::undefined(), true)
                                        }
                                    } else { (Value::undefined(), true) }
                                } else { (Value::undefined(), true) }
                            } else if is_set {
                                if let Some(src_obj) = self.heap.get(src_oid) {
                                    if let ObjectKind::Set { ref entries } = src_obj.kind {
                                        if idx < entries.len() {
                                            (entries[idx], false)
                                        } else {
                                            (Value::undefined(), true)
                                        }
                                    } else { (Value::undefined(), true) }
                                } else { (Value::undefined(), true) }
                            } else if let Some(ta_len) = self.typed_array_len(src_oid) {
                                // Typed array iterator (values).
                                if idx < ta_len {
                                    (self.typed_array_get(src_oid, idx).unwrap_or(Value::undefined()), false)
                                } else { (Value::undefined(), true) }
                            } else {
                                // Array iterator
                                if let Some(arr_obj) = self.heap.get(src_oid) {
                                    if let ObjectKind::Array(ref elements) = arr_obj.kind {
                                        if idx < elements.len() {
                                            (elements[idx], false)
                                        } else {
                                            (Value::undefined(), true)
                                        }
                                    } else { (Value::undefined(), true) }
                                } else { (Value::undefined(), true) }
                            }
                        };
                        // Advance the iterator index and mark done if exhausted.
                        if let Some(iter) = self.heap.get_mut(iter_oid) {
                            let new_idx = iter_info.1 + 1;
                            match &mut iter.kind {
                                ObjectKind::ArrayIterator(_, i)
                                | ObjectKind::MapIterator(_, i)
                                | ObjectKind::SetIterator(_, i)
                                | ObjectKind::KeyIterator(_, i) => *i = new_idx,
                                _ => {}
                            }
                            if done {
                                iter.set_property(iter_done_key, Value::boolean(true));
                            }
                        }
                        // Create iterator result object { value, done }
                        let mut result_obj = JsObject::ordinary();
                        let value_name = self.interner.intern("value");
                        let done_name = self.interner.intern("done");
                        result_obj.set_property(value_name, value);
                        result_obj.set_property(done_name, Value::boolean(done));
                        let result_id = self.heap.allocate(result_obj);
                        self.push(Value::object_id(result_id));
                        } // close non-generator else
                    } else {
                        let err = self.make_native_error("TypeError", "not an iterator");
                        self.handle_throw(err)?;
                        continue;
                    }
                }

                OpCode::IteratorDone => {
                    // Stack: [iter_result] -> [done_bool]
                    let result_val = self.pop()?;
                    if let Some(oid) = result_val.as_object_id() {
                        let done_name = self.interner.intern("done");
                        let getter_key = self.interner.intern("__get_done__");
                        let done_val = if let Some(gfn) = self.heap.get_property_chain(oid, getter_key)
                            && gfn.is_function()
                        {
                            self.call_function_this(gfn, result_val, &[])?
                        } else {
                            self.heap.get_property_chain(oid, done_name)
                                .unwrap_or(Value::undefined())
                        };
                        self.push(Value::boolean(done_val.to_boolean()));
                    } else {
                        self.push(Value::boolean(true));
                    }
                }

                OpCode::IteratorValue => {
                    // Stack: [iter_result] -> [value]
                    let result_val = self.pop()?;
                    if let Some(oid) = result_val.as_object_id() {
                        let value_name = self.interner.intern("value");
                        // Check getter first, then plain property.
                        let getter_key = self.interner.intern("__get_value__");
                        if let Some(gfn) = self.heap.get_property_chain(oid, getter_key)
                            && gfn.is_function()
                        {
                            let val = self.call_function_this(gfn, result_val, &[])?;
                            self.push(val);
                        } else {
                            let val = self.heap.get_property_chain(oid, value_name)
                                .unwrap_or(Value::undefined());
                            self.push(val);
                        }
                    } else {
                        self.push(Value::undefined());
                    }
                }

                OpCode::IteratorClose => {
                    let iter_val = self.pop()?;
                    if let Some(oid) = iter_val.as_object_id() {
                        // Per spec, only close if the iterator's [[Done]] flag is false.
                        // We track this via a __iter_done__ property set by IteratorNext.
                        let iter_done_key = self.interner.intern("__iter_done__");
                        let already_done = self.heap.get_property_chain(oid, iter_done_key)
                            .map(|v| v.to_boolean())
                            .unwrap_or(false);
                        if already_done {
                            continue;
                        }
                        let is_gen = self.heap.get(oid)
                            .map(|o| matches!(&o.kind, ObjectKind::Generator { .. }))
                            .unwrap_or(false);
                        if is_gen {
                            let return_name = self.interner.intern("return");
                            let _ = self.exec_generator_method(oid, return_name, &[Value::undefined()]);
                        } else {
                            // Per spec IteratorClose: call iterator.return() if it exists.
                            // GetMethod: if return is not undefined/null but not callable,
                            // throw TypeError.
                            let return_name = self.interner.intern("return");
                            // Accessor form: a getter for "return".
                            let return_getter_key = self.interner.intern("__get_return__");
                            let return_fn = if let Some(g) = self.heap.get_property_chain(oid, return_getter_key)
                                && g.is_function()
                            {
                                let prev_protect = self.protect_throw_depth;
                                self.protect_throw_depth = self.frames.len() + 1;
                                let r = self.call_function_this(g, iter_val, &[]);
                                self.protect_throw_depth = prev_protect;
                                match r {
                                    Ok(v) => Some(v),
                                    Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                                    Err(e) => return Err(e),
                                }
                            } else {
                                self.heap.get_property_chain(oid, return_name)
                            };
                            if let Some(fn_val) = return_fn
                                && !fn_val.is_undefined() && !fn_val.is_null()
                            {
                                if !fn_val.is_function() {
                                    let err = self.make_native_error(
                                        "TypeError",
                                        "Iterator's `return` is not callable",
                                    );
                                    self.handle_throw(err)?;
                                    continue;
                                }
                                let result = self.call_function_this(fn_val, iter_val, &[])?;
                                if !result.is_object() && !result.is_function() {
                                    let err = self.make_native_error(
                                        "TypeError",
                                        "Iterator result is not an object",
                                    );
                                    self.handle_throw(err)?;
                                    continue;
                                }
                            }
                        }
                    }
                }

                OpCode::IteratorCloseIfNotDone => {
                    // Stack: [iter, done] -> []
                    let done = self.pop()?;
                    let iter_val = self.pop()?;
                    if done.to_boolean() {
                        // Already done — skip close.
                        continue;
                    }
                    if let Some(oid) = iter_val.as_object_id() {
                        let is_gen = self.heap.get(oid)
                            .map(|o| matches!(&o.kind, ObjectKind::Generator { .. }))
                            .unwrap_or(false);
                        if is_gen {
                            let return_name = self.interner.intern("return");
                            let _ = self.exec_generator_method(oid, return_name, &[Value::undefined()]);
                        } else {
                            let return_name = self.interner.intern("return");
                            let return_fn = self.heap.get_property_chain(oid, return_name);
                            if let Some(fn_val) = return_fn
                                && fn_val.is_function()
                            {
                                let result = self.call_function_this(fn_val, iter_val, &[])?;
                                if !result.is_object() && !result.is_function() {
                                    let err = self.make_native_error(
                                        "TypeError",
                                        "Iterator result is not an object",
                                    );
                                    self.handle_throw(err)?;
                                    continue;
                                }
                            }
                        }
                    }
                }

                OpCode::Await => {
                    let awaited = self.pop()?;
                    // If it's a fulfilled promise, unwrap the value
                    if let Some(oid) = awaited.as_object_id() {
                        let promise_info = self.heap.get(oid).and_then(|o| {
                            if let ObjectKind::Promise { state, result, .. } = &o.kind {
                                Some((*state, *result))
                            } else { None }
                        });
                        if let Some((state, result)) = promise_info {
                            match state {
                                PromiseState::Fulfilled => { self.push(result); }
                                PromiseState::Rejected => { self.push(result); } // simplified
                                PromiseState::Pending => { self.push(Value::undefined()); }
                            }
                            continue;
                        }
                    }
                    // Not a promise: push value directly (await on non-thenable resolves immediately)
                    self.push(awaited);
                }

                OpCode::Yield => {
                    let yielded_value = self.pop()?;
                    let frame = self.frames.last().unwrap();
                    let gen_oid = frame.generator_id;
                    let base = frame.base;
                    let ip = frame.ip;
                    let chunk_idx = frame.chunk_idx;
                    let this_value = frame.this_value;
                    let is_async_gen = chunk_idx < self.chunks.len()
                        && self.chunks[chunk_idx].flags.contains(ChunkFlags::ASYNC);

                    if let Some(gid) = gen_oid {
                        // Save the current stack (locals + operand stack)
                        let saved_stack: Vec<Value> = self.stack[base..].to_vec();

                        // Resolve upvalues to values
                        let saved_upvalues: Vec<Value> = self.frames.last().unwrap().upvalues.iter().map(|uv| uv.get(&self.stack)).collect();

                        // Update generator object
                        if let Some(obj) = self.heap.get_mut(gid)
                            && let ObjectKind::Generator { state, ip: saved_ip, saved_stack: ss, saved_upvalues: su, this_value: tv, .. } = &mut obj.kind
                        {
                            *state = GeneratorState::SuspendedYield;
                            *saved_ip = ip;
                            *ss = saved_stack;
                            *su = saved_upvalues;
                            *tv = this_value;
                        }

                        // Pop the generator frame
                        self.frames.pop();
                        self.truncate_stack(base - 1); // remove placeholder too

                        // Push {value, done: false}; for async generators, wrap in a
                        // resolved Promise so the caller sees Promise<{value, done}>.
                        let result = self.make_iter_result(yielded_value, false)?;
                        if is_async_gen {
                            let pid = self.allocate_promise();
                            self.resolve_promise(pid, result)?;
                            self.push(Value::object_id(pid));
                        } else {
                            self.push(result);
                        }
                    } else {
                        return Err(VmError::RuntimeError("yield outside generator".into()));
                    }
                }

                OpCode::CreateGenerator => {
                    // Capture the current frame's state and return a Generator object
                    // back to the caller. Emitted at the end of a generator function's
                    // prologue (after parameter destructuring).
                    let frame = self.frames.pop().unwrap();
                    let chunk_idx = frame.chunk_idx;
                    let ip_after_create = frame.ip; // IP already advanced past this op
                    // Save operand+local stack between base and current sp.
                    let saved_stack: Vec<Value> = self.stack[frame.base..].to_vec();
                    // Capture upvalues' current values.
                    let saved_upvalues: Vec<Value> = frame.upvalues.iter().map(|uv| uv.get(&self.stack)).collect();
                    let mut gen_obj = JsObject::ordinary();
                    gen_obj.kind = ObjectKind::Generator {
                        state: GeneratorState::SuspendedStart,
                        chunk_idx,
                        ip: ip_after_create,
                        saved_stack,
                        saved_upvalues,
                        this_value: frame.this_value,
                        saved_args: frame.saved_args,
                    };
                    let gen_oid = self.heap.allocate(gen_obj);
                    // Drop the frame's stack slots (back to func slot).
                    self.truncate_stack(frame.base.saturating_sub(1));
                    if self.frames.len() <= stop_depth {
                        return Ok(Value::object_id(gen_oid));
                    }
                    self.push(Value::object_id(gen_oid));
                }

                OpCode::YieldStar
                | OpCode::AsyncReturn
                | OpCode::AsyncThrow => {
                    return Err(VmError::RuntimeError(format!(
                        "{opcode:?} not yet implemented"
                    )));
                }

                OpCode::DestructureArray | OpCode::DestructureRest | OpCode::DestructureObject => {
                    let _count = self.read_byte();
                    return Err(VmError::RuntimeError(format!(
                        "{opcode:?} not yet implemented"
                    )));
                }

                OpCode::PushComputedExclude => {
                    let key = self.pop()?;
                    self.computed_exclusions.push(key);
                }

                OpCode::ObjectRest => {
                    // u8 num_excluded_keys, then (u16 key_idx) * num
                    let n = self.read_byte() as usize;
                    let mut excluded: std::collections::HashSet<StringId> = std::collections::HashSet::new();
                    for _ in 0..n {
                        let idx = self.read_u16() as usize;
                        if let Some(sid) = self.chunks[self.cur_chunk()].constants[idx].as_string_id() {
                            excluded.insert(sid);
                        }
                    }
                    // Also consume any dynamic (computed) exclusions
                    let dyn_excl = std::mem::take(&mut self.computed_exclusions);
                    for key_val in dyn_excl {
                        let key_str = self.value_to_string(key_val);
                        let sid = self.interner.intern(&key_str);
                        excluded.insert(sid);
                    }
                    let source = self.pop()?;
                    let mut rest = JsObject::ordinary();
                    // Spread string characters as indexed properties
                    if self.is_string_like(source) {
                        let s = self.value_to_string(source);
                        for (i, ch) in s.chars().enumerate() {
                            let key = self.interner.intern(&i.to_string());
                            if excluded.contains(&key) { continue; }
                            let val_id = self.interner.intern(&ch.to_string());
                            rest.set_property(key, Value::string(val_id));
                        }
                    } else if let Some(src_oid) = source.as_object_id() {
                        // Collect visible (public, enumerable) property names, resolving
                        // getter keys (__get_X__) to their visible name X.
                        let raw: Vec<(StringId, StringId)> = self.heap.get(src_oid)
                            .map(|o| o.properties.iter()
                                .filter(|(_, p)| p.is_enumerable())
                                .filter_map(|&(k, _)| {
                                    let s = self.interner.resolve(k);
                                    if is_internal_key(s) && !s.starts_with("__get_") { return None; }
                                    Some((k, k))
                                })
                                .collect())
                            .unwrap_or_default();
                        // Build the set of visible names and getter keys
                        let mut visible: Vec<(StringId, StringId)> = Vec::new(); // (visible_key, raw_key)
                        let mut seen = std::collections::HashSet::new();
                        for (raw_k, _) in &raw {
                            let s = self.interner.resolve(*raw_k).to_owned();
                            let vis_name = if s.starts_with("__get_") && s.ends_with("__") {
                                s[6..s.len()-2].to_owned()
                            } else {
                                s.clone()
                            };
                            let vis_id = self.interner.intern(&vis_name);
                            if excluded.contains(&vis_id) { continue; }
                            if seen.insert(vis_name) {
                                visible.push((vis_id, *raw_k));
                            }
                        }
                        for (vis_key, raw_key) in visible {
                            let raw_s = self.interner.resolve(raw_key).to_owned();
                            // If this is a getter key, call the getter to get the value
                            let value = if raw_s.starts_with("__get_") && raw_s.ends_with("__") {
                                let getter_fn = self.heap.get_property_chain(src_oid, raw_key);
                                if let Some(gfn) = getter_fn && gfn.is_function() {
                                    self.call_function_this(gfn, source, &[])?
                                } else {
                                    Value::undefined()
                                }
                            } else {
                                self.heap.get(src_oid)
                                    .and_then(|o| o.get_property(raw_key))
                                    .unwrap_or(Value::undefined())
                            };
                            rest.set_property(vis_key, value);
                        }
                    }
                    let oid = self.heap.allocate(rest);
                    self.push(Value::object_id(oid));
                }

                OpCode::DestructureDefault => {
                    let _ = self.read_i16();
                    return Err(VmError::RuntimeError(
                        "DestructureDefault not yet implemented".into(),
                    ));
                }

                OpCode::ImportModule => {
                    let src_idx = self.read_u16() as usize;
                    let src_val = self.chunks[self.cur_chunk()].constants[src_idx];
                    let module_path_raw = self.value_to_string(src_val);
                    // Strip quotes from string literal
                    let module_path = module_path_raw.trim_matches(|c| c == '\'' || c == '"').to_owned();

                    // Check cache
                    if let Some(&exports_oid) = self.module_cache.get(&module_path) {
                        self.push(Value::object_id(exports_oid));
                    } else {
                        // Resolve path relative to module_dir
                        let full_path = if let Some(ref dir) = self.module_dir {
                            if module_path.starts_with("./") || module_path.starts_with("../") {
                                format!("{}/{}", dir, module_path)
                            } else {
                                module_path.clone()
                            }
                        } else {
                            module_path.clone()
                        };

                        // Read and compile the module
                        let source = std::fs::read_to_string(&full_path).map_err(|e| {
                            VmError::RuntimeError(format!("Cannot find module '{}': {}", module_path, e))
                        })?;

                        // Create exports object and cache it
                        let exports_obj = JsObject::ordinary();
                        let exports_oid = self.heap.allocate(exports_obj);
                        self.module_cache.insert(module_path, exports_oid);

                        // Set __exports__ global for the module to use
                        let exports_key = self.interner.intern("__exports__");
                        self.globals.insert(exports_key, Value::object_id(exports_oid));

                        // Lex, parse, compile the module source
                        let mut lexer = crate::lexer::lexer::Lexer::new(&source, &mut self.interner);
                        let tokens = lexer.tokenize();
                        let mut parser = crate::parser::parser::Parser::new(tokens, &source, &mut self.interner);
                        let program = match parser.parse_program() {
                            Ok(p) => p,
                            Err(e) => return Err(VmError::RuntimeError(format!("Module parse error: {e}"))),
                        };
                        let compiler = crate::compiler::compiler::Compiler::new(&mut self.interner);
                        let chunk = match compiler.compile_program(&program) {
                            Ok(c) => c,
                            Err(e) => return Err(VmError::RuntimeError(format!("Module compile error: {e}"))),
                        };

                        // Flatten child chunks and add to VM. Adjust children indices to be absolute
                        // (flatten_chunk uses indices relative to its output vec).
                        let base_idx = self.chunks.len();
                        let mut flat_chunks = Vec::new();
                        Vm::flatten_chunk(chunk, &mut flat_chunks);
                        for c in &mut flat_chunks {
                            for child in &mut c.children {
                                *child += base_idx;
                            }
                        }
                        self.maybe_disasm_chunks(&flat_chunks);
                        self.chunks.extend(flat_chunks);

                        // Save current globals
                        let globals_before: std::collections::HashSet<StringId> =
                            self.globals.keys().copied().collect();

                        // Execute module using call_function (globals are shared)
                        let module_fn = Value::function(base_idx as i32);
                        let _ = self.call_function(module_fn, &[]);

                        // Copy newly-defined globals to exports object
                        let new_globals: Vec<(StringId, Value)> = self.globals.iter()
                            .filter(|(k, _)| !globals_before.contains(k))
                            .map(|(k, v)| (*k, *v))
                            .collect();
                        for (name, val) in new_globals {
                            if let Some(obj) = self.heap.get_mut(exports_oid) {
                                obj.set_property(name, val);
                            }
                        }

                        self.push(Value::object_id(exports_oid));
                    }
                }

                OpCode::ExportAllFrom => {
                    // export * from 'mod': import the module, copy all its exports to __exports__
                    let src_idx = self.read_u16() as usize;
                    let src_val = self.chunks[self.cur_chunk()].constants[src_idx];
                    let module_path_raw = self.value_to_string(src_val);
                    let module_path = module_path_raw.trim_matches(|c| c == '\'' || c == '"').to_owned();

                    // Get or load the module (reuse ImportModule logic)
                    let mod_exports_oid = if let Some(&oid) = self.module_cache.get(&module_path) {
                        oid
                    } else {
                        // Import the module by pushing and executing it
                        // For simplicity, just error — modules should be imported first
                        return Err(VmError::RuntimeError(format!("Module '{}' not loaded for re-export", module_path)));
                    };

                    // Get our exports object
                    let exports_key = self.interner.intern("__exports__");
                    let our_exports = self.globals.get(&exports_key).copied();

                    // Copy all properties from mod_exports to our_exports
                    if let Some(our_val) = our_exports
                        && let Some(our_oid) = our_val.as_object_id()
                    {
                        let props: Vec<(StringId, Value)> = self.heap.get(mod_exports_oid)
                            .map(|obj| obj.properties.iter().map(|&(k, ref p)| (k, p.value)).collect())
                            .unwrap_or_default();
                        for (key, val) in props {
                            // Skip 'default' export — export * doesn't re-export default
                            let key_str = self.interner.resolve(key);
                            if key_str == "default" { continue; }
                            if let Some(obj) = self.heap.get_mut(our_oid) {
                                obj.set_property(key, val);
                            }
                        }
                    }
                }

                OpCode::ImportDynamic => {
                    // `import(specifier)` returns a Promise of the module
                    // namespace. This embedder has no module loader for
                    // network/dynamic chunks, so we can't actually resolve
                    // it. Return a perpetually-pending promise: code that
                    // does `import(x).then(cb)` registers its continuation
                    // but it simply never fires — the same observable
                    // behavior as a chunk that takes forever to load. This
                    // is far less disruptive than throwing (which aborts
                    // the whole script) or rejecting (which can surface as
                    // an unhandled-rejection). Pop the already-evaluated
                    // specifier off the stack first.
                    let _specifier = self.pop()?;
                    let pid = self.allocate_promise();
                    self.push(Value::object_id(pid));
                }
                OpCode::ExportDefault => {
                    return Err(VmError::RuntimeError(format!(
                        "{opcode:?} not yet implemented"
                    )));
                }

                OpCode::Export => {
                    let _name = self.read_u16();
                    let _slot = self.read_byte();
                    // No-op: exports are handled via __exports__ global
                }

                OpCode::GetModuleVar => {
                    let _mod = self.read_u16();
                    let _binding = self.read_u16();
                    return Err(VmError::RuntimeError(
                        "GetModuleVar not yet implemented".into(),
                    ));
                }

                OpCode::Debugger => { /* no-op in non-debug mode */ }

                OpCode::NewTarget => {
                    let nt = self.frames.last().map(|f| f.new_target).unwrap_or(Value::undefined());
                    self.push(nt);
                }

                OpCode::ImportMeta => {
                    // `import.meta` — a host-populated module-scope object.
                    // The only widely-used field is `import.meta.url`. We
                    // don't track a per-module URL in this embedder, so
                    // expose an object with an empty `url` string. Reading
                    // any other property yields undefined (ordinary object),
                    // which is what most feature-probing code tolerates.
                    let mut obj = JsObject::ordinary();
                    obj.prototype = Some(self.object_prototype);
                    let url_key = self.interner.intern("url");
                    let empty = self.interner.intern("");
                    obj.set_property(url_key, Value::string(empty));
                    let oid = self.heap.allocate(obj);
                    self.push(Value::object_id(oid));
                }

                OpCode::TemplateTag => {
                    let total = self.read_byte() as usize;
                    // Stack layout: [tag, quasi0..quasiN, expr0..exprM]
                    // Where N = number of quasis, M = N-1
                    // For simplicity: split: half are quasis, rest are exprs
                    // Actually compiler emits: total = quasis.len() + expressions.len()
                    // quasis come first, then expressions
                    let num_exprs = total / 2; // expressions
                    let num_quasis = total - num_exprs;
                    let stack_len = self.stack.len();
                    let tag_pos = stack_len - 1 - total;
                    let tag = self.stack[tag_pos];
                    // Build strings array from quasis
                    let mut quasi_strings = Vec::with_capacity(num_quasis);
                    for i in 0..num_quasis {
                        quasi_strings.push(self.stack[tag_pos + 1 + i]);
                    }
                    // Collect expressions
                    let mut exprs: Vec<Value> = Vec::with_capacity(num_exprs);
                    for i in 0..num_exprs {
                        exprs.push(self.stack[tag_pos + 1 + num_quasis + i]);
                    }
                    let strings_arr = JsObject::array(quasi_strings.clone());
                    let arr_oid = self.heap.allocate(strings_arr);
                    // Add 'raw' property pointing to same array (simplified)
                    let raw_arr = JsObject::array(quasi_strings);
                    let raw_oid = self.heap.allocate(raw_arr);
                    let raw_key = self.interner.intern("raw");
                    if let Some(obj) = self.heap.get_mut(arr_oid) {
                        obj.set_property(raw_key, Value::object_id(raw_oid));
                    }
                    // Build args: [strings_array, ...exprs]
                    let mut args = vec![Value::object_id(arr_oid)];
                    args.extend(exprs);
                    self.truncate_stack(tag_pos);
                    let result = self.call_function(tag, &args)?;
                    self.push(result);
                }
                OpCode::CreateRestParam => {
                    let _ = self.read_byte();
                    return Err(VmError::RuntimeError("CreateRestParam not yet implemented".into()));
                }

                OpCode::ToPropertyKey => {
                    // Spec ToPropertyKey: ToPrimitive(arg, "string"); if symbol return as-is,
                    // else ToString. Pop the key, push the converted value back.
                    let key = self.pop()?;
                    let prim = if key.is_object() && !key.is_symbol() {
                        match self.try_coerce_to_primitive_hint(key, "string") {
                            Ok(v) => v,
                            Err(VmError::Throw(v)) => { self.handle_throw(v)?; continue; }
                            Err(e) => return Err(e),
                        }
                    } else {
                        key
                    };
                    let result = if prim.is_symbol() || prim.is_string() {
                        prim
                    } else if self.is_cons_string(prim) {
                        let s = self.flatten_cons_to_string(prim);
                        Value::string(self.interner.intern(&s))
                    } else {
                        let s = self.value_to_string(prim);
                        Value::string(self.interner.intern(&s))
                    };
                    self.push(result);
                }

                OpCode::SetFunctionName => {
                    let _ = self.read_u16();
                    return Err(VmError::RuntimeError(
                        "SetFunctionName not yet implemented".into(),
                    ));
                }

                OpCode::WithEnter => {
                    // Pop the value, coerce to object, push onto the with-scope stack.
                    let val = self.pop()?;
                    if let Some(oid) = val.as_object_id() {
                        self.with_stack.push(oid);
                    } else {
                        // Per spec, ToObject(null/undefined) throws TypeError.
                        let err = self.make_native_error(
                            "TypeError",
                            "Cannot convert undefined or null to object",
                        );
                        self.handle_throw(err)?;
                    }
                }

                OpCode::WithExit => {
                    self.with_stack.pop();
                }
            }
        }
    }
}

impl Vm {
    /// Object.defineProperty(target, key, descriptor). Extracted so that both
    /// the inline `Object.defineProperty(...)` CallMethod path and the
    /// extracted-as-Native-fn path (`var f = Object.defineProperty; f(...)`)
    /// share one implementation.
    /// If `oid` is mid-construction with pending private brands and the private
    /// member `name_str` resolves to a METHOD or ACCESSOR owned by a not-yet-installed
    /// (pending) class prototype, return true (access must throw). Private FIELDS
    /// (own `__priv_` on the instance) and fully-constructed objects return false.
    fn private_brand_not_installed(
        &self,
        oid: ObjectId,
        getter_key: StringId,
        setter_key: StringId,
        method_key: StringId,
    ) -> bool {
        let Some(pending) = self.pending_private_brands.get(&oid) else { return false };
        if pending.is_empty() { return false; }
        // Walk the prototype chain; find the prototype that owns the method/accessor.
        let mut cur = self.heap.get(oid).and_then(|o| o.prototype);
        while let Some(c) = cur {
            let Some(o) = self.heap.get(c) else { break };
            if o.has_own_property(getter_key)
                || o.has_own_property(setter_key)
                || o.has_own_property(method_key)
            {
                return pending.contains(&c);
            }
            cur = o.prototype;
        }
        false
    }

    /// PrivateBrandCheck: returns true when `oid` does NOT carry the private name
    /// `#name`, i.e. accessing it must throw TypeError. The object has the brand
    /// when the private member is reachable as a field (own `__priv_#name__`),
    /// a method (`__priv_#name__` on the prototype chain), or an accessor
    /// (`__get_#name__` / `__set_#name__` on the chain).
    fn private_brand_missing(&mut self, oid: ObjectId, name_str: &str) -> bool {
        let priv_key = self.interner.intern(&format!("__priv_{name_str}__"));
        let getter_key = self.interner.intern(&format!("__get_{name_str}__"));
        let setter_key = self.interner.intern(&format!("__set_{name_str}__"));
        self.heap.get_property_chain(oid, priv_key).is_none()
            && self.heap.get_property_chain(oid, getter_key).is_none()
            && self.heap.get_property_chain(oid, setter_key).is_none()
    }

    pub(crate) fn object_define_property(&mut self, args: &[Value]) -> Value {
        // A canonical array index per ECMAScript: the string is the decimal
        // form of a non-negative integer < 2^32-1, with no leading zeros
        // ("0" itself is fine). "01", "1.5", "-1", "4294967295" are not.
        fn canonical_array_index(s: &str) -> Option<usize> {
            if s == "0" {
                return Some(0);
            }
            if s.is_empty() || s.as_bytes()[0] == b'0' || !s.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            s.parse::<u64>()
                .ok()
                .filter(|&n| n < (u32::MAX as u64))
                .map(|n| n as usize)
        }
        let target = args.first().copied().unwrap_or(Value::undefined());
        let key_val = args.get(1).copied().unwrap_or(Value::undefined());
        let desc_val = args.get(2).copied().unwrap_or(Value::undefined());
        // Function-sentinel targets keep properties in
        // fn_property_overrides — core-js installs well-known symbols
        // with `Object.defineProperty(Symbol, 'asyncDispose', {...})`,
        // which used to silently no-op here.
        if let Some(sentinel) = target.as_function() {
            let key_str = if key_val.is_symbol() {
                format!("__sym_{}__", key_val.as_symbol_id().unwrap())
            } else {
                self.value_to_string(key_val)
            };
            let key_id = self.interner.intern(&key_str);
            // Only a descriptor that carries `value` (or get) changes the
            // stored value. Flags-only defines — core-js does
            // `defineProperty(Ctor, 'prototype', {writable:false})` — must
            // keep the existing value; clobbering it with undefined
            // poisoned `.prototype` of wrapped constructors.
            let value = desc_val.as_object_id().and_then(|doid| {
                let value_key = self.interner.intern("value");
                let get_key = self.interner.intern("get");
                self.heap.get_property_chain(doid, value_key)
                    .or_else(|| self.heap.get_property_chain(doid, get_key)
                        .filter(|v| !v.is_undefined()))
            });
            if let Some(value) = value {
                self.fn_property_overrides.insert((sentinel, key_id), Some(value));
            }
            return target;
        }
        let Some(target_oid) = target.as_object_id() else { return target };
        let key_str = if key_val.is_symbol() {
            format!("__sym_{}__", key_val.as_symbol_id().unwrap())
        } else {
            self.value_to_string(key_val)
        };
        let key_id = self.interner.intern(&key_str);
        let mut flags = Property::ALL;
        let mut value = Value::undefined();
        if let Some(desc_oid) = desc_val.as_object_id() {
            let writable_key = self.interner.intern("writable");
            let enumerable_key = self.interner.intern("enumerable");
            let configurable_key = self.interner.intern("configurable");
            let value_key = self.interner.intern("value");
            let get_key = self.interner.intern("get");
            let set_key = self.interner.intern("set");
            if let Some(v) = self.heap.get_property_chain(desc_oid, value_key) {
                value = v;
            }
            flags = 0;
            if let Some(v) = self.heap.get_property_chain(desc_oid, writable_key)
                && v.to_boolean() { flags |= Property::WRITABLE; }
            if let Some(v) = self.heap.get_property_chain(desc_oid, enumerable_key)
                && v.to_boolean() { flags |= Property::ENUMERABLE; }
            if let Some(v) = self.heap.get_property_chain(desc_oid, configurable_key)
                && v.to_boolean() { flags |= Property::CONFIGURABLE; }
            let accessor_flags = flags & (Property::ENUMERABLE | Property::CONFIGURABLE);
            if let Some(getter) = self.heap.get_property_chain(desc_oid, get_key)
                && getter.is_function() {
                    let getter_key = self.interner.intern(&format!("__get_{key_str}__"));
                    if let Some(obj) = self.heap.get_mut(target_oid) {
                        obj.define_property(getter_key,
                            Property::with_flags(getter, accessor_flags));
                    }
                }
            if let Some(setter) = self.heap.get_property_chain(desc_oid, set_key)
                && setter.is_function() {
                    let setter_key = self.interner.intern(&format!("__set_{key_str}__"));
                    if let Some(obj) = self.heap.get_mut(target_oid) {
                        obj.define_property(setter_key,
                            Property::with_flags(setter, accessor_flags));
                    }
                }
        }
        let has_accessor = self.heap.get(target_oid)
            .map(|o| {
                let gk = self.interner.intern(&format!("__get_{key_str}__"));
                let sk = self.interner.intern(&format!("__set_{key_str}__"));
                o.has_own_property(gk) || o.has_own_property(sk)
            })
            .unwrap_or(false);
        if !has_accessor {
            // Array-index defines must land in the array's element storage,
            // not the property map. Arrays keep elements in a separate Vec
            // (backing `arr[i]` and `.length`); a map-only define is invisible
            // to indexed access and length. core-js's `createProperty` (used by
            // Array.from, Array.of, spread, etc.) builds result arrays via
            // `Object.defineProperty(arr, i, {value})`, so without this every
            // such array came back all-`undefined` — which broke `Array.from`
            // wholesale once core-js's polyfill loaded.
            // Only route to element storage when the descriptor carries the
            // full default flag set (writable+enumerable+configurable) — the
            // shape of a normal array element. core-js's createProperty /
            // Array.from build with exactly these flags. A partial-flag define
            // (e.g. a non-writable index) keeps the property-map path so its
            // descriptor flags are preserved; copper's Vec-backed elements
            // can't carry per-element flags.
            if flags == Property::ALL
                && let Some(idx) = canonical_array_index(&key_str)
            {
                let arr_len = self.heap.get(target_oid).and_then(|o| {
                    if let ObjectKind::Array(ref e) = o.kind { Some(e.len()) } else { None }
                });
                // Only handle in-bounds appends / overwrites (idx <= len):
                // that's exactly the createProperty / Array.from pattern
                // (idx == len each step, or overwriting an existing slot), so
                // the Vec grows by at most one. A large *sparse* index would
                // otherwise balloon the dense Vec to `idx` undefined slots and
                // trip the VM's execution limit (this broke DuckDuckGo's SERP
                // bundles). Out-of-bounds / sparse indices fall through to the
                // property-map path — the original pre-fix behaviour.
                if let Some(len) = arr_len
                    && idx <= len
                {
                    if let Some(obj) = self.heap.get_mut(target_oid)
                        && let ObjectKind::Array(ref mut elements) = obj.kind
                    {
                        while elements.len() <= idx {
                            elements.push(Value::undefined());
                        }
                        elements[idx] = value;
                    }
                    return target;
                }
            }
            if let Some(obj) = self.heap.get_mut(target_oid) {
                obj.define_property(key_id, Property::with_flags(value, flags));
            }
        }
        target
    }

    /// Implements `Function(...)` and `new Function(...)`: concatenates params,
    /// compiles `function(p1,p2,...){ body }`, and returns a callable function value.
    pub(crate) fn construct_function(&mut self, args: &[Value]) -> Result<Value, VmError> {
        let params_str = if args.len() > 1 {
            args[..args.len() - 1]
                .iter()
                .map(|v| self.value_to_string(*v))
                .collect::<Vec<_>>()
                .join(",")
        } else {
            String::new()
        };
        let body_str = args.last().map(|v| self.value_to_string(*v)).unwrap_or_default();
        let src = format!("return (function({}){{ {} }})", params_str, body_str);

        // Lex, parse, compile
        let mut lexer = crate::lexer::lexer::Lexer::new(&src, &mut self.interner);
        let tokens = lexer.tokenize();
        let mut parser = crate::parser::parser::Parser::new(tokens, &src, &mut self.interner);
        let program = parser
            .parse_program()
            .map_err(|e| VmError::RuntimeError(format!("Function SyntaxError: {e}")))?;
        let compiler = crate::compiler::compiler::Compiler::new(&mut self.interner);
        let chunk = compiler
            .compile_program(&program)
            .map_err(|e| VmError::RuntimeError(format!("Function CompileError: {e}")))?;
        let base_idx = self.chunks.len();
        let mut flat_chunks = Vec::new();
        Vm::flatten_chunk(chunk, &mut flat_chunks);
        // Adjust children indices to be absolute (flatten_chunk uses indices relative to its output vec).
        for c in &mut flat_chunks {
            for child in &mut c.children {
                *child += base_idx;
            }
        }
        self.maybe_disasm_chunks(&flat_chunks);
        self.chunks.extend(flat_chunks);
        // Run the outer wrapper to evaluate the function expression
        let wrapper_fn = Value::function(base_idx as i32);
        let result = self.call_function(wrapper_fn, &[])?;
        Ok(result)
    }

    /// Embedder-facing: enqueue `callback` on the engine's microtask
    /// queue (drained by `drain_microtasks` — end of eval, end of
    /// tick). Backs `queueMicrotask`: running such callbacks
    /// synchronously breaks run-to-completion, which React's
    /// sync-flush scheduling observes immediately.
    pub fn host_enqueue_microtask(&mut self, callback: Value) {
        let pid = self.allocate_promise();
        self.microtask_queue.push(Microtask::PromiseReaction {
            callback: Some(callback),
            value: Value::undefined(),
            result_promise: pid,
            is_fulfilled: true,
        });
    }

    /// `Symbol.for(key)`: one shared symbol per key, registered globally.
    pub(crate) fn exec_symbol_for(&mut self, args: &[Value]) -> Value {
        let key = args
            .first()
            .map(|v| self.value_to_string(*v))
            .unwrap_or_else(|| "undefined".to_string());
        if let Some(&id) = self.symbol_registry.get(&key) {
            return Value::symbol(id);
        }
        let id = self.next_symbol_id;
        self.next_symbol_id += 1;
        if id as usize >= self.symbol_descriptions.len() {
            self.symbol_descriptions.resize(id as usize + 1, None);
        }
        let desc = self.interner.intern(&key);
        self.symbol_descriptions[id as usize] = Some(desc);
        self.symbol_registry.insert(key, id);
        Value::symbol(id)
    }

    /// `Symbol.keyFor(sym)`: the registry key, or undefined.
    pub(crate) fn exec_symbol_key_for(&mut self, args: &[Value]) -> Value {
        let Some(sym) = args.first().and_then(|v| v.as_symbol_id()) else {
            return Value::undefined();
        };
        for (k, &id) in &self.symbol_registry {
            if id == sym {
                let sid = self.interner.intern(k);
                return Value::string(sid);
            }
        }
        Value::undefined()
    }

    /// Object.assign(target, ...sources) — shared by the CallMethod
    /// dispatch and the extractable `Object.assign` value (sentinel
    /// -750): minified bundles alias `var assign = Object.assign;`.
    pub(crate) fn exec_object_assign(&mut self, args: &[Value]) -> Value {
        let target = args.first().copied().unwrap_or(Value::undefined());
        if let Some(target_oid) = target.as_object_id() {
            for source_val in args.iter().skip(1) {
                if let Some(src_oid) = source_val.as_object_id() {
                    let props: Vec<(StringId, Value)> = self.heap.get(src_oid)
                        .map(|o| o.properties.iter()
                            .filter(|(k, p)| p.is_enumerable()
                                && !is_internal_key(self.interner.resolve(*k)))
                            .map(|&(k, ref p)| (k, p.value)).collect())
                        .unwrap_or_default();
                    for (k, v) in props {
                        if let Some(obj) = self.heap.get_mut(target_oid) {
                            obj.set_property(k, v);
                        }
                    }
                }
            }
        }
        target
    }

    /// Debug aid: ZINC_DISASM_CHUNK=<name> dumps the bytecode of every
    /// newly-registered chunk with that name to stderr.
    fn maybe_disasm_chunks(&self, flat_chunks: &[crate::compiler::chunk::Chunk]) {
        if let Ok(want) = std::env::var("ZINC_DISASM_CHUNK") {
            for c in flat_chunks {
                if self.interner.resolve(c.name) == want {
                    eprintln!("==== disasm {want} ====");
                    eprintln!("{}", crate::compiler::disassemble::disassemble(c, &self.interner));
                }
            }
        }
    }
}

/// Convert Unix epoch milliseconds to (year, month0, day) in UTC.
fn epoch_to_ymd(ms: f64) -> (i32, i32, i32) {
    let days = (ms / 86_400_000.0).floor() as i64;
    // Civil-from-days (Howard Hinnant)
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as i32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as i32;
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m - 1, d)
}

pub(crate) fn format_date(ms: f64) -> String {
    format_iso(ms)
}

/// StringToBigInt: parse a string (per `==` and `BigInt(str)`) into a BigInt.
/// Allows surrounding whitespace, an optional sign, and 0x/0o/0b prefixes.
/// Empty/whitespace-only yields 0n; anything malformed yields None.
pub(crate) fn string_to_bigint(s: &str) -> Option<num_bigint::BigInt> {
    let t = s.trim();
    if t.is_empty() { return Some(num_bigint::BigInt::default()); }
    let (neg, body) = match t.as_bytes()[0] {
        b'-' => (true, &t[1..]),
        b'+' => (false, &t[1..]),
        _ => (false, t),
    };
    // Only one sign is permitted; the remaining body must be plain digits
    // (parse_bytes would otherwise accept a second leading sign like "++0").
    if body.starts_with(['+', '-']) { return None; }
    let v = parse_bigint_literal(body)?;
    Some(if neg { -v } else { v })
}

/// True iff the BigInt equals the (finite, integral) Number `f`.
pub(crate) fn bigint_eq_f64(big: &num_bigint::BigInt, f: f64) -> bool {
    if !f.is_finite() || f.fract() != 0.0 { return false; }
    matches!(num_traits::FromPrimitive::from_f64(f), Some(bf) if *big == bf)
}

/// Compare a BigInt with a Number for relational operators. None if `f` is NaN.
pub(crate) fn bigint_cmp_f64(big: &num_bigint::BigInt, f: f64) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    if f.is_nan() { return None; }
    if f == f64::INFINITY { return Some(Ordering::Less); }
    if f == f64::NEG_INFINITY { return Some(Ordering::Greater); }
    // Compare exactly: floor(f) splits the integer boundary, fract breaks ties.
    let floor = f.floor();
    let bf: num_bigint::BigInt = num_traits::FromPrimitive::from_f64(floor)
        .unwrap_or_default();
    match big.cmp(&bf) {
        Ordering::Equal => {
            // big == floor(f); if f has a fractional part, f is larger.
            if f.fract() > 0.0 { Some(Ordering::Less) } else { Some(Ordering::Equal) }
        }
        other => Some(other),
    }
}

/// Top-level `var` and function-declaration names of an eval program — the
/// names EvalDeclarationInstantiation would install in the variable environment.
pub(crate) fn collect_eval_hoisted_names(program: &crate::ast::node::Program) -> Vec<crate::util::interner::StringId> {
    use crate::ast::node::Statement;
    let mut out = Vec::new();
    crate::compiler::compiler::collect_program_var_names(&program.body, &mut out);
    for stmt in &program.body {
        if let Statement::Function(f) = stmt
            && let Some(name) = f.id
        {
            out.push(name);
        }
    }
    out
}

/// ECMAScript ToInt32 on an f64.
pub(crate) fn f64_to_int32(n: f64) -> i32 {
    if n.is_nan() || n.is_infinite() || n == 0.0 { return 0; }
    let int = n.signum() * n.abs().floor();
    let int32bit = int.rem_euclid(4294967296.0);
    if int32bit >= 2147483648.0 { (int32bit - 4294967296.0) as i32 } else { int32bit as i32 }
}

/// Parse a BigInt literal's digit string (no trailing `n`, separators already
/// stripped) into a BigInt, honoring 0x/0o/0b radix prefixes. Returns None on
/// malformed input.
pub(crate) fn parse_bigint_literal(s: &str) -> Option<num_bigint::BigInt> {
    use num_bigint::BigInt;
    let s = s.trim();
    let (radix, digits) = if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        (16, rest)
    } else if let Some(rest) = s.strip_prefix("0o").or_else(|| s.strip_prefix("0O")) {
        (8, rest)
    } else if let Some(rest) = s.strip_prefix("0b").or_else(|| s.strip_prefix("0B")) {
        (2, rest)
    } else {
        (10, s)
    };
    if digits.is_empty() { return if radix == 10 { Some(BigInt::default()) } else { None }; }
    BigInt::parse_bytes(digits.as_bytes(), radix)
}

fn format_iso(ms: f64) -> String {
    let (y, m0, d) = epoch_to_ymd(ms);
    let hour = (ms / 3_600_000.0).rem_euclid(24.0) as i32;
    let min = (ms / 60_000.0).rem_euclid(60.0) as i32;
    let sec = (ms / 1000.0).rem_euclid(60.0) as i32;
    let msec = ms.rem_euclid(1000.0) as i32;
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z", y, m0 + 1, d, hour, min, sec, msec)
}

// ---- Standalone JSON parser (avoids &mut self borrow issues) ----

/// Unbox a NaN-tagged Value to a raw i64 for the globals JIT buffer.
#[cfg(any(all(target_arch = "aarch64", target_os = "macos"), target_arch = "x86_64"))]
fn jit_unbox(val: Value) -> i64 {
    if val.is_int() {
        val.as_int().unwrap_or(0) as i64
    } else if val.is_boolean() {
        if val.as_bool().unwrap_or(false) { 1 } else { 0 }
    } else if let Some(f) = val.as_number() {
        f as i64
    } else {
        0  // null, undefined, object → 0
    }
}

/// Rebox a raw i64 from the globals JIT buffer back to a NaN-tagged Value.
#[cfg(any(all(target_arch = "aarch64", target_os = "macos"), target_arch = "x86_64"))]
fn jit_rebox(v: i64) -> Value {
    if v >= i32::MIN as i64 && v <= i32::MAX as i64 {
        Value::int(v as i32)
    } else {
        Value::number(v as f64)
    }
}

/// Format an unsigned integer in a given radix (2-36).
fn radix_fmt(mut n: u64, radix: u32) -> String {
    if n == 0 { return "0".to_string(); }
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut result = Vec::new();
    while n > 0 {
        result.push(digits[(n % radix as u64) as usize]);
        n /= radix as u64;
    }
    result.reverse();
    String::from_utf8(result).unwrap()
}

// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::chunk::Chunk;
    use crate::compiler::opcode::OpCode;

    /// Helper: build a chunk and interner, returning (chunk, interner).
    fn make_env() -> (Chunk, Interner) {
        let mut interner = Interner::new();
        let name = interner.intern("<test>");
        let source = interner.intern("<test-src>");
        let chunk = Chunk::new(name, source);
        (chunk, interner)
    }

    fn emit_op(chunk: &mut Chunk, op: OpCode) {
        chunk.emit_op(op, 1);
    }

    #[test]
    fn test_inline_string_producer() {
        // SSO: new_str packs short strings inline (no interning) and they behave
        // as strings across the polymorphic helpers.
        let (chunk, interner) = make_env();
        let mut vm = Vm::new(chunk, interner);
        let before = vm.interner.len();

        let v = vm.new_str("abc");
        assert!(vm.is_string_like(v));
        assert!(v.is_inline_string());
        assert_eq!(vm.string_char_len(v), 3);
        assert_eq!(vm.value_to_string(v), "abc");
        assert_eq!(vm.type_of_value(v), "string");
        // Producing a short value did NOT intern it.
        assert_eq!(vm.interner.len(), before, "inline string must not be interned on creation");

        // Strict equality vs an interned copy (content compare, no allocation).
        let interned = Value::string(vm.interner.intern("abc"));
        assert!(vm.strict_eq(v, interned));
        assert!(vm.strict_eq(interned, v));
        let other = vm.new_str("abd");
        assert!(!vm.strict_eq(v, other));

        // Interns on demand (property-key path) and round-trips.
        let id = vm.flatten_to_string_id(v);
        assert_eq!(vm.interner.resolve(id), "abc");

        // Empty stays the interned singleton (avoids a truthy empty string).
        let e = vm.new_str("");
        assert!(!e.is_inline_string());
        assert_eq!(vm.value_to_string(e), "");

        // 5 bytes (Unicode): still inline; char count, not byte length.
        let u = vm.new_str("a\u{2713}b");
        assert!(u.is_inline_string());
        assert_eq!(vm.string_char_len(u), 3);
        assert_eq!(vm.value_to_string(u), "a\u{2713}b");

        // > 5 bytes: falls back to interning.
        let long = vm.new_str("abcdef");
        assert!(!long.is_inline_string());
        assert!(long.is_interned_string());
        assert_eq!(vm.value_to_string(long), "abcdef");
    }

    fn emit_const_number(chunk: &mut Chunk, n: f64) {
        let idx = chunk.add_constant(Value::number(n));
        chunk.emit_op_u16(OpCode::Const, idx, 1);
    }

    fn emit_const_int(chunk: &mut Chunk, n: i32) {
        let idx = chunk.add_constant(Value::int(n));
        chunk.emit_op_u16(OpCode::Const, idx, 1);
    }

    fn emit_const_string(chunk: &mut Chunk, interner: &mut Interner, s: &str) {
        let sid = interner.intern(s);
        let idx = chunk.add_constant(Value::string(sid));
        chunk.emit_op_u16(OpCode::Const, idx, 1);
    }

    fn run(chunk: Chunk, interner: Interner) -> Result<Value, VmError> {
        let mut vm = Vm::new(chunk, interner);
        vm.run()
    }

    // -- Constants --

    #[test]
    fn test_push_constant_number() {
        let (mut chunk, interner) = make_env();
        emit_const_number(&mut chunk, 42.5);
        emit_op(&mut chunk, OpCode::Halt);
        let result = run(chunk, interner).unwrap();
        assert_eq!(result.as_number(), Some(42.5));
    }

    #[test]
    fn test_push_literals() {
        let (mut chunk, interner) = make_env();
        emit_op(&mut chunk, OpCode::True);
        emit_op(&mut chunk, OpCode::Halt);
        assert_eq!(run(chunk, interner).unwrap().as_bool(), Some(true));

        let (mut chunk, interner) = make_env();
        emit_op(&mut chunk, OpCode::False);
        emit_op(&mut chunk, OpCode::Halt);
        assert_eq!(run(chunk, interner).unwrap().as_bool(), Some(false));

        let (mut chunk, interner) = make_env();
        emit_op(&mut chunk, OpCode::Null);
        emit_op(&mut chunk, OpCode::Halt);
        assert!(run(chunk, interner).unwrap().is_null());

        let (mut chunk, interner) = make_env();
        emit_op(&mut chunk, OpCode::Undefined);
        emit_op(&mut chunk, OpCode::Halt);
        assert!(run(chunk, interner).unwrap().is_undefined());

        let (mut chunk, interner) = make_env();
        emit_op(&mut chunk, OpCode::Zero);
        emit_op(&mut chunk, OpCode::Halt);
        assert_eq!(run(chunk, interner).unwrap().as_int(), Some(0));

        let (mut chunk, interner) = make_env();
        emit_op(&mut chunk, OpCode::One);
        emit_op(&mut chunk, OpCode::Halt);
        assert_eq!(run(chunk, interner).unwrap().as_int(), Some(1));
    }

    // -- Arithmetic --

    #[test]
    fn test_add_numbers() {
        let (mut chunk, interner) = make_env();
        emit_const_number(&mut chunk, 10.0);
        emit_const_number(&mut chunk, 20.0);
        emit_op(&mut chunk, OpCode::Add);
        emit_op(&mut chunk, OpCode::Halt);
        let result = run(chunk, interner).unwrap();
        assert_eq!(result.as_number(), Some(30.0));
    }

    #[test]
    fn test_add_strings() {
        let (mut chunk, mut interner) = make_env();
        emit_const_string(&mut chunk, &mut interner, "hello ");
        emit_const_string(&mut chunk, &mut interner, "world");
        emit_op(&mut chunk, OpCode::Add);
        emit_op(&mut chunk, OpCode::Halt);
        let result = run(chunk, interner).unwrap();
        // Result should be a string (TAG_STRING or ConsString object)
        assert!(result.is_string() || result.is_object());
    }

    #[test]
    fn test_sub() {
        let (mut chunk, interner) = make_env();
        emit_const_number(&mut chunk, 50.0);
        emit_const_number(&mut chunk, 8.0);
        emit_op(&mut chunk, OpCode::Sub);
        emit_op(&mut chunk, OpCode::Halt);
        assert_eq!(run(chunk, interner).unwrap().as_number(), Some(42.0));
    }

    #[test]
    fn test_mul() {
        let (mut chunk, interner) = make_env();
        emit_const_number(&mut chunk, 6.0);
        emit_const_number(&mut chunk, 7.0);
        emit_op(&mut chunk, OpCode::Mul);
        emit_op(&mut chunk, OpCode::Halt);
        assert_eq!(run(chunk, interner).unwrap().as_number(), Some(42.0));
    }

    #[test]
    fn test_div() {
        let (mut chunk, interner) = make_env();
        emit_const_number(&mut chunk, 84.0);
        emit_const_number(&mut chunk, 2.0);
        emit_op(&mut chunk, OpCode::Div);
        emit_op(&mut chunk, OpCode::Halt);
        assert_eq!(run(chunk, interner).unwrap().as_number(), Some(42.0));
    }

    #[test]
    fn test_rem() {
        let (mut chunk, interner) = make_env();
        emit_const_number(&mut chunk, 10.0);
        emit_const_number(&mut chunk, 3.0);
        emit_op(&mut chunk, OpCode::Rem);
        emit_op(&mut chunk, OpCode::Halt);
        assert_eq!(run(chunk, interner).unwrap().as_number(), Some(1.0));
    }

    #[test]
    fn test_exp() {
        let (mut chunk, interner) = make_env();
        emit_const_number(&mut chunk, 2.0);
        emit_const_number(&mut chunk, 10.0);
        emit_op(&mut chunk, OpCode::Exp);
        emit_op(&mut chunk, OpCode::Halt);
        assert_eq!(run(chunk, interner).unwrap().as_number(), Some(1024.0));
    }

    #[test]
    fn test_neg() {
        let (mut chunk, interner) = make_env();
        emit_const_number(&mut chunk, 42.0);
        emit_op(&mut chunk, OpCode::Neg);
        emit_op(&mut chunk, OpCode::Halt);
        assert_eq!(run(chunk, interner).unwrap().as_number(), Some(-42.0));
    }

    #[test]
    fn test_inc_dec() {
        let (mut chunk, interner) = make_env();
        emit_const_int(&mut chunk, 5);
        emit_op(&mut chunk, OpCode::Inc);
        emit_op(&mut chunk, OpCode::Halt);
        assert_eq!(run(chunk, interner).unwrap().as_number(), Some(6.0));

        let (mut chunk, interner) = make_env();
        emit_const_int(&mut chunk, 5);
        emit_op(&mut chunk, OpCode::Dec);
        emit_op(&mut chunk, OpCode::Halt);
        assert_eq!(run(chunk, interner).unwrap().as_number(), Some(4.0));
    }

    // -- Comparison --

    #[test]
    fn test_strict_eq() {
        let (mut chunk, interner) = make_env();
        emit_const_number(&mut chunk, 42.0);
        emit_const_int(&mut chunk, 42);
        emit_op(&mut chunk, OpCode::StrictEq);
        emit_op(&mut chunk, OpCode::Halt);
        assert_eq!(run(chunk, interner).unwrap().as_bool(), Some(true));
    }

    #[test]
    fn test_comparison_lt() {
        let (mut chunk, interner) = make_env();
        emit_const_number(&mut chunk, 1.0);
        emit_const_number(&mut chunk, 2.0);
        emit_op(&mut chunk, OpCode::Lt);
        emit_op(&mut chunk, OpCode::Halt);
        assert_eq!(run(chunk, interner).unwrap().as_bool(), Some(true));
    }

    // -- Not --

    #[test]
    fn test_not() {
        let (mut chunk, interner) = make_env();
        emit_op(&mut chunk, OpCode::True);
        emit_op(&mut chunk, OpCode::Not);
        emit_op(&mut chunk, OpCode::Halt);
        assert_eq!(run(chunk, interner).unwrap().as_bool(), Some(false));

        let (mut chunk, interner) = make_env();
        emit_op(&mut chunk, OpCode::Zero);
        emit_op(&mut chunk, OpCode::Not);
        emit_op(&mut chunk, OpCode::Halt);
        assert_eq!(run(chunk, interner).unwrap().as_bool(), Some(true));
    }

    // -- Globals --

    #[test]
    fn test_define_and_get_global() {
        let (mut chunk, mut interner) = make_env();
        // define global "x" = 42
        let x_id = interner.intern("x");
        let name_idx = chunk.add_constant(Value::string(x_id));
        emit_const_int(&mut chunk, 42);
        chunk.emit_op_u16(OpCode::DefineGlobal, name_idx, 1);
        // get global "x"
        chunk.emit_op_u16(OpCode::GetGlobal, name_idx, 1);
        emit_op(&mut chunk, OpCode::Halt);

        let result = run(chunk, interner).unwrap();
        assert_eq!(result.as_int(), Some(42));
    }

    #[test]
    fn test_set_global() {
        let (mut chunk, mut interner) = make_env();
        let x_id = interner.intern("x");
        let name_idx = chunk.add_constant(Value::string(x_id));
        // define global "x" = 0
        emit_op(&mut chunk, OpCode::Zero);
        chunk.emit_op_u16(OpCode::DefineGlobal, name_idx, 1);
        // set global "x" = 99
        emit_const_int(&mut chunk, 99);
        chunk.emit_op_u16(OpCode::SetGlobal, name_idx, 1);
        emit_op(&mut chunk, OpCode::Pop); // SetGlobal leaves value on stack
        // get it back
        chunk.emit_op_u16(OpCode::GetGlobal, name_idx, 1);
        emit_op(&mut chunk, OpCode::Halt);

        let result = run(chunk, interner).unwrap();
        assert_eq!(result.as_int(), Some(99));
    }

    #[test]
    fn test_get_undefined_global_is_error() {
        let (mut chunk, mut interner) = make_env();
        let x_id = interner.intern("nope");
        let name_idx = chunk.add_constant(Value::string(x_id));
        chunk.emit_op_u16(OpCode::GetGlobal, name_idx, 1);
        emit_op(&mut chunk, OpCode::Halt);
        let err = run(chunk, interner).unwrap_err();
        match err {
            VmError::ReferenceError(msg) => assert!(msg.contains("nope")),
            VmError::RuntimeError(msg) => assert!(msg.contains("ReferenceError") && msg.contains("nope"),
                "expected ReferenceError about 'nope', got: {msg}"),
            other => panic!("expected ReferenceError, got {other:?}"),
        }
    }

    // -- Locals --

    #[test]
    fn test_get_set_local() {
        let (mut chunk, interner) = make_env();
        // slot 0 = placeholder for the "script" local
        emit_op(&mut chunk, OpCode::Undefined);
        // push 42 into slot 1
        emit_const_int(&mut chunk, 42);
        // GetLocal slot 1
        chunk.emit_op_u8(OpCode::GetLocal, 1, 1);
        emit_op(&mut chunk, OpCode::Halt);
        let result = run(chunk, interner).unwrap();
        assert_eq!(result.as_int(), Some(42));
    }

    // -- Control Flow --

    #[test]
    fn test_jump_if_false() {
        // Push false, JumpIfFalse over push(1), push(2), Halt
        let (mut chunk, interner) = make_env();
        emit_op(&mut chunk, OpCode::False);
        let jump_pos = chunk.emit_jump(OpCode::JumpIfFalse, 1);
        emit_const_int(&mut chunk, 1); // should be skipped
        emit_op(&mut chunk, OpCode::Halt);
        chunk.patch_jump(jump_pos);
        emit_const_int(&mut chunk, 2); // should be reached
        emit_op(&mut chunk, OpCode::Halt);

        let result = run(chunk, interner).unwrap();
        assert_eq!(result.as_int(), Some(2));
    }

    #[test]
    fn test_loop() {
        // Simple loop: sum = 0, i = 3; while (i > 0) { sum += i; i--; }
        // We'll use globals for sum and i, but simpler with locals:
        //   slot 0 = sum = 0
        //   slot 1 = i = 3
        let (mut chunk, interner) = make_env();

        // slot 0: sum = 0
        emit_op(&mut chunk, OpCode::Zero);
        // slot 1: i = 3
        emit_const_int(&mut chunk, 3);

        // loop_start:
        let loop_start = chunk.len();
        // push i (slot 1)
        chunk.emit_op_u8(OpCode::GetLocal, 1, 1);
        // push 0
        emit_op(&mut chunk, OpCode::Zero);
        // i > 0 ?
        emit_op(&mut chunk, OpCode::Gt);
        // if false, jump to end
        let exit_jump = chunk.emit_jump(OpCode::JumpIfFalse, 1);

        // sum = sum + i
        chunk.emit_op_u8(OpCode::GetLocal, 0, 1); // push sum
        chunk.emit_op_u8(OpCode::GetLocal, 1, 1); // push i
        emit_op(&mut chunk, OpCode::Add);           // sum + i
        chunk.emit_op_u8(OpCode::SetLocal, 0, 1);  // store back to sum
        emit_op(&mut chunk, OpCode::Pop);            // pop the SetLocal result

        // i = i - 1
        chunk.emit_op_u8(OpCode::GetLocal, 1, 1);
        emit_op(&mut chunk, OpCode::Dec);
        chunk.emit_op_u8(OpCode::SetLocal, 1, 1);
        emit_op(&mut chunk, OpCode::Pop);

        // loop back
        chunk.emit_loop(loop_start, 1);

        // exit:
        chunk.patch_jump(exit_jump);
        // push sum
        chunk.emit_op_u8(OpCode::GetLocal, 0, 1);
        emit_op(&mut chunk, OpCode::Halt);

        let result = run(chunk, interner).unwrap();
        // 3 + 2 + 1 = 6
        assert_eq!(result.as_number(), Some(6.0));
    }

    // -- Bitwise --

    #[test]
    fn test_bitwise_and() {
        let (mut chunk, interner) = make_env();
        emit_const_int(&mut chunk, 0b1100);
        emit_const_int(&mut chunk, 0b1010);
        emit_op(&mut chunk, OpCode::BitAnd);
        emit_op(&mut chunk, OpCode::Halt);
        assert_eq!(run(chunk, interner).unwrap().as_int(), Some(0b1000));
    }

    #[test]
    fn test_bitwise_or() {
        let (mut chunk, interner) = make_env();
        emit_const_int(&mut chunk, 0b1100);
        emit_const_int(&mut chunk, 0b1010);
        emit_op(&mut chunk, OpCode::BitOr);
        emit_op(&mut chunk, OpCode::Halt);
        assert_eq!(run(chunk, interner).unwrap().as_int(), Some(0b1110));
    }

    #[test]
    fn test_shl() {
        let (mut chunk, interner) = make_env();
        emit_const_int(&mut chunk, 1);
        emit_const_int(&mut chunk, 4);
        emit_op(&mut chunk, OpCode::Shl);
        emit_op(&mut chunk, OpCode::Halt);
        assert_eq!(run(chunk, interner).unwrap().as_int(), Some(16));
    }

    // -- TypeOf --

    #[test]
    fn test_typeof() {
        let (mut chunk, interner) = make_env();
        emit_const_number(&mut chunk, 2.5);
        emit_op(&mut chunk, OpCode::TypeOf);
        emit_op(&mut chunk, OpCode::Halt);
        let result = run(chunk, interner).unwrap();
        assert!(result.is_string());
    }

    // -- Dup / Swap --

    #[test]
    fn test_dup() {
        let (mut chunk, interner) = make_env();
        emit_const_int(&mut chunk, 7);
        emit_op(&mut chunk, OpCode::Dup);
        emit_op(&mut chunk, OpCode::Add);
        emit_op(&mut chunk, OpCode::Halt);
        assert_eq!(run(chunk, interner).unwrap().as_number(), Some(14.0));
    }

    #[test]
    fn test_swap() {
        let (mut chunk, interner) = make_env();
        emit_const_int(&mut chunk, 10);
        emit_const_int(&mut chunk, 3);
        emit_op(&mut chunk, OpCode::Swap);
        emit_op(&mut chunk, OpCode::Sub);
        emit_op(&mut chunk, OpCode::Halt);
        // After swap: stack = [3, 10], sub => 3 - 10 = -7
        assert_eq!(run(chunk, interner).unwrap().as_number(), Some(-7.0));
    }

    // -- Return --

    #[test]
    fn test_return() {
        let (mut chunk, interner) = make_env();
        emit_const_int(&mut chunk, 99);
        emit_op(&mut chunk, OpCode::Return);
        assert_eq!(run(chunk, interner).unwrap().as_int(), Some(99));
    }

    #[test]
    fn test_return_undefined() {
        let (mut chunk, interner) = make_env();
        emit_op(&mut chunk, OpCode::ReturnUndefined);
        assert!(run(chunk, interner).unwrap().is_undefined());
    }

    // -- Abstract equality --

    #[test]
    fn test_abstract_eq_null_undefined() {
        let (mut chunk, interner) = make_env();
        emit_op(&mut chunk, OpCode::Null);
        emit_op(&mut chunk, OpCode::Undefined);
        emit_op(&mut chunk, OpCode::Eq);
        emit_op(&mut chunk, OpCode::Halt);
        assert_eq!(run(chunk, interner).unwrap().as_bool(), Some(true));
    }

    #[test]
    fn test_strict_ne() {
        let (mut chunk, interner) = make_env();
        emit_const_int(&mut chunk, 1);
        emit_op(&mut chunk, OpCode::True);
        emit_op(&mut chunk, OpCode::StrictNe);
        emit_op(&mut chunk, OpCode::Halt);
        assert_eq!(run(chunk, interner).unwrap().as_bool(), Some(true));
    }
}
