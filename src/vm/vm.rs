use std::collections::HashMap;
use std::fmt;

use crate::compiler::chunk::Chunk;
use crate::compiler::opcode::OpCode;
use crate::compiler::chunk::ChunkFlags;
use crate::runtime::object::{GeneratorState, JsObject, ObjectHeap, ObjectId, ObjectKind, PromiseState, Property, trace_value};
use crate::runtime::value::Value;
use crate::util::interner::{Interner, StringId};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum VmError {
    TypeError(String),
    ReferenceError(String),
    RuntimeError(String),
}

impl fmt::Display for VmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VmError::TypeError(msg) => write!(f, "TypeError: {msg}"),
            VmError::ReferenceError(msg) => write!(f, "ReferenceError: {msg}"),
            VmError::RuntimeError(msg) => write!(f, "RuntimeError: {msg}"),
        }
    }
}

impl std::error::Error for VmError {}

// ---------------------------------------------------------------------------
// VM
// ---------------------------------------------------------------------------

/// An upvalue: a reference to a variable that may still be on the stack (open)
/// or has been moved to the heap (closed).
#[derive(Clone)]
pub(crate) enum UpvalueLocation {
    /// Points to a stack slot (variable still on stack).
    Open(usize),
    /// Value has been closed over (moved to heap when enclosing function returned).
    Closed(Value),
}

#[derive(Clone)]
pub(crate) struct Upvalue {
    pub(crate) location: UpvalueLocation,
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
    pub(crate) argc: usize,
    /// Snapshot of the actual argument values, captured at call time before
    /// locals can overwrite the same stack slots (needed for `arguments` object).
    pub(crate) saved_args: Vec<Value>,
}

/// An active exception handler (pushed by PushExcHandler).
#[allow(dead_code)]
pub(crate) struct ExcHandler {
    pub(crate) catch_target: u16,
    pub(crate) finally_target: u16,
    pub(crate) stack_depth: usize,
    pub(crate) frame_idx: usize,
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
    pub(crate) microtask_queue: Vec<Microtask>,
    /// Upvalues for each closure, indexed by closure_id.
    pub(crate) closure_upvalues: Vec<Vec<Upvalue>>,
    /// Call counter per chunk index (for JIT hotspot detection).
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    pub(crate) call_counts: HashMap<usize, u32>,
    /// JIT-compiled native functions, keyed by chunk index.
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    pub(crate) jit_functions: HashMap<usize, crate::jit::compiler::JitFunction>,
    /// console.log output buffer (for testing)
    pub output: Vec<String>,
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
    /// Cached Math object ID for fast dispatch
    pub(crate) math_oid: Option<ObjectId>,
    /// Cached JSON object ID for fast dispatch
    pub(crate) json_oid: Option<ObjectId>,
    /// Symbol descriptions: index = symbol_id, value = optional description StringId
    pub(crate) symbol_descriptions: Vec<Option<StringId>>,
    /// Next symbol ID to allocate
    pub(crate) next_symbol_id: u32,
    /// Well-known symbol IDs
    pub(crate) sym_iterator: u32,
    pub(crate) sym_has_instance: u32,
    pub(crate) sym_to_primitive: u32,
    pub(crate) sym_to_string_tag: u32,
    pub(crate) sym_species: u32,
    pub(crate) sym_unscopables: u32,
    /// Fuel counter: instructions executed (incremented in 1024-step chunks).
    pub(crate) steps: u64,
    /// Max instructions before returning an error. 0 = unlimited.
    pub(crate) max_steps: u64,
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
        let array_prototype = heap.allocate(arr_proto);

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

        // Create Math object with constants
        let mut math_obj = JsObject::ordinary();
        let pi_name = interner.intern("PI");
        math_obj.set_property(pi_name, Value::number(std::f64::consts::PI));
        let e_name = interner.intern("E");
        math_obj.set_property(e_name, Value::number(std::f64::consts::E));
        let ln2_name = interner.intern("LN2");
        math_obj.set_property(ln2_name, Value::number(std::f64::consts::LN_2));
        let ln10_name = interner.intern("LN10");
        math_obj.set_property(ln10_name, Value::number(std::f64::consts::LN_10));
        let sqrt2_name = interner.intern("SQRT2");
        math_obj.set_property(sqrt2_name, Value::number(std::f64::consts::SQRT_2));
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
        let eval_name = interner.intern("eval");
        globals.insert(eval_name, Value::function(-560));
        let symbol_name = interner.intern("Symbol");
        globals.insert(symbol_name, Value::function(-570));
        let map_name = interner.intern("Map");
        globals.insert(map_name, Value::function(-540));
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

