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
