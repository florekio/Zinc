use crate::runtime::object::{GeneratorState, JsObject, ObjectId, ObjectKind};
use crate::runtime::value::Value;
use crate::util::interner::StringId;

use super::vm::{CallFrame, Upvalue, UpvalueLocation, Vm, VmError};

/// Signal from generator method dispatch back to the main run loop.
pub(crate) enum GeneratorAction {
    /// Generator is done — return this value directly.
    Done(Value),
    /// Generator frame was pushed — continue the main run loop.
    Resumed,
}

impl Vm {
    /// Create a `{value, done}` iterator result object.
    pub(crate) fn make_iter_result(&mut self, value: Value, done: bool) -> Result<Value, VmError> {
        let mut obj = JsObject::ordinary();
        let value_key = self.interner.intern("value");
        let done_key = self.interner.intern("done");
        obj.set_property(value_key, value);
        obj.set_property(done_key, Value::boolean(done));
        let oid = self.heap.allocate(obj);
        Ok(Value::object_id(oid))
    }

    /// Dispatch a generator method call (.next, .return, .throw).
    /// Returns `Resumed` if a generator frame was pushed (caller should `continue` the main loop).
    /// Returns `Done(val)` if the result is immediately available.
    pub(crate) fn exec_generator_method(
        &mut self,
        gen_oid: ObjectId,
        method_name: StringId,
        args: &[Value],
    ) -> Result<GeneratorAction, VmError> {
        let name = self.interner.resolve(method_name).to_owned();
        // yield* delegation: while the generator is suspended inside a
        // yield*, next/throw/return forward to the inner iterator.
        {
            let del_key = self.interner.intern("__yield_star_iter__");
            let suspended = self.heap.get(gen_oid).is_some_and(|o| matches!(
                &o.kind,
                ObjectKind::Generator { state: GeneratorState::SuspendedYield, .. }
            ));
            if suspended
                && let Some(iter_val) = self.heap.get(gen_oid).and_then(|o| o.get_property(del_key))
                && !iter_val.is_undefined()
            {
                return self.yield_star_delegate(gen_oid, iter_val, &name, args);
            }
        }
        match name.as_str() {
            "next" => {
                let input = args.first().copied().unwrap_or(Value::undefined());
                self.generator_resume(gen_oid, input)
            }
            "return" => {
                let val = args.first().copied().unwrap_or(Value::undefined());
                let suspended_mid_body = self.heap.get(gen_oid).is_some_and(|o| matches!(
                    &o.kind,
                    ObjectKind::Generator { state: GeneratorState::SuspendedYield, .. }
                ));
                if suspended_mid_body {
                    // Resume with a return completion: unwind from the yield
                    // point with a unique sentinel so try/finally blocks run;
                    // when the sentinel escapes the frame, the close is done.
                    let sentinel_oid = self.heap.allocate(JsObject::ordinary());
                    match self.generator_abrupt_resume(
                        gen_oid,
                        Value::object_id(sentinel_oid),
                        Some(sentinel_oid),
                    )? {
                        AbruptOutcome::SentinelEscaped => {
                            let result = self.make_iter_result(val, true)?;
                            Ok(GeneratorAction::Done(result))
                        }
                        AbruptOutcome::Threw(v) => Err(VmError::Throw(v)),
                        AbruptOutcome::Completed(result) => Ok(GeneratorAction::Done(result)),
                    }
                } else {
                    if let Some(obj) = self.heap.get_mut(gen_oid)
                        && let ObjectKind::Generator { state, .. } = &mut obj.kind
                    {
                        *state = GeneratorState::Completed;
                    }
                    let result = self.make_iter_result(val, true)?;
                    Ok(GeneratorAction::Done(result))
                }
            }
            "throw" => {
                let exc = args.first().copied().unwrap_or(Value::undefined());
                let suspended_mid_body = self.heap.get(gen_oid).is_some_and(|o| matches!(
                    &o.kind,
                    ObjectKind::Generator { state: GeneratorState::SuspendedYield, .. }
                ));
                if suspended_mid_body {
                    // Resume throwing `exc` at the yield point: catch blocks may
                    // handle it (the generator continues to a yield/return), and
                    // finallys run before it escapes.
                    match self.generator_abrupt_resume(gen_oid, exc, None)? {
                        AbruptOutcome::SentinelEscaped => Err(VmError::Throw(exc)),
                        AbruptOutcome::Threw(v) => Err(VmError::Throw(v)),
                        AbruptOutcome::Completed(result) => Ok(GeneratorAction::Done(result)),
                    }
                } else {
                    if let Some(obj) = self.heap.get_mut(gen_oid)
                        && let ObjectKind::Generator { state, .. } = &mut obj.kind
                    {
                        *state = GeneratorState::Completed;
                    }
                    Err(VmError::Throw(exc))
                }
            }
            _ => Ok(GeneratorAction::Done(Value::undefined())),
        }
    }


