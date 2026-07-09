use crate::compiler::chunk::ChunkFlags;
use crate::runtime::value::Value;

use super::vm::{Vm, VmError, CallFrame};

impl Vm {
    /// Call a closure value with the given arguments and run it to completion.
    pub fn call_function(&mut self, func_val: Value, args: &[Value]) -> Result<Value, VmError> {
        self.call_function_this(func_val, Value::undefined(), args)
    }

    /// If the callee captured a with-scope chain at creation (closure created
    /// inside a `with` body), push it onto the with-stack for the duration of
    /// the call. Returns the with-stack length from *before* the push — the
    /// value for the new frame's `with_base`, so the return-path truncation
    /// pops the captured entries again.
    pub(crate) fn with_base_for_call(&mut self, closure_id: usize) -> usize {
        let base = self.with_stack.len();
        if let Some(captured) = self.closure_withs.get(&closure_id) {
            let captured = captured.clone();
            self.with_stack.extend(captured.iter().copied());
        }
        base
    }

    /// Like `call_function_this`, but if the target is an async function the
    /// body is invoked and its result/throw is wrapped into a fulfilled or
    /// rejected Promise (matching the regular call dispatch path).
    pub(crate) fn call_with_async_wrap(
        &mut self,
        func_val: Value,
        this_value: Value,
        args: &[Value],
    ) -> Result<Value, VmError> {
        if let Some(packed) = func_val.as_function() {
            let chunk_idx = (packed & 0xFFFF) as usize;
            if packed >= 0
                && chunk_idx >= 1
                && chunk_idx < self.chunks.len()
                && self.chunks[chunk_idx].flags.contains(ChunkFlags::ASYNC)
            {
                let promise_id = self.allocate_promise();
                match self.call_function_this(func_val, this_value, args) {
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
                return Ok(Value::object_id(promise_id));
            }
        }
        self.call_function_this(func_val, this_value, args)
    }

    /// Build a bound-function object over `target` (a function value —
    /// sentinel, packed bytecode closure, or function object), per
    /// Function.prototype.bind. Shared by the named `.bind(...)` method
    /// dispatch and the extracted-value sentinel (-597).
    pub(crate) fn make_bound_function(&mut self, target: Value, bound_this: Value, bound_args: Vec<Value>) -> Value {
        use crate::runtime::object::{JsObject, ObjectKind, FunctionKind};
        let func_obj_id = if let Some(oid) = target.as_object_id() {
            oid
        } else if let Some(packed) = target.as_function() {
            if packed < 0 {
                let fobj = JsObject {
                    properties: Vec::new(), prototype: None,
                    kind: ObjectKind::Function(FunctionKind::NativeSentinel { sentinel: packed }),
                    marked: false, extensible: true,
                };
                self.heap.allocate(fobj)
            } else {
                // Keep the FULL packed value (closure id in the high
                // bits) so the bound function sees its upvalues.
                let chunk_only = (packed & 0xFFFF) as usize;
                let name = if chunk_only < self.chunks.len() { self.chunks[chunk_only].name } else { self.interner.intern("<bound>") };
                let fobj = JsObject::function_bytecode(packed as usize, name);
                self.heap.allocate(fobj)
            }
        } else {
            // Not callable: produce a bound shell over undefined — calls
            // will return undefined like other non-callable paths.
            let fobj = JsObject {
                properties: Vec::new(), prototype: None,
                kind: ObjectKind::Function(FunctionKind::NativeSentinel { sentinel: 0 }),
                marked: false, extensible: true,
            };
            self.heap.allocate(fobj)
        };
        let bound = JsObject {
            properties: Vec::new(), prototype: None,
            kind: ObjectKind::Function(FunctionKind::Bound {
                target: func_obj_id,
                this_val: bound_this,
                args: bound_args,
            }),
            marked: false, extensible: true,
        };
        Value::object_id(self.heap.allocate(bound))
    }

    pub fn call_function_this(&mut self, func_val: Value, this_value: Value, args: &[Value]) -> Result<Value, VmError> {
        // Host-supplied native function (heap-allocated callable).
        if let Some(oid) = func_val.as_object_id() {
            let native_fn = self.heap.get(oid).and_then(|o| {
                if let crate::runtime::object::ObjectKind::Function(
                    crate::runtime::object::FunctionKind::Native { func, .. },
                ) = &o.kind {
                    Some(func.clone())
                } else { None }
            });
            if let Some(func) = native_fn {
                return match (func)(self, this_value, args) {
                    Ok(v) => Ok(v),
                    Err(reason) => Err(VmError::Throw(reason)),
                };
            }
        }
        // Class-like objects: if the callee carries a `__constructor__`
        // slot, the runtime treats it as callable (mirrors typeof
        // returning "function"). Unwrap to the underlying function and
        // dispatch through that — otherwise call.apply.bind / super()
        // and any other indirect call would silently produce undefined
        // and break Closure-compiled bundles (google.com /search).
        if !func_val.is_function() && let Some(oid) = func_val.as_object_id() {
            let ctor_key = self.interner.intern("__constructor__");
            if let Some(ctor) = self.heap.get(oid)
                .and_then(|o| o.get_property(ctor_key))
                .filter(|v| v.is_function())
            {
                return self.call_function_this(ctor, this_value, args);
            }
        }
        // Bound / bytecode / sentinel FUNCTION OBJECTS: unwrap to the
        // underlying callable. `arr[i](...)` desugars to `.call(...)`
        // which lands here, and React's useState setter is a bound
        // function read out of the hook array — both silently produced
        // undefined before.
        if !func_val.is_function() && let Some(oid) = func_val.as_object_id() {
            use crate::runtime::object::FunctionKind as FK;
            enum Unwrapped {
                Bound(crate::runtime::object::ObjectId, Value, Vec<Value>),
                Direct(i32),
            }
            let unwrapped = self.heap.get(oid).and_then(|o| match &o.kind {
                crate::runtime::object::ObjectKind::Function(FK::Bound { target, this_val, args }) =>
                    Some(Unwrapped::Bound(*target, *this_val, args.clone())),
                crate::runtime::object::ObjectKind::Function(FK::Bytecode { chunk_idx, .. }) =>
                    Some(Unwrapped::Direct(*chunk_idx as i32)),
                crate::runtime::object::ObjectKind::Function(FK::NativeSentinel { sentinel }) =>
                    Some(Unwrapped::Direct(*sentinel)),
                _ => None,
            });
            match unwrapped {
                Some(Unwrapped::Bound(target, bound_this, bound_args)) => {
                    let full: Vec<Value> =
                        bound_args.into_iter().chain(args.iter().copied()).collect();
                    // Recurse: the target may itself be a function object.
                    return self.call_function_this(Value::object_id(target), bound_this, &full);
                }
                Some(Unwrapped::Direct(packed)) => {
                    return self.call_function_this(Value::function(packed), this_value, args);
                }
                None => {}
            }
        }
        if !func_val.is_function() {
            return Ok(Value::undefined());
        }
        let packed = func_val.as_function().unwrap();
        // Function.prototype.call / apply / bind extracted as VALUES
        // (sentinels -595/-596/-597). `this_value` is the target
        // function. core-js's uncurryThis is `bind.bind(call, call)` —
        // calling its result lands here with this = the function being
        // uncurried; without this dispatch every uncurried primordial
        // silently returned undefined.
        if packed == -595 {
            let new_this = args.first().copied().unwrap_or(Value::undefined());
            let rest: &[Value] = args.get(1..).unwrap_or(&[]);
            return self.call_function_this(this_value, new_this, rest);
        }
        if packed == -596 {
            let new_this = args.first().copied().unwrap_or(Value::undefined());
            let list: Vec<Value> = args.get(1).and_then(|v| v.as_object_id())
                .and_then(|oid| self.heap.get(oid))
                .and_then(|o| if let crate::runtime::object::ObjectKind::Array(e) = &o.kind { Some(e.clone()) } else { None })
                .unwrap_or_default();
            return self.call_function_this(this_value, new_this, &list);
        }
        if packed == -597 {
            let bound_this = args.first().copied().unwrap_or(Value::undefined());
            let bound_args: Vec<Value> = args.get(1..).map(|s| s.to_vec()).unwrap_or_default();
            return Ok(self.make_bound_function(this_value, bound_this, bound_args));
        }
        // Extracted String.prototype methods (sentinels -200 - idx).
        // `this_value` is the receiver string; core-js uncurries these
        // constantly (`b("".slice)`, `b("".charCodeAt)`, ...).
        if (-224..=-200).contains(&packed) {
            const STRING_METHOD_NAMES: &[&str] = &[
                "charAt", "charCodeAt", "indexOf", "lastIndexOf", "includes",
                "startsWith", "endsWith", "slice", "substring", "toUpperCase",
                "toLowerCase", "trim", "trimStart", "trimEnd", "split",
                "replace", "repeat", "padStart", "padEnd", "concat",
                "match", "search", "replaceAll", "codePointAt", "at",
            ];
            let idx = (-200 - packed) as usize;
            if idx < STRING_METHOD_NAMES.len() {
                let s = if self.is_cons_string(this_value) {
                    self.flatten_cons_to_string(this_value)
                } else if let Some(sid) = this_value.as_string_id() {
                    self.interner.resolve(sid).to_owned()
                } else {
                    self.value_to_string(this_value)
                };
                let ascii = self.string_is_ascii(this_value);
                let mid = self.interner.intern(STRING_METHOD_NAMES[idx]);
                return Ok(self.exec_string_method(&s, mid, args, ascii));
            }
        }
        // Native global function sentinels (no this)
        if (-536..=-500).contains(&packed) && packed != -507 {
            return Ok(self.exec_global_fn(packed, args));
        }
        if packed == -507 {
            // Array called as a function constructs, per spec.
            let elements: Vec<Value> = if args.len() == 1 {
                if let Some(n) = args[0].as_number()
                    && n.is_finite() && n.fract() == 0.0 && n >= 0.0 && n <= u32::MAX as f64
                {
                    vec![Value::undefined(); (n as usize).min(10_000_000)]
                } else if let Some(n) = args[0].as_int() {
                    if n >= 0 { vec![Value::undefined(); (n as usize).min(10_000_000)] } else { vec![args[0]] }
                } else {
                    vec![args[0]]
                }
            } else {
                args.to_vec()
            };
            let mut arr_obj = crate::runtime::object::JsObject::array(elements);
            arr_obj.prototype = Some(self.array_prototype);
            let oid = self.heap.allocate(arr_obj);
            return Ok(Value::object_id(oid));
        }
        if packed == -750 {
            // Extracted Object.assign value
            return Ok(self.exec_object_assign(args));
        }
        if packed == -751 {
            // Extracted Array.isArray value
            return Ok(self.exec_global_fn(-507, args));
        }
        if packed == -752 {
            return Ok(self.exec_symbol_for(args));
        }
        if packed == -753 {
            return Ok(self.exec_symbol_key_for(args));
        }
        // Math method sentinels (-700 to -726)
        if (-726..=-700).contains(&packed) {
            return Ok(self.exec_math_sentinel(packed, args));
        }
        // Date() called as function (not constructor) returns current date string
        if packed == -550 {
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as f64)
                .unwrap_or(0.0);
            let s = crate::vm::vm::format_date(ms);
            let id = self.interner.intern(&s);
            return Ok(Value::string(id));
        }
        // Native this-dependent method sentinels (-590 to -599 and -600 to -629 for Array.prototype)
        if (-635..=-590).contains(&packed) || packed == -639 || packed == -640 {
            return Ok(self.exec_native_method(packed, this_value, args));
        }
        // Promise resolve/reject sentinels (used by promise chaining for thenable
        // adoption). Encoding mirrors the Call opcode handler.
        if packed <= -600_000 && packed > -700_000 {
            let pid = crate::runtime::object::ObjectId((-600_000 - packed) as u32);
            let val = args.first().copied().unwrap_or(Value::undefined());
            self.resolve_promise(pid, val)?;
            return Ok(Value::undefined());
        }
        if packed <= -700_000 && packed > -800_000 {
            let pid = crate::runtime::object::ObjectId((-700_000 - packed) as u32);
            let val = args.first().copied().unwrap_or(Value::undefined());
            self.reject_promise(pid, val)?;
            return Ok(Value::undefined());
        }
        // Promise combinator (Promise.all/race/allSettled/any) callbacks.
        // Mirrors the inline Call opcode encoding so microtask drain can route.
        if packed <= -1_000_000_000 && packed > -2_100_000_000 {
            let encoded = (-1_000_000_000i64 - packed as i64) as u32;
            let tracker_oid = crate::runtime::object::ObjectId(encoded / 2048);
            let index = ((encoded % 2048) / 2) as usize;
            let is_reject = encoded & 1 == 1;
            let val = args.first().copied().unwrap_or(Value::undefined());
            if is_reject {
                self.handle_combinator_reject(tracker_oid, index, val)?;
            } else {
                self.handle_combinator_resolve(tracker_oid, index, val)?;
            }
            return Ok(Value::undefined());
        }
        // Promise.prototype.finally fulfill wrapper: call the user finally cb,
        // then propagate the original value.
        if packed <= -1_100_000 && packed > -1_200_000 {
            let tracker_oid = crate::runtime::object::ObjectId((-1_100_000 - packed) as u32);
            let val = args.first().copied().unwrap_or(Value::undefined());
            if let Some(obj) = self.heap.get(tracker_oid)
                && let crate::runtime::object::ObjectKind::FinallyTracker { callback, .. } = &obj.kind
            {
                let cb = *callback;
                let _ = self.call_function(cb, &[]);
            }
            return Ok(val);
        }
        if packed <= -1_200_000 && packed > -1_300_000 {
            let tracker_oid = crate::runtime::object::ObjectId((-1_200_000 - packed) as u32);
            let val = args.first().copied().unwrap_or(Value::undefined());
            if let Some(obj) = self.heap.get(tracker_oid)
                && let crate::runtime::object::ObjectKind::FinallyTracker { callback, .. } = &obj.kind
            {
                let cb = *callback;
                let _ = self.call_function(cb, &[]);
            }
            // Reject: bubble the rejection by returning Throw.
            return Err(VmError::Throw(val));
        }
        let closure_id = ((packed as u32) >> 16) as usize;
        let chunk_idx = (packed & 0xFFFF) as usize;
        if chunk_idx < 1 || chunk_idx >= self.chunks.len() {
            return Ok(Value::undefined());
        }

        // Generator function: fall through to the normal call path. The body's
        // `CreateGenerator` opcode (emitted in the prologue) captures frame state
        // and returns a generator object — this makes parameter destructuring
        // and default-value evaluation eager, per spec.

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
        // Drop args beyond the declared params so the callee's locals (which the
        // compiler places at slot = param_count) don't alias extra arguments —
        // e.g. a `.map` callback that declares one param but is handed
        // (element, index, array). `arguments` still sees them via saved_args
        // below (built from the `args` slice, not the stack).
        self.stack.truncate(func_pos + 1 + expected);

        let upvalues = if closure_id < self.closure_upvalues.len() {
            self.closure_upvalues[closure_id].clone()
        } else {
            Vec::new()
        };

        // Arrow functions use the `this` captured at creation (lexical).
        let effective_this = if self.chunks[chunk_idx].flags.contains(ChunkFlags::ARROW) {
            self.closure_arrow_ctx.get(&closure_id).map(|(t, _)| *t)
                .or_else(|| self.frames.last().map(|f| f.this_value))
                .unwrap_or(Value::undefined())
        } else if !self.chunks[chunk_idx].flags.contains(ChunkFlags::STRICT) {
            // Non-strict: coerce null/undefined to globalThis, and primitive
            // values (number/string/boolean) to their wrapper object.
            if this_value.is_undefined() || this_value.is_null() {
                Value::object_id(self.global_this_oid)
            } else if this_value.is_int() || this_value.is_number()
                || this_value.is_string() || self.is_cons_string(this_value)
                || this_value.as_bool().is_some()
            {
                self.box_primitive(this_value)
            } else {
                this_value
            }
        } else {
            this_value
        };

        let stop_depth = self.frames.len();

        // Arrow functions inherit new.target from the enclosing scope; ordinary
        // calls (not via `new`) have new.target = undefined.
        let new_target = if self.chunks[chunk_idx].flags.contains(ChunkFlags::ARROW) {
            self.closure_arrow_ctx.get(&closure_id).map(|(_, nt)| *nt)
                .or_else(|| self.frames.last().map(|f| f.new_target))
                .unwrap_or(Value::undefined())
        } else {
            Value::undefined()
        };
        // Direct eval: inherit the caller's with-visibility (the frame shares
        // the caller's slice; it owns no entries of its own to pop on error).
        let inherited_base = self.eval_inherit_with_base.take();
        let with_base = inherited_base.unwrap_or_else(|| self.with_base_for_call(closure_id));
        self.frames.push(CallFrame {
            chunk_idx, ip: 0, base: func_pos + 1,
            upvalues, this_value: effective_this, is_constructor: false,
            pending_super_call: false, generator_id: None, argc: args.len(),
            saved_args: args.to_vec(), arguments_oid: None, is_derived_ctor: false, super_called: false,
            new_target,
            await_super_result: false,
            with_base,
        });

        // Run using the full main dispatch loop, stopping when our frame returns.
        let result = self.run_until(stop_depth);
        if result.is_err() {
            // Clean up any frames and stack slots that weren't popped due to the error.
            // This keeps the VM in a consistent state when the caller swallows the error.
            while self.frames.len() > stop_depth {
                self.frames.pop();
            }
            self.truncate_stack(func_pos);
            // Pop captured with-entries this call pushed — but not for an
            // eval frame, whose with_base points into the caller's own slice.
            if inherited_base.is_none() {
                self.with_stack.truncate(with_base);
            }
        }
        result
    }
}
