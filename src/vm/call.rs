use crate::compiler::chunk::ChunkFlags;
use crate::runtime::value::Value;

use super::vm::{Vm, VmError, CallFrame};

impl Vm {
    /// Call a closure value with the given arguments and run it to completion.
    pub fn call_function(&mut self, func_val: Value, args: &[Value]) -> Result<Value, VmError> {
        self.call_function_this(func_val, Value::undefined(), args)
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

    /// Call a closure with a specific `this` binding.
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
        if !func_val.is_function() {
            return Ok(Value::undefined());
        }
        let packed = func_val.as_function().unwrap();
        // Native global function sentinels (no this)
        if (-536..=-500).contains(&packed) {
            return Ok(self.exec_global_fn(packed, args));
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
        if (-635..=-590).contains(&packed) {
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
        // Promise combinator (Promise.all/race/allSettled/any) resolve callback.
        // Mirrors the inline Call opcode encoding so microtask drain can route.
        if packed <= -800_000 && packed > -900_000 {
            let encoded = (-800_000 - packed) as u32;
            let tracker_oid = crate::runtime::object::ObjectId(encoded / 1024);
            let index = (encoded % 1024) as usize;
            let val = args.first().copied().unwrap_or(Value::undefined());
            self.handle_combinator_resolve(tracker_oid, index, val)?;
            return Ok(Value::undefined());
        }
        // Promise combinator reject callback.
        if packed <= -900_000 && packed > -1_000_000 {
            let encoded = (-900_000 - packed) as u32;
            let tracker_oid = crate::runtime::object::ObjectId(encoded / 1024);
            let index = (encoded % 1024) as usize;
            let val = args.first().copied().unwrap_or(Value::undefined());
            self.handle_combinator_reject(tracker_oid, index, val)?;
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

        let upvalues = if closure_id < self.closure_upvalues.len() {
            self.closure_upvalues[closure_id].clone()
        } else {
            Vec::new()
        };

        // Arrow functions inherit `this` from the enclosing scope.
        let effective_this = if self.chunks[chunk_idx].flags.contains(ChunkFlags::ARROW) {
            self.frames.last().map(|f| f.this_value).unwrap_or(Value::undefined())
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
            self.frames.last().map(|f| f.new_target).unwrap_or(Value::undefined())
        } else {
            Value::undefined()
        };
        self.frames.push(CallFrame {
            chunk_idx, ip: 0, base: func_pos + 1,
            upvalues, this_value: effective_this, is_constructor: false,
            pending_super_call: false, generator_id: None, argc: args.len(),
            saved_args: args.to_vec(), arguments_oid: None, is_derived_ctor: false, super_called: false,
            new_target,
            await_super_result: false,
        });

        // Run using the full main dispatch loop, stopping when our frame returns.
        let result = self.run_until(stop_depth);
        if result.is_err() {
            // Clean up any frames and stack slots that weren't popped due to the error.
            // This keeps the VM in a consistent state when the caller swallows the error.
            while self.frames.len() > stop_depth {
                self.frames.pop();
            }
            self.stack.truncate(func_pos);
        }
        result
    }
}