    /// Suspend the CURRENT (innermost) frame as generator `gid`: capture its
    /// stack/upvalues, detach its exception handlers, record the resume ip,
    /// and pop the frame. Shared by Yield and YieldStar.
    pub(crate) fn suspend_current_generator(&mut self, gid: ObjectId) {
        let frame = self.frames.last().unwrap();
        let base = frame.base;
        let ip = frame.ip;
        let this_value = frame.this_value;
        let saved_stack: Vec<Value> = self.stack[base..].to_vec();
        let saved_upvalues: Vec<Value> =
            frame.upvalues.iter().map(|uv| uv.get(&self.stack)).collect();

        // Detach this frame's exception handlers: their frame index and stack
        // depth are only valid for THIS activation. They're re-attached
        // (repositioned) when the generator resumes.
        let fidx = self.frames.len() - 1;
        let mut handlers = Vec::new();
        while let Some(h) = self.exc_handlers.last() {
            if h.frame_idx == fidx {
                let h = self.exc_handlers.pop().unwrap();
                handlers.push(crate::runtime::object::SavedExcHandler {
                    catch_target: h.catch_target,
                    finally_target: h.finally_target,
                    rel_stack_depth: h.stack_depth.saturating_sub(base),
                });
            } else {
                break;
            }
        }
        handlers.reverse();

        if let Some(obj) = self.heap.get_mut(gid)
            && let ObjectKind::Generator { state, ip: saved_ip, saved_stack: ss, saved_upvalues: su, this_value: tv, saved_handlers: sh, .. } = &mut obj.kind
        {
            *state = GeneratorState::SuspendedYield;
            *saved_ip = ip;
            *ss = saved_stack;
            *su = saved_upvalues;
            *tv = this_value;
            *sh = handlers;
        }

        self.frames.pop();
        self.truncate_stack(base - 1); // remove placeholder too
    }

    /// Read a property of an iterator result, running an accessor if one is
    /// defined (`get done() { throw ... }` must propagate). Errors surface as
    /// Err(Throw).
    pub(crate) fn read_iter_prop(&mut self, obj_val: Value, name: &str) -> Result<Value, VmError> {
        let Some(oid) = obj_val.as_object_id() else {
            return Ok(Value::undefined());
        };
        let getter_key = self.interner.intern(&format!("__get_{name}__"));
        if let Some(gfn) = self.heap.get_property_chain(oid, getter_key)
            && gfn.is_function()
        {
            let prev = self.protect_throw_depth;
            self.protect_throw_depth = self.frames.len() + 1;
            let r = self.call_function_this(gfn, obj_val, &[]);
            self.protect_throw_depth = prev;
            return r;
        }
        let key = self.interner.intern(name);
        Ok(self.heap.get_property_chain(oid, key).unwrap_or(Value::undefined()))
    }

    /// GetMethod for the iteration protocol: getter-aware read of
    /// `iter[name]`; Ok(None) when undefined/null. Non-callable values are
    /// returned as Some — callers decide whether that's a TypeError.
    fn get_iter_method(&mut self, iter_val: Value, name: &str) -> Result<Option<Value>, VmError> {
        let v = self.read_iter_prop(iter_val, name)?;
        if v.is_undefined() || v.is_null() {
            return Ok(None);
        }
        Ok(Some(v))
    }

