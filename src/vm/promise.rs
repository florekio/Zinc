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
        // Per ResolvePromise, resolving with a thenable adopts its state
        // instead of fulfilling with the promise object as the value (an
        // async function returning a promise resolves to its settled value).
        if let Some(inner_oid) = value.as_object_id()
            && inner_oid != oid
            && self.heap.get(inner_oid)
                .is_some_and(|o| matches!(o.kind, ObjectKind::Promise { .. }))
        {
            let then_name = self.interner.intern("then");
            let resolve_sentinel = Value::function(-600_000 - oid.0 as i32);
            let reject_sentinel = Value::function(-700_000 - oid.0 as i32);
            self.exec_promise_method(inner_oid, then_name, &[resolve_sentinel, reject_sentinel])?;
            return Ok(());
        }
        // Generic thenables: Get(value, "then") is observable (a poisoned
        // getter rejects); a callable then is invoked with the resolving
        // functions, adopting the thenable's eventual state.
        if let Some(inner_oid) = value.as_object_id()
            && inner_oid != oid
            && !matches!(self.heap.get(inner_oid).map(|o| &o.kind), Some(ObjectKind::Promise { .. }))
        {
            let then_val = match self.getter_aware_get(inner_oid, "then") {
                Ok(v) => v.unwrap_or(Value::undefined()),
                Err(VmError::Throw(err)) => {
                    return self.reject_promise(oid, err);
                }
                Err(e) => return Err(e),
            };
            if self.value_callable(then_val) {
                let resolve_sentinel = Value::function(-600_000 - oid.0 as i32);
                let reject_sentinel = Value::function(-700_000 - oid.0 as i32);
                let prev = self.protect_throw_depth;
                self.protect_throw_depth = self.frames.len() + 1;
                let r = self.call_function_this(then_val, value, &[resolve_sentinel, reject_sentinel]);
                self.protect_throw_depth = prev;
                match r {
                    Ok(_) => {}
                    Err(VmError::Throw(err)) => {
                        // A throw AFTER resolve/reject already ran is ignored.
                        let still_pending = self.heap.get(oid).is_some_and(|o| {
                            matches!(o.kind, ObjectKind::Promise { state: PromiseState::Pending, .. })
                        });
                        if still_pending {
                            return self.reject_promise(oid, err);
                        }
                    }
                    Err(e) => return Err(e),
                }
                return Ok(());
            }
        }
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
                let child_id = self.allocate_promise();
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
                let pid = self.allocate_promise();
                self.resolve_promise(pid, val)?;
                Ok(Value::object_id(pid))
            }
            "reject" => {
                let val = args.first().copied().unwrap_or(Value::undefined());
                let pid = self.allocate_promise();
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
        // GetIterator(iterable): abrupt completions (non-iterables, poisoned
        // @@iterator) REJECT the returned promise rather than throwing.
        let iterable = args.first().copied().unwrap_or(Value::undefined());
        let elements: Vec<Value> = match self.simple_iterable_to_list(iterable) {
            Ok(list) => list,
            Err(VmError::Throw(err)) => {
                let pid = self.allocate_promise();
                self.reject_promise(pid, err)?;
                return Ok(Value::object_id(pid));
            }
            Err(e) => return Err(e),
        };

        // Create result promise
        let result_pid = self.allocate_promise();

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
                    let msg_id = self.interner.intern("All promises were rejected");
                    let err = self.make_aggregate_error(vec![], Value::string(msg_id));
                    self.reject_promise(result_pid, err)?;
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

        // Get(C, "resolve") once, observably: a user override of
        // Promise.resolve — data property or accessor — is read exactly once
        // and invoked per element (invoke-resolve tests).
        let resolve_id = self.interner.intern("resolve");
        let resolve_getter_id = self.interner.intern("__get_resolve__");
        let resolve_override = if let Some(Some(g)) = self.fn_property_overrides
            .get(&(-520, resolve_getter_id))
            .copied()
        {
            if self.value_callable(g) {
                let prev = self.protect_throw_depth;
                self.protect_throw_depth = self.frames.len() + 1;
                let r = self.call_function_this(g, Value::function(-520), &[]);
                self.protect_throw_depth = prev;
                match r {
                    Ok(v) if self.value_callable(v) => Some(v),
                    Ok(_) => None,
                    Err(VmError::Throw(err)) => {
                        let pid = self.allocate_promise();
                        self.reject_promise(pid, err)?;
                        return Ok(Value::object_id(pid));
                    }
                    Err(e) => return Err(e),
                }
            } else {
                None
            }
        } else {
            self.fn_property_overrides
                .get(&(-520, resolve_id))
                .copied()
                .flatten()
                .filter(|v| self.value_callable(*v))
        };
        // For each element, wrap with Promise.resolve and attach callbacks
        for (i, elem) in elements.iter().enumerate() {
            // Promise.resolve(elem)
            let resolved_pid = if let Some(rf) = resolve_override {
                let prev = self.protect_throw_depth;
                self.protect_throw_depth = self.frames.len() + 1;
                let r = self.call_function_this(rf, Value::function(-520), &[*elem]);
                self.protect_throw_depth = prev;
                let next = match r {
                    Ok(v) => v,
                    Err(VmError::Throw(err)) => {
                        self.reject_promise(result_pid, err)?;
                        return Ok(Value::object_id(result_pid));
                    }
                    Err(e) => return Err(e),
                };
                if let Some(noid) = next.as_object_id()
                    .filter(|o| matches!(self.heap.get(*o).map(|x| &x.kind), Some(ObjectKind::Promise { .. })))
                {
                    noid
                } else {
                    let pid = self.allocate_promise();
                    self.resolve_promise(pid, next)?;
                    pid
                }
            } else if let Some(oid) = elem.as_object_id()
                && let Some(obj) = self.heap.get(oid)
                    && matches!(&obj.kind, ObjectKind::Promise { .. }) {
                        oid
                    } else {
                let pid = self.allocate_promise();
                self.resolve_promise(pid, *elem)?;
                pid
            };

            // Create resolve/reject callback sentinels.
            // Encoding: -1_000_000_000 - (tracker_oid * 2048 + index * 2 + is_reject).
            // The old -800_000/-900_000 ranges were only 100k wide: any
            // tracker allocated at heap id >= 98 overflowed into the next
            // range and Promise.all silently misrouted its callbacks
            // (resolve decoded as reject for a different tracker).
            let encoded = tracker_oid.0 as i64 * 2048 + i as i64 * 2;
            let resolve_sentinel = Value::function((-1_000_000_000i64 - encoded) as i32);
            let reject_sentinel = Value::function((-1_000_000_000i64 - encoded - 1) as i32);

            // Invoke(nextPromise, "then", ...) is a real Get + Call: an own
            // "then" override (data or getter) is observable, and abrupt
            // completions reject the combinator promise and stop iteration.
            let then_key = self.interner.intern("then");
            let get_then_key = self.interner.intern("__get_then__");
            let has_own_then = self.heap.get(resolved_pid).is_some_and(|o| {
                o.has_own_property(then_key) || o.has_own_property(get_then_key)
            });
            if has_own_then {
                let promise_val = Value::object_id(resolved_pid);
                let prev_protect = self.protect_throw_depth;
                self.protect_throw_depth = self.frames.len() + 1;
                let r = self.getter_aware_get(resolved_pid, "then")
                    .and_then(|tv| {
                        let tv = tv.unwrap_or(Value::undefined());
                        if self.value_callable(tv) {
                            self.call_function_this(tv, promise_val, &[resolve_sentinel, reject_sentinel])
                        } else {
                            let err = self.make_native_error("TypeError", "then is not a function");
                            Err(VmError::Throw(err))
                        }
                    });
                self.protect_throw_depth = prev_protect;
                match r {
                    Ok(_) => {}
                    Err(VmError::Throw(err)) => {
                        self.reject_promise(result_pid, err)?;
                        return Ok(Value::object_id(result_pid));
                    }
                    Err(e) => return Err(e),
                }
                continue;
            }
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
                    let msg_id = self.interner.intern("All promises were rejected");
                    let agg = self.make_aggregate_error(errs, Value::string(msg_id));
                    self.reject_promise(result_promise, agg)?;
                }
            }
        }
        Ok(())
    }

    // ---- Embedder-facing promise API ------------------------------------
    //
    // Lets a host function hand a *pending* promise to JS and settle it
    // later — after a native async operation (network fetch, file read)
    // completes on another thread. The pending promise is pinned as a GC
    // root until settlement so the host's ObjectId can never dangle or
    // be reused; settling unpins it (reachability via reactions keeps it
    // alive from there if JS still cares).
    //
    // Settlement enqueues reactions as microtasks — the embedder runs
    // `drain_microtasks()` afterwards, exactly as it already does for
    // timer callbacks.

    /// Allocate a pending promise, pin it, and return `(handle, value)`:
    /// keep the handle to settle it later, hand the value to JS.
    pub fn host_promise_create(&mut self) -> (ObjectId, Value) {
        let pid = self.allocate_promise();
        self.host_roots.push(pid);
        (pid, Value::object_id(pid))
    }

    /// Fulfill a promise created with [`host_promise_create`]. No-op if
    /// it already settled.
    pub fn host_promise_resolve(&mut self, pid: ObjectId, value: Value) {
        self.host_roots.retain(|&r| r != pid);
        let _ = self.resolve_promise(pid, value);
    }

    /// Reject a promise created with [`host_promise_create`]. No-op if
    /// it already settled.
    pub fn host_promise_reject(&mut self, pid: ObjectId, reason: Value) {
        self.host_roots.retain(|&r| r != pid);
        let _ = self.reject_promise(pid, reason);
    }
}

