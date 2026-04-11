use crate::runtime::object::{JsObject, ObjectId, ObjectKind, PromiseReaction, PromiseState, CombinatorKind};
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
                let on_fulfilled = args.first().copied().filter(|v| v.is_function());
                let on_rejected = args.get(1).copied().filter(|v| v.is_function());
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
                let on_rejected = args.first().copied().filter(|v| v.is_function());
                // Same as .then(undefined, onRejected)
                let then_name = self.interner.intern("then");
                let then_args = [Value::undefined(), on_rejected.unwrap_or(Value::undefined())];
                self.exec_promise_method(oid, then_name, &then_args)
            }
            "finally" => {
                let on_finally = args.first().copied().filter(|v| v.is_function());
                let then_name = self.interner.intern("then");
                if let Some(cb) = on_finally {
                    // Create fulfill sentinel: calls callback then propagates original value
                    let tracker = JsObject {
                        properties: vec![],
                        prototype: None,
                        kind: ObjectKind::FinallyTracker { callback: cb, is_reject: false },
                        marked: false,
                        extensible: true,
                    };
                    let tracker_oid = self.heap.allocate(tracker);
                    let fulfill_sentinel = Value::function(-1_100_000 - tracker_oid.0 as i32);

                    // Create reject sentinel: calls callback then propagates original reason
                    let tracker2 = JsObject {
                        properties: vec![],
                        prototype: None,
                        kind: ObjectKind::FinallyTracker { callback: cb, is_reject: true },
                        marked: false,
                        extensible: true,
                    };
                    let tracker2_oid = self.heap.allocate(tracker2);
                    let reject_sentinel = Value::function(-1_200_000 - tracker2_oid.0 as i32);

                    self.exec_promise_method(oid, then_name, &[fulfill_sentinel, reject_sentinel])
                } else {
                    self.exec_promise_method(oid, then_name, &[Value::undefined(), Value::undefined()])
                }
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
            "all" | "race" | "allSettled" | "any" => {
                let kind = match name.as_str() {
                    "all" => CombinatorKind::All,
                    "race" => CombinatorKind::Race,
                    "allSettled" => CombinatorKind::AllSettled,
                    "any" => CombinatorKind::Any,
                    _ => unreachable!(),
                };
                self.exec_promise_combinator(kind, args)
            }
            _ => Ok(Value::undefined()),
        }
    }

    /// Implement Promise.all, race, allSettled, any
    fn exec_promise_combinator(&mut self, kind: CombinatorKind, args: &[Value]) -> Result<Value, VmError> {
        // Extract the iterable (first arg, expected to be an array)
        let iterable = args.first().copied().unwrap_or(Value::undefined());
        let elements: Vec<Value> = if let Some(oid) = iterable.as_object_id()
            && let Some(obj) = self.heap.get(oid)
                && let ObjectKind::Array(ref elems) = obj.kind {
                    elems.clone()
                } else {
            vec![]
        };

        // Create result promise
        let result_promise = JsObject::promise();
        let result_pid = self.heap.allocate(result_promise);

        let count = elements.len();

        // Empty array: resolve immediately
        if count == 0 {
            match kind {
                CombinatorKind::All | CombinatorKind::AllSettled => {
                    let arr = JsObject::array(vec![]);
                    let arr_oid = self.heap.allocate(arr);
                    self.resolve_promise(result_pid, Value::object_id(arr_oid))?;
                }
                CombinatorKind::Race => {
                    // Race with empty array: forever pending (per spec)
                }
                CombinatorKind::Any => {
                    // Any with empty array: reject with AggregateError
                    let msg = self.interner.intern("All promises were rejected");
                    self.reject_promise(result_pid, Value::string(msg))?;
                }
            }
            return Ok(Value::object_id(result_pid));
        }

        // Create combinator tracker
        let tracker = JsObject {
            properties: vec![],
            prototype: None,
            kind: ObjectKind::PromiseCombinator {
                kind,
                remaining: count,
                values: vec![Value::undefined(); count],
                result_promise: result_pid,
                errors: vec![Value::undefined(); count],
            },
            marked: false,
            extensible: true,
        };
        let tracker_oid = self.heap.allocate(tracker);

        // For each element, wrap with Promise.resolve and attach callbacks
        for (i, elem) in elements.iter().enumerate() {
            // Promise.resolve(elem)
            let resolved_pid = if let Some(oid) = elem.as_object_id()
                && let Some(obj) = self.heap.get(oid)
                    && matches!(&obj.kind, ObjectKind::Promise { .. }) {
                        oid
                    } else {
                let p = JsObject::promise();
                let pid = self.heap.allocate(p);
                self.resolve_promise(pid, *elem)?;
                pid
            };

            // Create resolve/reject callback sentinels
            // Sentinel encoding: -800_000 - (tracker_oid * 1024 + index) for resolve
            //                    -900_000 - (tracker_oid * 1024 + index) for reject
            let resolve_sentinel = Value::function(-800_000 - (tracker_oid.0 as i32 * 1024 + i as i32));
            let reject_sentinel = Value::function(-900_000 - (tracker_oid.0 as i32 * 1024 + i as i32));

            // Attach .then(resolve_cb, reject_cb)
            let then_name = self.interner.intern("then");
            self.exec_promise_method(resolved_pid, then_name, &[resolve_sentinel, reject_sentinel])?;
        }

        Ok(Value::object_id(result_pid))
    }

    /// Handle a combinator resolve callback (sentinel in -800_000 range)
    pub(crate) fn handle_combinator_resolve(&mut self, tracker_oid: ObjectId, index: usize, value: Value) -> Result<(), VmError> {
        // Read current state
        let (kind, _remaining, result_promise) = {
            let obj = self.heap.get(tracker_oid).ok_or_else(|| VmError::RuntimeError("invalid combinator".into()))?;
            if let ObjectKind::PromiseCombinator { kind, remaining, result_promise, .. } = &obj.kind {
                (*kind, *remaining, *result_promise)
            } else {
                return Ok(());
            }
        };

        match kind {
            CombinatorKind::All => {
                // Store value at index, decrement remaining
                if let Some(obj) = self.heap.get_mut(tracker_oid)
                    && let ObjectKind::PromiseCombinator { remaining: rem, values, .. } = &mut obj.kind {
                        values[index] = value;
                        *rem -= 1;
                    }
                let new_remaining = self.heap.get(tracker_oid)
                    .map(|o| if let ObjectKind::PromiseCombinator { remaining, .. } = &o.kind { *remaining } else { 1 })
                    .unwrap_or(1);
                if new_remaining == 0 {
                    let vals = if let Some(obj) = self.heap.get(tracker_oid)
                        && let ObjectKind::PromiseCombinator { values, .. } = &obj.kind {
                            values.clone()
                        } else { vec![] };
                    let arr = JsObject::array(vals);
                    let arr_oid = self.heap.allocate(arr);
                    self.resolve_promise(result_promise, Value::object_id(arr_oid))?;
                }
            }
            CombinatorKind::Race => {
                // First to resolve wins
                self.resolve_promise(result_promise, value)?;
            }
            CombinatorKind::AllSettled => {
                // Store {status: "fulfilled", value} at index
                let status_key = self.interner.intern("status");
                let value_key = self.interner.intern("value");
                let fulfilled_str = self.interner.intern("fulfilled");
                let mut entry = JsObject::ordinary();
                entry.set_property(status_key, Value::string(fulfilled_str));
                entry.set_property(value_key, value);
                let entry_oid = self.heap.allocate(entry);

                if let Some(obj) = self.heap.get_mut(tracker_oid)
                    && let ObjectKind::PromiseCombinator { remaining: rem, values, .. } = &mut obj.kind {
                        values[index] = Value::object_id(entry_oid);
                        *rem -= 1;
                    }
                let new_remaining = self.heap.get(tracker_oid)
                    .map(|o| if let ObjectKind::PromiseCombinator { remaining, .. } = &o.kind { *remaining } else { 1 })
                    .unwrap_or(1);
                if new_remaining == 0 {
                    let vals = if let Some(obj) = self.heap.get(tracker_oid)
                        && let ObjectKind::PromiseCombinator { values, .. } = &obj.kind {
                            values.clone()
                        } else { vec![] };
                    let arr = JsObject::array(vals);
                    let arr_oid = self.heap.allocate(arr);
                    self.resolve_promise(result_promise, Value::object_id(arr_oid))?;
                }
            }
            CombinatorKind::Any => {
                // First to resolve wins
                self.resolve_promise(result_promise, value)?;
            }
        }
        Ok(())
    }

    /// Handle a combinator reject callback (sentinel in -900_000 range)
    pub(crate) fn handle_combinator_reject(&mut self, tracker_oid: ObjectId, index: usize, reason: Value) -> Result<(), VmError> {
        let (kind, _remaining, result_promise) = {
            let obj = self.heap.get(tracker_oid).ok_or_else(|| VmError::RuntimeError("invalid combinator".into()))?;
            if let ObjectKind::PromiseCombinator { kind, remaining, result_promise, .. } = &obj.kind {
                (*kind, *remaining, *result_promise)
            } else {
                return Ok(());
            }
        };

        match kind {
            CombinatorKind::All => {
                // First rejection rejects the result
                self.reject_promise(result_promise, reason)?;
            }
            CombinatorKind::Race => {
                // First to settle wins
                self.reject_promise(result_promise, reason)?;
            }
            CombinatorKind::AllSettled => {
                // Store {status: "rejected", reason} at index
                let status_key = self.interner.intern("status");
                let reason_key = self.interner.intern("reason");
                let rejected_str = self.interner.intern("rejected");
                let mut entry = JsObject::ordinary();
                entry.set_property(status_key, Value::string(rejected_str));
                entry.set_property(reason_key, reason);
                let entry_oid = self.heap.allocate(entry);

                if let Some(obj) = self.heap.get_mut(tracker_oid)
                    && let ObjectKind::PromiseCombinator { remaining: rem, values, .. } = &mut obj.kind {
                        values[index] = Value::object_id(entry_oid);
                        *rem -= 1;
                    }
                let new_remaining = self.heap.get(tracker_oid)
                    .map(|o| if let ObjectKind::PromiseCombinator { remaining, .. } = &o.kind { *remaining } else { 1 })
                    .unwrap_or(1);
                if new_remaining == 0 {
                    let vals = if let Some(obj) = self.heap.get(tracker_oid)
                        && let ObjectKind::PromiseCombinator { values, .. } = &obj.kind {
                            values.clone()
                        } else { vec![] };
                    let arr = JsObject::array(vals);
                    let arr_oid = self.heap.allocate(arr);
                    self.resolve_promise(result_promise, Value::object_id(arr_oid))?;
                }
            }
            CombinatorKind::Any => {
                // Store error, decrement remaining
                if let Some(obj) = self.heap.get_mut(tracker_oid)
                    && let ObjectKind::PromiseCombinator { remaining: rem, errors, .. } = &mut obj.kind {
                        errors[index] = reason;
                        *rem -= 1;
                    }
                let new_remaining = self.heap.get(tracker_oid)
                    .map(|o| if let ObjectKind::PromiseCombinator { remaining, .. } = &o.kind { *remaining } else { 1 })
                    .unwrap_or(1);
                if new_remaining == 0 {
                    // All rejected — reject with AggregateError
                    let errs = if let Some(obj) = self.heap.get(tracker_oid)
                        && let ObjectKind::PromiseCombinator { errors, .. } = &obj.kind {
                            errors.clone()
                        } else { vec![] };
                    let err_arr = JsObject::array(errs);
                    let err_oid = self.heap.allocate(err_arr);
                    let msg = self.interner.intern("All promises were rejected");
                    let mut agg = JsObject::ordinary();
                    let msg_key = self.interner.intern("message");
                    let errors_key = self.interner.intern("errors");
                    let name_key = self.interner.intern("name");
                    let name_val = self.interner.intern("AggregateError");
                    agg.set_property(msg_key, Value::string(msg));
                    agg.set_property(errors_key, Value::object_id(err_oid));
                    agg.set_property(name_key, Value::string(name_val));
                    let agg_oid = self.heap.allocate(agg);
                    self.reject_promise(result_promise, Value::object_id(agg_oid))?;
                }
            }
        }
        Ok(())
    }
}