    /// Invoke `iter.<method>(args)` following the iteration protocol.
    /// Generators dispatch through their intrinsic methods (running the
    /// resumed frame to its next suspension); everything else through a
    /// protected call. Errors surface as Err(Throw).
    pub(crate) fn iter_protocol_call(
        &mut self,
        iter_val: Value,
        method: &str,
        args: &[Value],
    ) -> Result<Value, VmError> {
        if let Some(oid) = iter_val.as_object_id()
            && self.heap.get(oid).is_some_and(|o| matches!(o.kind, ObjectKind::Generator { .. }))
        {
            let mname = self.interner.intern(method);
            return match self.exec_generator_method(oid, mname, args)? {
                GeneratorAction::Done(v) => Ok(v),
                GeneratorAction::Resumed => {
                    let depth = self.frames.len() - 1;
                    self.run_until(depth)
                }
            };
        }
        let m = self.get_iter_method(iter_val, method)?;
        let Some(m) = m else {
            return Err(VmError::Throw(self.make_native_error(
                "TypeError",
                &format!("The iterator does not provide a '{method}' method"),
            )));
        };
        let prev = self.protect_throw_depth;
        self.protect_throw_depth = self.frames.len() + 1;
        let r = self.call_function_this(m, iter_val, args);
        self.protect_throw_depth = prev;
        r
    }

    /// Clear the yield* delegation marker on a generator.
    fn clear_delegation(&mut self, gen_oid: ObjectId) {
        let del_key = self.interner.intern("__yield_star_iter__");
        if let Some(obj) = self.heap.get_mut(gen_oid) {
            obj.set_property(del_key, Value::undefined());
        }
    }

    /// Resume the outer generator by throwing `exc` at the suspended yield*
    /// (after clearing delegation): its catch/finally blocks see the error.
    fn resume_outer_throw(&mut self, gen_oid: ObjectId, exc: Value) -> Result<GeneratorAction, VmError> {
        self.clear_delegation(gen_oid);
        match self.generator_abrupt_resume(gen_oid, exc, None)? {
            AbruptOutcome::SentinelEscaped => Err(VmError::Throw(exc)),
            AbruptOutcome::Threw(v) => Err(VmError::Throw(v)),
            AbruptOutcome::Completed(result) => Ok(GeneratorAction::Done(result)),
        }
    }

