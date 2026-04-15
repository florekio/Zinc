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
                if let Some(obj) = self.heap.get_mut(gen_oid)
                    && let ObjectKind::Generator { state, .. } = &mut obj.kind
                {
                    *state = GeneratorState::Completed;
                }
                let result = self.make_iter_result(val, true)?;
                Ok(GeneratorAction::Done(result))
            }
            "throw" => {
                if let Some(obj) = self.heap.get_mut(gen_oid)
                    && let ObjectKind::Generator { state, .. } = &mut obj.kind
                {
                    *state = GeneratorState::Completed;
                }
                let msg = args
                    .first()
                    .map(|v| self.value_to_string(*v))
                    .unwrap_or_else(|| "undefined".into());
                Err(VmError::RuntimeError(msg))
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
                } => Some((
                    *state,
                    *chunk_idx,
                    *ip,
                    saved_stack.clone(),
                    saved_upvalues.clone(),
                    *this_value,
                )),
                _ => None,
            }
        };

        let (state, chunk_idx, ip, saved_stack, saved_upvalues, this_value) =
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
                        location: UpvalueLocation::Closed(*v),
                    })
                    .collect();

                // Push generator frame
                self.frames.push(CallFrame {
                    chunk_idx,
                    ip,
                    base,
                    upvalues,
                    this_value,
                    is_constructor: false,
                    pending_super_call: false,
                    generator_id: Some(gen_oid),
                    argc: 0,
                    saved_args: Vec::new(),
                });

                // For SuspendedYield, the input becomes the result of the yield expression
                if state == GeneratorState::SuspendedYield {
                    self.push(input);
                }

                Ok(GeneratorAction::Resumed)
            }
        }
    }
}
