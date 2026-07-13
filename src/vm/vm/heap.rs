//! Heap and frame maintenance: stack truncation, upvalue closing, the
//! microtask queue, and the mark-and-sweep collector with its root set.

use super::*;

impl Vm {
    /// Close all open upvalues that point to stack slots >= `from`.
    ///
    /// Heavy scripts (Google's homepage submit pipeline is the
    /// canonical example) accumulate stale `Open` upvalues in
    /// `closure_upvalues` storage: their original stack slot was
    /// already truncated away on a previous `Return`, but the
    /// upvalue itself outlived the closure that captured it. The
    /// next time `close_upvalues_above` runs with a low `from`,
    /// the naive `self.stack[stack_idx]` panics with "index out
    /// of bounds".
    ///
    /// The right long-term fix is to call `close_upvalues_above`
    /// at every stack-shrink site — but the VM has ~15 of those
    /// and threading the call through each is a larger
    /// refactor. For now: bound-check the indexing and close any
    /// stale upvalue to `undefined`. A subsequent read of the
    /// upvalue then returns `undefined` cleanly instead of
    /// crashing the host.
    /// Close any open upvalue cells whose slots are about to be
    /// destroyed, then shrink the stack. Every stack shrink must go
    /// through here: truncating past an open cell without closing it
    /// leaves the cell aimed at memory a later frame will reuse, so a
    /// surviving closure reads foreign values (react-dom's scheduler
    /// state silently became another frame's locals this way).
    pub(crate) fn truncate_stack(&mut self, to: usize) {
        if !self.open_upvalues.is_empty() {
            self.close_upvalues_above(to);
        }
        self.stack.truncate(to);
    }