    /// Handle next/throw/return on a generator suspended in `yield*`
    /// delegation: forward the operation to the inner iterator per
    /// 27.5.3.7 (yield* runtime semantics).
    fn yield_star_delegate(
        &mut self,
        gen_oid: ObjectId,
        iter_val: Value,
        method: &str,
        args: &[Value],
    ) -> Result<GeneratorAction, VmError> {
        let arg = args.first().copied().unwrap_or(Value::undefined());
        // Generators have intrinsic next/throw/return (dispatched by kind,
        // not own properties) — skip the GetMethod probing for them.
        let inner_is_gen = iter_val.as_object_id()
            .and_then(|oid| self.heap.get(oid))
            .is_some_and(|o| matches!(o.kind, ObjectKind::Generator { .. }));
        match method {
            "next" | "throw" => {
                if method == "throw" && !inner_is_gen {
                    // GetMethod(iter, "throw"); errors and missing methods
                    // surface inside the outer generator.
                    let m = match self.get_iter_method(iter_val, "throw") {
                        Ok(m) => m,
                        Err(VmError::Throw(e)) => return self.resume_outer_throw(gen_oid, e),
                        Err(e) => return Err(e),
                    };
                    if m.is_none() {
                        // Close the inner iterator, then throw TypeError into
                        // the outer generator.
                        if let Ok(Some(_)) = self.get_iter_method(iter_val, "return") {
                            let _ = self.iter_protocol_call(iter_val, "return", &[]);
                        }
                        let err = self.make_native_error(
                            "TypeError",
                            "The iterator does not provide a 'throw' method",
                        );
                        return self.resume_outer_throw(gen_oid, err);
                    }
                }
                let step = self.iter_protocol_call(iter_val, method, &[arg]);
                let res = match step {
                    Ok(r) => r,
                    Err(VmError::Throw(e)) => return self.resume_outer_throw(gen_oid, e),
                    Err(e) => return Err(e),
                };
                if res.as_object_id().is_none() {
                    let err = self.make_native_error("TypeError", "Iterator result is not an object");
                    return self.resume_outer_throw(gen_oid, err);
                }
                let done = match self.read_iter_prop(res, "done") {
                    Ok(v) => self.truthy(v),
                    Err(VmError::Throw(e)) => return self.resume_outer_throw(gen_oid, e),
                    Err(e) => return Err(e),
                };
                if !done {
                    // Forward the inner result verbatim; stay delegating.
                    return Ok(GeneratorAction::Done(res));
                }
                let value = match self.read_iter_prop(res, "value") {
                    Ok(v) => v,
                    Err(VmError::Throw(e)) => return self.resume_outer_throw(gen_oid, e),
                    Err(e) => return Err(e),
                };
                // Inner completed: resume the outer with its value as the
                // result of the yield* expression.
                self.clear_delegation(gen_oid);
                match self.generator_resume(gen_oid, value)? {
                    GeneratorAction::Resumed => Ok(GeneratorAction::Resumed),
                    done => Ok(done),
                }
            }
            "return" => {
                if !inner_is_gen {
                    let m = match self.get_iter_method(iter_val, "return") {
                        Ok(m) => m,
                        Err(VmError::Throw(e)) => return self.resume_outer_throw(gen_oid, e),
                        Err(e) => return Err(e),
                    };
                    if m.is_none() {
                        // No return method: return-complete the outer generator
                        // (its finallys run via the close sentinel).
                        self.clear_delegation(gen_oid);
                        let mname = self.interner.intern("return");
                        return self.exec_generator_method(gen_oid, mname, args);
                    }
                }
                let step = self.iter_protocol_call(iter_val, "return", &[arg]);
                let res = match step {
                    Ok(r) => r,
                    Err(VmError::Throw(e)) => return self.resume_outer_throw(gen_oid, e),
                    Err(e) => return Err(e),
                };
                if res.as_object_id().is_none() {
                    let err = self.make_native_error("TypeError", "Iterator result is not an object");
                    return self.resume_outer_throw(gen_oid, err);
                }
                let done = match self.read_iter_prop(res, "done") {
                    Ok(v) => self.truthy(v),
                    Err(VmError::Throw(e)) => return self.resume_outer_throw(gen_oid, e),
                    Err(e) => return Err(e),
                };
                if !done {
                    // Spec: remain suspended and delegating; the caller sees
                    // the inner (not-done) result.
                    return Ok(GeneratorAction::Done(res));
                }
                let value = match self.read_iter_prop(res, "value") {
                    Ok(v) => v,
                    Err(VmError::Throw(e)) => return self.resume_outer_throw(gen_oid, e),
                    Err(e) => return Err(e),
                };
                // Return-complete the outer generator with the inner's value:
                // finallys run; a finally's own return/throw overrides.
                self.clear_delegation(gen_oid);
                let mname = self.interner.intern("return");
                self.exec_generator_method(gen_oid, mname, &[value])
            }
            _ => Ok(GeneratorAction::Done(Value::undefined())),
        }
    }

    /// Resume a generator: push its frame so the main run loop continues execution.
    /// Returns `Resumed` if a frame was pushed, `Done` if the generator is already completed.
    pub(crate) fn generator_resume(
        &mut self,
        gen_oid: ObjectId,
        input: Value,
    ) -> Result<GeneratorAction, VmError> {
        // Extract generator state
        let gen_data = {
            let obj = self.heap.get(gen_oid).ok_or_else(|| {
                VmError::RuntimeError("generator object not found".into())
            })?;
            match &obj.kind {
                ObjectKind::Generator {
                    state,
                    chunk_idx,
                    ip,
                    saved_stack,
                    saved_upvalues,
                    this_value,
                    saved_args,
                    saved_handlers,
                } => Some((
                    *state,
                    *chunk_idx,
                    *ip,
                    saved_stack.clone(),
                    saved_upvalues.clone(),
                    *this_value,
                    saved_args.clone(),
                    saved_handlers.clone(),
                )),
                _ => None,
            }
        };

        let (state, chunk_idx, ip, saved_stack, saved_upvalues, this_value, saved_args, saved_handlers) =
            gen_data.ok_or_else(|| VmError::TypeError("not a generator".into()))?;

        match state {
            GeneratorState::Completed => {
                let result = self.make_iter_result(Value::undefined(), true)?;
                Ok(GeneratorAction::Done(result))
            }
            GeneratorState::Executing => {
                Err(VmError::TypeError("generator is already executing".into()))
            }
            GeneratorState::SuspendedStart | GeneratorState::SuspendedYield => {
                // Mark as executing
                if let Some(obj) = self.heap.get_mut(gen_oid)
                    && let ObjectKind::Generator { state, .. } = &mut obj.kind
                {
                    *state = GeneratorState::Executing;
                }

                // Push placeholder for "function slot" (at base - 1)
                self.push(Value::undefined());
                let base = self.stack.len();

                // Restore saved locals + operand stack
                for val in &saved_stack {
                    self.push(*val);
                }

                // Build upvalues (all closed)
                let upvalues = saved_upvalues
                    .iter()
                    .map(|v| Upvalue {
                        cell: std::rc::Rc::new(std::cell::RefCell::new(
                            UpvalueLocation::Closed(*v),
                        )),
                    })
                    .collect();

                // Push generator frame
                let argc = saved_args.len();
                self.frames.push(CallFrame {
                    chunk_idx,
                    ip,
                    base,
                    upvalues,
                    this_value,
                    is_constructor: false,
                    pending_super_call: false,
                    generator_id: Some(gen_oid),
                    argc,
                    saved_args,
                    arguments_oid: None, is_derived_ctor: false, super_called: false,
                    new_target: Value::undefined(),
                    await_super_result: false,
                    with_base: self.with_stack.len(),
                });

                // Re-attach the frame's exception handlers at their new
                // absolute positions (detached at suspension — see Yield).
                let fidx = self.frames.len() - 1;
                for h in &saved_handlers {
                    self.exc_handlers.push(crate::vm::vm::ExcHandler {
                        catch_target: h.catch_target,
                        finally_target: h.finally_target,
                        stack_depth: base + h.rel_stack_depth,
                        frame_idx: fidx,
                        with_depth: self.with_stack.len(),
                    });
                }

                // For SuspendedYield, the input becomes the result of the yield expression
                if state == GeneratorState::SuspendedYield {
                    self.push(input);
                }

                Ok(GeneratorAction::Resumed)
            }
        }
    }

