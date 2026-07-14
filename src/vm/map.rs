use crate::runtime::object::{JsObject, ObjectId, ObjectKind};
use crate::runtime::value::Value;
use crate::util::interner::StringId;

use super::vm::{Vm, VmError};

impl Vm {
    /// Drive the iterator protocol on `val`: resolve Symbol.iterator
    /// through the property chain, call it, then call `.next()` until
    /// done, collecting yielded values. Returns Ok(None) when `val`
    /// exposes no callable Symbol.iterator — callers fall back to their
    /// own semantics. Used by the Map/Set constructors so arbitrary
    /// iterables work (core-js's checkCorrectnessOfIteration constructs
    /// `new Map(fakeIterable)` and rejects the native Map if the
    /// protocol isn't consumed, then dies wrapping it).
    pub(crate) fn collect_iterable(&mut self, val: Value) -> Result<Option<Vec<Value>>, VmError> {
        let Some(oid) = val.as_object_id() else { return Ok(None) };
        let sym_key = self.interner.intern(&format!("__sym_{}__", self.sym_iterator));
        let Some(iter_fn) = self.heap.get_property_chain(oid, sym_key) else {
            return Ok(None);
        };
        if !iter_fn.is_function() && iter_fn.as_object_id().is_none() {
            return Ok(None);
        }
        let prev_protect = self.protect_throw_depth;
        self.protect_throw_depth = self.frames.len() + 1;
        let iter_r = self.call_function_this(iter_fn, val, &[]);
        self.protect_throw_depth = prev_protect;
        let iter = iter_r?;
        let next_key = self.interner.intern("next");
        let Some(ioid) = iter.as_object_id() else { return Ok(None) };
        let Some(next_fn) = self.heap.get_property_chain(ioid, next_key) else {
            return Ok(None);
        };
        let done_key = self.interner.intern("done");
        let value_key = self.interner.intern("value");
        let mut out = Vec::new();
        // Hard cap: a protocol-driven loop over a hostile/buggy iterator
        // must not hang the browser.
        let iter_is_gen = self.heap.get(ioid)
            .is_some_and(|o| matches!(o.kind, ObjectKind::Generator { .. }));
        for _ in 0..1_000_000usize {
            let step = if iter_is_gen {
                // Generators resume through their own machinery — the shared
                // %IteratorPrototype%.next would report done immediately.
                // Resume + nested run until the generator frame unwinds.
                match self.generator_resume(ioid, Value::undefined())? {
                    crate::vm::generator::GeneratorAction::Done(_) => {
                        return Ok(Some(out));
                    }
                    crate::vm::generator::GeneratorAction::Resumed => {
                        let depth = self.frames.len();
                        let prev_protect = self.protect_throw_depth;
                        self.protect_throw_depth = depth;
                        let r = self.run_until(depth - 1);
                        self.protect_throw_depth = prev_protect;
                        // Yield RETURNS the {value, done} result from the
                        // nested run (it does not push it for this caller).
                        r?
                    }
                }
            } else {
                let prev_protect = self.protect_throw_depth;
                self.protect_throw_depth = self.frames.len() + 1;
                let step_r = self.call_function_this(next_fn, iter, &[]);
                self.protect_throw_depth = prev_protect;
                step_r?
            };
            let Some(soid) = step.as_object_id() else { break };
            let done = self.heap.get(soid)
                .and_then(|o| o.get_property(done_key))
                .map(|v| v.to_boolean())
                .unwrap_or(false);
            if done {
                break;
            }
            let v = self.heap.get(soid)
                .and_then(|o| o.get_property(value_key))
                .unwrap_or(Value::undefined());
            out.push(v);
        }
        Ok(Some(out))
    }