        // globalThis: create a global object whose own property lookups
        // proxy to the globals map. For simplicity we expose a plain object
        // pre-populated with the common primitives; reads/writes go through
        // the object, not the globals map.
        let global_this_obj = JsObject::ordinary();
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

        
        Self {
            chunks,
            frames: vec![CallFrame { chunk_idx: 0, ip: 0, base: 0, upvalues: Vec::new(), this_value: Value::undefined(), is_constructor: false, pending_super_call: false, generator_id: None, argc: 0, saved_args: Vec::new() }],
            stack: Vec::with_capacity(256),
            globals,
            interner,
            heap,
            globals_vec,
            global_ic: HashMap::new(),
            global_version: 0,
            global_ic_version: HashMap::new(),
            exc_handlers: Vec::new(),
            microtask_queue: Vec::new(),
            closure_upvalues: Vec::new(),
            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            call_counts: HashMap::new(),
            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            jit_functions: HashMap::new(),
            output: Vec::new(),
            module_cache: HashMap::new(),
            module_dir: None,
            regex_cache: crate::vm::regexp::RegexCache::new(),
            func_prototypes: HashMap::new(),
            object_prototype,
            function_prototype,
            array_prototype,
            math_oid: Some(math_oid),
            json_oid: Some(json_oid),
            symbol_descriptions: sym_descs,
            next_symbol_id: 6, // 0-5 are well-known
            sym_iterator: 0,
            sym_has_instance: 1,
            sym_to_primitive: 2,
            sym_to_string_tag: 3,
            sym_species: 4,
            sym_unscopables: 5,
            steps: 0,
            max_steps: 0,
        }
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
    pub(crate) fn close_upvalues_above(&mut self, from: usize) {
        // Close upvalues in all frames
        for frame in &mut self.frames {
            for uv in &mut frame.upvalues {
                if let UpvalueLocation::Open(stack_idx) = &uv.location
                    && *stack_idx >= from {
                        let val = self.stack[*stack_idx];
                        uv.location = UpvalueLocation::Closed(val);
                    }
            }
        }
        // Close upvalues in the closure storage
        for closure_uvs in &mut self.closure_upvalues {
            for uv in closure_uvs {
                if let UpvalueLocation::Open(stack_idx) = &uv.location
                    && *stack_idx >= from {
                        let val = self.stack[*stack_idx];
                        uv.location = UpvalueLocation::Closed(val);
                    }
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
                            Ok(result) => self.resolve_promise(result_promise, result)?,
                            Err(_e) => {
                                let msg = self.interner.intern("callback error");
                                self.reject_promise(result_promise, Value::string(msg))?;
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
                if let UpvalueLocation::Closed(val) = &uv.location
                    && let Some(oid) = trace_value(*val)
                {
                    roots.push(oid);
                }
            }
        }

        // Root 5: closure upvalues
        for closure_uvs in &self.closure_upvalues {
            for uv in closure_uvs {
                if let UpvalueLocation::Closed(val) = &uv.location
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

        // Root 8: function prototype cache
        for &oid in self.func_prototypes.values() {
            roots.push(oid);
        }

        // Root 9: math_oid, json_oid
        if let Some(oid) = self.math_oid { roots.push(oid); }
        if let Some(oid) = self.json_oid { roots.push(oid); }

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
            self.stack.truncate(handler.stack_depth);
            self.push(Value::object_id(oid));
            self.frames.last_mut().unwrap().ip = handler.catch_target as usize;
            Ok(())
        } else {
            Err(VmError::TypeError(msg.to_owned()))
        }
    }

    fn get_type_error_prototype(&mut self) -> Option<crate::runtime::object::ObjectId> {
        let type_error_name = self.interner.intern("TypeError");
        let proto_key = self.interner.intern("prototype");
        let ctor = self.globals.get(&type_error_name).copied()?;
        let oid = ctor.as_object_id()?;
        self.heap.get(oid)?.get_property(proto_key).and_then(|v| v.as_object_id())
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
            let id = val.as_string_id().unwrap();
            let s = self.interner.resolve(id).trim();
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

    /// ECMAScript ToPrimitive: call valueOf() then toString() on an object.
    /// Returns Ok(Some(primitive)) on success, Ok(None) if no methods exist,
    /// or Err if a method throws.
    pub(crate) fn call_value_of_to_string(&mut self, val: Value) -> Option<Value> {
        let oid = val.as_object_id()?;
        let obj = self.heap.get(oid)?;

        // Try valueOf first
        let value_of_name = self.interner.intern("valueOf");
        if let Some(method_val) = obj.get_property(value_of_name)
            && method_val.is_function()
        {
            match self.call_function(method_val, &[]) {
                Ok(result) if !result.is_object() => return Some(result),
                Ok(_) => {} // returned object, try toString
                Err(_) => return None, // method threw
            }
        }

        // Then try toString
        let to_string_name = self.interner.intern("toString");
        let obj = self.heap.get(oid)?;
        if let Some(method_val) = obj.get_property(to_string_name)
            && method_val.is_function()
        {
            match self.call_function(method_val, &[]) {
                Ok(result) if !result.is_object() => return Some(result),
                Ok(_) => {} // returned object
                Err(_) => return None,
            }
        }

        None
    }

    #[inline(always)]
    pub(crate) fn pop_numbers(&mut self) -> Result<(f64, f64), VmError> {
        let b = self.pop()?;
        let a = self.pop()?;
        // ToPrimitive for objects
        let a = if a.is_object() { self.call_value_of_to_string(a).unwrap_or(a) } else { a };
        let b = if b.is_object() { self.call_value_of_to_string(b).unwrap_or(b) } else { b };
        Ok((self.to_f64(a), self.to_f64(b)))
    }

    /// Pop two values, convert to i32 (for bitwise ops).
    pub(crate) fn pop_ints(&mut self) -> Result<(i32, i32), VmError> {
        let bv = self.pop()?;
        let av = self.pop()?;
        // ToPrimitive for objects
        let av = if av.is_object() { self.call_value_of_to_string(av).unwrap_or(av) } else { av };
        let bv = if bv.is_object() { self.call_value_of_to_string(bv).unwrap_or(bv) } else { bv };
        let b = self.to_i32(bv)?;
        let a = self.to_i32(av)?;
        Ok((a, b))
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
        if let Some(id) = val.as_string_id() {
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
            if f.is_nan() {
                "NaN".into()
            } else if f.is_infinite() {
                if f > 0.0 {
                    "Infinity".into()
                } else {
                    "-Infinity".into()
                }
            } else {
                // Use JS-like number formatting
                let s = format!("{f}");
                s
            }
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
                    ObjectKind::Array(elements) => {
                        let parts: Vec<String> = elements.iter().map(|v| self.value_to_string(*v)).collect();
                        parts.join(",")
                    }
                    ObjectKind::Wrapper(inner) => self.value_to_string(*inner),
                    ObjectKind::RegExp { pattern, flags } => {
                        format!("/{pattern}/{flags}")
                    }
                    _ => {
                        // Check for Error-like objects: scan properties for "message" string
                        if let Some(obj) = self.heap.get(oid) {
                            let mut msg_val: Option<Value> = None;
                            let mut name_val: Option<Value> = None;
                            for &(k, ref prop) in &obj.properties {
                                let ks = self.interner.resolve(k);
                                if ks == "message" { msg_val = Some(prop.value); }
                                else if ks == "name" { name_val = Some(prop.value); }
                            }
                            if let Some(mv) = msg_val
                                && mv.is_string() {
                                    let msg = self.interner.resolve(mv.as_string_id().unwrap()).to_owned();
                                    let name = name_val
                                        .and_then(|v| v.as_string_id())
                                        .map(|id| self.interner.resolve(id).to_owned())
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
                    if matches!(obj.kind, ObjectKind::ConsString { .. }) { return "string"; }
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

    // ---- ConsString helpers -----------------------------------------------

    /// Returns true if val is a heap object of kind ConsString.
    #[inline(always)]
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
        val.is_string() || self.is_cons_string(val)
    }

    /// Returns the character length of a string-like value in O(1) for ConsString.
    pub(crate) fn string_char_len(&self, val: Value) -> u32 {
        if let Some(id) = val.as_string_id() {
            self.interner.resolve(id).chars().count() as u32
        } else if let Some(oid) = val.as_object_id()
            && let Some(obj) = self.heap.get(oid)
            && let ObjectKind::ConsString { len, .. } = obj.kind {
            len
        } else {
            0
        }
    }

    /// Flatten a ConsString (or regular string) to a plain String without interning.
    /// Uses an iterative traversal to avoid stack overflow on deep trees.
    pub(crate) fn flatten_cons_to_string(&self, val: Value) -> String {
        if let Some(id) = val.as_string_id() {
            return self.interner.resolve(id).to_owned();
        }
        let capacity = self.string_char_len(val) as usize;
        let mut result = String::with_capacity(capacity);
        let mut stack = Vec::new();
        stack.push(val);
        while let Some(cur) = stack.pop() {
            if let Some(id) = cur.as_string_id() {
                result.push_str(self.interner.resolve(id));
            } else if let Some(oid) = cur.as_object_id()
                && let Some(obj) = self.heap.get(oid)
                && let ObjectKind::ConsString { left, right, .. } = obj.kind {
                // Push right first so left is processed first (LIFO)
                stack.push(right);
                stack.push(left);
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
    ///   - same type: strict equality
    ///   - null == undefined (and vice versa)
    ///   - number == string: coerce string to number
    pub(crate) fn abstract_eq(&mut self, a: Value, b: Value) -> bool {
        // Fast path: identical bits
        if a.raw() == b.raw() {
            // NaN !== NaN
            if a.is_float() {
                let f = a.as_number().unwrap();
                return !f.is_nan();
            }
            return true;
        }

        // null == undefined
        if a.is_nullish() && b.is_nullish() {
            return true;
        }

        // Both numbers (int/float mix)
        if a.is_number() && b.is_number() {
            return a.as_number() == b.as_number();
        }

        // Both strings (including ConsString)
        if self.is_string_like(a) && self.is_string_like(b) {
            return self.flatten_cons_to_string(a) == self.flatten_cons_to_string(b);
        }

        // Both booleans
        if a.is_boolean() && b.is_boolean() {
            return false; // already handled by raw() check
        }

        // number == string: coerce string to number
        if a.is_number() && b.is_string() {
            if let Some(n) = self.string_to_number(b) {
                return a.as_number() == Some(n);
            }
            return false;
        }
        if a.is_string() && b.is_number() {
            if let Some(n) = self.string_to_number(a) {
                return b.as_number() == Some(n);
            }
            return false;
        }

        // boolean vs other: coerce boolean to number, retry
        if a.is_boolean() {
            let num_a = if a.as_bool().unwrap() { 1.0 } else { 0.0 };
            return self.abstract_eq(Value::number(num_a), b);
        }
        if b.is_boolean() {
            let num_b = if b.as_bool().unwrap() { 1.0 } else { 0.0 };
            return self.abstract_eq(a, Value::number(num_b));
        }

        // object vs primitive: unwrap wrapper only when the OTHER side is primitive
        // (object == object compares references, not values)
        if a.is_object() && !b.is_object() {
            let pa = self.coerce_to_primitive(a);
            if pa.raw() != a.raw() {
                return self.abstract_eq(pa, b);
            }
        }
        if b.is_object() && !a.is_object() {
            let pb = self.coerce_to_primitive(b);
            if pb.raw() != b.raw() {
                return self.abstract_eq(a, pb);
            }
        }

        false
    }

    /// Strict equality (===).
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
        // ConsString equality: compare flattened content
        let a_str = self.is_string_like(a);
        let b_str = self.is_string_like(b);
        if a_str && b_str {
            return self.flatten_cons_to_string(a) == self.flatten_cons_to_string(b);
        }
        false
    }

    /// Try to parse a string value as a number (for == coercion).
    pub(crate) fn string_to_number(&self, val: Value) -> Option<f64> {
        let id = val.as_string_id()?;
        let s = self.interner.resolve(id).trim();
        if s.is_empty() {
            return Some(0.0);
        }
        s.parse::<f64>().ok()
    }

    // ---- Main execution loop ----------------------------------------------

    pub fn run(&mut self) -> Result<Value, VmError> {
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        self.try_partial_jit();

        let result = self.run_until(0)?;
        // Flatten any ConsString result to a TAG_STRING so callers get a normal string value
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
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
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
                self.stack.truncate(frame.base.saturating_sub(1));
                if self.frames.len() <= stop_depth {
                    return Ok(result);
                }
                self.push(result);
                continue;
            }

            let byte = self.read_byte();
            // Safety: bytecode was compiled by our own compiler, all opcodes are valid
            let opcode = unsafe { std::mem::transmute::<u8, OpCode>(byte) };

            match opcode {
                // ---- Constants & Literals --------------------------------
                OpCode::Const => {
                    let index = self.read_u16() as usize;
                    let chunk = self.cur_chunk();
                    let val = self.chunks[chunk].constants.get(index).copied().unwrap_or(Value::undefined());
                    self.push(val);
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
                    self.stack.truncate(new_len);
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

                    // ToPrimitive for objects before type check
                    let a_prim = if a.is_object() {
                        self.coerce_to_primitive(a)
                    } else { a };
                    let b_prim = if b.is_object() {
                        self.coerce_to_primitive(b)
                    } else { b };

                    let a_is_str = a_prim.is_string() || self.is_cons_string(a_prim) || self.is_string_wrapper(a_prim);
                    let b_is_str = b_prim.is_string() || self.is_cons_string(b_prim) || self.is_string_wrapper(b_prim);

                    if a_is_str || b_is_str {
                        // Normalize each side to a string-like value (TAG_STRING or ConsString)
                        let left_val = if self.is_string_like(a_prim) {
                            a_prim
                        } else {
                            let s = self.value_to_string(a_prim);
                            let id = self.interner.intern(&s);
                            Value::string(id)
                        };
                        let right_val = if self.is_string_like(b_prim) {
                            b_prim
                        } else {
                            let s = self.value_to_string(b_prim);
                            let id = self.interner.intern(&s);
                            Value::string(id)
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
                        let na = self.to_f64(a_prim);
                        let nb = self.to_f64(b_prim);
                        self.push_number(na + nb);
                    }
                }

                OpCode::Sub => {
                    let (a, b) = self.pop_numbers()?;
                    self.push_number(a - b);
                }

                OpCode::Mul => {
                    let (a, b) = self.pop_numbers()?;
                    self.push_number(a * b);
                }

                OpCode::Div => {
                    let (a, b) = self.pop_numbers()?;
                    self.push_number(a / b);
                }

                OpCode::Rem => {
                    let (a, b) = self.pop_numbers()?;
                    self.push_number(a % b);
                }

                OpCode::Exp => {
                    let (a, b) = self.pop_numbers()?;
                    self.push_number(a.powf(b));
                }

                OpCode::Neg => {
                    let val = self.pop()?;
                    let prim = if val.is_object() { self.coerce_to_number_primitive(val) } else { val };
                    self.push_number(-self.to_f64(prim));
                }

                OpCode::Pos => {
                    let val = self.pop()?;
                    let prim = if val.is_object() { self.coerce_to_number_primitive(val) } else { val };
                    self.push_number(self.to_f64(prim));
                }

                OpCode::Inc => {
                    let val = self.pop()?;
                    self.push_number(self.to_f64(val) + 1.0);
                }

                OpCode::Dec => {
                    let val = self.pop()?;
                    self.push_number(self.to_f64(val) - 1.0);
                }

                // ---- Bitwise ---------------------------------------------
                OpCode::BitAnd => {
                    let (a, b) = self.pop_ints()?;
                    self.push(Value::int(a & b));
                }

                OpCode::BitOr => {
                    let (a, b) = self.pop_ints()?;
                    self.push(Value::int(a | b));
                }

                OpCode::BitXor => {
                    let (a, b) = self.pop_ints()?;
                    self.push(Value::int(a ^ b));
                }

                OpCode::BitNot => {
                    let val = self.pop()?;
                    let n = self.to_i32(val)?;
                    self.push(Value::int(!n));
                }

                OpCode::Shl => {
                    let (a, b) = self.pop_ints()?;
                    let shift = (b as u32) & 0x1F;
                    self.push(Value::int(a.wrapping_shl(shift)));
                }

                OpCode::Shr => {
                    let (a, b) = self.pop_ints()?;
                    let shift = (b as u32) & 0x1F;
                    self.push(Value::int(a.wrapping_shr(shift)));
                }

                OpCode::UShr => {
                    let b_val = self.pop()?;
                    let a_val = self.pop()?;
                    let a = self.to_u32(a_val)?;
                    let b = self.to_u32(b_val)? & 0x1F;
                    let result = a >> b;
                    // Result is u32; if it fits in i32, use SMI, otherwise float
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
                    let result = self.abstract_eq(a, b);
                    self.push(Value::boolean(result));
                }

                OpCode::Ne => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    let result = self.abstract_eq(a, b);
                    self.push(Value::boolean(!result));
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
                    let bv = self.pop()?; let b = self.coerce_to_primitive(bv);
                    let av = self.pop()?; let a = self.coerce_to_primitive(av);
                    if self.is_string_like(a) && self.is_string_like(b) {
                        let sa = self.flatten_cons_to_string(a);
                        let sb = self.flatten_cons_to_string(b);
                        self.push(Value::boolean(sa < sb));
                    } else {
                        self.push(Value::boolean(self.to_f64(a) < self.to_f64(b)));
                    }
                }

                OpCode::Le => {
                    let bv = self.pop()?; let b = self.coerce_to_primitive(bv);
                    let av = self.pop()?; let a = self.coerce_to_primitive(av);
                    if self.is_string_like(a) && self.is_string_like(b) {
                        let sa = self.flatten_cons_to_string(a);
                        let sb = self.flatten_cons_to_string(b);
                        self.push(Value::boolean(sa <= sb));
                    } else {
                        self.push(Value::boolean(self.to_f64(a) <= self.to_f64(b)));
                    }
                }

                OpCode::Gt => {
                    let bv = self.pop()?; let b = self.coerce_to_primitive(bv);
                    let av = self.pop()?; let a = self.coerce_to_primitive(av);
                    if self.is_string_like(a) && self.is_string_like(b) {
                        let sa = self.flatten_cons_to_string(a);
                        let sb = self.flatten_cons_to_string(b);
                        self.push(Value::boolean(sa > sb));
                    } else {
                        self.push(Value::boolean(self.to_f64(a) > self.to_f64(b)));
                    }
                }

                OpCode::Ge => {
                    let bv = self.pop()?; let b = self.coerce_to_primitive(bv);
                    let av = self.pop()?; let a = self.coerce_to_primitive(av);
                    if self.is_string_like(a) && self.is_string_like(b) {
                        let sa = self.flatten_cons_to_string(a);
                        let sb = self.flatten_cons_to_string(b);
                        self.push(Value::boolean(sa >= sb));
                    } else {
                        self.push(Value::boolean(self.to_f64(a) >= self.to_f64(b)));
                    }
                }

                // ---- Logical / Unary -------------------------------------
                OpCode::Not => {
                    let val = self.pop()?;
                    self.push(Value::boolean(!val.to_boolean()));
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
                    let val = self.globals.get(&name_id).copied().unwrap_or(Value::undefined());
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
                    if !val.to_boolean() {
                        self.frames.last_mut().unwrap().ip = (self.frames.last().unwrap().ip as isize + offset as isize) as usize;
                    }
                }

                OpCode::JumpIfTrue => {
                    let offset = self.read_i16();
                    let val = self.pop()?;
                    if val.to_boolean() {
                        self.frames.last_mut().unwrap().ip = (self.frames.last().unwrap().ip as isize + offset as isize) as usize;
                    }
                }

                OpCode::JumpIfFalsePeek => {
                    let offset = self.read_i16();
                    let val = self.peek()?;
                    if !val.to_boolean() {
                        self.frames.last_mut().unwrap().ip = (self.frames.last().unwrap().ip as isize + offset as isize) as usize;
                    }
                }

                OpCode::JumpIfTruePeek => {
                    let offset = self.read_i16();
                    let val = self.peek()?;
                    if val.to_boolean() {
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
                        // Use saved_args (captured at call time) so locals that share
                        // stack slots with args don't corrupt the arguments object.
                        let args = self.frames[frame_idx].saved_args.clone();
                        let arr = JsObject::array(args);
                        let oid = self.heap.allocate(arr);
                        self.push(Value::object_id(oid));
                    } else {
                        match self.globals.get(&name_id).copied() {
                            Some(val) => self.push(val),
                            None => {
                                // Create a ReferenceError object and throw it
                                let name = self.interner.resolve(name_id).to_owned();
                                let msg = format!("{name} is not defined");
                                if !self.exc_handlers.is_empty() {
                                    // Catchable: create error object and use exception handler
                                    let mut err = JsObject::ordinary();
                                    let msg_key = self.interner.intern("message");
                                    let msg_id = self.interner.intern(&msg);
                                    err.set_property(msg_key, Value::string(msg_id));
                                    let name_key = self.interner.intern("name");
                                    let name_val = self.interner.intern("ReferenceError");
                                    err.set_property(name_key, Value::string(name_val));
                                    let oid = self.heap.allocate(err);
                                    let err_val = Value::object_id(oid);
                                    // Jump to handler
                                    let handler = self.exc_handlers.pop().unwrap();
                                    self.stack.truncate(handler.stack_depth);
                                    self.push(err_val);
                                    self.frames.last_mut().unwrap().ip = handler.catch_target as usize;
                                } else {
                                    return Err(VmError::ReferenceError(msg));
                                }
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
                    self.globals.insert(name_id, val);
                    // Sync to fast Vec
                    let idx = name_id.0 as usize;
                    if idx >= self.globals_vec.len() { self.globals_vec.resize(idx + 1, Value::null()); }
                    self.globals_vec[idx] = val;
                    self.global_version += 1;
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
                    self.stack[base + slot] = val;
                }

                // ---- Functions -------------------------------------------
                OpCode::Call => {
                    let mut argc = self.read_byte() as usize;
                    let func_pos = self.stack.len() - 1 - argc;
                    let func_val = self.stack[func_pos];

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

                            // Check if this is an async function
                            if self.chunks[chunk_idx].flags.contains(ChunkFlags::ASYNC) {
                                // Create a promise, run body synchronously, resolve with result
                                let promise = JsObject::promise();
                                let promise_id = self.heap.allocate(promise);
                                let args_vec: Vec<Value> = (0..argc).map(|i| self.stack[func_pos + 1 + i]).collect();
                                self.stack.truncate(func_pos);
                                match self.call_function(func_val, &args_vec) {
                                    Ok(val) => { self.resolve_promise(promise_id, val)?; }
                                    Err(_e) => {
                                        let msg = self.interner.intern("async function error");
                                        self.reject_promise(promise_id, Value::string(msg))?;
                                    }
                                }
                                self.push(Value::object_id(promise_id));
                                continue;
                            }

                            // ---- JIT: check if we have compiled native code ----
                            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
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
                                    self.stack.truncate(func_pos);
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

                            // ---- Generator: create generator object instead of executing ----
                            if self.chunks[chunk_idx].flags.contains(crate::compiler::chunk::ChunkFlags::GENERATOR) {
                                // Resolve upvalues to values for snapshot
                                let saved_upvalues: Vec<Value> = if closure_id < self.closure_upvalues.len() {
                                    self.closure_upvalues[closure_id].iter().map(|uv| {
                                        match &uv.location {
                                            UpvalueLocation::Open(idx) => self.stack.get(*idx).copied().unwrap_or(Value::undefined()),
                                            UpvalueLocation::Closed(v) => *v,
                                        }
                                    }).collect()
                                } else {
                                    Vec::new()
                                };

                                // Save just the arguments as the initial stack
                                let expected = self.chunks[chunk_idx].param_count as usize;
                                let saved_stack: Vec<Value> = (0..expected.max(argc))
                                    .map(|i| {
                                        if i < argc {
                                            self.stack[func_pos + 1 + i]
                                        } else {
                                            Value::undefined()
                                        }
                                    })
                                    .collect();

                                let mut gen_obj = JsObject::ordinary();
                                gen_obj.kind = ObjectKind::Generator {
                                    state: GeneratorState::SuspendedStart,
                                    chunk_idx,
                                    ip: 0,
                                    saved_stack,
                                    saved_upvalues,
                                    this_value: Value::undefined(),
                                };
                                let gen_oid = self.heap.allocate(gen_obj);
                                self.stack.truncate(func_pos);
                                self.push(Value::object_id(gen_oid));
                                continue;
                            }

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
                                if let Some(f) = self.frames.last_mut() { f.pending_super_call = false; }
                                self.frames.last().unwrap().this_value
                            } else if self.chunks[chunk_idx].flags.contains(ChunkFlags::ARROW) {
                                // Arrow functions inherit `this` from enclosing scope
                                self.frames.last().map(|f| f.this_value).unwrap_or(Value::undefined())
                            } else {
                                Value::undefined()
                            };

                            let saved_args: Vec<Value> = (0..argc)
                                .map(|i| self.stack.get(func_pos + 1 + i).copied().unwrap_or(Value::undefined()))
                                .collect();
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
                                saved_args,
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
                            self.stack.truncate(func_pos);
                            self.resolve_promise(pid, val)?;
                            self.push(Value::undefined());
                            continue;
                        }
                        if s <= -700_000 && s > -800_000 {
                            // Promise reject
                            let pid = ObjectId((-700_000 - s) as u32);
                            let val = if argc > 0 { self.stack[func_pos + 1] } else { Value::undefined() };
                            self.stack.truncate(func_pos);
                            self.reject_promise(pid, val)?;
                            self.push(Value::undefined());
                            continue;
                        }
                        // Promise combinator resolve callback
                        if s <= -800_000 && s > -900_000 {
                            let encoded = (-800_000 - s) as u32;
                            let tracker_oid = ObjectId(encoded / 1024);
                            let index = (encoded % 1024) as usize;
                            let val = if argc > 0 { self.stack[func_pos + 1] } else { Value::undefined() };
                            self.stack.truncate(func_pos);
                            self.handle_combinator_resolve(tracker_oid, index, val)?;
                            self.push(Value::undefined());
                            continue;
                        }
                        // Promise combinator reject callback
                        if s <= -900_000 && s > -1_000_000 {
                            let encoded = (-900_000 - s) as u32;
                            let tracker_oid = ObjectId(encoded / 1024);
                            let index = (encoded % 1024) as usize;
                            let val = if argc > 0 { self.stack[func_pos + 1] } else { Value::undefined() };
                            self.stack.truncate(func_pos);
                            self.handle_combinator_reject(tracker_oid, index, val)?;
                            self.push(Value::undefined());
                            continue;
                        }
                        // Promise.finally fulfill callback
                        if s <= -1_100_000 && s > -1_200_000 {
                            let tracker_oid = ObjectId((-1_100_000 - s) as u32);
                            let val = if argc > 0 { self.stack[func_pos + 1] } else { Value::undefined() };
                            self.stack.truncate(func_pos);
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
                            self.stack.truncate(func_pos);
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
                        self.stack.truncate(func_pos);
                        self.push(Value::symbol(id));
                        continue;
                    }

                    // RegExp(pattern, flags) — without new, same as new RegExp
                    if func_val.is_function() && func_val.as_function() == Some(-580) {
                        let pattern = if argc > 0 { self.value_to_string(self.stack[func_pos + 1]) } else { String::new() };
                        let flags = if argc > 1 { self.value_to_string(self.stack[func_pos + 2]) } else { String::new() };
                        let obj = JsObject {
                            properties: Vec::new(), prototype: None,
                            kind: ObjectKind::RegExp { pattern, flags },
                            marked: false, extensible: true,
                        };
                        let oid = self.heap.allocate(obj);
                        self.stack.truncate(func_pos);
                        self.push(Value::object_id(oid));
                        continue;
                    }

                    // Function(...args) — last arg is body, previous args are param names
                    if func_val.is_function() && func_val.as_function() == Some(-551) {
                        let args: Vec<Value> = (0..argc).map(|i| self.stack[func_pos + 1 + i]).collect();
                        self.stack.truncate(func_pos);
                        let result = self.construct_function(&args)?;
                        self.push(result);
                        continue;
                    }

                    // Check for eval()
                    if func_val.is_function() && func_val.as_function() == Some(-560) {
                        let code = if argc > 0 { self.stack[func_pos + 1] } else { Value::undefined() };
                        self.stack.truncate(func_pos);
                        // eval with non-string argument returns the argument
                        if !code.is_string() {
                            self.push(code);
                            continue;
                        }
                        let code_str = {
                            let sid = code.as_string_id().unwrap();
                            self.interner.resolve(sid).to_owned()
                        };
                        // Lex, parse, compile
                        let mut lexer = crate::lexer::lexer::Lexer::new(&code_str, &mut self.interner);
                        let tokens = lexer.tokenize();
                        let mut parser = crate::parser::parser::Parser::new(tokens, &code_str, &mut self.interner);
                        let program = match parser.parse_program() {
                            Ok(p) => p,
                            Err(e) => {
                                return Err(VmError::RuntimeError(format!("eval SyntaxError: {e}")));
                            }
                        };
                        let compiler = crate::compiler::compiler::Compiler::new(&mut self.interner);
                        let chunk = match compiler.compile_program(&program) {
                            Ok(c) => c,
                            Err(e) => {
                                return Err(VmError::RuntimeError(format!("eval CompileError: {e}")));
                            }
                        };
                        // Flatten and add chunks to VM
                        let base_idx = self.chunks.len();
                        let mut flat_chunks = Vec::new();
                        Vm::flatten_chunk(chunk, &mut flat_chunks);
                        self.chunks.extend(flat_chunks);
                        // Execute as a function call
                        let eval_fn = Value::function(base_idx as i32);
                        let result = self.call_function(eval_fn, &[])?;
                        self.push(result);
                        continue;
                    }

                    // Check for native global function sentinels
                    if func_val.is_function() {
                        let sentinel = func_val.as_function().unwrap();
                        if (-536..=-500).contains(&sentinel) {
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[func_pos + 1 + i]).collect();
                            let result = self.exec_global_fn(sentinel, &args);
                            self.stack.truncate(func_pos);
                            self.push(result);
                            continue;
                        }
                        if (-629..=-590).contains(&sentinel) {
                            // Native this-dependent methods called standalone (this=undefined)
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[func_pos + 1 + i]).collect();
                            let result = self.exec_native_method(sentinel, Value::undefined(), &args);
                            self.stack.truncate(func_pos);
                            self.push(result);
                            continue;
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
                                self.stack.truncate(func_pos);
                                let result = self.call_function_this(fn_val, this_val, &call_args)?;
                                self.push(result);
                                continue;
                            }
                        }
                    }

                    // Only throw TypeError for explicit non-callable primitives.
                    // Leave undefined alone to avoid breaking unimplemented built-ins.
                    let is_explicit_nonfunc = func_val.is_null()
                        || func_val.as_bool().is_some()
                        || func_val.is_number()
                        || func_val.is_int()
                        || (func_val.is_string() && func_val.as_string_id().is_some());
                    if is_explicit_nonfunc {
                        let type_name = if func_val.is_null() { "null".to_owned() }
                            else if let Some(b) = func_val.as_bool() { b.to_string() }
                            else if func_val.is_string() {
                                let sid = func_val.as_string_id().unwrap();
                                format!("\"{}\"", self.interner.resolve(sid))
                            } else { self.value_to_string(func_val) };
                        let msg = format!("{type_name} is not a function");
                        self.stack.truncate(func_pos);
                        self.throw_type_error(&msg)?;
                        continue;
                    }
                    // Unknown/undefined — silently return undefined
                    self.stack.truncate(func_pos);
                    self.push(Value::undefined());
                }

                OpCode::Return => {
                    let result = self.pop()?;
                    let frame = self.frames.pop().unwrap();
                    if !self.closure_upvalues.is_empty() {
                        self.close_upvalues_above(frame.base.saturating_sub(1));
                    }
                    // Generator return: mark completed, produce {value, done: true}
                    if let Some(gid) = frame.generator_id {
                        if let Some(obj) = self.heap.get_mut(gid)
                            && let ObjectKind::Generator { state, .. } = &mut obj.kind
                        {
                            *state = GeneratorState::Completed;
                        }
                        self.stack.truncate(frame.base.saturating_sub(1));
                        let iter_result = self.make_iter_result(result, true)?;
                        self.push(iter_result);
                    } else if self.frames.len() <= stop_depth {
                        self.stack.truncate(frame.base.saturating_sub(1));
                        return Ok(result);
                    } else {
                        self.stack.truncate(frame.base.saturating_sub(1));
                        self.push(result);
                    }
                }

                OpCode::ReturnUndefined => {
                    let frame = self.frames.pop().unwrap();
                    let result = if frame.is_constructor { frame.this_value } else { Value::undefined() };
                    if !self.closure_upvalues.is_empty() {
                        self.close_upvalues_above(frame.base.saturating_sub(1));
                    }
                    // Generator return: mark completed, produce {value: undefined, done: true}
                    if let Some(gid) = frame.generator_id {
                        if let Some(obj) = self.heap.get_mut(gid)
                            && let ObjectKind::Generator { state, .. } = &mut obj.kind
                        {
                            *state = GeneratorState::Completed;
                        }
                        self.stack.truncate(frame.base.saturating_sub(1));
                        let iter_result = self.make_iter_result(Value::undefined(), true)?;
                        self.push(iter_result);
                    } else if self.frames.len() <= stop_depth {
                        self.stack.truncate(frame.base.saturating_sub(1));
                        return Ok(result);
                    } else {
                        self.stack.truncate(frame.base.saturating_sub(1));
                        self.push(result);
                    }
                }

                // ---- Object / Array (placeholders) -----------------------
                OpCode::CreateObject => {
                    let obj = JsObject::ordinary();
                    let id = self.heap.allocate(obj);
                    self.push(Value::object_id(id));
                }

                OpCode::CreateArray => {
                    let hint = self.read_u16() as usize;
                    let elements = Vec::with_capacity(hint);
                    let obj = JsObject::array(elements);
                    let id = self.heap.allocate(obj);
                    self.push(Value::object_id(id));
                }

                // ---- Miscellaneous ---------------------------------------
                OpCode::Halt => {
                    return Ok(if self.stack.is_empty() {
                        Value::undefined()
                    } else {
                        self.pop()?
                    });
                }

                OpCode::CollectRest => {
                    let start_idx = self.read_byte() as usize;
                    let target_slot = self.read_byte() as usize;
                    let frame = self.frames.last().unwrap();
                    let base = frame.base;
                    let argc = frame.argc;
                    // Collect args from start_idx..argc into an array
                    let mut rest_elements = Vec::new();
                    for i in start_idx..argc {
                        if base + i < self.stack.len() {
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
                            match &frame.upvalues[idx].location {
                                UpvalueLocation::Open(stack_idx) => self.stack[*stack_idx],
                                UpvalueLocation::Closed(val) => *val,
                            }
                        } else {
                            Value::undefined()
                        }
                    };
                    self.push(val);
                }

                OpCode::SetUpvalue => {
                    let idx = self.read_byte() as usize;
                    let val = self.peek()?;
                    let frame_idx = self.frames.len() - 1;
                    if idx < self.frames[frame_idx].upvalues.len() {
                        match self.frames[frame_idx].upvalues[idx].location {
                            UpvalueLocation::Open(stack_idx) => {
                                self.stack[stack_idx] = val;
                            }
                            UpvalueLocation::Closed(_) => {
                                self.frames[frame_idx].upvalues[idx].location = UpvalueLocation::Closed(val);
                                // Also update the canonical closure storage so future calls see the change
                                // Find which closure this frame belongs to and update it
                                for closure_uvs in &mut self.closure_upvalues {
                                    if idx < closure_uvs.len()
                                        && let UpvalueLocation::Closed(_) = &closure_uvs[idx].location {
                                            closure_uvs[idx].location = UpvalueLocation::Closed(val);
                                        }
                                }
                            }
                        }
                    }
                }

                OpCode::CloseUpvalue => {
                    // Close the topmost local: move its value from the stack into
                    // all upvalues that reference that stack slot.
                    let stack_idx = self.stack.len() - 1;
                    let val = self.stack[stack_idx];
                    // Walk all frames and close any upvalues pointing to this slot
                    for frame in &mut self.frames {
                        for uv in &mut frame.upvalues {
                            if let UpvalueLocation::Open(si) = &uv.location
                                && *si == stack_idx {
                                    uv.location = UpvalueLocation::Closed(val);
                                }
                        }
                    }
                    // Also close in closure_upvalues storage
                    for closure_uvs in &mut self.closure_upvalues {
                        for uv in closure_uvs {
                            if let UpvalueLocation::Open(si) = &uv.location
                                && *si == stack_idx {
                                    uv.location = UpvalueLocation::Closed(val);
                                }
                        }
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
                    let result = if let Some(oid) = obj_val.as_object_id() {
                        if let Some(key_id) = key.as_string_id() {
                            let key_str = self.interner.resolve(key_id).to_owned();
                            let getter_key = self.interner.intern(&format!("__get_{key_str}__"));
                            let setter_key = self.interner.intern(&format!("__set_{key_str}__"));
                            // Delete the direct property and any accessor properties
                            let r1 = self.heap.get_mut(oid).map(|o| o.delete_property(key_id)).unwrap_or(true);
                            let r2 = self.heap.get_mut(oid).map(|o| o.delete_property(getter_key)).unwrap_or(true);
                            let r3 = self.heap.get_mut(oid).map(|o| o.delete_property(setter_key)).unwrap_or(true);
                            r1 && r2 && r3
                        } else {
                            true
                        }
                    } else {
                        true
                    };
                    self.push(Value::boolean(result));
                }

                OpCode::DeleteGlobal => {
                    let name_idx = self.read_u16() as usize;
                    let name_val = self.chunks[self.cur_chunk()].constants[name_idx];
                    let name_id = name_val.as_string_id().unwrap();
                    let deleted = self.globals.remove(&name_id).is_some();
                    self.push(Value::boolean(deleted));
                }

                OpCode::InstanceOf => {
                    let constructor = self.pop()?;
                    let obj = self.pop()?;
                    // Special case: built-in constructors
                    if constructor.is_function() {
                        let s = constructor.as_function().unwrap();
                        if s == -508 {
                            // Object constructor: true for any object
                            self.push(Value::boolean(obj.is_object()));
                            continue;
                        }
                        if s == -507 {
                            // Array constructor: true if it's an array
                            let is_arr = obj.as_object_id()
                                .and_then(|oid| self.heap.get(oid))
                                .map(|o| matches!(&o.kind, ObjectKind::Array(_)))
                                .unwrap_or(false);
                            self.push(Value::boolean(is_arr));
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
                                            -514 => "SyntaxError", _ => "",
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
                    let result = if let Some(oid) = obj.as_object_id() {
                        if let Some(kid) = key.as_string_id() {
                            // Walk prototype chain for 'in' operator
                            self.heap.get_property_chain(oid, kid).is_some()
                        } else if let Some(idx) = key.as_int() {
                            // Numeric key: check array elements
                            self.heap.get(oid)
                                .map(|o| if let ObjectKind::Array(ref elems) = o.kind {
                                    idx >= 0 && (idx as usize) < elems.len()
                                } else { false })
                                .unwrap_or(false)
                        } else { false }
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
                        if cached != 0xFF {
                            if let Some(&(k, ref prop)) = obj.properties.get(cached as usize)
                                && k == name_id {
                                let val = prop.value;
                                self.pop()?;
                                self.push(val);
                                continue;
                            }
                            // IC miss: fall through to linear scan, update cache below
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
                    if peeked.is_null() {
                        self.pop()?;
                        let type_name = "null";
                        let prop = self.interner.resolve(name_id).to_owned();
                        let msg = format!("Cannot read properties of {type_name} (reading '{prop}')");
                        if !self.exc_handlers.is_empty() {
                            let mut err = JsObject::ordinary();
                            let msg_key = self.interner.intern("message");
                            let msg_id = self.interner.intern(&msg);
                            err.set_property(msg_key, Value::string(msg_id));
                            let nk = self.interner.intern("name");
                            let nv = self.interner.intern("TypeError");
                            err.set_property(nk, Value::string(nv));
                            let oid = self.heap.allocate(err);
                            let handler = self.exc_handlers.pop().unwrap();
                            self.stack.truncate(handler.stack_depth);
                            self.push(Value::object_id(oid));
                            self.frames.last_mut().unwrap().ip = handler.catch_target as usize;
                            continue;
                        }
                        return Err(VmError::TypeError(msg));
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
                        // Check for array-specific properties
                        if name_str == "length"
                            && let Some(obj) = self.heap.get(oid)
                                && let ObjectKind::Array(ref elements) = obj.kind {
                                    self.push(Value::int(elements.len() as i32));
                                    continue;
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
                        // Check for array methods (push, pop, etc.)
                        if matches!(name_str,
                            "push" | "pop" | "join" | "indexOf" | "lastIndexOf"
                            | "includes" | "map" | "filter" | "forEach"
                            | "find" | "findIndex" | "findLast" | "findLastIndex"
                            | "some" | "every" | "reduce" | "reduceRight"
                            | "reverse" | "shift" | "unshift"
                            | "splice" | "slice" | "concat" | "sort"
                            | "fill" | "copyWithin" | "flat" | "flatMap"
                            | "at" | "keys" | "values" | "entries" | "toString"
                            | "toSorted" | "toReversed" | "toSpliced" | "with"
                        ) {
                            // Store as function sentinel for typeof correctness
                            let sentinel = -((oid.0 as i32 + 1) * 1000 + name_id.0 as i32);
                            self.push(Value::function(sentinel));
                            // Also push the object back since CallMethod expects it
                            // Actually -- the object was already popped. For CallMethod,
                            // the compiler pushes obj first, then looks up the method.
                            // Let me just store the sentinel and handle in CallMethod.
                            continue;
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
                                _ => Value::undefined(),
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
                        let val = self.heap.get_property_chain(oid, name_id)
                            .unwrap_or(Value::undefined());
                        self.push(val);
                    } else if obj_val.is_string() {
                        // String property/method access
                        let sid = obj_val.as_string_id().unwrap();
                        let s = self.interner.resolve(sid);
                        match name_str {
                            "length" => self.push(Value::int(s.chars().count() as i32)),
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
                            _ => self.push(Value::undefined()),
                        }
                    } else if obj_val.is_function() {
                        // Property access on sentinel globals (Number.NaN, etc)
                        let sentinel = obj_val.as_function().unwrap();
                        let result = match sentinel {
                            -505 => match name_str {
                                "NaN" => Value::number(f64::NAN),
                                "POSITIVE_INFINITY" => Value::number(f64::INFINITY),
                                "NEGATIVE_INFINITY" => Value::number(f64::NEG_INFINITY),
                                "MAX_VALUE" => Value::number(f64::MAX),
                                "MIN_VALUE" => Value::number(f64::MIN_POSITIVE),
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
                            -570 => match name_str {
                                "iterator" => Value::symbol(self.sym_iterator),
                                "hasInstance" => Value::symbol(self.sym_has_instance),
                                "toPrimitive" => Value::symbol(self.sym_to_primitive),
                                "toStringTag" => Value::symbol(self.sym_to_string_tag),
                                "species" => Value::symbol(self.sym_species),
                                "unscopables" => Value::symbol(self.sym_unscopables),
                                _ => Value::undefined(),
                            },
                            -504 => match name_str {
                                // String static methods exposed as sentinels
                                "fromCharCode" => Value::function(-534),
                                "fromCodePoint" => Value::function(-535),
                                "raw" => Value::function(-536),
                                _ => Value::undefined(),
                            },
                            -507 => match name_str {
                                // Array static properties
                                "prototype" => Value::object_id(self.array_prototype),
                                _ => Value::undefined(),
                            },
                            -508 => match name_str {
                                // Object static properties
                                "prototype" => Value::object_id(self.object_prototype),
                                _ => Value::undefined(),
                            },
                            -551 => match name_str {
                                // Function static properties
                                "prototype" => Value::object_id(self.function_prototype),
                                _ => Value::undefined(),
                            },
                            _ => {
                                // User-defined function properties
                                match name_str {
                                    "prototype" => {
                                        // Get or create the prototype object for this function
                                        if let Some(&proto_oid) = self.func_prototypes.get(&sentinel) {
                                            Value::object_id(proto_oid)
                                        } else {
                                            let mut proto = JsObject::ordinary();
                                            proto.prototype = Some(self.object_prototype);
                                            let ctor_key = self.interner.intern("constructor");
                                            // constructor is writable+configurable but NOT enumerable
                                            proto.define_property(ctor_key, Property::with_flags(
                                                obj_val, Property::WRITABLE | Property::CONFIGURABLE
                                            ));
                                            let proto_oid = self.heap.allocate(proto);
                                            self.func_prototypes.insert(sentinel, proto_oid);
                                            Value::object_id(proto_oid)
                                        }
                                    }
                                    "name" => {
                                        let chunk_idx = (sentinel & 0xFFFF) as usize;
                                        if chunk_idx < self.chunks.len() {
                                            let name = self.chunks[chunk_idx].name;
                                            Value::string(name)
                                        } else {
                                            let s = self.interner.intern("");
                                            Value::string(s)
                                        }
                                    }
                                    "length" => {
                                        let chunk_idx = (sentinel & 0xFFFF) as usize;
                                        if chunk_idx < self.chunks.len() {
                                            // Function.length = params before first default
                                            Value::int(self.chunks[chunk_idx].formal_length as i32)
                                        } else {
                                            Value::int(0)
                                        }
                                    }
                                    "call" | "apply" | "bind" => {
                                        // Return function sentinel for method dispatch
                                        obj_val
                                    }
                                    _ => Value::undefined(),
                                }
                            }
                        };
                        self.push(result);
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
                    if let Some(oid) = obj_val.as_object_id() {
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
                        if let Some(sfn) = setter_fn
                            && sfn.is_function()
                        {
                            let _ = self.call_function_this(sfn, obj_val, &[val]);
                        } else if let Some(obj) = self.heap.get_mut(oid) {
                            // IC: update or insert — record the slot for future fast access
                            let pos = obj.properties.iter().position(|(k, _)| *k == name_id);
                            obj.set_property(name_id, val);
                            let slot = pos.unwrap_or(obj.properties.len().saturating_sub(1));
                            if slot <= 254 {
                                self.chunks[chunk_idx].property_ic[ic_slot] = slot as u8;
                            }
                        }
                    }
                    self.push(val);
                }

                OpCode::GetElement => {
                    let key = self.pop()?;
                    let obj_val = self.pop()?;
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
                            // Float index
                            if let Some(idx) = key.as_number() {
                                let val = elements.get(idx as usize).copied().unwrap_or(Value::undefined());
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
                        // String property lookup — check getter first, then plain property
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
                            let val = self.heap.get_property_chain(oid, name_id)
                                .unwrap_or(Value::undefined());
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
                            let val = self.heap.get_property_chain(oid, name_id)
                                .unwrap_or(Value::undefined());
                            self.push(val);
                            continue;
                        }
                    }
                    // String bracket index access: "hello"[0] → "h" (also handles ConsString)
                    let string_val_opt: Option<String> = if obj_val.is_string() {
                        let sid = obj_val.as_string_id().unwrap();
                        Some(self.interner.resolve(sid).to_owned())
                    } else if self.is_cons_string(obj_val) {
                        Some(self.flatten_cons_to_string(obj_val))
                    } else { None };
                    if let Some(s) = string_val_opt {
                        if let Some(i) = key.as_int() {
                            if i >= 0
                                && let Some(ch) = s.chars().nth(i as usize) {
                                    let ch_id = self.interner.intern(&ch.to_string());
                                    self.push(Value::string(ch_id));
                                    continue;
                                }
                        } else if let Some(idx) = key.as_number() {
                            let i = idx as usize;
                            if idx >= 0.0 && idx.fract() == 0.0
                                && let Some(ch) = s.chars().nth(i) {
                                    let ch_id = self.interner.intern(&ch.to_string());
                                    self.push(Value::string(ch_id));
                                    continue;
                                }
                        } else if let Some(name_id) = key.as_string_id() {
                            let name = self.interner.resolve(name_id);
                            if name == "length" {
                                self.push(Value::int(s.chars().count() as i32));
                                continue;
                            }
                        }
                    }
                    self.push(Value::undefined());
                }

                OpCode::SetElement => {
                    let val = self.pop()?;
                    let key = self.pop()?;
                    let obj_val = self.pop()?;
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
                            if let Some(idx) = key.as_number() {
                                let idx = idx as usize;
                                while elements.len() <= idx {
                                    elements.push(Value::undefined());
                                }
                                elements[idx] = val;
                                self.push(val);
                                continue;
                            }
                        }
                        if let Some(name_id) = key.as_string_id() {
                            // Check for setter first
                            let name_str = self.interner.resolve(name_id).to_owned();
                            let setter_key = self.interner.intern(&format!("__set_{name_str}__"));
                            if let Some(sfn) = self.heap.get_property_chain(oid, setter_key)
                                && sfn.is_function() {
                                    let _ = self.call_function_this(sfn, obj_val, &[val]);
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
                        // Check for private getter (__get_#name__)
                        let getter_key_str = format!("__get_{}__", name_str);
                        let getter_key = self.interner.intern(&getter_key_str);
                        let getter_fn = self.heap.get_property_chain(oid, getter_key);
                        if let Some(gfn) = getter_fn && gfn.is_function() {
                            let result = self.call_function_this(gfn, obj_val, &[])?;
                            self.push(result);
                        } else {
                            let val = self.heap.get_property_chain(oid, name_id)
                                .unwrap_or(Value::undefined());
                            self.push(val);
                        }
                    } else {
                        self.push(Value::undefined());
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
                        // Check for private setter (__set_#name__)
                        let setter_key_str = format!("__set_{}__", name_str);
                        let setter_key = self.interner.intern(&setter_key_str);
                        let setter_fn = self.heap.get_property_chain(oid, setter_key);
                        if let Some(sfn) = setter_fn && sfn.is_function() {
                            let _ = self.call_function_this(sfn, obj_val, &[value]);
                        } else if let Some(obj) = self.heap.get_mut(oid) {
                            obj.set_property(name_id, value);
                        }
                    }
                    self.push(value);
                }

                OpCode::CallMethod => {
                    let argc = self.read_byte() as usize;
                    let method_name_idx = self.read_u16() as usize;
                    let method_name = self.chunks[self.cur_chunk()].constants[method_name_idx]
                        .as_string_id().unwrap();
                    // Stack layout: [..., obj, arg0, ..., argN]
                    let obj_pos = self.stack.len() - 1 - argc;
                    let obj_val = self.stack[obj_pos];

                    // Look up the method on the object (walking prototype chain)
                    let method_val = if let Some(oid) = obj_val.as_object_id() {
                        self.heap.get_property_chain(oid, method_name)
                    } else {
                        None
                    };

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
                                if sentinel == -102 {
                                    eprintln!("{line}"); // console.error -> stderr
                                } else {
                                    println!("{line}");
                                }
                                self.output.push(line);
                                self.stack.truncate(obj_pos);
                                self.push(Value::undefined());
                                continue;
                            }
                        }

                    // Check if the obj is a string (or ConsString) and dispatch string method
                    let string_for_method = if obj_val.is_string() {
                        let sid = obj_val.as_string_id().unwrap();
                        Some(self.interner.resolve(sid).to_owned())
                    } else if self.is_cons_string(obj_val) {
                        Some(self.flatten_cons_to_string(obj_val))
                    } else {
                        None
                    };
                    if let Some(s) = string_for_method {
                        let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                        let result = self.exec_string_method(&s, method_name, &args);
                        self.stack.truncate(obj_pos);
                        self.push(result);
                        continue;
                    }

                    // Number primitive methods: (42).toString(16), (3.14).toFixed(2)
                    if obj_val.is_number() || obj_val.is_int() {
                        let mn = self.interner.resolve(method_name).to_owned();
                        let n = self.to_f64(obj_val);
                        let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                        let result = match mn.as_str() {
                            "toString" => {
                                let radix = args.first().and_then(|v| v.as_number()).unwrap_or(10.0) as u32;
                                let s = if radix == 10 {
                                    self.value_to_string(obj_val)
                                } else if n.fract() == 0.0 && n.is_finite() {
                                    // Integer with non-10 radix
                                    let i = n as i64;
                                    if i >= 0 { radix_fmt(i as u64, radix) }
                                    else { format!("-{}", radix_fmt((-i) as u64, radix)) }
                                } else {
                                    self.value_to_string(obj_val)
                                };
                                let id = self.interner.intern(&s);
                                Value::string(id)
                            }
                            "valueOf" => obj_val,
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
                        self.stack.truncate(obj_pos);
                        self.push(result);
                        continue;
                    }

                    // Boolean primitive methods: true.toString()
                    if obj_val.is_boolean() {
                        let mn = self.interner.resolve(method_name).to_owned();
                        let result = match mn.as_str() {
                            "toString" => {
                                let s = if obj_val.as_bool().unwrap() { "true" } else { "false" };
                                let id = self.interner.intern(s);
                                Value::string(id)
                            }
                            "valueOf" => obj_val,
                            _ => Value::undefined(),
                        };
                        self.stack.truncate(obj_pos);
                        self.push(result);
                        continue;
                    }

                    // Check for Function.prototype.call/apply/bind
                    if obj_val.is_function() || (obj_val.is_object() && obj_val.as_object_id()
                        .and_then(|oid| self.heap.get(oid))
                        .map(|o| matches!(&o.kind, ObjectKind::Function(_)))
                        .unwrap_or(false))
                    {
                        let mn = self.interner.resolve(method_name).to_owned();
                        match mn.as_str() {
                            "call" => {
                                let this_arg = if argc > 0 { self.stack[obj_pos + 1] } else { Value::undefined() };
                                let call_args: Vec<Value> = (1..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                                self.stack.truncate(obj_pos);
                                let result = self.call_function_this(obj_val, this_arg, &call_args)?;
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
                                self.stack.truncate(obj_pos);
                                let result = self.call_function_this(obj_val, this_arg, &call_args)?;
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
                                            // User bytecode function — wrap as Bytecode
                                            let chunk_idx = (packed & 0xFFFF) as usize;
                                            let name = if chunk_idx < self.chunks.len() { self.chunks[chunk_idx].name } else { self.interner.intern("<bound>") };
                                            let fobj = JsObject::function_bytecode(chunk_idx, name);
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
                                self.stack.truncate(obj_pos);
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
                                self.stack.truncate(obj_pos);
                                self.push(result);
                                continue;
                            }
                        // Check for Generator methods (.next, .return, .throw)
                        if let Some(obj) = self.heap.get(oid)
                            && matches!(&obj.kind, ObjectKind::Generator { .. })
                        {
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                            // Clear CallMethod operands before resuming
                            self.stack.truncate(obj_pos);
                            let action = self.exec_generator_method(oid, method_name, &args)?;
                            match action {
                                crate::vm::generator::GeneratorAction::Done(result) => {
                                    self.push(result);
                                    continue;
                                }
                                crate::vm::generator::GeneratorAction::Resumed => {
                                    // Generator frame pushed — main loop will execute it
                                    continue;
                                }
                            }
                        }
                        // Check for RegExp methods
                        if let Some(obj) = self.heap.get(oid)
                            && matches!(&obj.kind, ObjectKind::RegExp { .. })
                        {
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                            let result = self.exec_regexp_method(oid, method_name, &args)?;
                            self.stack.truncate(obj_pos);
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
                                self.stack.truncate(obj_pos);
                                self.push(Value::int(sz as i32));
                                continue;
                            }
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                            let result = self.exec_map_method(oid, method_name, &args)?;
                            self.stack.truncate(obj_pos);
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
                                self.stack.truncate(obj_pos);
                                self.push(Value::int(sz as i32));
                                continue;
                            }
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                            let result = self.exec_set_method(oid, method_name, &args)?;
                            self.stack.truncate(obj_pos);
                            self.push(result);
                            continue;
                        }
                        // Check for WeakMap methods
                        if let Some(obj) = self.heap.get(oid)
                            && matches!(&obj.kind, ObjectKind::WeakMap { .. })
                        {
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                            let result = self.exec_weakmap_method(oid, method_name, &args)?;
                            self.stack.truncate(obj_pos);
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
                            self.stack.truncate(obj_pos);
                            self.push(result);
                            continue;
                        }
                        // Check for WeakSet methods
                        if let Some(obj) = self.heap.get(oid)
                            && matches!(&obj.kind, ObjectKind::WeakSet { .. })
                        {
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                            let result = self.exec_weakset_method(oid, method_name, &args)?;
                            self.stack.truncate(obj_pos);
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
                                "pow" => Value::number(self.to_f64(arg0).powf(self.to_f64(arg1))),
                                "max" => Value::number(self.to_f64(arg0).max(self.to_f64(arg1))),
                                "min" => Value::number(self.to_f64(arg0).min(self.to_f64(arg1))),
                                _ => {
                                    let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                                    self.exec_math_method(method_name, &args)
                                }
                            };
                            self.stack.truncate(obj_pos);
                            self.push(result);
                            continue;
                        }
                        // Check for JSON methods (fast: cached ObjectId comparison)
                        if self.json_oid == Some(oid) {
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                            let result = self.exec_json_method(method_name, &args);
                            self.stack.truncate(obj_pos);
                            self.push(result);
                            continue;
                        }
                    }

                    // Object.prototype methods (hasOwnProperty, toString, valueOf, etc.)
                    if let Some(oid) = obj_val.as_object_id() {
                        let mn = self.interner.resolve(method_name).to_owned();
                        match mn.as_str() {
                            "hasOwnProperty" => {
                                let key = if argc > 0 { self.value_to_string(self.stack[obj_pos + 1]) } else { String::new() };
                                let key_id = self.interner.intern(&key);
                                let getter_key = self.interner.intern(&format!("__get_{key}__"));
                                let setter_key = self.interner.intern(&format!("__set_{key}__"));
                                let has = self.heap.get(oid).map(|o| {
                                    o.has_own_property(key_id)
                                        || o.has_own_property(getter_key)
                                        || o.has_own_property(setter_key)
                                }).unwrap_or(false);
                                self.stack.truncate(obj_pos);
                                self.push(Value::boolean(has));
                                continue;
                            }
                            "propertyIsEnumerable" => {
                                let key = if argc > 0 { self.value_to_string(self.stack[obj_pos + 1]) } else { String::new() };
                                let key_id = self.interner.intern(&key);
                                let is_enum = self.heap.get(oid)
                                    .and_then(|o| o.get_property_descriptor(key_id))
                                    .map(|p| p.is_enumerable())
                                    .unwrap_or(false);
                                self.stack.truncate(obj_pos);
                                self.push(Value::boolean(is_enum));
                                continue;
                            }
                            "toString" => {
                                // Check for Error-like object: has `name` and `message` → "Name: message"
                                let error_str = if let Some(o) = self.heap.get(oid) {
                                    let name_key = self.interner.intern("name");
                                    let msg_key = self.interner.intern("message");
                                    let name_v = o.get_property(name_key);
                                    let msg_v = o.get_property(msg_key);
                                    if let (Some(nv), Some(mv)) = (name_v, msg_v) {
                                        let name_s = self.value_to_string(nv);
                                        let msg_s = self.value_to_string(mv);
                                        if !name_s.is_empty() {
                                            Some(if msg_s.is_empty() { name_s } else { format!("{name_s}: {msg_s}") })
                                        } else { None }
                                    } else { None }
                                } else { None };
                                if let Some(s) = error_str {
                                    let id = self.interner.intern(&s);
                                    self.stack.truncate(obj_pos);
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
                                self.stack.truncate(obj_pos);
                                self.push(Value::string(id));
                                continue;
                            }
                            "valueOf" => {
                                self.stack.truncate(obj_pos);
                                self.push(obj_val);
                                continue;
                            }
                            _ => {}
                        }
                    }

                    // Try to call as a closure method on an object (walk prototype chain)
                    if let Some(oid) = obj_val.as_object_id() {
                        let method_val = self.heap.get_property_chain(oid, method_name);
                        if let Some(mv) = method_val
                            && mv.is_function() {
                                let packed = mv.as_function().unwrap();
                                let closure_id = ((packed as u32) >> 16) as usize;
                                let chunk_idx = (packed & 0xFFFF) as usize;
                                if chunk_idx >= 1 && chunk_idx < self.chunks.len() {
                                    // Generator methods: create a generator object instead of executing
                                    if self.chunks[chunk_idx].flags.contains(crate::compiler::chunk::ChunkFlags::GENERATOR) {
                                        let saved_upvalues: Vec<Value> = if closure_id < self.closure_upvalues.len() {
                                            self.closure_upvalues[closure_id].iter().map(|uv| {
                                                match &uv.location {
                                                    UpvalueLocation::Open(idx) => self.stack.get(*idx).copied().unwrap_or(Value::undefined()),
                                                    UpvalueLocation::Closed(v) => *v,
                                                }
                                            }).collect()
                                        } else { Vec::new() };
                                        let expected = self.chunks[chunk_idx].param_count as usize;
                                        let saved_stack: Vec<Value> = (0..expected.max(argc))
                                            .map(|i| {
                                                if i < argc { self.stack[obj_pos + 1 + i] }
                                                else { Value::undefined() }
                                            }).collect();
                                        let mut gen_obj = JsObject::ordinary();
                                        gen_obj.kind = ObjectKind::Generator {
                                            state: GeneratorState::SuspendedStart,
                                            chunk_idx,
                                            ip: 0,
                                            saved_stack,
                                            saved_upvalues,
                                            this_value: obj_val,
                                        };
                                        let gen_oid = self.heap.allocate(gen_obj);
                                        self.stack.truncate(obj_pos);
                                        self.push(Value::object_id(gen_oid));
                                        continue;
                                    }
                                    // Restructure stack: [obj, args...] -> [args...]
                                    // Put closure in func_pos, shift args
                                    self.stack[obj_pos] = mv;
                                    let mut actual_argc = argc;
                                    let expected = self.chunks[chunk_idx].param_count as usize;
                                    while actual_argc < expected {
                                        self.push(Value::undefined());
                                        actual_argc += 1;
                                    }
                                    let upvalues = if closure_id < self.closure_upvalues.len() {
                                        self.closure_upvalues[closure_id].clone()
                                    } else { Vec::new() };
                                    let saved_args: Vec<Value> = (0..argc)
                                        .map(|i| self.stack.get(obj_pos + 1 + i).copied().unwrap_or(Value::undefined()))
                                        .collect();
                                    self.frames.push(CallFrame {
                                        chunk_idx, ip: 0, base: obj_pos + 1,
                                        upvalues, this_value: obj_val, is_constructor: false,
                                        pending_super_call: false, generator_id: None, argc,
                                        saved_args,
                                    });
                                    continue;
                                }
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
                            self.stack.truncate(obj_pos);
                            self.push(result);
                            continue;
                        }
                    }

                    // Object static methods (Object.keys)
                    if obj_val.is_function() && obj_val.as_function() == Some(-508) {
                        let mn = self.interner.resolve(method_name).to_owned();
                        let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                        let result = match mn.as_str() {
                            "keys" => {
                                if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
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
                                        for (k, en) in props {
                                            if !en { continue; }
                                            let name = self.interner.resolve(k);
                                            if let Ok(n) = name.parse::<u64>() {
                                                numeric.push((n, k));
                                            } else {
                                                string.push(k);
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
                                                    .filter(|(_, p)| p.is_enumerable())
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
                                            .filter(|(_, p)| p.is_enumerable())
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
                            "assign" => {
                                let target = args.first().copied().unwrap_or(Value::undefined());
                                if let Some(target_oid) = target.as_object_id() {
                                    for source_val in args.iter().skip(1) {
                                        if let Some(src_oid) = source_val.as_object_id() {
                                            let props: Vec<(StringId, Value)> = self.heap.get(src_oid)
                                                .map(|o| o.properties.iter()
                                                    .filter(|(_, p)| p.is_enumerable())
                                                    .map(|&(k, ref p)| (k, p.value)).collect())
                                                .unwrap_or_default();
                                            for (k, v) in props {
                                                if let Some(obj) = self.heap.get_mut(target_oid) {
                                                    obj.set_property(k, v);
                                                }
                                            }
                                        }
                                    }
                                    target
                                } else { target }
                            }
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
                            "defineProperty" => {
                                let target = args.first().copied().unwrap_or(Value::undefined());
                                let key_val = args.get(1).copied().unwrap_or(Value::undefined());
                                let desc_val = args.get(2).copied().unwrap_or(Value::undefined());
                                if let Some(target_oid) = target.as_object_id() {
                                    let key_str = self.value_to_string(key_val);
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
                                        // Handle getter/setter
                                        if let Some(getter) = self.heap.get_property_chain(desc_oid, get_key)
                                            && getter.is_function() {
                                                let getter_key = self.interner.intern(&format!("__get_{key_str}__"));
                                                if let Some(obj) = self.heap.get_mut(target_oid) {
                                                    obj.set_property(getter_key, getter);
                                                }
                                            }
                                        if let Some(setter) = self.heap.get_property_chain(desc_oid, set_key)
                                            && setter.is_function() {
                                                let setter_key = self.interner.intern(&format!("__set_{key_str}__"));
                                                if let Some(obj) = self.heap.get_mut(target_oid) {
                                                    obj.set_property(setter_key, setter);
                                                }
                                            }
                                    }
                                    // Only create data property if no getter/setter was defined
                                    // (accessor and data descriptors are mutually exclusive)
                                    let has_accessor = self.heap.get(target_oid)
                                        .map(|o| {
                                            let gk = self.interner.intern(&format!("__get_{key_str}__"));
                                            let sk = self.interner.intern(&format!("__set_{key_str}__"));
                                            o.has_own_property(gk) || o.has_own_property(sk)
                                        })
                                        .unwrap_or(false);
                                    if !has_accessor
                                        && let Some(obj) = self.heap.get_mut(target_oid) {
                                            obj.define_property(key_id, Property::with_flags(value, flags));
                                    }
                                    target
                                } else { target }
                            }
                            "defineProperties" => {
                                // Simplified: treat like Object.assign for now
                                args.first().copied().unwrap_or(Value::undefined())
                            }
                            "getOwnPropertyDescriptor" => {
                                if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
                                    let key_str = args.get(1).map(|v| self.value_to_string(*v)).unwrap_or_default();
                                    let key_id = self.interner.intern(&key_str);
                                    // Check for accessor properties first
                                    let getter_key_str = format!("__get_{key_str}__");
                                    let setter_key_str = format!("__set_{key_str}__");
                                    let getter_key = self.interner.intern(&getter_key_str);
                                    let setter_key = self.interner.intern(&setter_key_str);
                                    let getter = self.heap.get(oid).and_then(|o| o.get_property(getter_key));
                                    let setter = self.heap.get(oid).and_then(|o| o.get_property(setter_key));
                                    if getter.is_some() || setter.is_some() {
                                        let mut desc = JsObject::ordinary();
                                        let get_key = self.interner.intern("get");
                                        let set_key = self.interner.intern("set");
                                        let en_key = self.interner.intern("enumerable");
                                        let cf_key = self.interner.intern("configurable");
                                        desc.set_property(get_key, getter.unwrap_or(Value::undefined()));
                                        desc.set_property(set_key, setter.unwrap_or(Value::undefined()));
                                        desc.set_property(en_key, Value::boolean(false));
                                        desc.set_property(cf_key, Value::boolean(true));
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
                                        } else { Value::undefined() }
                                } else { Value::undefined() }
                            }
                            "getOwnPropertyNames" => {
                                if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
                                    let raw_props: Vec<StringId> = self.heap.get(oid)
                                        .map(|o| o.properties.iter().map(|(k, _)| *k).collect())
                                        .unwrap_or_default();
                                    let mut seen = std::collections::HashSet::new();
                                    let mut names: Vec<Value> = Vec::new();
                                    for k in raw_props {
                                        let s = self.interner.resolve(k).to_owned();
                                        // Convert __get_X__ / __set_X__ → X
                                        let real = if (s.starts_with("__get_") || s.starts_with("__set_")) && s.ends_with("__") {
                                            s[6..s.len()-2].to_owned()
                                        } else {
                                            s.clone()
                                        };
                                        if seen.insert(real.clone()) {
                                            let id = self.interner.intern(&real);
                                            names.push(Value::string(id));
                                        }
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
                                if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
                                    Value::boolean(self.heap.get(oid).map(|o| o.extensible).unwrap_or(false))
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
                            _ => Value::undefined(),
                        };
                        self.stack.truncate(obj_pos);
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
                                self.stack.truncate(obj_pos);
                                self.push(Value::boolean(is_arr));
                                continue;
                            }
                            "from" => {
                                let source = if argc > 0 { self.stack[obj_pos + 1] } else { Value::undefined() };
                                let map_fn = if argc > 1 { Some(self.stack[obj_pos + 2]) } else { None };
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
                                                if let Some(len_val) = obj.get_property(length_key) {
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
                                    let sid = source.as_string_id().unwrap();
                                    let s = self.interner.resolve(sid).to_owned();
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
                                self.stack.truncate(obj_pos);
                                self.push(Value::object_id(new_oid));
                                continue;
                            }
                            "of" => {
                                let items: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                                let arr = JsObject::array(items);
                                let new_oid = self.heap.allocate(arr);
                                self.stack.truncate(obj_pos);
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
                        self.stack.truncate(obj_pos);
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
                            self.stack.truncate(obj_pos);
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
                            self.stack.truncate(obj_pos);
                            self.push(Value::string(id));
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
                            self.stack.truncate(obj_pos);
                            self.push(result);
                            continue;
                        }
                    }

                    // Check for Promise static methods (Promise.resolve/reject)
                    if obj_val.is_function() && obj_val.as_function() == Some(-520) {
                        let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                        let result = self.exec_promise_static(method_name, &args)?;
                        self.stack.truncate(obj_pos);
                        self.push(result);
                        continue;
                    }

                    // Generic method call fallthrough - push undefined
                    self.stack.truncate(obj_pos);
                    self.push(Value::undefined());
                }

                OpCode::Construct => {
                    let argc = self.read_byte() as usize;
                    let func_pos = self.stack.len() - 1 - argc;
                    let func_val = self.stack[func_pos];

                    // Handle Promise constructor
                    if func_val.is_function() && func_val.as_function() == Some(-520) {
                        let executor = if argc > 0 { self.stack[func_pos + 1] } else { Value::undefined() };
                        let p = JsObject::promise();
                        let pid = self.heap.allocate(p);
                        // Create resolve/reject sentinels
                        let resolve_val = Value::function(-600_000 - pid.0 as i32);
                        let reject_val = Value::function(-700_000 - pid.0 as i32);
                        // Call the executor
                        if executor.is_function() {
                            let _ = self.call_function(executor, &[resolve_val, reject_val]);
                        }
                        self.stack.truncate(func_pos);
                        self.push(Value::object_id(pid));
                        continue;
                    }

                    // Handle Map/Set/WeakMap/WeakSet constructors
                    if func_val.is_function() {
                        let sentinel = func_val.as_function().unwrap();
                        match sentinel {
                            -540 => { // new Map()
                                let mut entries = Vec::new();
                                // Optional iterable argument (array of [key, value] pairs)
                                if argc > 0
                                    && let Some(arr_oid) = self.stack[func_pos + 1].as_object_id()
                                        && let Some(obj) = self.heap.get(arr_oid)
                                            && let ObjectKind::Array(ref elems) = obj.kind {
                                                let elems = elems.clone();
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
                                    properties: Vec::new(), prototype: None,
                                    kind: ObjectKind::Map { entries }, marked: false, extensible: true,
                                };
                                let oid = self.heap.allocate(obj);
                                self.stack.truncate(func_pos);
                                self.push(Value::object_id(oid));
                                continue;
                            }
                            -541 => { // new Set()
                                let mut entries = Vec::new();
                                if argc > 0
                                    && let Some(arr_oid) = self.stack[func_pos + 1].as_object_id()
                                        && let Some(obj) = self.heap.get(arr_oid)
                                            && let ObjectKind::Array(ref elems) = obj.kind {
                                                entries = elems.clone();
                                            }
                                let obj = JsObject {
                                    properties: Vec::new(), prototype: None,
                                    kind: ObjectKind::Set { entries }, marked: false, extensible: true,
                                };
                                let oid = self.heap.allocate(obj);
                                self.stack.truncate(func_pos);
                                self.push(Value::object_id(oid));
                                continue;
                            }
                            -542 => { // new WeakMap()
                                let obj = JsObject {
                                    properties: Vec::new(), prototype: None,
                                    kind: ObjectKind::WeakMap { entries: Vec::new() }, marked: false, extensible: true,
                                };
                                let oid = self.heap.allocate(obj);
                                self.stack.truncate(func_pos);
                                self.push(Value::object_id(oid));
                                continue;
                            }
                            -543 => { // new WeakSet()
                                let obj = JsObject {
                                    properties: Vec::new(), prototype: None,
                                    kind: ObjectKind::WeakSet { entries: Vec::new() }, marked: false, extensible: true,
                                };
                                let oid = self.heap.allocate(obj);
                                self.stack.truncate(func_pos);
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
                                self.stack.truncate(func_pos);
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
                            properties: Vec::new(), prototype: None,
                            kind: ObjectKind::RegExp { pattern, flags },
                            marked: false, extensible: true,
                        };
                        let oid = self.heap.allocate(obj);
                        self.stack.truncate(func_pos);
                        self.push(Value::object_id(oid));
                        continue;
                    }

                    // new Function(...args)
                    if func_val.is_function() && func_val.as_function() == Some(-551) {
                        let args: Vec<Value> = (0..argc).map(|i| self.stack[func_pos + 1 + i]).collect();
                        self.stack.truncate(func_pos);
                        let result = self.construct_function(&args)?;
                        self.push(result);
                        continue;
                    }

                    // Handle wrapper constructors (new Number, new Boolean, new String)
                    if func_val.is_function() {
                        let sentinel = func_val.as_function().unwrap();
                        if (-507..=-504).contains(&sentinel) {
                            let arg = if argc > 0 { self.stack[func_pos + 1] } else { Value::undefined() };
                            let wrapped = match sentinel {
                                -504 => { // String
                                    let s = self.value_to_string(arg);
                                    let id = self.interner.intern(&s);
                                    Value::string(id)
                                }
                                -505 => Value::number(self.to_f64(arg)), // Number
                                -506 => Value::boolean(arg.to_boolean()), // Boolean
                                _ => arg,
                            };
                            let mut obj = JsObject::ordinary();
                            obj.kind = ObjectKind::Wrapper(wrapped);
                            if sentinel == -504
                                && let Some(sid) = wrapped.as_string_id() {
                                    let len = self.interner.resolve(sid).chars().count() as i32;
                                    let len_key = self.interner.intern("length");
                                    obj.set_property(len_key, Value::int(len));
                                }
                            let oid = self.heap.allocate(obj);
                            self.stack.truncate(func_pos);
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
                        self.stack.truncate(func_pos);
                        self.push(Value::object_id(oid));
                        continue;
                    }

                    // Handle Object constructor: new Object()
                    if func_val.is_function() && func_val.as_function() == Some(-508) {
                        let obj = JsObject::ordinary();
                        let oid = self.heap.allocate(obj);
                        self.stack.truncate(func_pos);
                        self.push(Value::object_id(oid));
                        continue;
                    }

                    // Handle Error constructors
                    if func_val.is_function() {
                        let sentinel = func_val.as_function().unwrap();
                        if (-514..=-510).contains(&sentinel) {
                            let error_type = match sentinel {
                                -510 => "Error",
                                -511 => "TypeError",
                                -512 => "RangeError",
                                -513 => "ReferenceError",
                                -514 => "SyntaxError",
                                _ => "Error",
                            };
                            let msg = if argc > 0 {
                                self.value_to_string(self.stack[func_pos + 1])
                            } else {
                                String::new()
                            };
                            let mut err_obj = JsObject::ordinary();
                            let msg_key = self.interner.intern("message");
                            let msg_id = self.interner.intern(&msg);
                            err_obj.set_property(msg_key, Value::string(msg_id));
                            let name_key = self.interner.intern("name");
                            let name_id = self.interner.intern(error_type);
                            err_obj.set_property(name_key, Value::string(name_id));
                            let stack_key = self.interner.intern("stack");
                            let stack_str = format!("{error_type}: {msg}");
                            let stack_id = self.interner.intern(&stack_str);
                            err_obj.set_property(stack_key, Value::string(stack_id));
                            let oid = self.heap.allocate(err_obj);
                            self.stack.truncate(func_pos);
                            self.push(Value::object_id(oid));
                            continue;
                        }
                    }

                    // Create a new object for `this`, linked to F.prototype
                    let mut new_obj = JsObject::ordinary();
                    if func_val.is_function() {
                        let packed = func_val.as_function().unwrap();
                        // Get or create the prototype from the cache
                        if let Some(&proto_oid) = self.func_prototypes.get(&packed) {
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
                        let ctor_val = self.heap.get(class_oid)
                            .and_then(|o| o.get_property(ctor_key));
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

                        // Apply instance fields (stored as __ifield_{name}__ on the class)
                        let ifield_prefix = "__ifield_";
                        let instance_fields: Vec<(String, Value)> = self.heap.get(class_oid)
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
                            let real_key = self.interner.intern(&field_name);
                            if let Some(new_o) = self.heap.get_mut(new_oid) {
                                new_o.set_property(real_key, field_val);
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
                                        saved_args,
                                    });
                                    continue;
                                }
                            }
                        // No constructor -- just return the object with prototype methods
                        self.stack.truncate(func_pos);
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
                                saved_args,
                            });
                            continue;
                        }
                    }

                    self.stack.truncate(func_pos);
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
                    let result = self.call_function(func_val, &args)?;
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
                    self.stack.truncate(self.stack.len() - argc - 1); // pop everything we just pushed

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
                                _ => vec![],
                            },
                            None => vec![],
                        }
                    } else if source.is_string() {
                        let sid = source.as_string_id().unwrap();
                        let s = self.interner.resolve(sid).to_owned();
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
                        // Resolve the key to a StringId (coercing numbers/symbols if needed)
                        let name_id = if let Some(sid) = key.as_string_id() {
                            Some(sid)
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
                            None
                        };
                        if let Some(name_id) = name_id
                            && let Some(obj) = self.heap.get_mut(oid) {
                                obj.set_property(name_id, val);
                            }
                    }
                }

                OpCode::DefineGetter | OpCode::DefineSetter => {
                    let func = self.pop()?;
                    let key = self.pop()?;
                    let obj_val = self.peek()?;
                    if let Some(oid) = obj_val.as_object_id()
                        && let Some(name_id) = key.as_string_id()
                        && let Some(obj) = self.heap.get_mut(oid)
                    {
                        let name_str = self.interner.resolve(name_id).to_owned();
                        let accessor_key = if opcode == OpCode::DefineGetter {
                            self.interner.intern(&format!("__get_{name_str}__"))
                        } else {
                            self.interner.intern(&format!("__set_{name_str}__"))
                        };
                        obj.set_property(accessor_key, func);
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
                    let obj = JsObject::regexp(pattern, flags);
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
                        let index = self.read_byte() as usize;

                        if is_local {
                            // Capture from current frame's local stack slot
                            let base = self.frames.last().unwrap().base;
                            let stack_idx = base + index;
                            upvalues.push(Upvalue {
                                location: UpvalueLocation::Open(stack_idx),
                            });
                        } else {
                            // Capture from current frame's upvalue (transitive)
                            let parent_uv = self.frames.last().unwrap().upvalues.get(index).cloned();
                            if let Some(uv) = parent_uv {
                                upvalues.push(uv);
                            } else {
                                upvalues.push(Upvalue {
                                    location: UpvalueLocation::Closed(Value::undefined()),
                                });
                            }
                        }
                    }

                    // Store closure as chunk index (int), but also store upvalues
                    // We need a way to associate upvalues with the closure value.
                    // For now, use the closure_upvalues map.
                    let closure_id = self.closure_upvalues.len();
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
                    // The class itself is represented as an ordinary object with a __proto__ property
                    let mut class_obj = JsObject::ordinary();
                    let proto_key = self.interner.intern("prototype");
                    class_obj.set_property(proto_key, Value::object_id(proto_oid));
                    // Mark as class with default constructor (so typeof returns "function")
                    let ctor_key = self.interner.intern("__constructor__");
                    class_obj.set_property(ctor_key, Value::undefined());
                    // Set name: non-writable, non-enumerable, configurable
                    let name_key = self.interner.intern("name");
                    class_obj.define_property(name_key, Property::with_flags(Value::string(class_name_id), Property::CONFIGURABLE));
                    // Set length: 0 default (updated when constructor is found)
                    let length_key = self.interner.intern("length");
                    class_obj.define_property(length_key, Property::with_flags(Value::int(0), Property::CONFIGURABLE));
                    let class_oid = self.heap.allocate(class_obj);
                    // Set proto.constructor = class (non-enumerable, writable, configurable)
                    let constructor_key = self.interner.intern("constructor");
                    if let Some(proto) = self.heap.get_mut(proto_oid) {
                        proto.define_property(constructor_key, Property::with_flags(Value::object_id(class_oid), Property::WRITABLE | Property::CONFIGURABLE));
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
                                // Check if this is the constructor
                                let constructor_name = self.interner.intern("constructor");
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
                    let class_val = self.peek()?;
                    if let Some(class_oid) = class_val.as_object_id()
                        && let Some(class_obj) = self.heap.get_mut(class_oid) {
                            // Static methods: non-enumerable, writable, configurable
                            class_obj.define_property(name_id, Property::with_flags(
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
                            class_obj.set_property(name_id, field_val);
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
                        if let Some(proto_oid) = proto_oid
                            && let Some(proto) = self.heap.get_mut(proto_oid) {
                                proto.define_property(name_id, Property::with_flags(
                                    method_val, Property::WRITABLE | Property::CONFIGURABLE
                                ));
                            }
                    }
                }

                OpCode::Inherit => {
                    // Stack: [class, superclass] — superclass is on top
                    let super_val = self.pop()?;
                    let class_val = self.peek()?;

                    if let Some(super_oid) = super_val.as_object_id()
                        && let Some(class_oid) = class_val.as_object_id()
                    {
                        let proto_key = self.interner.intern("prototype");

                        // Get superclass's prototype
                        let super_proto = self.heap.get(super_oid)
                            .and_then(|o| o.get_property(proto_key))
                            .and_then(|v| v.as_object_id());

                        // Get subclass's prototype
                        let sub_proto = self.heap.get(class_oid)
                            .and_then(|o| o.get_property(proto_key))
                            .and_then(|v| v.as_object_id());

                        // Link: subclass.prototype.__proto__ = superclass.prototype
                        if let (Some(sub_pid), Some(super_pid)) = (sub_proto, super_proto)
                            && let Some(sub_proto_obj) = self.heap.get_mut(sub_pid)
                        {
                            sub_proto_obj.prototype = Some(super_pid);
                        }

                        // Store superclass reference for super() calls
                        let super_key = self.interner.intern("__super__");
                        if let Some(class_obj) = self.heap.get_mut(class_oid) {
                            class_obj.set_property(super_key, super_val);
                        }
                    }
                }

                OpCode::GetSuperClass => {
                    // Push parent class prototype: this.__class__.__super__.prototype
                    let this_val = self.frames.last().unwrap().this_value;
                    let class_key = self.interner.intern("__class__");
                    let super_key = self.interner.intern("__super__");
                    let proto_key = self.interner.intern("prototype");
                    let super_class = this_val.as_object_id()
                        .and_then(|oid| self.heap.get(oid))
                        .and_then(|obj| obj.get_property(class_key))
                        .and_then(|cv| cv.as_object_id())
                        .and_then(|cid| self.heap.get(cid))
                        .and_then(|cls| cls.get_property(super_key));
                    // Try super_class.prototype first, fall back to super_class itself
                    let result = if let Some(sv) = super_class {
                        if let Some(sid) = sv.as_object_id() {
                            self.heap.get(sid)
                                .and_then(|s| s.get_property(proto_key))
                                .or(Some(sv))
                        } else { Some(sv) }
                    } else { None };
                    self.push(result.unwrap_or(Value::undefined()));
                    self.frames.last_mut().unwrap().pending_super_call = true;
                }

                OpCode::GetSuperConstructor => {
                    // Resolve parent constructor: this.__class__.__super__.__constructor__
                    let this_val = self.frames.last().unwrap().this_value;
                    let class_key = self.interner.intern("__class__");
                    let super_key = self.interner.intern("__super__");
                    let ctor_key = self.interner.intern("__constructor__");

                    let result = this_val.as_object_id()
                        .and_then(|oid| self.heap.get(oid))
                        .and_then(|obj| obj.get_property(class_key))
                        .and_then(|cv| cv.as_object_id())
                        .and_then(|cid| self.heap.get(cid))
                        .and_then(|cls| cls.get_property(super_key))
                        .and_then(|sv| sv.as_object_id())
                        .and_then(|sid| self.heap.get(sid))
                        .and_then(|sup| sup.get_property(ctor_key));

                    self.push(result.unwrap_or(Value::undefined()));

                    // Mark that the next Call should propagate this_value
                    self.frames.last_mut().unwrap().pending_super_call = true;
                }

                OpCode::Throw => {
                    let val = self.pop()?;
                    // Look for an exception handler in any frame (unwinding as needed)
                    if let Some(handler) = self.exc_handlers.pop() {
                        // Unwind frames until we're at the handler's frame
                        while self.frames.len() > handler.frame_idx + 1 {
                            self.frames.pop();
                        }
                        self.stack.truncate(handler.stack_depth);
                        self.push(val);
                        let frame = self.frames.last_mut().unwrap();
                        frame.ip = handler.catch_target as usize;
                    } else {
                        let msg = self.value_to_string(val);
                        return Err(VmError::RuntimeError(msg));
                    }
                }

                OpCode::PushExcHandler => {
                    let catch_target = self.read_u16();
                    let finally_target = self.read_u16();
                    self.exc_handlers.push(ExcHandler {
                        catch_target,
                        finally_target,
                        stack_depth: self.stack.len(),
                        frame_idx: self.frames.len() - 1,
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
                                    // Object: walk prototype chain
                                    let mut all_keys = Vec::new();
                                    let mut seen = std::collections::HashSet::new();
                                    let mut cur = Some(oid);
                                    let mut depth = 0;
                                    while let Some(cid) = cur {
                                        if depth > 64 { break; }
                                        if let Some(obj) = self.heap.get(cid) {
                                            for &(k, ref p) in &obj.properties {
                                                if p.is_enumerable() && seen.insert(k) {
                                                    all_keys.push(k);
                                                }
                                            }
                                            cur = obj.prototype;
                                        } else { break; }
                                        depth += 1;
                                    }
                                    all_keys
                                }
                            })
                            .unwrap_or_default();
                        let iter_obj = JsObject {
                            properties: Vec::new(), prototype: None,
                            kind: ObjectKind::KeyIterator(keys, 0),
                            marked: false, extensible: true,
                        };
                        let iter_id = self.heap.allocate(iter_obj);
                        self.push(Value::object_id(iter_id));
                    } else {
                        // Primitive: empty iterator
                        let iter_obj = JsObject {
                            properties: Vec::new(), prototype: None,
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
                        // Generators are their own iterators
                        let is_generator = self.heap.get(oid)
                            .map(|o| matches!(&o.kind, ObjectKind::Generator { .. }))
                            .unwrap_or(false);
                        if is_generator {
                            self.push(val); // pass through as-is
                        } else if self.heap.get(oid)
                            .map(|o| matches!(&o.kind, ObjectKind::Array(_)))
                            .unwrap_or(false) {
                            // Array iterator
                            let iter_obj = JsObject {
                                properties: Vec::new(),
                                prototype: None,
                                kind: ObjectKind::ArrayIterator(oid, 0),
                                marked: false,
                                extensible: true,
                            };
                            let iter_id = self.heap.allocate(iter_obj);
                            self.push(Value::object_id(iter_id));
                        } else if let Some(kind_info) = self.heap.get(oid).and_then(|o| match &o.kind {
                            ObjectKind::Map { entries } => Some((true, entries.clone(), Vec::<Value>::new())),
                            ObjectKind::Set { entries } => Some((false, Vec::<(Value,Value)>::new(), entries.clone())),
                            _ => None,
                        }) {
                            // Build a temporary array to iterate
                            let elements: Vec<Value> = if kind_info.0 {
                                // Map: build [k,v] pair arrays
                                kind_info.1.into_iter().map(|(k, v)| {
                                    let pair_arr = JsObject::array(vec![k, v]);
                                    Value::object_id(self.heap.allocate(pair_arr))
                                }).collect()
                            } else {
                                // Set: just values
                                kind_info.2
                            };
                            let arr = JsObject::array(elements);
                            let arr_oid = self.heap.allocate(arr);
                            let iter_obj = JsObject {
                                properties: Vec::new(),
                                prototype: None,
                                kind: ObjectKind::ArrayIterator(arr_oid, 0),
                                marked: false,
                                extensible: true,
                            };
                            let iter_id = self.heap.allocate(iter_obj);
                            self.push(Value::object_id(iter_id));
                        } else {
                            // Object key iterator (for...in)
                            let keys: Vec<_> = self.heap.get(oid)
                                .map(|_| {
                                    // Walk prototype chain for for-in
                                    let mut all_keys = Vec::new();
                                    let mut seen = std::collections::HashSet::new();
                                    let mut cur = Some(oid);
                                    let mut depth = 0;
                                    while let Some(cid) = cur {
                                        if depth > 64 { break; }
                                        if let Some(obj) = self.heap.get(cid) {
                                            for &(k, ref p) in &obj.properties {
                                                if p.is_enumerable() && seen.insert(k) {
                                                    all_keys.push(k);
                                                }
                                            }
                                            cur = obj.prototype;
                                        } else { break; }
                                        depth += 1;
                                    }
                                    all_keys
                                })
                                .unwrap_or_default();
                            let iter_obj = JsObject {
                                properties: Vec::new(),
                                prototype: None,
                                kind: ObjectKind::KeyIterator(keys, 0),
                                marked: false,
                                extensible: true,
                            };
                            let iter_id = self.heap.allocate(iter_obj);
                            self.push(Value::object_id(iter_id));
                        }
                    } else if val.is_string() {
                        // String iterator: iterate over characters
                        let sid = val.as_string_id().unwrap();
                        let s = self.interner.resolve(sid).to_owned();
                        let chars: Vec<Value> = s.chars().map(|c| {
                            let id = self.interner.intern(&c.to_string());
                            Value::string(id)
                        }).collect();
                        let arr = JsObject::array(chars);
                        let arr_oid = self.heap.allocate(arr);
                        let iter_obj = JsObject {
                            properties: Vec::new(),
                            prototype: None,
                            kind: ObjectKind::ArrayIterator(arr_oid, 0),
                            marked: false,
                            extensible: true,
                        };
                        let iter_id = self.heap.allocate(iter_obj);
                        self.push(Value::object_id(iter_id));
                    } else {
                        return Err(VmError::TypeError("not iterable".into()));
                    }
                }

                OpCode::GetAsyncIterator => {
                    return Err(VmError::RuntimeError("async iterators not yet implemented".into()));
                }

                OpCode::IteratorNext => {
                    // Stack: [iterator] -> [iterator_result]
                    let iter_val = self.pop()?;
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
                            // Array iterator
                            let arr_oid = iter_info.0.unwrap();
                            let idx = iter_info.1;
                            let arr = self.heap.get(arr_oid);
                            if let Some(arr_obj) = arr {
                                if let ObjectKind::Array(ref elements) = arr_obj.kind {
                                    if idx < elements.len() {
                                        (elements[idx], false)
                                    } else {
                                        (Value::undefined(), true)
                                    }
                                } else {
                                    (Value::undefined(), true)
                                }
                            } else {
                                (Value::undefined(), true)
                            }
                        };
                        // Advance the iterator index
                        if let Some(iter) = self.heap.get_mut(iter_oid) {
                            let new_idx = iter_info.1 + 1;
                            match &mut iter.kind {
                                ObjectKind::ArrayIterator(_, i) => *i = new_idx,
                                ObjectKind::KeyIterator(_, i) => *i = new_idx,
                                _ => {}
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
                        return Err(VmError::TypeError("not an iterator".into()));
                    }
                }

                OpCode::IteratorDone => {
                    // Stack: [iter_result] -> [done_bool]
                    let result_val = self.pop()?;
                    if let Some(oid) = result_val.as_object_id() {
                        let done_name = self.interner.intern("done");
                        let done = self.heap.get(oid)
                            .and_then(|o| o.get_property(done_name))
                            .map(|v| v.to_boolean())
                            .unwrap_or(true);
                        self.push(Value::boolean(done));
                    } else {
                        self.push(Value::boolean(true));
                    }
                }

                OpCode::IteratorValue => {
                    // Stack: [iter_result] -> [value]
                    let result_val = self.pop()?;
                    if let Some(oid) = result_val.as_object_id() {
                        let value_name = self.interner.intern("value");
                        let val = self.heap.get(oid)
                            .and_then(|o| o.get_property(value_name))
                            .unwrap_or(Value::undefined());
                        self.push(val);
                    } else {
                        self.push(Value::undefined());
                    }
                }

                OpCode::IteratorClose => {
                    self.pop()?; // just discard the iterator
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
                    let _chunk_idx = frame.chunk_idx;
                    let this_value = frame.this_value;

                    if let Some(gid) = gen_oid {
                        // Save the current stack (locals + operand stack)
                        let saved_stack: Vec<Value> = self.stack[base..].to_vec();

                        // Resolve upvalues to values
                        let saved_upvalues: Vec<Value> = self.frames.last().unwrap().upvalues.iter().map(|uv| {
                            match &uv.location {
                                UpvalueLocation::Open(idx) => self.stack.get(*idx).copied().unwrap_or(Value::undefined()),
                                UpvalueLocation::Closed(v) => *v,
                            }
                        }).collect();

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
                        self.stack.truncate(base - 1); // remove placeholder too

                        // Push {value, done: false}
                        let result = self.make_iter_result(yielded_value, false)?;
                        self.push(result);
                    } else {
                        return Err(VmError::RuntimeError("yield outside generator".into()));
                    }
                }

                OpCode::YieldStar
                | OpCode::CreateGenerator
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
                    let source = self.pop()?;
                    let mut rest = JsObject::ordinary();
                    if let Some(src_oid) = source.as_object_id() {
                        let props: Vec<(StringId, Value)> = self.heap.get(src_oid)
                            .map(|o| o.properties.iter()
                                .filter(|(k, p)| p.is_enumerable() && !excluded.contains(k))
                                .map(|&(k, ref p)| (k, p.value))
                                .collect())
                            .unwrap_or_default();
                        for (k, v) in props {
                            rest.set_property(k, v);
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

                        // Flatten child chunks and add to VM
                        let base_idx = self.chunks.len();
                        let mut flat_chunks = Vec::new();
                        Vm::flatten_chunk(chunk, &mut flat_chunks);
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

                OpCode::ImportDynamic | OpCode::ExportDefault => {
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

                OpCode::NewTarget | OpCode::ImportMeta => {
                    return Err(VmError::RuntimeError(format!(
                        "{opcode:?} not yet implemented"
                    )));
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
                    self.stack.truncate(tag_pos);
                    let result = self.call_function(tag, &args)?;
                    self.push(result);
                }
                OpCode::CreateRestParam => {
                    let _ = self.read_byte();
                    return Err(VmError::RuntimeError("CreateRestParam not yet implemented".into()));
                }

                OpCode::ToPropertyKey => {
                    return Err(VmError::RuntimeError(
                        "ToPropertyKey not yet implemented".into(),
                    ));
                }

                OpCode::SetFunctionName => {
                    let _ = self.read_u16();
                    return Err(VmError::RuntimeError(
                        "SetFunctionName not yet implemented".into(),
                    ));
                }

                OpCode::WithEnter | OpCode::WithExit => {
                    return Err(VmError::RuntimeError(format!(
                        "{opcode:?} not yet implemented"
                    )));
                }
            }
        }
    }
}

impl Vm {
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
        let src = format!("(function({}){{ {} }})", params_str, body_str);

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
        self.chunks.extend(flat_chunks);
        // Run the outer wrapper to evaluate the function expression
        let wrapper_fn = Value::function(base_idx as i32);
        let result = self.call_function(wrapper_fn, &[])?;
        Ok(result)
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
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
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
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
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
        emit_const_number(&mut chunk, 3.14);
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