impl Vm {
    /// NewPromiseCapability(C) for extracted Promise statics. Returns
    /// Ok(None) for the native Promise constructor (fast path), Err for
    /// non-constructors, and Ok(Some((instance, resolve, reject))) for a
    /// custom constructor: it is invoked with a capability executor that
    /// captures the resolve/reject it is handed.
    pub(crate) fn promise_new_capability(
        &mut self,
        this: Value,
    ) -> Result<Option<(Value, Value, Value)>, Value> {
        use crate::compiler::chunk::ChunkFlags;
        use std::sync::{Arc, Mutex};
        if this.as_function() == Some(-520) {
            return Ok(None);
        }
        let Some(packed) = this.as_function() else {
            // Function objects (bound/native) pass through the native path;
            // class objects (constructor marker) too — subclass identity is
            // approximated by the native machinery. Everything else is a
            // non-constructor receiver.
            let ctor_key = self.interner.intern("__constructor__");
            let ok = this.as_object_id()
                .and_then(|o| self.heap.get(o))
                .is_some_and(|o| {
                    matches!(o.kind, ObjectKind::Function(_))
                        || o.get_property(ctor_key).is_some()
                });
            if ok {
                return Ok(None);
            }
            return Err(self.make_native_error(
                "TypeError",
                "Promise method called on a non-constructor receiver",
            ));
        };
        if packed < 0 {
            return Err(self.make_native_error(
                "TypeError",
                "Promise capability constructor is not a constructor",
            ));
        }
        let chunk_idx = (packed & 0xFFFF) as usize;
        if chunk_idx < self.chunks.len() && self.chunks[chunk_idx].flags.contains(ChunkFlags::ARROW) {
            return Err(self.make_native_error(
                "TypeError",
                "Promise capability constructor is not a constructor",
            ));
        }
        // GetCapabilitiesExecutor: capture the (resolve, reject) pair the
        // constructor hands to the executor.
        let cap: Arc<Mutex<(Value, Value)>> =
            Arc::new(Mutex::new((Value::undefined(), Value::undefined())));
        let cap2 = cap.clone();
        let executor: crate::runtime::object::NativeFn =
            std::sync::Arc::new(move |vm: &mut Vm, _this: Value, args: &[Value]| {
                let mut g = cap2.lock().unwrap();
                // GetCapabilitiesExecutor: re-invocation after the slots were
                // set to non-undefined values throws.
                if !g.0.is_undefined() || !g.1.is_undefined() {
                    return Err(vm.make_native_error(
                        "TypeError",
                        "Promise executor has already been invoked with non-undefined arguments",
                    ));
                }
                g.0 = args.first().copied().unwrap_or(Value::undefined());
                g.1 = args.get(1).copied().unwrap_or(Value::undefined());
                Ok(Value::undefined())
            });
        let exec_obj = JsObject {
            properties: Vec::new(),
            prototype: None,
            kind: ObjectKind::Function(crate::runtime::object::FunctionKind::Native {
                name: self.interner.intern("executor"),
                func: executor,
            }),
            marked: false,
            extensible: true,
        };
        let exec_val = Value::object_id(self.heap.allocate(exec_obj));
        // Construct(C, «executor»): fresh this; a returned object wins.
        let fresh = self.heap.allocate(JsObject::ordinary());
        let ret = match self.call_function_this(this, Value::object_id(fresh), &[exec_val]) {
            Ok(v) => v,
            Err(VmError::Throw(v)) => return Err(v),
            Err(e) => {
                let msg = format!("{e:?}");
                return Err(self.make_native_error("Error", &msg));
            }
        };
        let instance = if ret.as_object_id().is_some() { ret } else { Value::object_id(fresh) };
        let (res, rej) = *cap.lock().unwrap();
        // If the constructor didn't hand the executor callable resolve /
        // reject functions, the capability is invalid.
        let callable = |vm: &Self, v: Value| {
            v.is_function()
                || v.as_object_id()
                    .and_then(|o| vm.heap.get(o))
                    .is_some_and(|o| matches!(o.kind, ObjectKind::Function(_)))
        };
        if !callable(self, res) || !callable(self, rej) {
            return Err(self.make_native_error(
                "TypeError",
                "Promise capability resolve or reject is not callable",
            ));
        }
        Ok(Some((instance, res, rej)))
    }
}
