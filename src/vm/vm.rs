use std::collections::HashMap;
use std::fmt;

use crate::compiler::chunk::Chunk;
use crate::compiler::opcode::OpCode;
use crate::compiler::chunk::ChunkFlags;
use crate::runtime::object::{JsObject, ObjectHeap, ObjectId, ObjectKind, PromiseReaction, PromiseState};
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
enum UpvalueLocation {
    /// Points to a stack slot (variable still on stack).
    Open(usize),
    /// Value has been closed over (moved to heap when enclosing function returned).
    Closed(Value),
}

#[derive(Clone)]
struct Upvalue {
    location: UpvalueLocation,
}

struct CallFrame {
    chunk_idx: usize,
    ip: usize,
    base: usize,
    upvalues: Vec<Upvalue>,
    /// The `this` value for this call.
    this_value: Value,
    /// If true, ReturnUndefined returns this_value instead.
    is_constructor: bool,
}

/// An active exception handler (pushed by PushExcHandler).
#[allow(dead_code)]
struct ExcHandler {
    catch_target: u16,
    finally_target: u16,
    stack_depth: usize,
    frame_idx: usize,
}

#[derive(Clone)]
enum Microtask {
    PromiseReaction {
        callback: Option<Value>,
        value: Value,
        result_promise: ObjectId,
        is_fulfilled: bool,
    },
}

/// Inline cache entry for GetGlobal: (name_id, cached_value).
/// Keyed by (chunk_idx, bytecode_offset).
type GlobalIC = HashMap<(usize, usize), (StringId, Value)>;

pub struct Vm {
    chunks: Vec<Chunk>,
    frames: Vec<CallFrame>,
    stack: Vec<Value>,
    globals: HashMap<StringId, Value>,
    /// Fast global lookup by StringId index (parallel to HashMap for hot path).
    globals_vec: Vec<Value>,
    interner: Interner,
    heap: ObjectHeap,
    #[allow(dead_code)]
    global_ic: GlobalIC,
    #[allow(dead_code)]
    global_version: u64,
    #[allow(dead_code)]
    global_ic_version: HashMap<(usize, usize), u64>,
    exc_handlers: Vec<ExcHandler>,
    microtask_queue: Vec<Microtask>,
    /// Upvalues for each closure, indexed by closure_id.
    closure_upvalues: Vec<Vec<Upvalue>>,
    /// console.log output buffer (for testing)
    pub output: Vec<String>,
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

        // Create console object with log/warn/error methods
        let mut console_obj = JsObject::ordinary();
        let log_id = interner.intern("log");
        console_obj.set_property(log_id, Value::int(-100)); // sentinel for console.log
        let warn_id = interner.intern("warn");
        console_obj.set_property(warn_id, Value::int(-101)); // sentinel for console.warn
        let error_id = interner.intern("error");
        console_obj.set_property(error_id, Value::int(-102)); // sentinel for console.error
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
        globals.insert(parse_int_name, Value::int(-500));
        let parse_float_name = interner.intern("parseFloat");
        globals.insert(parse_float_name, Value::int(-501));
        let is_nan_name = interner.intern("isNaN");
        globals.insert(is_nan_name, Value::int(-502));
        let is_finite_name = interner.intern("isFinite");
        globals.insert(is_finite_name, Value::int(-503));
        let str_name = interner.intern("String");
        globals.insert(str_name, Value::int(-504));
        let num_name = interner.intern("Number");
        globals.insert(num_name, Value::int(-505));
        let bool_name = interner.intern("Boolean");
        globals.insert(bool_name, Value::int(-506));
        let arr_is_arr = interner.intern("Array");
        globals.insert(arr_is_arr, Value::int(-507));
        let object_name = interner.intern("Object");
        globals.insert(object_name, Value::int(-508));

        // Promise constructor
        let promise_name = interner.intern("Promise");
        globals.insert(promise_name, Value::int(-520));