    /// Array.from over a protocol iterable: maps each yielded value
    /// through `map_fn` DURING iteration and performs IteratorClose
    /// (calls the iterator's `return`) when the map fn throws, then
    /// rethrows. Returns Ok(None) when `val` has no callable
    /// Symbol.iterator. core-js's SAFE_CLOSING probe requires exactly
    /// this shape before it will trust native collections.
    pub(crate) fn array_from_iterable(
        &mut self,
        val: Value,
        map_fn: Option<Value>,
    ) -> Result<Option<Vec<Value>>, VmError> {
        let Some(oid) = val.as_object_id() else { return Ok(None) };
        let sym_key = self.interner.intern(&format!("__sym_{}__", self.sym_iterator));
        let Some(iter_fn) = self.heap.get_property_chain(oid, sym_key) else {
            return Ok(None);
        };
        let callable = |vm: &Vm, v: Value| v.is_function()
            || v.as_object_id().is_some_and(|o| vm.heap.get(o)
                .is_some_and(|x| matches!(&x.kind, ObjectKind::Function(_))));
        if !callable(self, iter_fn) {
            return Ok(None);
        }
        let iter = self.call_function_this(iter_fn, val, &[])?;
        let Some(ioid) = iter.as_object_id() else { return Ok(None) };
        let next_key = self.interner.intern("next");
        let Some(next_fn) = self.heap.get_property_chain(ioid, next_key).filter(|v| callable(self, *v)) else {
            return Ok(None);
        };
        let done_key = self.interner.intern("done");
        let value_key = self.interner.intern("value");
        let return_key = self.interner.intern("return");
        let mut out = Vec::new();
        for i in 0..1_000_000usize {
            let step = self.call_function_this(next_fn, iter, &[])?;
            let Some(soid) = step.as_object_id() else { break };
            let done = self.heap.get(soid)
                .and_then(|o| o.get_property(done_key))
                .map(|v| v.to_boolean())
                .unwrap_or(false);
            if done {
                break;
            }
            let v = self.heap.get(soid)
                .and_then(|o| o.get_property(value_key))
                .unwrap_or(Value::undefined());
            if let Some(mfn) = map_fn {
                match self.call_function(mfn, &[v, Value::int(i as i32)]) {
                    Ok(mapped) => out.push(mapped),
                    Err(e) => {
                        // IteratorClose: give the iterator its return()
                        // call, swallow any secondary error, rethrow the
                        // original abrupt completion.
                        if let Some(ret_fn) = self.heap.get_property_chain(ioid, return_key)
                            && callable(self, ret_fn)
                        {
                            let _ = self.call_function_this(ret_fn, iter, &[]);
                        }
                        return Err(e);
                    }
                }
            } else {
                out.push(v);
            }
        }
        Ok(Some(out))
    }

    /// Wrap a materialized value list in an iterator: snapshot the items
    /// into a hidden array and walk it with an ArrayIterator carrying the
    /// shared iterator prototype.
    pub(crate) fn make_tagged_key_iterator(&mut self, items: Vec<Value>, tag: Option<&str>) -> Value {
        let arr = JsObject::array(items);
        let arr_oid = self.heap.allocate(arr);
        let iter_proto = match tag {
            Some(t) => self.kind_iterator_prototype(t),
            None => self.iterator_prototype_oid(),
        };
        let iter = JsObject {
            properties: Vec::new(),
            prototype: Some(iter_proto),
            kind: ObjectKind::ArrayIterator(arr_oid, 0),
            marked: false,
            extensible: true,
        };
        Value::object_id(self.heap.allocate(iter))
    }

