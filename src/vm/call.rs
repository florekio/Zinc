use crate::compiler::opcode::OpCode;
use crate::runtime::object::{ObjectId, ObjectKind, PromiseState};
use crate::runtime::value::Value;

use super::vm::{Vm, VmError, Upvalue, UpvalueLocation, CallFrame};

impl Vm {
    /// Call a closure value with the given arguments and run it to completion.
    /// Saves/restores the main run loop's frame depth so the callback executes
    /// as a nested call and returns its result.
    pub(crate) fn call_function(&mut self, func_val: Value, args: &[Value]) -> Result<Value, VmError> {
        if !func_val.is_function() {
            return Ok(Value::undefined());
        }
        let packed = func_val.as_function().unwrap();
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
            pending_super_call: false, generator_id: None,
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
                    self.push(Value::function(packed));
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
                    if cb_func.is_function() {
                        let s = cb_func.as_function().unwrap();
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
                                        && mv.is_function() && mv.as_function().unwrap() <= -100 && mv.as_function().unwrap() >= -102 {
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
                    if obj_val.is_function() && obj_val.as_function() == Some(-520)
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
}