    pub(crate) fn close_upvalues_above(&mut self, from: usize) {
        let stack_len = self.stack.len();
        let dead: Vec<usize> = self
            .open_upvalues
            .keys()
            .copied()
            .filter(|idx| *idx >= from)
            .collect();
        static WATCH: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
        let watch = *WATCH.get_or_init(|| std::env::var("ZINC_WATCH_SLOT").ok().and_then(|v| v.parse().ok()));
        for idx in dead {
            if let Some(cell) = self.open_upvalues.remove(&idx) {
                let val = if idx < stack_len {
                    self.stack[idx]
                } else {
                    // Stale entry whose slot was already truncated away
                    // on a path that didn't close — degrade to undefined.
                    Value::undefined()
                };
                if watch == Some(idx) {
                    eprintln!(
                        "[slotwatch] CLOSE slot {} from={} stack_len={} value={} chunk={} ip={}",
                        idx, from, stack_len, self.type_of_value(val),
                        self.frames.last().map(|f| self.interner.resolve(self.chunks[f.chunk_idx].name).to_string()).unwrap_or_default(),
                        self.frames.last().map(|f| f.ip).unwrap_or(0)
                    );
                }
                *cell.borrow_mut() = UpvalueLocation::Closed(val);
            }
        }
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
                            Ok(result) => {
                                // If the callback returned a thenable (Promise),
                                // adopt its state instead of resolving with the
                                // promise as the value.
                                let inner_promise = result.as_object_id().and_then(|oid| {
                                    self.heap.get(oid).and_then(|o| {
                                        if matches!(&o.kind, ObjectKind::Promise { .. }) {
                                            Some(oid)
                                        } else { None }
                                    })
                                });
                                if let Some(inner_oid) = inner_promise {
                                    // Adopt the inner promise's state: when it
                                    // settles, forward to result_promise.
                                    let then_name = self.interner.intern("then");
                                    let resolve_sentinel = Value::function(-600_000 - result_promise.0 as i32);
                                    let reject_sentinel = Value::function(-700_000 - result_promise.0 as i32);
                                    self.exec_promise_method(inner_oid, then_name, &[resolve_sentinel, reject_sentinel])?;
                                } else {
                                    self.resolve_promise(result_promise, result)?;
                                }
                            }
                            Err(VmError::Throw(reason)) => {
                                self.reject_promise(result_promise, reason)?;
                            }
                            Err(VmError::TypeError(msg)) => {
                                let err = self.make_native_error("TypeError", &msg);
                                self.reject_promise(result_promise, err)?;
                            }
                            Err(VmError::ReferenceError(msg)) => {
                                let err = self.make_native_error("ReferenceError", &msg);
                                self.reject_promise(result_promise, err)?;
                            }
                            Err(VmError::RuntimeError(msg)) => {
                                let err = self.make_native_error("Error", &msg);
                                self.reject_promise(result_promise, err)?;
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

    /// Run mark-and-sweep garbage collection.
    pub fn collect_gc(&mut self) {
        let mut roots: Vec<ObjectId> = Vec::new();

        // Root 0: live promise combinators. Their trackers are only reachable
        // via oids encoded into sentinel function values (untraceable), so root
        // them explicitly. Drop any whose result promise has already settled
        // (or whose tracker is gone) so the list can't grow without bound.
        let live_combinators: Vec<ObjectId> = std::mem::take(&mut self.pending_combinators)
            .into_iter()
            .filter(|&t| {
                let result_promise = match self.heap.get(t).map(|o| &o.kind) {
                    Some(ObjectKind::PromiseCombinator { result_promise, .. }) => *result_promise,
                    _ => return false,
                };
                matches!(
                    self.heap.get(result_promise).map(|o| &o.kind),
                    Some(ObjectKind::Promise {
                        state: crate::runtime::object::PromiseState::Pending,
                        ..
                    })
                )
            })
            .collect();
        roots.extend(live_combinators.iter().copied());
        self.pending_combinators = live_combinators;

        // Root 1: stack
        for val in &self.stack {
            if let Some(oid) = trace_value(*val) { roots.push(oid); }
        }

        // Root 2: globals
        for val in self.globals.values() {
            if let Some(oid) = trace_value(*val) { roots.push(oid); }
        }

        // Root 3: globals_vec
        for val in &self.globals_vec {
            if let Some(oid) = trace_value(*val) { roots.push(oid); }
        }

        // Root 4: call frames
        for frame in &self.frames {
            if let Some(oid) = trace_value(frame.this_value) { roots.push(oid); }
            if let Some(gid) = frame.generator_id { roots.push(gid); }
            for uv in &frame.upvalues {
                if let UpvalueLocation::Closed(val) = &*uv.cell.borrow()
                    && let Some(oid) = trace_value(*val)
                {
                    roots.push(oid);
                }
            }
        }

        // Root 5: closure upvalues
        for closure_uvs in &self.closure_upvalues {
            for uv in closure_uvs {
                if let UpvalueLocation::Closed(val) = &*uv.cell.borrow()
                    && let Some(oid) = trace_value(*val)
                {
                    roots.push(oid);
                }
            }
        }

        // Root 5b: active and closure-captured with-scope objects
        roots.extend(self.with_stack.iter().copied());
        for captured in self.closure_withs.values() {
            roots.extend(captured.iter().copied());
        }

        // Root 5c: class evaluations on closures' private-environment chains
        for env in self.closure_private_env.values() {
            roots.extend(env.iter().copied());
        }

        // Root 5d: arrow closures' captured `this` / new.target / arguments
        for (t, nt) in self.closure_arrow_ctx.values() {
            if let Some(oid) = trace_value(*t) { roots.push(oid); }
            if let Some(oid) = trace_value(*nt) { roots.push(oid); }
        }
        for v in self.closure_arrow_args.values() {
            if let Some(oid) = trace_value(*v) { roots.push(oid); }
        }

        // Root 6: microtask queue
        for task in &self.microtask_queue {
            match task {
                Microtask::PromiseReaction { callback, value, result_promise, .. } => {
                    if let Some(cb) = callback
                        && let Some(oid) = trace_value(*cb)
                    {
                        roots.push(oid);
                    }
                    if let Some(oid) = trace_value(*value) { roots.push(oid); }
                    roots.push(*result_promise);
                }
            }
        }

        // Root 7: singleton prototype objects
        roots.push(self.object_prototype);
        roots.push(self.function_prototype);
        roots.push(self.array_prototype);
        roots.push(self.boolean_prototype);
        roots.push(self.number_prototype);
        roots.push(self.string_prototype);

        // Root 8: function prototype cache
        for &oid in self.func_prototypes.values() {
            roots.push(oid);
        }
        // Root 8b: shared builtin-iterator prototype. Without this the
        // cached oid dangles after a sweep and every later iterator
        // chains to a recycled object (silent corruption; surfaced as
        // async-generator tests hanging once allocation crossed the GC
        // threshold).
        if let Some(oid) = self.iterator_prototype {
            roots.push(oid);
        }

        // Root 8c: function property overrides — holds the lazily
        // created NativeFn objects for extracted statics
        // (Object.create, Object.defineProperty, ...) plus user-set
        // function properties. These were collectable before; tests
        // only survived because GC rarely fired between caching and
        // use.
        for ov in self.fn_property_overrides.values() {
            if let Some(val) = ov
                && let Some(oid) = trace_value(*val)
            {
                roots.push(oid);
            }
        }

        // Root 9: math_oid, json_oid
        if let Some(oid) = self.math_oid { roots.push(oid); }
        if let Some(oid) = self.json_oid { roots.push(oid); }

        // Root 10: embedder-pinned objects (pending host promises etc.)
        roots.extend_from_slice(&self.host_roots);

        self.heap.mark_from_roots(&roots);
        self.heap.sweep();
        self.heap.gc_threshold = (self.heap.gc_threshold * 2).max(256);
    }
}
