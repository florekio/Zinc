use crate::runtime::object::{JsObject, ObjectId, ObjectKind};
use crate::runtime::value::Value;
use crate::util::interner::StringId;

use super::vm::{Vm, VmError};

impl Vm {
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
                let keys: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Map { entries } = &o.kind {
                        entries.iter().map(|(k, _)| *k).collect()
                    } else { vec![] })
                    .unwrap_or_default();
                let arr = JsObject::array(keys);
                let arr_oid = self.heap.allocate(arr);
                Ok(Value::object_id(arr_oid))
            }
            "values" => {
                let vals: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Map { entries } = &o.kind {
                        entries.iter().map(|(_, v)| *v).collect()
                    } else { vec![] })
                    .unwrap_or_default();
                let arr = JsObject::array(vals);
                let arr_oid = self.heap.allocate(arr);
                Ok(Value::object_id(arr_oid))
            }
            "entries" => {
                let entries: Vec<(Value, Value)> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Map { entries } = &o.kind { entries.clone() } else { vec![] })
                    .unwrap_or_default();
                let mut result = Vec::new();
                for (k, v) in entries {
                    let pair = JsObject::array(vec![k, v]);
                    result.push(Value::object_id(self.heap.allocate(pair)));
                }
                let arr = JsObject::array(result);
                let arr_oid = self.heap.allocate(arr);
                Ok(Value::object_id(arr_oid))
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
                let vals: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Set { entries } = &o.kind { entries.clone() } else { vec![] })
                    .unwrap_or_default();
                let arr = JsObject::array(vals);
                let arr_oid = self.heap.allocate(arr);
                Ok(Value::object_id(arr_oid))
            }
            "entries" => {
                let entries: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Set { entries } = &o.kind { entries.clone() } else { vec![] })
                    .unwrap_or_default();
                let mut result = Vec::new();
                for v in entries {
                    let pair = JsObject::array(vec![v, v]);
                    result.push(Value::object_id(self.heap.allocate(pair)));
                }
                let arr = JsObject::array(result);
                let arr_oid = self.heap.allocate(arr);
                Ok(Value::object_id(arr_oid))
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