    /// Resume a suspended generator by throwing `exc` at the yield point and
    /// running it to completion. Finallys (and catch blocks, for gen.throw)
    /// execute; handlers below the generator frame are protected, so nothing
    /// escapes into the caller's try blocks.
    fn generator_abrupt_resume(
        &mut self,
        gen_oid: ObjectId,
        exc: Value,
        sentinel: Option<ObjectId>,
    ) -> Result<AbruptOutcome, VmError> {
        let pre_frames = self.frames.len();
        let pre_stack = self.stack.len();
        match self.generator_resume(gen_oid, Value::undefined())? {
            GeneratorAction::Resumed => {}
            GeneratorAction::Done(v) => return Ok(AbruptOutcome::Completed(v)),
        }
        // Discard the resume input the yield expression would have produced.
        self.pop()?;
        let gen_depth = self.frames.len();
        let prev_protect = self.protect_throw_depth;
        self.protect_throw_depth = gen_depth;
        let run = match self.handle_throw(exc) {
            Ok(()) => self.run_until(gen_depth - 1),
            Err(e) => Err(e),
        };
        self.protect_throw_depth = prev_protect;
        // Mark completed unless the generator suspended again (gen.throw
        // caught by the body and followed by another yield).
        let suspended_again = self.heap.get(gen_oid).is_some_and(|o| matches!(
            &o.kind,
            ObjectKind::Generator { state: GeneratorState::SuspendedYield, .. }
        ));
        if !suspended_again
            && let Some(obj) = self.heap.get_mut(gen_oid)
            && let ObjectKind::Generator { state, .. } = &mut obj.kind
        {
            *state = GeneratorState::Completed;
        }
        match run {
            Ok(v) => Ok(AbruptOutcome::Completed(v)),
            Err(VmError::Throw(v)) => {
                // Unwind anything the aborted resume left behind.
                while self.frames.len() > pre_frames {
                    self.frames.pop();
                }
                self.truncate_stack(pre_stack);
                if sentinel.is_some() && v.as_object_id() == sentinel {
                    Ok(AbruptOutcome::SentinelEscaped)
                } else {
                    Ok(AbruptOutcome::Threw(v))
                }
            }
            Err(e) => Err(e),
        }
    }
}

/// Outcome of resuming a generator with an abrupt completion.
enum AbruptOutcome {
    /// The injected sentinel unwound out of the generator frame (clean close).
    SentinelEscaped,
    /// A different exception escaped (a finally threw, or gen.throw's
    /// exception was not caught).
    Threw(Value),
    /// The generator completed with a value (a finally returned, or the body
    /// caught the exception and ran to completion / yielded again).
    Completed(Value),
}