    pub(crate) fn exec_map_method(&mut self, oid: ObjectId, method_name: StringId, args: &[Value]) -> Result<Value, VmError> {
        let name = self.interner.resolve(method_name).to_owned();
        match name.as_str() {
            "get" => {
                let key = args.first().copied().unwrap_or(Value::undefined());
                if let Some(obj) = self.heap.get(oid)
                    && let ObjectKind::Map { entries } = &obj.kind {
                        for (k, v) in entries {
                            if self.strict_eq(*k, key) { return Ok(*v); }
                        }
                    }
                Ok(Value::undefined())
            }
            "set" => {
                let key = args.first().copied().unwrap_or(Value::undefined());
                let value = args.get(1).copied().unwrap_or(Value::undefined());
                // Find existing entry index first (immutable borrow)
                let existing_idx = self.heap.get(oid)
                    .and_then(|obj| if let ObjectKind::Map { entries } = &obj.kind {
                        entries.iter().position(|(k, _)| self.strict_eq(*k, key))
                    } else { None });
                if let Some(obj) = self.heap.get_mut(oid)
                    && let ObjectKind::Map { entries } = &mut obj.kind {
                        if let Some(idx) = existing_idx {
                            entries[idx].1 = value;
                        } else {
                            entries.push((key, value));
                        }
                    }
                Ok(Value::object_id(oid))
            }
            "has" => {
                let key = args.first().copied().unwrap_or(Value::undefined());
                let found = self.heap.get(oid)
                    .map(|obj| if let ObjectKind::Map { entries } = &obj.kind {
                        entries.iter().any(|(k, _)| self.strict_eq(*k, key))
                    } else { false })
                    .unwrap_or(false);
                Ok(Value::boolean(found))
            }
            "delete" => {
                let key = args.first().copied().unwrap_or(Value::undefined());
                let pos = self.heap.get(oid)
                    .and_then(|obj| if let ObjectKind::Map { entries } = &obj.kind {
                        entries.iter().position(|(k, _)| self.strict_eq(*k, key))
                    } else { None });
                if let Some(idx) = pos {
                    if let Some(obj) = self.heap.get_mut(oid)
                        && let ObjectKind::Map { entries } = &mut obj.kind {
                            entries.remove(idx);
                        }
                    Ok(Value::boolean(true))
                } else {
                    Ok(Value::boolean(false))
                }
            }
            "clear" => {
                if let Some(obj) = self.heap.get_mut(oid)
                    && let ObjectKind::Map { entries } = &mut obj.kind {
                        entries.clear();
                    }
                Ok(Value::undefined())
            }
            "forEach" => {
                let callback = args.first().copied().unwrap_or(Value::undefined());
                let entries: Vec<(Value, Value)> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Map { entries } = &o.kind { entries.clone() } else { vec![] })
                    .unwrap_or_default();
                for (k, v) in entries {
                    self.call_function(callback, &[v, k])?;
                }
                Ok(Value::undefined())
            }
            "keys" => {
                // Spec: returns an ITERATOR (core-js probes `.next()` on it),
                // realized as a KeyIterator over a key snapshot.
                let keys: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Map { entries } = &o.kind {
                        entries.iter().map(|(k, _)| *k).collect()
                    } else { vec![] })
                    .unwrap_or_default();
                Ok(self.make_tagged_key_iterator(keys, Some("Map")))
            }
            "values" => {
                let vals: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Map { entries } = &o.kind {
                        entries.iter().map(|(_, v)| *v).collect()
                    } else { vec![] })
                    .unwrap_or_default();
                Ok(self.make_tagged_key_iterator(vals, Some("Map")))
            }
            "entries" => {
                // Live MapIterator over the map itself, like for..of gets.
                let iter_proto = self.kind_iterator_prototype("Map");
                let iter = JsObject {
                    properties: Vec::new(),
                    prototype: Some(iter_proto),
                    kind: ObjectKind::MapIterator(oid, 0),
                    marked: false,
                    extensible: true,
                };
                Ok(Value::object_id(self.heap.allocate(iter)))
            }
            _ => Ok(Value::undefined()),
        }
    }

    pub(crate) fn exec_set_method(&mut self, oid: ObjectId, method_name: StringId, args: &[Value]) -> Result<Value, VmError> {
        let name = self.interner.resolve(method_name).to_owned();
        match name.as_str() {
            "add" => {
                let value = args.first().copied().unwrap_or(Value::undefined());
                let has = self.heap.get(oid)
                    .map(|obj| if let ObjectKind::Set { entries } = &obj.kind {
                        entries.iter().any(|v| self.strict_eq(*v, value))
                    } else { false })
                    .unwrap_or(false);
                if !has
                    && let Some(obj) = self.heap.get_mut(oid)
                        && let ObjectKind::Set { entries } = &mut obj.kind {
                            entries.push(value);
                        }
                Ok(Value::object_id(oid))
            }
            "has" => {
                let value = args.first().copied().unwrap_or(Value::undefined());
                let found = self.heap.get(oid)
                    .map(|obj| if let ObjectKind::Set { entries } = &obj.kind {
                        entries.iter().any(|v| self.strict_eq(*v, value))
                    } else { false })
                    .unwrap_or(false);
                Ok(Value::boolean(found))
            }
            "delete" => {
                let value = args.first().copied().unwrap_or(Value::undefined());
                let pos = self.heap.get(oid)
                    .and_then(|obj| if let ObjectKind::Set { entries } = &obj.kind {
                        entries.iter().position(|v| self.strict_eq(*v, value))
                    } else { None });
                if let Some(idx) = pos {
                    if let Some(obj) = self.heap.get_mut(oid)
                        && let ObjectKind::Set { entries } = &mut obj.kind {
                            entries.remove(idx);
                        }
                    Ok(Value::boolean(true))
                } else {
                    Ok(Value::boolean(false))
                }
            }
            "clear" => {
                if let Some(obj) = self.heap.get_mut(oid)
                    && let ObjectKind::Set { entries } = &mut obj.kind {
                        entries.clear();
                    }
                Ok(Value::undefined())
            }
            "forEach" => {
                let callback = args.first().copied().unwrap_or(Value::undefined());
                let entries: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Set { entries } = &o.kind { entries.clone() } else { vec![] })
                    .unwrap_or_default();
                for v in entries {
                    self.call_function(callback, &[v, v])?;
                }
                Ok(Value::undefined())
            }
            "values" | "keys" => {
                let iter_proto = self.kind_iterator_prototype("Set");
                let iter = JsObject {
                    properties: Vec::new(),
                    prototype: Some(iter_proto),
                    kind: ObjectKind::SetIterator(oid, 0),
                    marked: false,
                    extensible: true,
                };
                Ok(Value::object_id(self.heap.allocate(iter)))
            }
            "entries" => {
                // [v, v] pairs per spec, via a key-iterator snapshot.
                let entries: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Set { entries } = &o.kind { entries.clone() } else { vec![] })
                    .unwrap_or_default();
                let mut pairs = Vec::new();
                for v in entries {
                    let pair = JsObject::array(vec![v, v]);
                    pairs.push(Value::object_id(self.heap.allocate(pair)));
                }
                Ok(self.make_tagged_key_iterator(pairs, Some("Set")))
            }
            _ => Ok(Value::undefined()),
        }
    }

    pub(crate) fn exec_weakmap_method(&mut self, oid: ObjectId, method_name: StringId, args: &[Value]) -> Result<Value, VmError> {
        let name = self.interner.resolve(method_name).to_owned();
        match name.as_str() {
            "get" => {
                let key = args.first().and_then(|v| v.as_object_id());
                if let Some(key_oid) = key
                    && let Some(obj) = self.heap.get(oid)
                        && let ObjectKind::WeakMap { entries } = &obj.kind {
                            for (k, v) in entries {
                                if *k == key_oid { return Ok(*v); }
                            }
                        }
                Ok(Value::undefined())
            }
            "set" => {
                let key = args.first().and_then(|v| v.as_object_id());
                let value = args.get(1).copied().unwrap_or(Value::undefined());
                if let Some(key_oid) = key
                    && let Some(obj) = self.heap.get_mut(oid)
                        && let ObjectKind::WeakMap { entries } = &mut obj.kind {
                            for entry in entries.iter_mut() {
                                if entry.0 == key_oid { entry.1 = value; return Ok(Value::object_id(oid)); }
                            }
                            entries.push((key_oid, value));
                        }
                Ok(Value::object_id(oid))
            }
            "has" => {
                let key = args.first().and_then(|v| v.as_object_id());
                if let Some(key_oid) = key
                    && let Some(obj) = self.heap.get(oid)
                        && let ObjectKind::WeakMap { entries } = &obj.kind {
                            return Ok(Value::boolean(entries.iter().any(|(k, _)| *k == key_oid)));
                        }
                Ok(Value::boolean(false))
            }
            "delete" => {
                let key = args.first().and_then(|v| v.as_object_id());
                if let Some(key_oid) = key
                    && let Some(obj) = self.heap.get_mut(oid)
                        && let ObjectKind::WeakMap { entries } = &mut obj.kind
                            && let Some(pos) = entries.iter().position(|(k, _)| *k == key_oid) {
                                entries.remove(pos);
                                return Ok(Value::boolean(true));
                            }
                Ok(Value::boolean(false))
            }
            _ => Ok(Value::undefined()),
        }
    }

    pub(crate) fn exec_weakset_method(&mut self, oid: ObjectId, method_name: StringId, args: &[Value]) -> Result<Value, VmError> {
        let name = self.interner.resolve(method_name).to_owned();
        match name.as_str() {
            "add" => {
                if let Some(key_oid) = args.first().and_then(|v| v.as_object_id())
                    && let Some(obj) = self.heap.get_mut(oid)
                        && let ObjectKind::WeakSet { entries } = &mut obj.kind
                            && !entries.contains(&key_oid) {
                                entries.push(key_oid);
                            }
                Ok(Value::object_id(oid))
            }
            "has" => {
                if let Some(key_oid) = args.first().and_then(|v| v.as_object_id())
                    && let Some(obj) = self.heap.get(oid)
                        && let ObjectKind::WeakSet { entries } = &obj.kind {
                            return Ok(Value::boolean(entries.contains(&key_oid)));
                        }
                Ok(Value::boolean(false))
            }
            "delete" => {
                if let Some(key_oid) = args.first().and_then(|v| v.as_object_id())
                    && let Some(obj) = self.heap.get_mut(oid)
                        && let ObjectKind::WeakSet { entries } = &mut obj.kind
                            && let Some(pos) = entries.iter().position(|k| *k == key_oid) {
                                entries.remove(pos);
                                return Ok(Value::boolean(true));
                            }
                Ok(Value::boolean(false))
            }
            _ => Ok(Value::undefined()),
        }
    }
}
