use crate::runtime::object::{JsObject, ObjectId, ObjectKind, PromiseReaction, PromiseState};
use crate::runtime::value::Value;
use crate::util::interner::StringId;

use super::vm::{Vm, VmError, Microtask};

impl Vm {
    pub(crate) fn resolve_promise(&mut self, oid: ObjectId, value: Value) -> Result<(), VmError> {
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

    pub(crate) fn reject_promise(&mut self, oid: ObjectId, reason: Value) -> Result<(), VmError> {
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

    pub(crate) fn exec_promise_method(&mut self, oid: ObjectId, method_name: StringId, args: &[Value]) -> Result<Value, VmError> {
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

    pub(crate) fn exec_promise_static(&mut self, method_name: StringId, args: &[Value]) -> Result<Value, VmError> {
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
}