        // Error constructors
        let error_name = interner.intern("Error");
        globals.insert(error_name, Value::int(-510));
        let type_error_name = interner.intern("TypeError");
        globals.insert(type_error_name, Value::int(-511));
        let range_error_name = interner.intern("RangeError");
        globals.insert(range_error_name, Value::int(-512));
        let ref_error_name = interner.intern("ReferenceError");
        globals.insert(ref_error_name, Value::int(-513));
        let syntax_error_name = interner.intern("SyntaxError");
        globals.insert(syntax_error_name, Value::int(-514));

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
            frames: vec![CallFrame { chunk_idx: 0, ip: 0, base: 0, upvalues: Vec::new(), this_value: Value::undefined(), is_constructor: false }],
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
            output: Vec::new(),
        }
    }

    fn flatten_chunk(mut chunk: Chunk, out: &mut Vec<Chunk>) {
        let children = std::mem::take(&mut chunk.child_chunks);
        out.push(chunk);
        for child in children {
            Self::flatten_chunk(child, out);
        }
    }

    /// Close all open upvalues that point to stack slots >= `from`.
    fn close_upvalues_above(&mut self, from: usize) {
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

    // ---- String method dispatch ----
    fn exec_string_method(&mut self, s: &str, method_name: StringId, args: &[Value]) -> Value {
        let name = self.interner.resolve(method_name).to_owned();
        match name.as_str() {
            "charAt" => {
                let idx = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                let ch = s.chars().nth(idx).map(|c| c.to_string()).unwrap_or_default();
                let id = self.interner.intern(&ch);
                Value::string(id)
            }
            "charCodeAt" => {
                let idx = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                let code = s.chars().nth(idx).map(|c| c as u32 as f64).unwrap_or(f64::NAN);
                Value::number(code)
            }
            "indexOf" => {
                let search = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                let pos = s.find(&search).map(|i| i as i32).unwrap_or(-1);
                Value::int(pos)
            }
            "lastIndexOf" => {
                let search = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                let pos = s.rfind(&search).map(|i| i as i32).unwrap_or(-1);
                Value::int(pos)
            }
            "includes" => {
                let search = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                Value::boolean(s.contains(&search))
            }
            "startsWith" => {
                let search = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                Value::boolean(s.starts_with(&search))
            }
            "endsWith" => {
                let search = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                Value::boolean(s.ends_with(&search))
            }
            "slice" => {
                let len = s.len() as i32;
                let start = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
                let end = args.get(1).and_then(|v| v.as_number()).map(|n| n as i32).unwrap_or(len);
                let start = if start < 0 { (len + start).max(0) as usize } else { start.min(len) as usize };
                let end = if end < 0 { (len + end).max(0) as usize } else { end.min(len) as usize };
                let result = if start <= end { &s[start..end] } else { "" };
                let id = self.interner.intern(result);
                Value::string(id)
            }
            "substring" => {
                let len = s.len() as i32;
                let mut start = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
                let mut end = args.get(1).and_then(|v| v.as_number()).map(|n| n as i32).unwrap_or(len);
                start = start.max(0).min(len);
                end = end.max(0).min(len);
                if start > end { std::mem::swap(&mut start, &mut end); }
                let result = &s[start as usize..end as usize];
                let id = self.interner.intern(result);
                Value::string(id)
            }
            "toUpperCase" => {
                let result = s.to_uppercase();
                let id = self.interner.intern(&result);
                Value::string(id)
            }
            "toLowerCase" => {
                let result = s.to_lowercase();
                let id = self.interner.intern(&result);
                Value::string(id)
            }
            "trim" => {
                let id = self.interner.intern(s.trim());
                Value::string(id)
            }
            "trimStart" => {
                let id = self.interner.intern(s.trim_start());
                Value::string(id)
            }
            "trimEnd" => {
                let id = self.interner.intern(s.trim_end());
                Value::string(id)
            }
            "split" => {
                let sep = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                let parts: Vec<Value> = s.split(&sep).map(|part| {
                    let id = self.interner.intern(part);
                    Value::string(id)
                }).collect();
                let arr = JsObject::array(parts);
                let oid = self.heap.allocate(arr);
                Value::object_id(oid)
            }
            "replace" => {
                let search = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                let replacement = args.get(1).map(|v| self.value_to_string(*v)).unwrap_or_default();
                let result = s.replacen(&search, &replacement, 1);
                let id = self.interner.intern(&result);
                Value::string(id)
            }
            "repeat" => {
                let count = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                let result = s.repeat(count);
                let id = self.interner.intern(&result);
                Value::string(id)
            }
            "padStart" => {
                let target_len = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                let pad = args.get(1).map(|v| self.value_to_string(*v)).unwrap_or_else(|| " ".into());
                let mut result = s.to_string();
                while result.len() < target_len {
                    result.insert_str(0, &pad);
                }
                if result.len() > target_len { result.truncate(target_len); }
                let id = self.interner.intern(&result);
                Value::string(id)
            }
            "padEnd" => {
                let target_len = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                let pad = args.get(1).map(|v| self.value_to_string(*v)).unwrap_or_else(|| " ".into());
                let mut result = s.to_string();
                while result.len() < target_len {
                    result.push_str(&pad);
                }
                if result.len() > target_len { result.truncate(target_len); }
                let id = self.interner.intern(&result);
                Value::string(id)
            }
            "concat" => {
                let mut result = s.to_string();
                for arg in args {
                    result.push_str(&self.value_to_string(*arg));
                }
                let id = self.interner.intern(&result);
                Value::string(id)
            }
            _ => Value::undefined(),
        }
    }

    /// Call a closure value with the given arguments and run it to completion.
    /// Saves/restores the main run loop's frame depth so the callback executes
    /// as a nested call and returns its result.
    fn call_function(&mut self, func_val: Value, args: &[Value]) -> Result<Value, VmError> {
        if !func_val.is_int() {
            return Ok(Value::undefined());
        }
        let packed = func_val.as_int().unwrap();
        let closure_id = ((packed as u32) >> 16) as usize;
        let chunk_idx = (packed & 0xFFFF) as usize;
        if chunk_idx < 1 || chunk_idx >= self.chunks.len() {
            return Ok(Value::undefined());
        }

        let func_pos = self.stack.len();
        self.push(func_val);
        for arg in args {
            self.push(*arg);
        }
        let expected = self.chunks[chunk_idx].param_count as usize;
        let mut argc = args.len();
        while argc < expected {
            self.push(Value::undefined());
            argc += 1;
        }

        let upvalues = if closure_id < self.closure_upvalues.len() {
            self.closure_upvalues[closure_id].clone()
        } else {
            Vec::new()
        };

        self.frames.push(CallFrame {
            chunk_idx, ip: 0, base: func_pos + 1,
            upvalues, this_value: Value::undefined(), is_constructor: false,
        });

        // Run the callback by executing bytecode until its frame is popped.
        let target_frames = self.frames.len();
        loop {
            if self.frames.len() < target_frames {
                // Callback returned — result is on the stack
                return self.pop().or(Ok(Value::undefined()));
            }
            let ci = self.cur_chunk();
            let ip = self.cur_ip();
            if ip >= self.chunks[ci].code.len() {
                let frame = self.frames.pop().unwrap();
                self.stack.truncate(frame.base.saturating_sub(1));
                self.push(Value::undefined());
                return self.pop().or(Ok(Value::undefined()));
            }

            let byte = self.read_byte();
            let opcode = match OpCode::from_byte(byte) {
                Some(op) => op,
                None => return Err(VmError::RuntimeError(format!("invalid opcode: {byte:#04x}"))),
            };

            // Handle Return/ReturnUndefined specially to exit the callback
            match opcode {
                OpCode::Return => {
                    let result = self.pop()?;
                    let frame = self.frames.pop().unwrap();
                    self.close_upvalues_above(frame.base.saturating_sub(1));
                    self.stack.truncate(frame.base.saturating_sub(1));
                    return Ok(result);
                }
                OpCode::ReturnUndefined => {
                    let frame = self.frames.pop().unwrap();
                    self.close_upvalues_above(frame.base.saturating_sub(1));
                    self.stack.truncate(frame.base.saturating_sub(1));
                    return Ok(if frame.is_constructor { frame.this_value } else { Value::undefined() });
                }
                OpCode::Halt => {
                    return Ok(if self.stack.is_empty() { Value::undefined() } else { self.pop()? });
                }
                // For all other opcodes, we need the main dispatch.
                // Since we can't call run() recursively, handle the critical subset:
                OpCode::Const => { let i = self.read_u16() as usize; let v = self.chunks[self.cur_chunk()].constants[i]; self.push(v); }
                OpCode::Undefined => self.push(Value::undefined()),
                OpCode::Null => self.push(Value::null()),
                OpCode::True => self.push(Value::boolean(true)),
                OpCode::False => self.push(Value::boolean(false)),
                OpCode::Zero => self.push(Value::int(0)),
                OpCode::One => self.push(Value::int(1)),
                OpCode::Pop => { self.pop()?; }
                OpCode::Dup => { let v = self.peek()?; self.push(v); }
                OpCode::Add => {
                    let b = self.pop()?; let a = self.pop()?;
                    if a.is_string() || b.is_string() {
                        let sa = self.value_to_string(a); let sb = self.value_to_string(b);
                        let r = format!("{sa}{sb}"); let id = self.interner.intern(&r);
                        self.push(Value::string(id));
                    } else {
                        let na = a.as_number().unwrap_or(0.0); let nb = b.as_number().unwrap_or(0.0);
                        self.push_number(na + nb);
                    }
                }
                OpCode::Sub => { let (a,b) = self.pop_numbers()?; self.push_number(a-b); }
                OpCode::Mul => { let (a,b) = self.pop_numbers()?; self.push_number(a*b); }
                OpCode::Div => { let (a,b) = self.pop_numbers()?; self.push_number(a/b); }
                OpCode::Rem => { let (a,b) = self.pop_numbers()?; self.push_number(a%b); }
                OpCode::Exp => { let (a,b) = self.pop_numbers()?; self.push_number(a.powf(b)); }
                OpCode::Neg => { let v = self.pop()?; self.push_number(-self.to_f64(v)); }
                OpCode::Pos => { let v = self.pop()?; self.push_number(self.to_f64(v)); }
                OpCode::Not => { let v = self.pop()?; self.push(Value::boolean(!v.to_boolean())); }
                OpCode::Void => { self.pop()?; self.push(Value::undefined()); }
                OpCode::TypeOf => {
                    let v = self.pop()?;
                    let t = self.type_of_value(v);
                    let id = self.interner.intern(t);
                    self.push(Value::string(id));
                }
                OpCode::Swap => {
                    let len = self.stack.len();
                    if len >= 2 { self.stack.swap(len - 1, len - 2); }
                }
                OpCode::Nop => {}
                OpCode::Eq => { let b = self.pop()?; let a = self.pop()?; self.push(Value::boolean(self.abstract_eq(a,b))); }
                OpCode::StrictEq => { let b = self.pop()?; let a = self.pop()?; self.push(Value::boolean(self.strict_eq(a,b))); }
                OpCode::Lt => { let (a,b) = self.pop_numbers()?; self.push(Value::boolean(a<b)); }
                OpCode::Le => { let (a,b) = self.pop_numbers()?; self.push(Value::boolean(a<=b)); }
                OpCode::Gt => { let (a,b) = self.pop_numbers()?; self.push(Value::boolean(a>b)); }
                OpCode::Ge => { let (a,b) = self.pop_numbers()?; self.push(Value::boolean(a>=b)); }
                OpCode::GetLocal => { let s = self.read_byte() as usize; let b = self.frames.last().unwrap().base; self.push(self.stack[b+s]); }
                OpCode::SetLocal => { let s = self.read_byte() as usize; let v = self.peek()?; let b = self.frames.last().unwrap().base; self.stack[b+s] = v; }
                OpCode::GetGlobal => {
                    let ni = self.read_u16() as usize;
                    let nv = self.chunks[self.cur_chunk()].constants[ni];
                    let nid = nv.as_string_id().unwrap();
                    let ns = self.interner.resolve(nid);
                    if ns == "__this__" { self.push(self.frames.last().unwrap().this_value); }
                    else { let v = self.globals.get(&nid).copied().unwrap_or(Value::undefined()); self.push(v); }
                }
                OpCode::GetUpvalue => {
                    let idx = self.read_byte() as usize;
                    let frame = self.frames.last().unwrap();
                    let v = if idx < frame.upvalues.len() {
                        match &frame.upvalues[idx].location { UpvalueLocation::Open(si) => self.stack[*si], UpvalueLocation::Closed(v) => *v }
                    } else { Value::undefined() };
                    self.push(v);
                }
                OpCode::Jump => { let off = self.read_i16(); self.frames.last_mut().unwrap().ip = (self.frames.last().unwrap().ip as isize + off as isize) as usize; }
                OpCode::JumpIfFalse => { let off = self.read_i16(); let v = self.pop()?; if !v.to_boolean() { self.frames.last_mut().unwrap().ip = (self.frames.last().unwrap().ip as isize + off as isize) as usize; } }
                OpCode::JumpIfFalsePeek => { let off = self.read_i16(); let v = self.peek()?; if !v.to_boolean() { self.frames.last_mut().unwrap().ip = (self.frames.last().unwrap().ip as isize + off as isize) as usize; } }
                OpCode::JumpIfTruePeek => { let off = self.read_i16(); let v = self.peek()?; if v.to_boolean() { self.frames.last_mut().unwrap().ip = (self.frames.last().unwrap().ip as isize + off as isize) as usize; } }
                OpCode::JumpIfNullishPeek => { let off = self.read_i16(); let v = self.peek()?; if v.is_nullish() { self.frames.last_mut().unwrap().ip = (self.frames.last().unwrap().ip as isize + off as isize) as usize; } }
                OpCode::JumpIfTrue => { let off = self.read_i16(); let v = self.pop()?; if v.to_boolean() { self.frames.last_mut().unwrap().ip = (self.frames.last().unwrap().ip as isize + off as isize) as usize; } }
                OpCode::Loop => { let off = self.read_u16() as usize; self.frames.last_mut().unwrap().ip -= off; }
                OpCode::GetProperty => {
                    let name_idx = self.read_u16() as usize;
                    let name_val = self.chunks[self.cur_chunk()].constants[name_idx];
                    let name_id = name_val.as_string_id().unwrap();
                    let obj_val = self.pop()?;
                    if let Some(oid) = obj_val.as_object_id() {
                        let val = self.heap.get(oid).and_then(|o| o.get_property(name_id)).unwrap_or(Value::undefined());
                        self.push(val);
                    } else if obj_val.is_string() && self.interner.resolve(name_id) == "length" {
                        let sid = obj_val.as_string_id().unwrap();
                        self.push(Value::int(self.interner.resolve(sid).chars().count() as i32));
                    } else {
                        self.push(Value::undefined());
                    }
                }
                OpCode::SetProperty => {
                    let name_idx = self.read_u16() as usize;
                    let name_val = self.chunks[self.cur_chunk()].constants[name_idx];
                    let name_id = name_val.as_string_id().unwrap();
                    let val = self.pop()?;
                    let obj_val = self.pop()?;
                    if let Some(oid) = obj_val.as_object_id()
                        && let Some(obj) = self.heap.get_mut(oid) { obj.set_property(name_id, val); }
                    self.push(val);
                }
                OpCode::DefineGlobal => {
                    let ni = self.read_u16() as usize;
                    let nv = self.chunks[self.cur_chunk()].constants[ni];
                    let nid = nv.as_string_id().unwrap();
                    let val = self.pop()?;
                    self.globals.insert(nid, val);
                    let idx = nid.0 as usize;
                    if idx >= self.globals_vec.len() { self.globals_vec.resize(idx + 1, Value::null()); }
                    self.globals_vec[idx] = val;
                }
                OpCode::Closure => {
                    let child_rel_idx = self.read_u16() as usize;
                    let current = self.cur_chunk();
                    let abs_idx = current + 1 + child_rel_idx;
                    let upvalue_count = if abs_idx < self.chunks.len() { self.chunks[abs_idx].upvalue_count as usize } else { 0 };
                    let mut upvalues = Vec::with_capacity(upvalue_count);
                    for _ in 0..upvalue_count {
                        let is_local = self.read_byte() != 0;
                        let index = self.read_byte() as usize;
                        if is_local {
                            let base = self.frames.last().unwrap().base;
                            upvalues.push(Upvalue { location: UpvalueLocation::Open(base + index) });
                        } else {
                            let parent_uv = self.frames.last().unwrap().upvalues.get(index).cloned();
                            upvalues.push(parent_uv.unwrap_or(Upvalue { location: UpvalueLocation::Closed(Value::undefined()) }));
                        }
                    }
                    let closure_id = self.closure_upvalues.len();
                    self.closure_upvalues.push(upvalues);
                    let packed = ((closure_id as i32) << 16) | (abs_idx as i32 & 0xFFFF);
                    self.push(Value::int(packed));
                }
                OpCode::Await => {
                    let awaited = self.pop()?;
                    if let Some(oid) = awaited.as_object_id()
                        && let Some(obj) = self.heap.get(oid)
                            && let ObjectKind::Promise { state, result, .. } = &obj.kind {
                                match state {
                                    PromiseState::Fulfilled => { self.push(*result); continue; }
                                    _ => { self.push(Value::undefined()); continue; }
                                }
                            }
                    self.push(awaited);
                }
                OpCode::Call => {
                    // Support nested calls inside callbacks (e.g. resolve())
                    let cb_argc = self.read_byte() as usize;
                    let cb_func_pos = self.stack.len() - 1 - cb_argc;
                    let cb_func = self.stack[cb_func_pos];
                    // Resolve/reject sentinels
                    if cb_func.is_int() {
                        let s = cb_func.as_int().unwrap();
                        if s <= -600_000 && s > -700_000 {
                            let pid = ObjectId((-600_000 - s) as u32);
                            let val = if cb_argc > 0 { self.stack[cb_func_pos + 1] } else { Value::undefined() };
                            self.stack.truncate(cb_func_pos);
                            self.resolve_promise(pid, val)?;
                            self.push(Value::undefined());
                            continue;
                        }
                        if s <= -700_000 && s > -800_000 {
                            let pid = ObjectId((-700_000 - s) as u32);
                            let val = if cb_argc > 0 { self.stack[cb_func_pos + 1] } else { Value::undefined() };
                            self.stack.truncate(cb_func_pos);
                            self.reject_promise(pid, val)?;
                            self.push(Value::undefined());
                            continue;
                        }
                    }
                    // Other calls: truncate and push undefined
                    self.stack.truncate(cb_func_pos);
                    self.push(Value::undefined());
                }
                OpCode::CallMethod => {
                    let cb_argc = self.read_byte() as usize;
                    let _method_idx = self.read_u16();
                    let method_name_val = self.chunks[self.cur_chunk()].constants[_method_idx as usize];
                    let method_name_id = method_name_val.as_string_id();
                    let obj_pos = self.stack.len() - 1 - cb_argc;
                    let obj_val = self.stack[obj_pos];
                    // Console.log support inside callbacks
                    if let Some(oid) = obj_val.as_object_id()
                        && let Some(mid) = method_name_id {
                            let log_key = self.interner.intern("log");
                            let warn_key = self.interner.intern("warn");
                            let error_key = self.interner.intern("error");
                            if (mid == log_key || mid == warn_key || mid == error_key)
                                && let Some(obj) = self.heap.get(oid)
                                    && let Some(mv) = obj.get_property(mid)
                                        && mv.is_int() && mv.as_int().unwrap() <= -100 && mv.as_int().unwrap() >= -102 {
                                            let mut parts = Vec::new();
                                            for i in 0..cb_argc {
                                                parts.push(self.value_to_string(self.stack[obj_pos + 1 + i]));
                                            }
                                            let line = parts.join(" ");
                                            println!("{line}");
                                            self.output.push(line);
                                            self.stack.truncate(obj_pos);
                                            self.push(Value::undefined());
                                            continue;
                                        }
                        }
                    // Promise static methods (Promise.resolve/reject) inside callbacks
                    if obj_val.is_int() && obj_val.as_int() == Some(-520)
                        && let Some(mid) = method_name_id {
                            let args: Vec<Value> = (0..cb_argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                            let result = self.exec_promise_static(mid, &args)?;
                            self.stack.truncate(obj_pos);
                            self.push(result);
                            continue;
                        }
                    // Promise instance methods (.then/.catch)
                    if let Some(oid) = obj_val.as_object_id() {
                        let is_promise = self.heap.get(oid).map(|o| matches!(&o.kind, ObjectKind::Promise { .. })).unwrap_or(false);
                        if is_promise
                            && let Some(mid) = method_name_id {
                                let args: Vec<Value> = (0..cb_argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                                let result = self.exec_promise_method(oid, mid, &args)?;
                                self.stack.truncate(obj_pos);
                                self.push(result);
                                continue;
                            }
                    }
                    self.stack.truncate(obj_pos);
                    self.push(Value::undefined());
                }
                OpCode::SetGlobal => {
                    let ni = self.read_u16() as usize;
                    let nv = self.chunks[self.cur_chunk()].constants[ni];
                    let nid = nv.as_string_id().unwrap();
                    let val = self.peek()?;
                    self.globals.insert(nid, val);
                }
                OpCode::Ne => { let b = self.pop()?; let a = self.pop()?; self.push(Value::boolean(!self.abstract_eq(a,b))); }
                OpCode::StrictNe => { let b = self.pop()?; let a = self.pop()?; self.push(Value::boolean(!self.strict_eq(a,b))); }
                OpCode::Inc => { let v = self.pop()?; self.push_number(v.as_number().unwrap_or(0.0) + 1.0); }
                OpCode::Dec => { let v = self.pop()?; self.push_number(v.as_number().unwrap_or(0.0) - 1.0); }
                _ => {
                    return Err(VmError::RuntimeError(format!("opcode {opcode:?} not supported in callback")));
                }
            }
        }
    }

    // ---- Array method dispatch ----
    fn exec_array_method(&mut self, oid: crate::runtime::object::ObjectId, method_name: StringId, args: &[Value]) -> Result<Value, VmError> {
        let name = self.interner.resolve(method_name).to_owned();
        match name.as_str() {
            "push" => {
                if let Some(obj) = self.heap.get_mut(oid)
                    && let ObjectKind::Array(ref mut elements) = obj.kind {
                        for arg in args {
                            elements.push(*arg);
                        }
                        return Ok(Value::int(elements.len() as i32));
                    }
                Ok(Value::undefined())
            }
            "pop" => {
                if let Some(obj) = self.heap.get_mut(oid)
                    && let ObjectKind::Array(ref mut elements) = obj.kind {
                        return Ok(elements.pop().unwrap_or(Value::undefined()));
                    }
                Ok(Value::undefined())
            }
            "join" => {
                let sep = args.first().map(|v| self.value_to_string(*v)).unwrap_or_else(|| ",".into());
                if let Some(obj) = self.heap.get(oid)
                    && let ObjectKind::Array(ref elements) = obj.kind {
                        let parts: Vec<String> = elements.iter().map(|v| self.value_to_string(*v)).collect();
                        let result = parts.join(&sep);
                        let id = self.interner.intern(&result);
                        return Ok(Value::string(id));
                    }
                Ok(Value::undefined())
            }
            "indexOf" => {
                let search = args.first().copied().unwrap_or(Value::undefined());
                if let Some(obj) = self.heap.get(oid)
                    && let ObjectKind::Array(ref elements) = obj.kind {
                        for (i, elem) in elements.iter().enumerate() {
                            if self.strict_eq(*elem, search) {
                                return Ok(Value::int(i as i32));
                            }
                        }
                    }
                Ok(Value::int(-1))
            }
            "includes" => {
                let search = args.first().copied().unwrap_or(Value::undefined());
                if let Some(obj) = self.heap.get(oid)
                    && let ObjectKind::Array(ref elements) = obj.kind {
                        for elem in elements {
                            if self.strict_eq(*elem, search) {
                                return Ok(Value::boolean(true));
                            }
                        }
                    }
                Ok(Value::boolean(false))
            }
            "reverse" => {
                if let Some(obj) = self.heap.get_mut(oid)
                    && let ObjectKind::Array(ref mut elements) = obj.kind {
                        elements.reverse();
                    }
                Ok(Value::object_id(oid))
            }
            "shift" => {
                if let Some(obj) = self.heap.get_mut(oid)
                    && let ObjectKind::Array(ref mut elements) = obj.kind
                        && !elements.is_empty() {
                            return Ok(elements.remove(0));
                        }
                Ok(Value::undefined())
            }
            "unshift" => {
                if let Some(obj) = self.heap.get_mut(oid)
                    && let ObjectKind::Array(ref mut elements) = obj.kind {
                        for (i, arg) in args.iter().enumerate() {
                            elements.insert(i, *arg);
                        }
                        return Ok(Value::int(elements.len() as i32));
                    }
                Ok(Value::undefined())
            }
            "map" => {
                let callback = args.first().copied().unwrap_or(Value::undefined());
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                let mut results = Vec::with_capacity(elements.len());
                for (i, elem) in elements.iter().enumerate() {
                    let result = self.call_function(callback, &[*elem, Value::int(i as i32)])?;
                    results.push(result);
                }
                let arr = JsObject::array(results);
                let new_oid = self.heap.allocate(arr);
                Ok(Value::object_id(new_oid))
            }
            "filter" => {
                let callback = args.first().copied().unwrap_or(Value::undefined());
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                let mut results = Vec::new();
                for (i, elem) in elements.iter().enumerate() {
                    let result = self.call_function(callback, &[*elem, Value::int(i as i32)])?;
                    if result.to_boolean() {
                        results.push(*elem);
                    }
                }
                let arr = JsObject::array(results);
                let new_oid = self.heap.allocate(arr);
                Ok(Value::object_id(new_oid))
            }
            "reduce" => {
                let callback = args.first().copied().unwrap_or(Value::undefined());
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                let mut acc = if args.len() > 1 { args[1] } else if !elements.is_empty() { elements[0] } else { Value::undefined() };
                let start = if args.len() > 1 { 0 } else { 1 };
                for (i, elem) in elements.iter().enumerate().skip(start) {
                    acc = self.call_function(callback, &[acc, *elem, Value::int(i as i32)])?;
                }
                Ok(acc)
            }
            "forEach" => {
                let callback = args.first().copied().unwrap_or(Value::undefined());
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                for (i, elem) in elements.iter().enumerate() {
                    self.call_function(callback, &[*elem, Value::int(i as i32)])?;
                }
                Ok(Value::undefined())
            }
            "find" => {
                let callback = args.first().copied().unwrap_or(Value::undefined());
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                for (i, elem) in elements.iter().enumerate() {
                    let result = self.call_function(callback, &[*elem, Value::int(i as i32)])?;
                    if result.to_boolean() { return Ok(*elem); }
                }
                Ok(Value::undefined())
            }
            "some" => {
                let callback = args.first().copied().unwrap_or(Value::undefined());
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                for (i, elem) in elements.iter().enumerate() {
                    let result = self.call_function(callback, &[*elem, Value::int(i as i32)])?;
                    if result.to_boolean() { return Ok(Value::boolean(true)); }
                }
                Ok(Value::boolean(false))
            }
            "every" => {
                let callback = args.first().copied().unwrap_or(Value::undefined());
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                for (i, elem) in elements.iter().enumerate() {
                    let result = self.call_function(callback, &[*elem, Value::int(i as i32)])?;
                    if !result.to_boolean() { return Ok(Value::boolean(false)); }
                }
                Ok(Value::boolean(true))
            }
            _ => Ok(Value::undefined()),
        }
    }

    // ---- Math method dispatch ----
    fn exec_math_method(&mut self, method_name: StringId, args: &[Value]) -> Value {
        let name = self.interner.resolve(method_name).to_owned();
        let a = || args.first().and_then(|v| v.as_number()).unwrap_or(f64::NAN);
        let b = || args.get(1).and_then(|v| v.as_number()).unwrap_or(f64::NAN);

        let result = match name.as_str() {
            "abs" => a().abs(),
            "floor" => a().floor(),
            "ceil" => a().ceil(),
            "round" => a().round(),
            "trunc" => a().trunc(),
            "sqrt" => a().sqrt(),
            "cbrt" => a().cbrt(),
            "sign" => a().signum(),
            "pow" => a().powf(b()),
            "log" => a().ln(),
            "log2" => a().log2(),
            "log10" => a().log10(),
            "exp" => a().exp(),
            "sin" => a().sin(),
            "cos" => a().cos(),
            "tan" => a().tan(),
            "asin" => a().asin(),
            "acos" => a().acos(),
            "atan" => a().atan(),
            "atan2" => a().atan2(b()),
            "random" => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let t = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .subsec_nanos();
                    t as f64 / u32::MAX as f64
                }
                #[cfg(target_arch = "wasm32")]
                {
                    0.42
                }
            }
            "max" => {
                if args.is_empty() { return Value::number(f64::NEG_INFINITY); }
                let mut m = f64::NEG_INFINITY;
                for arg in args {
                    let n = arg.as_number().unwrap_or(f64::NAN);
                    if n.is_nan() { return Value::number(f64::NAN); }
                    if n > m { m = n; }
                }
                m
            }
            "min" => {
                if args.is_empty() { return Value::number(f64::INFINITY); }
                let mut m = f64::INFINITY;
                for arg in args {
                    let n = arg.as_number().unwrap_or(f64::NAN);
                    if n.is_nan() { return Value::number(f64::NAN); }
                    if n < m { m = n; }
                }
                m
            }
            _ => return Value::undefined(),
        };
        Value::number(result)
    }

    /// Check if a value is a String wrapper object.
    fn is_string_wrapper(&self, val: Value) -> bool {
        if let Some(oid) = val.as_object_id()
            && let Some(obj) = self.heap.get(oid)
                && let ObjectKind::Wrapper(inner) = &obj.kind {
                    return inner.is_string();
                }
        false
    }

    /// Unwrap a wrapper object to its primitive, or return the value as-is.
    fn to_primitive(&self, val: Value) -> Value {
        if let Some(oid) = val.as_object_id()
            && let Some(obj) = self.heap.get(oid)
                && let ObjectKind::Wrapper(inner) = &obj.kind {
                    return *inner;
                }
        val
    }

    // ---- Promise helpers ----

    fn resolve_promise(&mut self, oid: ObjectId, value: Value) -> Result<(), VmError> {
        // Clone reactions before mutating
        let reactions = {
            let obj = self.heap.get(oid).ok_or_else(|| VmError::RuntimeError("invalid promise".into()))?;
            if let ObjectKind::Promise { state, reactions, .. } = &obj.kind {
                if *state != PromiseState::Pending { return Ok(()); } // already settled
                reactions.clone()
            } else {
                return Ok(());
            }
        };
        // Transition to Fulfilled
        if let Some(obj) = self.heap.get_mut(oid)
            && let ObjectKind::Promise { state, result, reactions: r, .. } = &mut obj.kind {
                *state = PromiseState::Fulfilled;
                *result = value;
                r.clear();
            }
        // Enqueue reactions as microtasks
        for reaction in reactions {
            self.microtask_queue.push(Microtask::PromiseReaction {
                callback: reaction.on_fulfilled,
                value,
                result_promise: reaction.promise,
                is_fulfilled: true,
            });
        }
        Ok(())
    }

    fn reject_promise(&mut self, oid: ObjectId, reason: Value) -> Result<(), VmError> {
        let reactions = {
            let obj = self.heap.get(oid).ok_or_else(|| VmError::RuntimeError("invalid promise".into()))?;
            if let ObjectKind::Promise { state, reactions, .. } = &obj.kind {
                if *state != PromiseState::Pending { return Ok(()); }
                reactions.clone()
            } else {
                return Ok(());
            }
        };
        if let Some(obj) = self.heap.get_mut(oid)
            && let ObjectKind::Promise { state, result, reactions: r, .. } = &mut obj.kind {
                *state = PromiseState::Rejected;
                *result = reason;
                r.clear();
            }
        for reaction in reactions {
            self.microtask_queue.push(Microtask::PromiseReaction {
                callback: reaction.on_rejected,
                value: reason,
                result_promise: reaction.promise,
                is_fulfilled: false,
            });
        }
        Ok(())
    }

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

    fn exec_promise_method(&mut self, oid: ObjectId, method_name: StringId, args: &[Value]) -> Result<Value, VmError> {
        let name = self.interner.resolve(method_name).to_owned();
        match name.as_str() {
            "then" => {
                let on_fulfilled = args.first().copied().filter(|v| v.is_int());
                let on_rejected = args.get(1).copied().filter(|v| v.is_int());
                // Create child promise
                let child = JsObject::promise();
                let child_id = self.heap.allocate(child);
                let reaction = PromiseReaction { on_fulfilled, on_rejected, promise: child_id };

                // Check current state
                let (state, result) = {
                    let obj = self.heap.get(oid).unwrap();
                    if let ObjectKind::Promise { state, result, .. } = &obj.kind {
                        (*state, *result)
                    } else {
                        return Ok(Value::undefined());
                    }
                };

                match state {
                    PromiseState::Pending => {
                        if let Some(obj) = self.heap.get_mut(oid)
                            && let ObjectKind::Promise { reactions, .. } = &mut obj.kind {
                                reactions.push(reaction);
                            }
                    }
                    PromiseState::Fulfilled => {
                        self.microtask_queue.push(Microtask::PromiseReaction {
                            callback: on_fulfilled,
                            value: result,
                            result_promise: child_id,
                            is_fulfilled: true,
                        });
                    }
                    PromiseState::Rejected => {
                        self.microtask_queue.push(Microtask::PromiseReaction {
                            callback: on_rejected,
                            value: result,
                            result_promise: child_id,
                            is_fulfilled: false,
                        });
                    }
                }
                Ok(Value::object_id(child_id))
            }
            "catch" => {
                let on_rejected = args.first().copied().filter(|v| v.is_int());
                // Same as .then(undefined, onRejected)
                let then_args = [Value::undefined(), on_rejected.unwrap_or(Value::undefined())];
                self.exec_promise_method(oid, method_name, &then_args)
            }
            _ => Ok(Value::undefined()),
        }
    }

    fn exec_promise_static(&mut self, method_name: StringId, args: &[Value]) -> Result<Value, VmError> {
        let name = self.interner.resolve(method_name).to_owned();
        match name.as_str() {
            "resolve" => {
                let val = args.first().copied().unwrap_or(Value::undefined());
                // If already a promise, return it
                if let Some(oid) = val.as_object_id()
                    && let Some(obj) = self.heap.get(oid)
                        && matches!(&obj.kind, ObjectKind::Promise { .. }) {
                            return Ok(val);
                        }
                let p = JsObject::promise();
                let pid = self.heap.allocate(p);
                self.resolve_promise(pid, val)?;
                Ok(Value::object_id(pid))
            }
            "reject" => {
                let val = args.first().copied().unwrap_or(Value::undefined());
                let p = JsObject::promise();
                let pid = self.heap.allocate(p);
                self.reject_promise(pid, val)?;
                Ok(Value::object_id(pid))
            }
            _ => Ok(Value::undefined()),
        }
    }

    // ---- Global function dispatch ----
    fn exec_global_fn(&mut self, sentinel: i32, args: &[Value]) -> Value {
        match sentinel {
            -500 => { // parseInt
                let s = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                let radix = args.get(1).and_then(|v| v.as_number()).unwrap_or(10.0) as u32;
                let s = s.trim();
                let (s, neg) = if let Some(stripped) = s.strip_prefix('-') { (stripped, true) } else if let Some(stripped) = s.strip_prefix('+') { (stripped, false) } else { (s, false) };
                let s = if radix == 16 { s.strip_prefix("0x").or(s.strip_prefix("0X")).unwrap_or(s) } else { s };
                // Parse digits for the given radix
                let mut result = 0i64;
                let mut found = false;
                for c in s.chars() {
                    let d = c.to_digit(radix);
                    if let Some(d) = d { result = result * radix as i64 + d as i64; found = true; }
                    else { break; }
                }
                if !found { return Value::number(f64::NAN); }
                let result = if neg { -result } else { result };
                Value::number(result as f64)
            }
            -501 => { // parseFloat
                let s = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                let s = s.trim();
                Value::number(s.parse::<f64>().unwrap_or(f64::NAN))
            }
            -502 => { // isNaN
                let n = args.first().and_then(|v| v.as_number()).unwrap_or(f64::NAN);
                Value::boolean(n.is_nan())
            }
            -503 => { // isFinite
                let n = args.first().and_then(|v| v.as_number()).unwrap_or(f64::NAN);
                Value::boolean(n.is_finite())
            }
            -504 => { // String()
                let s = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                let id = self.interner.intern(&s);
                Value::string(id)
            }
            -505 => { // Number()
                let v = args.first().copied().unwrap_or(Value::int(0));
                if let Some(n) = v.as_number() { Value::number(n) }
                else if v.is_boolean() { Value::number(if v.as_bool().unwrap() { 1.0 } else { 0.0 }) }
                else if v.is_null() { Value::number(0.0) }
                else if v.is_undefined() { Value::number(f64::NAN) }
                else if v.is_string() {
                    let s = self.value_to_string(v);
                    Value::number(s.trim().parse::<f64>().unwrap_or(f64::NAN))
                }
                else { Value::number(f64::NAN) }
            }
            -506 => { // Boolean()
                let v = args.first().copied().unwrap_or(Value::boolean(false));
                Value::boolean(v.to_boolean())
            }
            -530 => { // Number.isNaN
                let v = args.first().copied().unwrap_or(Value::undefined());
                // Number.isNaN does NOT coerce — only true for actual NaN number values
                if v.is_float() { Value::boolean(v.as_float().unwrap().is_nan()) }
                else { Value::boolean(false) }
            }
            -531 => { // Number.isFinite
                let v = args.first().copied().unwrap_or(Value::undefined());
                if let Some(n) = v.as_number() { Value::boolean(n.is_finite()) }
                else { Value::boolean(false) }
            }
            -532 => { // Number.isInteger
                let v = args.first().copied().unwrap_or(Value::undefined());
                if let Some(n) = v.as_number() { Value::boolean(n.fract() == 0.0 && n.is_finite()) }
                else { Value::boolean(false) }
            }
            _ => Value::undefined(),
        }
    }

    // ---- JSON method dispatch ----
    fn exec_json_method(&mut self, method_name: StringId, args: &[Value]) -> Value {
        let name = self.interner.resolve(method_name).to_owned();
        match name.as_str() {
            "parse" => {
                let s = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                match self.json_parse(&s) {
                    Ok(val) => val,
                    Err(_) => Value::undefined(), // SyntaxError in real JS
                }
            }
            "stringify" => {
                let val = args.first().copied().unwrap_or(Value::undefined());
                let s = self.json_stringify(val);
                let id = self.interner.intern(&s);
                Value::string(id)
            }
            _ => Value::undefined(),
        }
    }

    // ---- JSON.parse: simple recursive descent ----
    fn json_parse(&mut self, input: &str) -> Result<Value, String> {
        let input = input.trim();
        let (val, _) = json_parse_value(input, &mut self.heap, &mut self.interner)?;
        Ok(val)
    }

    // ---- JSON.stringify ----
    fn json_stringify(&self, val: Value) -> String {
        if val.is_undefined() { return "undefined".into(); }
        if val.is_null() { return "null".into(); }
        if val.is_boolean() { return format!("{}", val.as_bool().unwrap()); }
        if val.is_int() { return format!("{}", val.as_int().unwrap()); }
        if val.is_float() {
            let n = val.as_float().unwrap();
            if n.is_nan() || n.is_infinite() { return "null".into(); }
            return format!("{n}");
        }
        if val.is_string() {
            let id = val.as_string_id().unwrap();
            let s = self.interner.resolve(id);
            return format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n").replace('\r', "\\r").replace('\t', "\\t"));
        }
        if let Some(oid) = val.as_object_id()
            && let Some(obj) = self.heap.get(oid) {
                match &obj.kind {
                    ObjectKind::Array(elements) => {
                        let parts: Vec<String> = elements.iter().map(|v| self.json_stringify(*v)).collect();
                        return format!("[{}]", parts.join(","));
                    }
                    _ => {
                        let parts: Vec<String> = obj.properties.iter().map(|(k, v)| {
                            let key = self.interner.resolve(*k);
                            format!("\"{}\":{}", key, self.json_stringify(*v))
                        }).collect();
                        return format!("{{{}}}", parts.join(","));
                    }
                }
            }
        "null".into()
    }

    pub fn take_interner(self) -> Interner {
        self.interner
    }

    pub fn interner(&self) -> &Interner {
        &self.interner
    }

    // ---- Stack helpers -----------------------------------------------------

    #[inline]
    fn push(&mut self, value: Value) {
        self.stack.push(value);
    }

    #[inline]
    fn pop(&mut self) -> Result<Value, VmError> {
        self.stack
            .pop()
            .ok_or_else(|| VmError::RuntimeError("stack underflow".into()))
    }

    #[inline]
    fn peek(&self) -> Result<Value, VmError> {
        self.stack
            .last()
            .copied()
            .ok_or_else(|| VmError::RuntimeError("stack underflow".into()))
    }

    // ---- Bytecode read helpers --------------------------------------------

    #[inline]
    fn cur_chunk(&self) -> usize {
        self.frames.last().unwrap().chunk_idx
    }

    #[inline]
    fn cur_ip(&self) -> usize {
        self.frames.last().unwrap().ip
    }

    #[inline]
    fn read_byte(&mut self) -> u8 {
        let frame = self.frames.last_mut().unwrap();
        let byte = self.chunks[frame.chunk_idx].code[frame.ip];
        frame.ip += 1;
        byte
    }

    #[inline]
    fn read_u16(&mut self) -> u16 {
        let frame = self.frames.last_mut().unwrap();
        let val = self.chunks[frame.chunk_idx].read_u16(frame.ip);
        frame.ip += 2;
        val
    }

    #[inline]
    fn read_i16(&mut self) -> i16 {
        let frame = self.frames.last_mut().unwrap();
        let val = self.chunks[frame.chunk_idx].read_i16(frame.ip);
        frame.ip += 2;
        val
    }

    // ---- Numeric helpers --------------------------------------------------

    /// Pop two values off the stack, convert each to f64.
    /// JS ToNumber: coerce any value to f64.
    #[inline(always)]
    fn to_f64(&self, val: Value) -> f64 {
        if let Some(n) = val.as_number() { return n; }
        if val.is_boolean() { return if val.as_bool().unwrap() { 1.0 } else { 0.0 }; }
        if val.is_null() { return 0.0; }
        if val.is_undefined() { return f64::NAN; }
        if val.is_string() {
            let id = val.as_string_id().unwrap();
            let s = self.interner.resolve(id).trim();
            if s.is_empty() { return 0.0; }
            return s.parse::<f64>().unwrap_or(f64::NAN);
        }
        // Wrapper objects: unwrap and coerce the primitive
        if let Some(oid) = val.as_object_id()
            && let Some(obj) = self.heap.get(oid)
                && let ObjectKind::Wrapper(inner) = &obj.kind {
                    return self.to_f64(*inner);
                }
        f64::NAN
    }

    #[inline(always)]
    fn pop_numbers(&mut self) -> Result<(f64, f64), VmError> {
        let b = self.pop()?;
        let a = self.pop()?;
        Ok((self.to_f64(a), self.to_f64(b)))
    }

    /// Pop two values, convert to i32 (for bitwise ops).
    fn pop_ints(&mut self) -> Result<(i32, i32), VmError> {
        let bv = self.pop()?;
        let av = self.pop()?;
        let b = self.to_i32(bv)?;
        let a = self.to_i32(av)?;
        Ok((a, b))
    }

    /// Convert a Value to i32 for bitwise operations (ToInt32).
    fn to_i32(&self, val: Value) -> Result<i32, VmError> {
        let n = self.to_f64(val);
        if n.is_nan() || n.is_infinite() || n == 0.0 { return Ok(0); }
        Ok(n as i32)
    }

    /// Convert a Value to u32 for unsigned right shift.
    fn to_u32(&self, val: Value) -> Result<u32, VmError> {
        let n = self.to_f64(val);
        if n.is_nan() || n.is_infinite() || n == 0.0 { return Ok(0); }
        Ok(n as u32)
    }

    /// Push a number result, using SMI when the value fits in i32 with no
    /// fractional part.  Preserves -0.0 as a float (JS distinguishes it).
    #[inline]
    fn push_number(&mut self, n: f64) {
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
    fn value_to_string(&self, val: Value) -> String {
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
        } else if let Some(oid) = val.as_object_id() {
            if let Some(obj) = self.heap.get(oid) {
                match &obj.kind {
                    ObjectKind::Array(elements) => {
                        let parts: Vec<String> = elements.iter().map(|v| self.value_to_string(*v)).collect();
                        parts.join(",")
                    }
                    ObjectKind::Wrapper(inner) => self.value_to_string(*inner),
                    _ => "[object Object]".into(),
                }
            } else {
                "[object Object]".into()
            }
        } else {
            "???".into()
        }
    }

    /// Return the typeof string for a value.
    fn type_of_value(&self, val: Value) -> &'static str {
        if val.is_undefined() {
            "undefined"
        } else if val.is_null() {
            "object"
        } else if val.is_boolean() {
            "boolean"
        } else if val.is_int() {
            // Check if this is a closure (packed chunk index) or a global fn sentinel
            let i = val.as_int().unwrap();
            let chunk_idx = (i & 0xFFFF) as usize;
            if (chunk_idx >= 1 && chunk_idx < self.chunks.len()) || i <= -500 {
                "function"
            } else {
                "number"
            }
        } else if val.is_number() {
            "number"
        } else if val.is_string() {
            "string"
        } else if val.is_symbol() {
            "symbol"
        } else if val.is_object() {
            if let Some(oid) = val.as_object_id()
                && let Some(obj) = self.heap.get(oid) {
                    // Classes have __constructor__ — typeof should be "function"
                    for k in obj.properties.keys() {
                        if self.interner.resolve(*k) == "__constructor__" {
                            return "function";
                        }
                    }
                }
            "object"
        } else {
            "undefined"
        }
    }

    // ---- Abstract equality (simplified) -----------------------------------

    /// Simplified abstract equality (==). Handles the most common cases:
    ///   - same type: strict equality
    ///   - null == undefined (and vice versa)
    ///   - number == string: coerce string to number
    fn abstract_eq(&self, a: Value, b: Value) -> bool {
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

        // Both strings
        if a.is_string() && b.is_string() {
            // String ids are interned, so equal ids mean equal strings.
            // Already handled by raw() check above.
            return false;
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
            let pa = self.to_primitive(a);
            if pa.raw() != a.raw() {
                return self.abstract_eq(pa, b);
            }
        }
        if b.is_object() && !a.is_object() {
            let pb = self.to_primitive(b);
            if pb.raw() != b.raw() {
                return self.abstract_eq(a, pb);
            }
        }

        false
    }

    /// Strict equality (===).
    fn strict_eq(&self, a: Value, b: Value) -> bool {
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
        false
    }

    /// Try to parse a string value as a number (for == coercion).
    fn string_to_number(&self, val: Value) -> Option<f64> {
        let id = val.as_string_id()?;
        let s = self.interner.resolve(id).trim();
        if s.is_empty() {
            return Some(0.0);
        }
        s.parse::<f64>().ok()
    }

    // ---- Main execution loop ----------------------------------------------

    pub fn run(&mut self) -> Result<Value, VmError> {
        loop {
            if self.frames.is_empty() {
                return Ok(if self.stack.is_empty() {
                    Value::undefined()
                } else {
                    self.pop()?
                });
            }

            let chunk_idx = self.cur_chunk();
            let ip = self.cur_ip();
            if ip >= self.chunks[chunk_idx].code.len() {
                return Ok(if self.stack.is_empty() {
                    Value::undefined()
                } else {
                    self.pop()?
                });
            }

            let byte = self.read_byte();
            let opcode = OpCode::from_byte(byte).ok_or_else(|| {
                VmError::RuntimeError(format!("invalid opcode: {byte:#04x}"))
            })?;

            match opcode {
                // ---- Constants & Literals --------------------------------
                OpCode::Const => {
                    let index = self.read_u16() as usize;
                    let val = self.chunks[self.cur_chunk()].constants[index];
                    self.push(val);
                }

                OpCode::ConstLong => {
                    let index = {
                        let v = self.chunks[self.cur_chunk()].read_u32(self.cur_ip());
                        self.frames.last_mut().unwrap().ip += 4;
                        v as usize
                    };
                    let val = self.chunks[self.cur_chunk()].constants[index];
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
                    let a_is_str = a.is_string() || self.is_string_wrapper(a);
                    let b_is_str = b.is_string() || self.is_string_wrapper(b);

                    if a_is_str || b_is_str {
                        let sa = self.value_to_string(a);
                        let sb = self.value_to_string(b);
                        let mut result = sa;
                        result.push_str(&sb);
                        let id = self.interner.intern(&result);
                        self.push(Value::string(id));
                    } else {
                        let na = self.to_f64(a);
                        let nb = self.to_f64(b);
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
                    self.push_number(-self.to_f64(val));
                }

                OpCode::Pos => {
                    let val = self.pop()?;
                    self.push_number(self.to_f64(val));
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
                    self.push(Value::boolean(self.abstract_eq(a, b)));
                }

                OpCode::Ne => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.push(Value::boolean(!self.abstract_eq(a, b)));
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
                    let bv = self.pop()?; let b = self.to_primitive(bv);
                    let av = self.pop()?; let a = self.to_primitive(av);
                    if a.is_string() && b.is_string() {
                        let sa = self.interner.resolve(a.as_string_id().unwrap());
                        let sb = self.interner.resolve(b.as_string_id().unwrap());
                        self.push(Value::boolean(sa < sb));
                    } else {
                        self.push(Value::boolean(self.to_f64(a) < self.to_f64(b)));
                    }
                }

                OpCode::Le => {
                    let bv = self.pop()?; let b = self.to_primitive(bv);
                    let av = self.pop()?; let a = self.to_primitive(av);
                    if a.is_string() && b.is_string() {
                        let sa = self.interner.resolve(a.as_string_id().unwrap());
                        let sb = self.interner.resolve(b.as_string_id().unwrap());
                        self.push(Value::boolean(sa <= sb));
                    } else {
                        self.push(Value::boolean(self.to_f64(a) <= self.to_f64(b)));
                    }
                }

                OpCode::Gt => {
                    let bv = self.pop()?; let b = self.to_primitive(bv);
                    let av = self.pop()?; let a = self.to_primitive(av);
                    if a.is_string() && b.is_string() {
                        let sa = self.interner.resolve(a.as_string_id().unwrap());
                        let sb = self.interner.resolve(b.as_string_id().unwrap());
                        self.push(Value::boolean(sa > sb));
                    } else {
                        self.push(Value::boolean(self.to_f64(a) > self.to_f64(b)));
                    }
                }

                OpCode::Ge => {
                    let bv = self.pop()?; let b = self.to_primitive(bv);
                    let av = self.pop()?; let a = self.to_primitive(av);
                    if a.is_string() && b.is_string() {
                        let sa = self.interner.resolve(a.as_string_id().unwrap());
                        let sb = self.interner.resolve(b.as_string_id().unwrap());
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
                        let frame = self.frames.last().unwrap();
                        let param_count = self.chunks[frame.chunk_idx].param_count as usize;
                        let base = frame.base;
                        let mut args = Vec::new();
                        for i in 0..param_count {
                            if base + i < self.stack.len() {
                                args.push(self.stack[base + i]);
                            }
                        }
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
                    let val = self.stack[base + slot];
                    self.push(val);
                }

                OpCode::SetLocal => {
                    let slot = self.read_byte() as usize;
                    let val = self.peek()?;
                    let base = self.frames.last().unwrap().base;
                    self.stack[base + slot] = val;
                }

                OpCode::GetLocalWide => {
                    let slot = self.read_u16() as usize;
                    let base = self.frames.last().unwrap().base;
                    let val = self.stack[base + slot];
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

                    if func_val.is_int() {
                        let packed = func_val.as_int().unwrap();
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

                            // Avoid clone when there are no upvalues (common fast path)
                            let upvalues = if closure_id < self.closure_upvalues.len()
                                && !self.closure_upvalues[closure_id].is_empty() {
                                self.closure_upvalues[closure_id].clone()
                            } else {
                                Vec::new()
                            };

                            self.frames.push(CallFrame {
                                chunk_idx,
                                ip: 0,
                                base: func_pos + 1,
                                upvalues,
                                this_value: Value::undefined(),
                                is_constructor: false,
                            });
                            continue;
                        }
                    }

                    // Check for Promise resolve/reject sentinels
                    if func_val.is_int() {
                        let s = func_val.as_int().unwrap();
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
                    }

                    // Check for native global function sentinels
                    if func_val.is_int() {
                        let sentinel = func_val.as_int().unwrap();
                        if (-532..=-500).contains(&sentinel) {
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[func_pos + 1 + i]).collect();
                            let result = self.exec_global_fn(sentinel, &args);
                            self.stack.truncate(func_pos);
                            self.push(result);
                            continue;
                        }
                    }

                    // Unknown function - pop everything, push undefined
                    self.stack.truncate(func_pos);
                    self.push(Value::undefined());
                }

                OpCode::Return => {
                    let result = self.pop()?;
                    let frame = self.frames.pop().unwrap();
                    // Only close upvalues if there are any (fast path: skip for most functions)
                    if !self.closure_upvalues.is_empty() {
                        self.close_upvalues_above(frame.base.saturating_sub(1));
                    }
                    if self.frames.is_empty() {
                        return Ok(result);
                    }
                    self.stack.truncate(frame.base.saturating_sub(1));
                    self.push(result);
                }

                OpCode::ReturnUndefined => {
                    let frame = self.frames.pop().unwrap();
                    let result = if frame.is_constructor { frame.this_value } else { Value::undefined() };
                    if !self.closure_upvalues.is_empty() {
                        self.close_upvalues_above(frame.base.saturating_sub(1));
                    }
                    if self.frames.is_empty() {
                        return Ok(result);
                    }
                    self.stack.truncate(frame.base.saturating_sub(1));
                    self.push(result);
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
                    self.pop()?;
                    self.push(Value::boolean(true));
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
                    // Simple instanceof: check if object's "name" matches constructor name
                    let result = if let Some(oid) = obj.as_object_id() {
                        if let Some(o) = self.heap.get(oid) {
                            let name_key = self.interner.intern("name");
                            if let Some(name_val) = o.get_property(name_key) {
                                // Check against known error constructors
                                if constructor.is_int() {
                                    let sentinel = constructor.as_int().unwrap();
                                    let ctor_name = match sentinel {
                                        -510 => "Error",
                                        -511 => "TypeError",
                                        -512 => "RangeError",
                                        -513 => "ReferenceError",
                                        -514 => "SyntaxError",
                                        _ => "",
                                    };
                                    if !ctor_name.is_empty() {
                                        if let Some(nid) = name_val.as_string_id() {
                                            let n = self.interner.resolve(nid);
                                            n == ctor_name
                                        } else { false }
                                    } else { false }
                                } else { false }
                            } else { false }
                        } else { false }
                    } else { false };
                    self.push(Value::boolean(result));
                }

                OpCode::In => {
                    let obj = self.pop()?;
                    let key = self.pop()?;
                    let result = if let Some(oid) = obj.as_object_id() {
                        if let Some(kid) = key.as_string_id() {
                            self.heap.get(oid).map(|o| o.get_property(kid).is_some()).unwrap_or(false)
                        } else { false }
                    } else { false };
                    self.push(Value::boolean(result));
                }

                OpCode::GetProperty => {
                    let name_idx = self.read_u16() as usize;
                    let name_val = self.chunks[self.cur_chunk()].constants[name_idx];
                    let name_id = name_val.as_string_id().ok_or_else(|| {
                        VmError::RuntimeError("GetProperty: expected string constant".into())
                    })?;
                    // TypeError for null/undefined property access
                    let peeked = self.peek()?;
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
                        // Check for array-specific properties
                        if name_str == "length"
                            && let Some(obj) = self.heap.get(oid)
                                && let ObjectKind::Array(ref elements) = obj.kind {
                                    self.push(Value::int(elements.len() as i32));
                                    continue;
                                }
                        // Check for array methods (push, pop, etc.)
                        if name_str == "push" || name_str == "pop" || name_str == "join"
                            || name_str == "indexOf" || name_str == "includes"
                            || name_str == "map" || name_str == "filter" || name_str == "forEach" {
                            // Store as sentinel: array_oid in high bits, method marker in low
                            // We'll handle these in CallMethod
                            let sentinel = -((oid.0 as i32 + 1) * 1000 + name_id.0 as i32);
                            self.push(Value::int(sentinel));
                            // Also push the object back since CallMethod expects it
                            // Actually -- the object was already popped. For CallMethod,
                            // the compiler pushes obj first, then looks up the method.
                            // Let me just store the sentinel and handle in CallMethod.
                            continue;
                        }
                        let val = self.heap.get(oid)
                            .and_then(|o| o.get_property(name_id))
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
                            | "slice" | "substring" | "toUpperCase" | "toLowerCase"
                            | "trim" | "trimStart" | "trimEnd"
                            | "split" | "replace" | "repeat"
                            | "padStart" | "padEnd" | "concat" => {
                                // Encode: string sentinel = -200 - method_index
                                let method_idx = match name_str {
                                    "charAt" => 0, "charCodeAt" => 1, "indexOf" => 2,
                                    "lastIndexOf" => 3, "includes" => 4, "startsWith" => 5,
                                    "endsWith" => 6, "slice" => 7, "substring" => 8,
                                    "toUpperCase" => 9, "toLowerCase" => 10,
                                    "trim" => 11, "trimStart" => 12, "trimEnd" => 13,
                                    "split" => 14, "replace" => 15, "repeat" => 16,
                                    "padStart" => 17, "padEnd" => 18, "concat" => 19,
                                    _ => 99,
                                };
                                self.push(Value::int(-200 - method_idx));
                            }
                            _ => self.push(Value::undefined()),
                        }
                    } else if obj_val.is_int() {
                        // Property access on sentinel globals (Number.NaN, etc)
                        let sentinel = obj_val.as_int().unwrap();
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
                                "isNaN" => Value::int(-530),
                                "isFinite" => Value::int(-531),
                                "isInteger" => Value::int(-532),
                                "parseInt" => Value::int(-500),
                                "parseFloat" => Value::int(-501),
                                _ => Value::undefined(),
                            },
                            _ => Value::undefined(),
                        };
                        self.push(result);
                    } else {
                        self.push(Value::undefined());
                    }
                }

                OpCode::SetProperty => {
                    let name_idx = self.read_u16() as usize;
                    let name_val = self.chunks[self.cur_chunk()].constants[name_idx];
                    let name_id = name_val.as_string_id().unwrap();
                    let val = self.pop()?;
                    let obj_val = self.pop()?;
                    if let Some(oid) = obj_val.as_object_id()
                        && let Some(obj) = self.heap.get_mut(oid) {
                            obj.set_property(name_id, val);
                        }
                    self.push(val);
                }

                OpCode::GetElement => {
                    let key = self.pop()?;
                    let obj_val = self.pop()?;
                    if let Some(oid) = obj_val.as_object_id()
                        && let Some(obj) = self.heap.get(oid) {
                            if let ObjectKind::Array(ref elements) = obj.kind {
                                // Numeric index into array
                                if let Some(idx) = key.as_number() {
                                    let idx = idx as usize;
                                    let val = elements.get(idx).copied().unwrap_or(Value::undefined());
                                    self.push(val);
                                    continue;
                                }
                            }
                            // String property lookup
                            if let Some(name_id) = key.as_string_id() {
                                let val = obj.get_property(name_id).unwrap_or(Value::undefined());
                                self.push(val);
                                continue;
                            }
                        }
                    self.push(Value::undefined());
                }

                OpCode::SetElement => {
                    let val = self.pop()?;
                    let key = self.pop()?;
                    let obj_val = self.pop()?;
                    if let Some(oid) = obj_val.as_object_id()
                        && let Some(obj) = self.heap.get_mut(oid) {
                            if let ObjectKind::Array(ref mut elements) = obj.kind
                                && let Some(idx) = key.as_number() {
                                    let idx = idx as usize;
                                    while elements.len() <= idx {
                                        elements.push(Value::undefined());
                                    }
                                    elements[idx] = val;
                                    self.push(val);
                                    continue;
                                }
                            if let Some(name_id) = key.as_string_id() {
                                obj.set_property(name_id, val);
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
                    let _ = self.read_i16();
                    return Err(VmError::RuntimeError(
                        "OptionalChain not yet implemented".into(),
                    ));
                }

                OpCode::GetPrivate | OpCode::SetPrivate => {
                    let _ = self.read_u16();
                    return Err(VmError::RuntimeError(format!(
                        "{opcode:?} not yet implemented"
                    )));
                }

                OpCode::CallMethod => {
                    let argc = self.read_byte() as usize;
                    let method_name_idx = self.read_u16() as usize;
                    let method_name = self.chunks[self.cur_chunk()].constants[method_name_idx]
                        .as_string_id().unwrap();
                    // Stack layout: [..., obj, arg0, ..., argN]
                    let obj_pos = self.stack.len() - 1 - argc;
                    let obj_val = self.stack[obj_pos];

                    // Look up the method on the object
                    let method_val = if let Some(oid) = obj_val.as_object_id() {
                        self.heap.get(oid).and_then(|o| o.get_property(method_name))
                    } else {
                        None
                    };

                    // Check for console.log/warn/error sentinels
                    if let Some(mv) = method_val
                        && mv.is_int() {
                            let sentinel = mv.as_int().unwrap();
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

                    // Check if the obj is a string and the method was resolved to a string sentinel
                    if obj_val.is_string() {
                        let sid = obj_val.as_string_id().unwrap();
                        let s = self.interner.resolve(sid).to_owned();
                        let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                        let result = self.exec_string_method(&s, method_name, &args);
                        self.stack.truncate(obj_pos);
                        self.push(result);
                        continue;
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
                        // Check for Math methods
                        let math_name = self.interner.intern("Math");
                        if self.globals.get(&math_name).map(|v| v.as_object_id()) == Some(Some(oid)) {
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                            let result = self.exec_math_method(method_name, &args);
                            self.stack.truncate(obj_pos);
                            self.push(result);
                            continue;
                        }
                        // Check for JSON methods
                        let json_name = self.interner.intern("JSON");
                        if self.globals.get(&json_name).map(|v| v.as_object_id()) == Some(Some(oid)) {
                            let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                            let result = self.exec_json_method(method_name, &args);
                            self.stack.truncate(obj_pos);
                            self.push(result);
                            continue;
                        }
                    }

                    // Try to call as a closure method on an object
                    if let Some(oid) = obj_val.as_object_id() {
                        let method_val = self.heap.get(oid)
                            .and_then(|o| o.get_property(method_name));
                        if let Some(mv) = method_val
                            && mv.is_int() {
                                let packed = mv.as_int().unwrap();
                                let closure_id = ((packed as u32) >> 16) as usize;
                                let chunk_idx = (packed & 0xFFFF) as usize;
                                if chunk_idx >= 1 && chunk_idx < self.chunks.len() {
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
                                    self.frames.push(CallFrame {
                                        chunk_idx, ip: 0, base: obj_pos + 1,
                                        upvalues, this_value: obj_val, is_constructor: false,
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
                    if obj_val.is_int() && obj_val.as_int() == Some(-508) {
                        let mn = self.interner.resolve(method_name).to_owned();
                        let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                        let result = match mn.as_str() {
                            "keys" => {
                                if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
                                    let keys: Vec<Value> = self.heap.get(oid)
                                        .map(|o| o.properties.keys().map(|k| Value::string(*k)).collect())
                                        .unwrap_or_default();
                                    let arr = JsObject::array(keys);
                                    Value::object_id(self.heap.allocate(arr))
                                } else { Value::undefined() }
                            }
                            "values" => {
                                if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
                                    let vals: Vec<Value> = self.heap.get(oid)
                                        .map(|o| o.properties.values().copied().collect())
                                        .unwrap_or_default();
                                    let arr = JsObject::array(vals);
                                    Value::object_id(self.heap.allocate(arr))
                                } else { Value::undefined() }
                            }
                            "entries" => {
                                if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
                                    let pairs: Vec<(Value, Value)> = self.heap.get(oid)
                                        .map(|o| o.properties.iter().map(|(k, v)| (Value::string(*k), *v)).collect())
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
                            _ => Value::undefined(),
                        };
                        self.stack.truncate(obj_pos);
                        self.push(result);
                        continue;
                    }

                    // Array.isArray
                    if obj_val.is_int() && obj_val.as_int() == Some(-507) {
                        let mn = self.interner.resolve(method_name).to_owned();
                        if mn == "isArray" {
                            let arg = if argc > 0 { self.stack[obj_pos + 1] } else { Value::undefined() };
                            let is_arr = arg.as_object_id()
                                .and_then(|oid| self.heap.get(oid))
                                .map(|o| matches!(&o.kind, ObjectKind::Array(_)))
                                .unwrap_or(false);
                            self.stack.truncate(obj_pos);
                            self.push(Value::boolean(is_arr));
                            continue;
                        }
                    }

                    // String.fromCharCode
                    if obj_val.is_int() && obj_val.as_int() == Some(-504) {
                        let mn = self.interner.resolve(method_name).to_owned();
                        if mn == "fromCharCode" {
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
                    }

                    // Check for Promise static methods (Promise.resolve/reject)
                    if obj_val.is_int() && obj_val.as_int() == Some(-520) {
                        let args: Vec<Value> = (0..argc).map(|i| self.stack[obj_pos + 1 + i]).collect();
                        let result = self.exec_promise_static(method_name, &args)?;
                        self.stack.truncate(obj_pos);
                        self.push(result);
                        continue;
                    }

                    // Generic method call - pop everything, push undefined
                    self.stack.truncate(obj_pos);
                    self.push(Value::undefined());
                }

                OpCode::Construct => {
                    let argc = self.read_byte() as usize;
                    let func_pos = self.stack.len() - 1 - argc;
                    let func_val = self.stack[func_pos];

                    // Handle Promise constructor
                    if func_val.is_int() && func_val.as_int() == Some(-520) {
                        let executor = if argc > 0 { self.stack[func_pos + 1] } else { Value::undefined() };
                        let p = JsObject::promise();
                        let pid = self.heap.allocate(p);
                        // Create resolve/reject sentinels
                        let resolve_val = Value::int(-600_000 - pid.0 as i32);
                        let reject_val = Value::int(-700_000 - pid.0 as i32);
                        // Call the executor
                        if executor.is_int() {
                            let _ = self.call_function(executor, &[resolve_val, reject_val]);
                        }
                        self.stack.truncate(func_pos);
                        self.push(Value::object_id(pid));
                        continue;
                    }

                    // Handle wrapper constructors (new Number, new Boolean, new String)
                    if func_val.is_int() {
                        let sentinel = func_val.as_int().unwrap();
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
                    if func_val.is_int() && func_val.as_int() == Some(-507) {
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
                    if func_val.is_int() && func_val.as_int() == Some(-508) {
                        let obj = JsObject::ordinary();
                        let oid = self.heap.allocate(obj);
                        self.stack.truncate(func_pos);
                        self.push(Value::object_id(oid));
                        continue;
                    }

                    // Handle Error constructors
                    if func_val.is_int() {
                        let sentinel = func_val.as_int().unwrap();
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

                    // Create a new object for `this`
                    let new_obj = JsObject::ordinary();
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

                        // Set prototype on the new object
                        if let Some(pv) = proto_val
                            && let Some(proto_oid) = pv.as_object_id() {
                                // Copy prototype methods to the new object
                                if let Some(proto) = self.heap.get(proto_oid) {
                                    let props: Vec<_> = proto.properties.iter()
                                        .map(|(k, v)| (*k, *v)).collect();
                                    if let Some(new_o) = self.heap.get_mut(new_oid) {
                                        for (k, v) in props {
                                            new_o.set_property(k, v);
                                        }
                                    }
                                }
                            }

                        if let Some(cv) = ctor_val
                            && cv.is_int() {
                                // Replace func on stack with this, push ctor as the call target
                                self.stack[func_pos] = this_val;
                                let packed = cv.as_int().unwrap();
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
                                    self.frames.push(CallFrame {
                                        chunk_idx, ip: 0, base: func_pos + 1,
                                        upvalues, this_value: this_val, is_constructor: true,
                                    });
                                    continue;
                                }
                            }
                        // No constructor -- just return the object with prototype methods
                        self.stack.truncate(func_pos);
                        self.push(this_val);
                        continue;
                    }

                    if func_val.is_int() {
                        let packed = func_val.as_int().unwrap();
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

                            self.frames.push(CallFrame {
                                chunk_idx,
                                ip: 0,
                                base: func_pos + 1,
                                upvalues,
                                this_value: this_val,
                                is_constructor: true,
                            });
                            continue;
                        }
                    }

                    self.stack.truncate(func_pos);
                    self.push(this_val);
                }

                OpCode::SpreadCall | OpCode::SpreadConstruct => {
                    let _argc = self.read_byte();
                    return Err(VmError::RuntimeError(format!(
                        "{opcode:?} not yet implemented"
                    )));
                }

                OpCode::SetArrayItem => {
                    let idx = {
                        let v = self.chunks[self.cur_chunk()].read_u32(self.cur_ip());
                        self.frames.last_mut().unwrap().ip += 4;
                        v as usize
                    };
                    let val = self.pop()?;
                    // Array is on stack below
                    let arr_val = self.peek()?;
                    if let Some(oid) = arr_val.as_object_id()
                        && let Some(obj) = self.heap.get_mut(oid)
                            && let ObjectKind::Array(ref mut elements) = obj.kind {
                                while elements.len() <= idx {
                                    elements.push(Value::undefined());
                                }
                                elements[idx] = val;
                            }
                }

                OpCode::ArraySpread | OpCode::ObjectSpread => {
                    let _source = self.pop()?;
                }

                OpCode::DefineDataProp => {
                    let val = self.pop()?;
                    let key = self.pop()?;
                    // Object is still on the stack
                    let obj_val = self.peek()?;
                    if let Some(oid) = obj_val.as_object_id()
                        && let Some(name_id) = key.as_string_id()
                            && let Some(obj) = self.heap.get_mut(oid) {
                                obj.set_property(name_id, val);
                            }
                }

                OpCode::DefineGetter | OpCode::DefineSetter => {
                    let _fn = self.pop()?;
                    let _key = self.pop()?;
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
                    let _pattern = self.read_u16();
                    let _flags = self.read_u16();
                    return Err(VmError::RuntimeError(
                        "CreateRegExp not yet implemented".into(),
                    ));
                }

                OpCode::Closure => {
                    let child_rel_idx = self.read_u16() as usize;
                    let current = self.cur_chunk();
                    let abs_idx = current + 1 + child_rel_idx;

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
                    self.push(Value::int(packed));
                }

                OpCode::ClosureLong => {
                    let child_rel_idx = {
                        let v = self.chunks[self.cur_chunk()].read_u32(self.cur_ip());
                        self.frames.last_mut().unwrap().ip += 4;
                        v as usize
                    };
                    let current = self.cur_chunk();
                    let abs_idx = current + 1 + child_rel_idx;
                    self.push(Value::int(abs_idx as i32));
                }

                OpCode::Class => {
                    let _name_idx = self.read_u16();
                    // Create a constructor placeholder and prototype object
                    let proto = JsObject::ordinary();
                    let proto_oid = self.heap.allocate(proto);
                    // The class itself is represented as an ordinary object with a __proto__ property
                    let mut class_obj = JsObject::ordinary();
                    let proto_key = self.interner.intern("prototype");
                    class_obj.set_property(proto_key, Value::object_id(proto_oid));
                    // Mark as class -- store proto_oid for ClassMethod to find
                    let class_oid = self.heap.allocate(class_obj);
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
                                    if let Some(class_obj) = self.heap.get_mut(class_oid) {
                                        let ctor_key = self.interner.intern("__constructor__");
                                        class_obj.set_property(ctor_key, method_val);
                                    }
                                } else {
                                    // Add method to prototype
                                    if let Some(proto) = self.heap.get_mut(proto_oid) {
                                        proto.set_property(name_id, method_val);
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
                            class_obj.set_property(name_id, method_val);
                        }
                }

                OpCode::ClassField | OpCode::ClassStaticField | OpCode::ClassPrivateMethod => {
                    let _ = self.read_u16();
                    let _val = self.pop()?; // consume field value
                }

                OpCode::Inherit => {
                    // Stack: [class, superclass]
                    let _super_val = self.pop()?;
                    // TODO: set up prototype chain
                }

                OpCode::GetSuperConstructor => {
                    self.push(Value::undefined()); // TODO
                }

                OpCode::Throw => {
                    let val = self.pop()?;
                    // Look for an exception handler
                    if let Some(handler) = self.exc_handlers.pop() {
                        // Unwind stack to the handler's saved depth
                        self.stack.truncate(handler.stack_depth);
                        // Push the thrown value (catch parameter)
                        self.push(val);
                        // Jump to catch target
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

                OpCode::GetIterator => {
                    let val = self.pop()?;
                    if let Some(oid) = val.as_object_id() {
                        let is_array = self.heap.get(oid)
                            .map(|o| matches!(&o.kind, ObjectKind::Array(_)))
                            .unwrap_or(false);
                        if is_array {
                            // Array iterator
                            let iter_obj = JsObject {
                                properties: std::collections::HashMap::new(),
                                prototype: None,
                                kind: ObjectKind::ArrayIterator(oid, 0),
                            };
                            let iter_id = self.heap.allocate(iter_obj);
                            self.push(Value::object_id(iter_id));
                        } else {
                            // Object key iterator (for...in)
                            let keys: Vec<_> = self.heap.get(oid)
                                .map(|o| o.properties.keys().copied().collect())
                                .unwrap_or_default();
                            let iter_obj = JsObject {
                                properties: std::collections::HashMap::new(),
                                prototype: None,
                                kind: ObjectKind::KeyIterator(keys, 0),
                            };
                            let iter_id = self.heap.allocate(iter_obj);
                            self.push(Value::object_id(iter_id));
                        }
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
                        let iter_info = {
                            let iter = self.heap.get(iter_oid).ok_or_else(|| {
                                VmError::RuntimeError("invalid iterator".into())
                            })?;
                            match &iter.kind {
                                ObjectKind::ArrayIterator(arr_id, idx) => (Some(*arr_id), *idx, false),
                                ObjectKind::KeyIterator(_, idx) => (None, *idx, true),
                                _ => return Err(VmError::TypeError("not an iterator".into())),
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

                OpCode::Yield
                | OpCode::YieldStar
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

                OpCode::DestructureDefault => {
                    let _ = self.read_i16();
                    return Err(VmError::RuntimeError(
                        "DestructureDefault not yet implemented".into(),
                    ));
                }

                OpCode::ImportModule => {
                    let _ = self.read_u16();
                    return Err(VmError::RuntimeError(
                        "ImportModule not yet implemented".into(),
                    ));
                }

                OpCode::ImportDynamic | OpCode::ExportDefault => {
                    return Err(VmError::RuntimeError(format!(
                        "{opcode:?} not yet implemented"
                    )));
                }

                OpCode::Export => {
                    let _name = self.read_u16();
                    let _slot = self.read_byte();
                    return Err(VmError::RuntimeError(
                        "Export not yet implemented".into(),
                    ));
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

                OpCode::TemplateTag | OpCode::CreateRestParam => {
                    let _ = self.read_byte();
                    return Err(VmError::RuntimeError(format!(
                        "{opcode:?} not yet implemented"
                    )));
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

// ---- Standalone JSON parser (avoids &mut self borrow issues) ----

fn json_parse_value<'s>(s: &'s str, heap: &mut ObjectHeap, interner: &mut Interner) -> Result<(Value, &'s str), String> {
    let s = s.trim_start();
    if s.is_empty() { return Err("unexpected end of JSON".into()); }
    match s.as_bytes()[0] {
        b'"' => json_parse_string(s, interner),
        b'{' => json_parse_object(s, heap, interner),
        b'[' => json_parse_array(s, heap, interner),
        b't' if s.starts_with("true") => Ok((Value::boolean(true), &s[4..])),
        b'f' if s.starts_with("false") => Ok((Value::boolean(false), &s[5..])),
        b'n' if s.starts_with("null") => Ok((Value::null(), &s[4..])),
        b'-' | b'0'..=b'9' => json_parse_number(s),
        _ => Err(format!("unexpected char in JSON: {}", s.chars().next().unwrap())),
    }
}

fn json_parse_string<'s>(s: &'s str, interner: &mut Interner) -> Result<(Value, &'s str), String> {
    let s = &s[1..];
    let mut result = String::new();
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => { let id = interner.intern(&result); return Ok((Value::string(id), &s[i + 1..])); }
            '\\' => { if let Some((_, esc)) = chars.next() { match esc {
                '"' => result.push('"'), '\\' => result.push('\\'), '/' => result.push('/'),
                'n' => result.push('\n'), 'r' => result.push('\r'), 't' => result.push('\t'),
                _ => { result.push('\\'); result.push(esc); }
            }}}
            _ => result.push(c),
        }
    }
    Err("unterminated string".into())
}

fn json_parse_number(s: &str) -> Result<(Value, &str), String> {
    let mut end = 0;
    let b = s.as_bytes();
    if end < b.len() && b[end] == b'-' { end += 1; }
    while end < b.len() && b[end].is_ascii_digit() { end += 1; }
    if end < b.len() && b[end] == b'.' { end += 1; while end < b.len() && b[end].is_ascii_digit() { end += 1; } }
    if end < b.len() && (b[end] == b'e' || b[end] == b'E') { end += 1; if end < b.len() && (b[end] == b'+' || b[end] == b'-') { end += 1; } while end < b.len() && b[end].is_ascii_digit() { end += 1; } }
    let n: f64 = s[..end].parse().map_err(|_| "invalid number".to_string())?;
    Ok((Value::number(n), &s[end..]))
}

fn json_parse_object<'s>(s: &'s str, heap: &mut ObjectHeap, interner: &mut Interner) -> Result<(Value, &'s str), String> {
    let mut s = &s[1..];
    let mut obj = JsObject::ordinary();
    s = s.trim_start();
    if let Some(rest) = s.strip_prefix('}') { let oid = heap.allocate(obj); return Ok((Value::object_id(oid), rest)); }
    loop {
        s = s.trim_start();
        let (key, rest) = json_parse_string(s, interner)?;
        s = rest.trim_start();
        if let Some(rest) = s.strip_prefix(':') { s = rest; } else { return Err("expected ':'".into()); }
        let (val, rest) = json_parse_value(s, heap, interner)?;
        s = rest;
        if let Some(kid) = key.as_string_id() { obj.set_property(kid, val); }
        s = s.trim_start();
        if let Some(rest) = s.strip_prefix(',') { s = rest; continue; }
        if let Some(rest) = s.strip_prefix('}') { s = rest; break; }
        return Err("expected ',' or '}'".into());
    }
    let oid = heap.allocate(obj);
    Ok((Value::object_id(oid), s))
}

fn json_parse_array<'s>(s: &'s str, heap: &mut ObjectHeap, interner: &mut Interner) -> Result<(Value, &'s str), String> {
    let mut s = &s[1..];
    let mut elems = Vec::new();
    s = s.trim_start();
    if let Some(rest) = s.strip_prefix(']') { let o = JsObject::array(elems); let oid = heap.allocate(o); return Ok((Value::object_id(oid), rest)); }
    loop {
        let (val, rest) = json_parse_value(s, heap, interner)?;
        s = rest; elems.push(val);
        s = s.trim_start();
        if let Some(rest) = s.strip_prefix(',') { s = rest; continue; }
        if let Some(rest) = s.strip_prefix(']') { s = rest; break; }
        return Err("expected ',' or ']'".into());
    }
    let o = JsObject::array(elems);
    let oid = heap.allocate(o);
    Ok((Value::object_id(oid), s))
}

// ---------------------------------------------------------------------------
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
        // Result should be a string -- we need a fresh interner reference to check.
        assert!(result.is_string());
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
