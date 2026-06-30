use crate::runtime::object::{JsObject, ObjectKind};
use crate::runtime::value::Value;
use crate::util::interner::StringId;

use super::vm::{Vm, VmError};

impl Vm {
    /// Box a primitive value into its corresponding wrapper object
    /// (Number/String/Boolean) so it can act as `this` in non-strict
    /// callable code. Non-primitives are returned unchanged.
    pub(crate) fn box_primitive(&mut self, val: Value) -> Value {
        let (proto, inner) = if val.is_int() || val.is_number() {
            (self.number_prototype, val)
        } else if val.is_string() || self.is_cons_string(val) {
            (self.string_prototype, val)
        } else if val.as_bool().is_some() {
            (self.boolean_prototype, val)
        } else {
            return val;
        };
        let mut obj = JsObject::ordinary();
        obj.kind = ObjectKind::Wrapper(inner);
        obj.prototype = Some(proto);
        if let Some(sid) = inner.as_string_id() {
            let len = self.interner.resolve(sid).chars().count() as i32;
            let len_key = self.interner.intern("length");
            obj.set_property(len_key, Value::int(len));
        }
        let oid = self.heap.allocate(obj);
        Value::object_id(oid)
    }

    // ---- String method dispatch ----
    pub(crate) fn exec_string_method(&mut self, s: &str, method_name: StringId, args: &[Value]) -> Value {
        let name = self.interner.resolve(method_name).to_owned();
        match name.as_str() {
            "charAt" => {
                let idx = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                let ch = s.chars().nth(idx).map(|c| c.to_string()).unwrap_or_default();
                self.new_str(&ch)
            }
            "charCodeAt" => {
                let idx = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                let code = s.chars().nth(idx).map(|c| c as u32 as f64).unwrap_or(f64::NAN);
                Value::number(code)
            }
            "indexOf" => {
                let search = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                let from = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0).max(0.0) as usize;
                if from >= s.len() {
                    return Value::int(if search.is_empty() { s.len() as i32 } else { -1 });
                }
                let sub: String = s.chars().skip(from).collect();
                let pos = sub.find(&search).map(|i| {
                    // Convert byte position back to char position + offset
                    (sub[..i].chars().count() + from) as i32
                }).unwrap_or(-1);
                Value::int(pos)
            }
            "lastIndexOf" => {
                let search = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                let pos = s.rfind(&search).map(|i| i as i32).unwrap_or(-1);
                Value::int(pos)
            }
            "includes" => {
                let search = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                let from = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0).max(0.0) as usize;
                if from >= s.len() { return Value::boolean(search.is_empty()); }
                let sub: String = s.chars().skip(from).collect();
                Value::boolean(sub.contains(&search))
            }
            "startsWith" => {
                let search = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                let from = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0).max(0.0) as usize;
                let sub: String = s.chars().skip(from).collect();
                Value::boolean(sub.starts_with(&search))
            }
            "endsWith" => {
                let search = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                let end_pos = args.get(1).and_then(|v| v.as_number()).map(|n| n as usize).unwrap_or(s.chars().count());
                let sub: String = s.chars().take(end_pos).collect();
                Value::boolean(sub.ends_with(&search))
            }
            "slice" => {
                let len = s.len() as i32;
                let start = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
                let end = args.get(1).and_then(|v| v.as_number()).map(|n| n as i32).unwrap_or(len);
                let start = if start < 0 { (len + start).max(0) as usize } else { start.min(len) as usize };
                let end = if end < 0 { (len + end).max(0) as usize } else { end.min(len) as usize };
                let result = if start <= end { &s[start..end] } else { "" };
                self.new_str(result)
            }
            "substring" => {
                let len = s.len() as i32;
                let mut start = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
                let mut end = args.get(1).and_then(|v| v.as_number()).map(|n| n as i32).unwrap_or(len);
                start = start.max(0).min(len);
                end = end.max(0).min(len);
                if start > end { std::mem::swap(&mut start, &mut end); }
                let result = &s[start as usize..end as usize];
                self.new_str(result)
            }
            "substr" => {
                let len = s.len() as i32;
                let mut start = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
                if start < 0 { start = (start + len).max(0); }
                let start = start.min(len) as usize;
                let length = args.get(1)
                    .and_then(|v| v.as_number())
                    .map(|n| n as i32)
                    .unwrap_or(len - start as i32)
                    .max(0) as usize;
                let end = (start + length).min(s.len());
                let result = &s[start..end];
                self.new_str(result)
            }
            "toUpperCase" => {
                let result = s.to_uppercase();
                self.new_str(&result)
            }
            "toLowerCase" => {
                let result = s.to_lowercase();
                self.new_str(&result)
            }
            "trim" => {
                self.new_str(s.trim())
            }
            "trimStart" => {
                self.new_str(s.trim_start())
            }
            "trimEnd" => {
                self.new_str(s.trim_end())
            }
            "normalize" => {
                // No Unicode normalization library: return as-is (sufficient for ASCII)
                self.new_str(s)
            }
            "split" => {
                // Check if separator is a RegExp
                if let Some(result) = self.exec_string_regex_method(s, "split", args) {
                    return result;
                }
                let sep = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                let limit = args.get(1).and_then(|v| v.as_number()).map(|n| n as usize);
                let mut parts: Vec<Value> = Vec::new();
                for part in s.split(&sep) {
                    if let Some(lim) = limit && parts.len() >= lim { break; }
                    let v = self.new_str(part);
                    parts.push(v);
                }
                let arr = JsObject::array(parts);
                let oid = self.heap.allocate(arr);
                Value::object_id(oid)
            }
            "replace" | "replaceAll" => {
                // Check if first arg is a RegExp
                if let Some(result) = self.exec_string_regex_method(s, &name, args) {
                    return result;
                }
                let search = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                let replacement_arg = args.get(1).copied().unwrap_or(Value::undefined());
                // Function replacement: call fn(match, offset, fullString) per spec.
                let result = if replacement_arg.is_function() {
                    let mut out = String::new();
                    let mut last_end = 0usize;
                    let mut iter_start = 0usize;
                    while let Some(rel) = s[iter_start..].find(&search) {
                        let abs = iter_start + rel;
                        out.push_str(&s[last_end..abs]);
                        let match_id = self.interner.intern(&search);
                        let s_id = self.interner.intern(s);
                        let cb_args = [
                            Value::string(match_id),
                            Value::int(abs as i32),
                            Value::string(s_id),
                        ];
                        let r = self.call_function_this(replacement_arg, Value::undefined(), &cb_args)
                            .unwrap_or(Value::undefined());
                        out.push_str(&self.value_to_string(r));
                        let advance = if search.is_empty() {
                            // Avoid infinite loop on empty pattern: advance one char.
                            s[abs..].chars().next().map(|c| c.len_utf8()).unwrap_or(1)
                        } else {
                            search.len()
                        };
                        last_end = abs + search.len();
                        iter_start = abs + advance;
                        if name != "replaceAll" { break; }
                        if iter_start > s.len() { break; }
                    }
                    out.push_str(&s[last_end..]);
                    out
                } else {
                    let replacement = self.value_to_string(replacement_arg);
                    if name == "replaceAll" {
                        s.replace(&search, &replacement)
                    } else {
                        s.replacen(&search, &replacement, 1)
                    }
                };
                self.new_str(&result)
            }
            "match" | "search" => {
                if let Some(result) = self.exec_string_regex_method(s, &name, args) {
                    return result;
                }
                Value::null()
            }
            "repeat" => {
                let count = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                let result = s.repeat(count);
                self.new_str(&result)
            }
            "padStart" => {
                let target_len = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                let pad = args.get(1).map(|v| self.value_to_string(*v)).unwrap_or_else(|| " ".into());
                let mut result = s.to_string();
                while result.len() < target_len {
                    result.insert_str(0, &pad);
                }
                if result.len() > target_len { result.truncate(target_len); }
                self.new_str(&result)
            }
            "padEnd" => {
                let target_len = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                let pad = args.get(1).map(|v| self.value_to_string(*v)).unwrap_or_else(|| " ".into());
                let mut result = s.to_string();
                while result.len() < target_len {
                    result.push_str(&pad);
                }
                if result.len() > target_len { result.truncate(target_len); }
                self.new_str(&result)
            }
            "concat" => {
                let mut result = s.to_string();
                for arg in args {
                    result.push_str(&self.value_to_string(*arg));
                }
                self.new_str(&result)
            }
            "codePointAt" => {
                let idx = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                match s.chars().nth(idx) {
                    Some(c) => Value::number(c as u32 as f64),
                    None => Value::undefined(),
                }
            }
            "at" => {
                let idx = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
                let len = s.chars().count() as i32;
                let actual = if idx < 0 { len + idx } else { idx };
                if actual >= 0 && (actual as usize) < len as usize {
                    let ch = s.chars().nth(actual as usize).unwrap().to_string();
                    self.new_str(&ch)
                } else {
                    Value::undefined()
                }
            }
            "toString" | "valueOf" => {
                self.new_str(s)
            }
            _ => Value::undefined(),
        }
    }

    // ---- Array method dispatch ----
    pub(crate) fn exec_array_method(&mut self, oid: crate::runtime::object::ObjectId, method_name: StringId, args: &[Value]) -> Result<Value, VmError> {
        let name = self.interner.resolve(method_name).to_owned();
        match name.as_str() {
            "push" => {
                if let Some(obj) = self.heap.get_mut(oid)
                    && let ObjectKind::Array(ref mut elements) = obj.kind {
                        for arg in args {
                            elements.push(*arg);
                        }
                        return Ok(Value::int(elements.len() as i32));
                    }
                Ok(Value::undefined())
            }
            "pop" => {
                if let Some(obj) = self.heap.get_mut(oid)
                    && let ObjectKind::Array(ref mut elements) = obj.kind {
                        return Ok(elements.pop().unwrap_or(Value::undefined()));
                    }
                Ok(Value::undefined())
            }
            "join" => {
                let sep = args.first()
                    .filter(|v| !v.is_undefined())
                    .map(|v| self.value_to_string(*v))
                    .unwrap_or_else(|| ",".into());
                if let Some(obj) = self.heap.get(oid)
                    && let ObjectKind::Array(ref elements) = obj.kind {
                        let parts: Vec<String> = elements.iter().map(|v| self.value_to_string(*v)).collect();
                        let result = parts.join(&sep);
                        let id = self.interner.intern(&result);
                        return Ok(Value::string(id));
                    }
                Ok(Value::undefined())
            }
            "indexOf" => {
                let search = args.first().copied().unwrap_or(Value::undefined());
                let from_idx = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0);
                if let Some(obj) = self.heap.get(oid)
                    && let ObjectKind::Array(ref elements) = obj.kind {
                        let len = elements.len() as i32;
                        let mut start = from_idx as i32;
                        if start < 0 { start = (len + start).max(0); }
                        for (i, elem) in elements.iter().enumerate().skip(start as usize) {
                            if self.strict_eq(*elem, search) {
                                return Ok(Value::int(i as i32));
                            }
                        }
                    }
                Ok(Value::int(-1))
            }
            "includes" => {
                let search = args.first().copied().unwrap_or(Value::undefined());
                if let Some(obj) = self.heap.get(oid)
                    && let ObjectKind::Array(ref elements) = obj.kind {
                        for elem in elements {
                            // SameValueZero: NaN equals NaN, +0 equals -0
                            if self.strict_eq(*elem, search) {
                                return Ok(Value::boolean(true));
                            }
                            // Both NaN case
                            if let (Some(a), Some(b)) = (elem.as_number(), search.as_number())
                                && a.is_nan() && b.is_nan() {
                                    return Ok(Value::boolean(true));
                                }
                        }
                    }
                Ok(Value::boolean(false))
            }
            "reverse" => {
                if let Some(obj) = self.heap.get_mut(oid)
                    && let ObjectKind::Array(ref mut elements) = obj.kind {
                        elements.reverse();
                    }
                Ok(Value::object_id(oid))
            }
            "shift" => {
                if let Some(obj) = self.heap.get_mut(oid)
                    && let ObjectKind::Array(ref mut elements) = obj.kind
                        && !elements.is_empty() {
                            return Ok(elements.remove(0));
                        }
                Ok(Value::undefined())
            }
            "unshift" => {
                if let Some(obj) = self.heap.get_mut(oid)
                    && let ObjectKind::Array(ref mut elements) = obj.kind {
                        for (i, arg) in args.iter().enumerate() {
                            elements.insert(i, *arg);
                        }
                        return Ok(Value::int(elements.len() as i32));
                    }
                Ok(Value::undefined())
            }
            "map" => {
                let callback = args.first().copied().unwrap_or(Value::undefined());
                let this_arg = args.get(1).copied().unwrap_or(Value::undefined());
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                let mut results = Vec::with_capacity(elements.len());
                for (i, elem) in elements.iter().enumerate() {
                    let result = self.call_function_this(callback, this_arg, &[*elem, Value::int(i as i32), Value::object_id(oid)])?;
                    results.push(result);
                }
                let arr = JsObject::array(results);
                let new_oid = self.heap.allocate(arr);
                Ok(Value::object_id(new_oid))
            }
            "filter" => {
                let callback = args.first().copied().unwrap_or(Value::undefined());
                let this_arg = args.get(1).copied().unwrap_or(Value::undefined());
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                let mut results = Vec::new();
                for (i, elem) in elements.iter().enumerate() {
                    let result = self.call_function_this(callback, this_arg, &[*elem, Value::int(i as i32), Value::object_id(oid)])?;
                    if result.to_boolean() {
                        results.push(*elem);
                    }
                }
                let arr = JsObject::array(results);
                let new_oid = self.heap.allocate(arr);
                Ok(Value::object_id(new_oid))
            }
            "reduce" => {
                let callback = args.first().copied().unwrap_or(Value::undefined());
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                let mut acc = if args.len() > 1 { args[1] } else if !elements.is_empty() { elements[0] } else { Value::undefined() };
                let start = if args.len() > 1 { 0 } else { 1 };
                for (i, elem) in elements.iter().enumerate().skip(start) {
                    acc = self.call_function(callback, &[acc, *elem, Value::int(i as i32)])?;
                }
                Ok(acc)
            }
            "forEach" => {
                let callback = args.first().copied().unwrap_or(Value::undefined());
                let this_arg = args.get(1).copied().unwrap_or(Value::undefined());
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                for (i, elem) in elements.iter().enumerate() {
                    self.call_function_this(callback, this_arg, &[*elem, Value::int(i as i32), Value::object_id(oid)])?;
                }
                Ok(Value::undefined())
            }
            "find" => {
                let callback = args.first().copied().unwrap_or(Value::undefined());
                let this_arg = args.get(1).copied().unwrap_or(Value::undefined());
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                for (i, elem) in elements.iter().enumerate() {
                    let result = self.call_function_this(callback, this_arg, &[*elem, Value::int(i as i32), Value::object_id(oid)])?;
                    if result.to_boolean() { return Ok(*elem); }
                }
                Ok(Value::undefined())
            }
            "some" => {
                let callback = args.first().copied().unwrap_or(Value::undefined());
                let this_arg = args.get(1).copied().unwrap_or(Value::undefined());
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                for (i, elem) in elements.iter().enumerate() {
                    let result = self.call_function_this(callback, this_arg, &[*elem, Value::int(i as i32), Value::object_id(oid)])?;
                    if result.to_boolean() { return Ok(Value::boolean(true)); }
                }
                Ok(Value::boolean(false))
            }
            "every" => {
                let callback = args.first().copied().unwrap_or(Value::undefined());
                let this_arg = args.get(1).copied().unwrap_or(Value::undefined());
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                for (i, elem) in elements.iter().enumerate() {
                    let result = self.call_function_this(callback, this_arg, &[*elem, Value::int(i as i32), Value::object_id(oid)])?;
                    if !result.to_boolean() { return Ok(Value::boolean(false)); }
                }
                Ok(Value::boolean(true))
            }
            "findIndex" => {
                let callback = args.first().copied().unwrap_or(Value::undefined());
                let this_arg = args.get(1).copied().unwrap_or(Value::undefined());
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                for (i, elem) in elements.iter().enumerate() {
                    let result = self.call_function_this(callback, this_arg, &[*elem, Value::int(i as i32), Value::object_id(oid)])?;
                    if result.to_boolean() { return Ok(Value::int(i as i32)); }
                }
                Ok(Value::int(-1))
            }
            "findLast" => {
                let callback = args.first().copied().unwrap_or(Value::undefined());
                let this_arg = args.get(1).copied().unwrap_or(Value::undefined());
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                for (i, elem) in elements.iter().enumerate().rev() {
                    let result = self.call_function_this(callback, this_arg, &[*elem, Value::int(i as i32), Value::object_id(oid)])?;
                    if result.to_boolean() { return Ok(*elem); }
                }
                Ok(Value::undefined())
            }
            "findLastIndex" => {
                let callback = args.first().copied().unwrap_or(Value::undefined());
                let this_arg = args.get(1).copied().unwrap_or(Value::undefined());
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                for (i, elem) in elements.iter().enumerate().rev() {
                    let result = self.call_function_this(callback, this_arg, &[*elem, Value::int(i as i32), Value::object_id(oid)])?;
                    if result.to_boolean() { return Ok(Value::int(i as i32)); }
                }
                Ok(Value::int(-1))
            }
            "reduceRight" => {
                let callback = args.first().copied().unwrap_or(Value::undefined());
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                if elements.is_empty() && args.len() <= 1 {
                    return Err(VmError::TypeError("reduceRight of empty array with no initial value".into()));
                }
                let mut acc = if args.len() > 1 { args[1] } else { *elements.last().unwrap() };
                let end = if args.len() > 1 { elements.len() } else { elements.len() - 1 };
                for i in (0..end).rev() {
                    acc = self.call_function(callback, &[acc, elements[i], Value::int(i as i32)])?;
                }
                Ok(acc)
            }
            "splice" => {
                let len = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.len() } else { 0 })
                    .unwrap_or(0);
                let raw_start = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
                let start = if raw_start < 0 { (len as i32 + raw_start).max(0) as usize } else { (raw_start as usize).min(len) };
                let delete_count = if args.len() >= 2 {
                    (args[1].as_number().unwrap_or(0.0) as i32).max(0) as usize
                } else {
                    len - start
                };
                let delete_count = delete_count.min(len - start);
                let insert_items: Vec<Value> = args.iter().skip(2).copied().collect();

                // Extract deleted elements
                let deleted: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind {
                        e[start..start + delete_count].to_vec()
                    } else { vec![] })
                    .unwrap_or_default();

                // Perform splice
                if let Some(obj) = self.heap.get_mut(oid)
                    && let ObjectKind::Array(ref mut elements) = obj.kind {
                        let tail: Vec<Value> = elements.drain(start..).collect();
                        for item in &insert_items {
                            elements.push(*item);
                        }
                        for item in tail.iter().skip(delete_count) {
                            elements.push(*item);
                        }
                    }

                let arr = JsObject::array(deleted);
                let new_oid = self.heap.allocate(arr);
                Ok(Value::object_id(new_oid))
            }
            "slice" => {
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                let len = elements.len() as i32;
                let raw_start = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
                let raw_end = args.get(1).and_then(|v| v.as_number()).map(|n| n as i32).unwrap_or(len);
                let start = if raw_start < 0 { (len + raw_start).max(0) as usize } else { raw_start.min(len) as usize };
                let end = if raw_end < 0 { (len + raw_end).max(0) as usize } else { raw_end.min(len) as usize };
                let sliced = if start < end { elements[start..end].to_vec() } else { vec![] };
                let arr = JsObject::array(sliced);
                let new_oid = self.heap.allocate(arr);
                Ok(Value::object_id(new_oid))
            }
            "concat" => {
                let mut result: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                for arg in args {
                    if let Some(arg_oid) = arg.as_object_id()
                        && let Some(obj) = self.heap.get(arg_oid)
                            && let ObjectKind::Array(ref elems) = obj.kind {
                                result.extend_from_slice(elems);
                            } else {
                        result.push(*arg);
                    }
                }
                let arr = JsObject::array(result);
                let new_oid = self.heap.allocate(arr);
                Ok(Value::object_id(new_oid))
            }
            "fill" => {
                let fill_val = args.first().copied().unwrap_or(Value::undefined());
                let len = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.len() } else { 0 })
                    .unwrap_or(0) as i32;
                let raw_start = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
                let raw_end = args.get(2).and_then(|v| v.as_number()).map(|n| n as i32).unwrap_or(len);
                let start = if raw_start < 0 { (len + raw_start).max(0) as usize } else { raw_start.min(len) as usize };
                let end = if raw_end < 0 { (len + raw_end).max(0) as usize } else { raw_end.min(len) as usize };
                if let Some(obj) = self.heap.get_mut(oid)
                    && let ObjectKind::Array(ref mut elements) = obj.kind {
                        for i in start..end.min(elements.len()) {
                            elements[i] = fill_val;
                        }
                    }
                Ok(Value::object_id(oid))
            }
            "copyWithin" => {
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                let len = elements.len() as i32;
                let raw_target = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
                let raw_start = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
                let raw_end = args.get(2).and_then(|v| v.as_number()).map(|n| n as i32).unwrap_or(len);
                let target = if raw_target < 0 { (len + raw_target).max(0) as usize } else { raw_target.min(len) as usize };
                let start = if raw_start < 0 { (len + raw_start).max(0) as usize } else { raw_start.min(len) as usize };
                let end = if raw_end < 0 { (len + raw_end).max(0) as usize } else { raw_end.min(len) as usize };
                if let Some(obj) = self.heap.get_mut(oid)
                    && let ObjectKind::Array(ref mut elems) = obj.kind {
                        let copy: Vec<Value> = elements[start..end].to_vec();
                        for (i, val) in copy.iter().enumerate() {
                            let idx = target + i;
                            if idx < elems.len() { elems[idx] = *val; }
                        }
                    }
                Ok(Value::object_id(oid))
            }
            "flat" => {
                let depth = args.first().and_then(|v| v.as_number()).unwrap_or(1.0) as usize;
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                let result = self.flatten_array(&elements, depth);
                let arr = JsObject::array(result);
                let new_oid = self.heap.allocate(arr);
                Ok(Value::object_id(new_oid))
            }
            "flatMap" => {
                let callback = args.first().copied().unwrap_or(Value::undefined());
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                let mut mapped = Vec::new();
                for (i, elem) in elements.iter().enumerate() {
                    let result = self.call_function(callback, &[*elem, Value::int(i as i32)])?;
                    if let Some(r_oid) = result.as_object_id()
                        && let Some(obj) = self.heap.get(r_oid)
                            && let ObjectKind::Array(ref inner) = obj.kind {
                                mapped.extend_from_slice(inner);
                            } else {
                        mapped.push(result);
                    }
                }
                let arr = JsObject::array(mapped);
                let new_oid = self.heap.allocate(arr);
                Ok(Value::object_id(new_oid))
            }
            "at" => {
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                let idx = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
                let actual = if idx < 0 { elements.len() as i32 + idx } else { idx } as usize;
                Ok(elements.get(actual).copied().unwrap_or(Value::undefined()))
            }
            "sort" => {
                let comparefn = args.first().copied().filter(|v| v.is_function());
                let mut elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                // Simple insertion sort to avoid issues with call_function during sort
                let len = elements.len();
                for i in 1..len {
                    let key = elements[i];
                    let mut j = i;
                    while j > 0 {
                        let cmp = if let Some(cfn) = comparefn {
                            let r = self.call_function(cfn, &[elements[j - 1], key])?;
                            r.as_number().unwrap_or(0.0)
                        } else {
                            let a_str = self.value_to_string(elements[j - 1]);
                            let b_str = self.value_to_string(key);
                            if a_str < b_str { -1.0 } else if a_str > b_str { 1.0 } else { 0.0 }
                        };
                        if cmp <= 0.0 { break; }
                        elements[j] = elements[j - 1];
                        j -= 1;
                    }
                    elements[j] = key;
                }
                if let Some(obj) = self.heap.get_mut(oid)
                    && let ObjectKind::Array(ref mut elems) = obj.kind {
                        *elems = elements;
                    }
                Ok(Value::object_id(oid))
            }
            "lastIndexOf" => {
                let search = args.first().copied().unwrap_or(Value::undefined());
                if let Some(obj) = self.heap.get(oid)
                    && let ObjectKind::Array(ref elements) = obj.kind {
                        for i in (0..elements.len()).rev() {
                            if self.strict_eq(elements[i], search) {
                                return Ok(Value::int(i as i32));
                            }
                        }
                    }
                Ok(Value::int(-1))
            }
            "toReversed" => {
                let mut elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                elements.reverse();
                let arr = JsObject::array(elements);
                Ok(Value::object_id(self.heap.allocate(arr)))
            }
            "toSorted" => {
                let comparefn = args.first().copied().filter(|v| v.is_function());
                let mut elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                let len = elements.len();
                for i in 1..len {
                    let key = elements[i];
                    let mut j = i;
                    while j > 0 {
                        let cmp = if let Some(cfn) = comparefn {
                            let r = self.call_function(cfn, &[elements[j - 1], key])?;
                            r.as_number().unwrap_or(0.0)
                        } else {
                            let a_str = self.value_to_string(elements[j - 1]);
                            let b_str = self.value_to_string(key);
                            if a_str < b_str { -1.0 } else if a_str > b_str { 1.0 } else { 0.0 }
                        };
                        if cmp <= 0.0 { break; }
                        elements[j] = elements[j - 1];
                        j -= 1;
                    }
                    elements[j] = key;
                }
                let arr = JsObject::array(elements);
                Ok(Value::object_id(self.heap.allocate(arr)))
            }
            "with" => {
                let idx_val = args.first().copied().unwrap_or(Value::undefined());
                let val = args.get(1).copied().unwrap_or(Value::undefined());
                let mut elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                let len = elements.len() as i32;
                let mut i = self.to_f64(idx_val) as i32;
                if i < 0 { i += len; }
                if i < 0 || i >= len {
                    return Err(VmError::RuntimeError("RangeError: Invalid index".into()));
                }
                elements[i as usize] = val;
                let arr = JsObject::array(elements);
                Ok(Value::object_id(self.heap.allocate(arr)))
            }
            "toSpliced" => {
                let mut elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                let len = elements.len() as i32;
                let start_val = args.first().copied().unwrap_or(Value::undefined());
                let mut start = self.to_f64(start_val) as i32;
                if start < 0 { start = (start + len).max(0); }
                let start = start.min(len) as usize;
                let delete_count = if args.len() > 1 {
                    (self.to_f64(args[1]) as i32).max(0).min(len - start as i32) as usize
                } else {
                    elements.len() - start
                };
                let new_items: Vec<Value> = args.iter().skip(2).copied().collect();
                elements.splice(start..start + delete_count, new_items);
                let arr = JsObject::array(elements);
                Ok(Value::object_id(self.heap.allocate(arr)))
            }
            "toString" => {
                // Array.prototype.toString is equivalent to join(",")
                if let Some(obj) = self.heap.get(oid)
                    && let ObjectKind::Array(ref elements) = obj.kind {
                        let parts: Vec<String> = elements.iter().map(|v| self.value_to_string(*v)).collect();
                        let result = parts.join(",");
                        let id = self.interner.intern(&result);
                        return Ok(Value::string(id));
                    }
                Ok(Value::undefined())
            }
            "keys" | "values" | "entries" => {
                // Spec-compliant Array Iterator that wraps the array directly so writes to
                // the underlying array are visible during iteration. The iterator's `kind`
                // (keys/values/entries) is encoded by reusing a small object: we attach
                // a `__kind__` property since ArrayIterator currently only models values.
                // For now, all three return an iterator over the array — `for..of` and
                // destructuring work uniformly. (Distinct keys/entries shapes can be added
                // later by extending ObjectKind::ArrayIterator with a kind tag.)
                let iter_obj = JsObject {
                    properties: Vec::new(),
                    prototype: Some(self.iterator_prototype_oid()),
                    kind: ObjectKind::ArrayIterator(oid, 0),
                    marked: false,
                    extensible: true,
                };
                let iter_oid = self.heap.allocate(iter_obj);
                // For keys/entries, mark the iterator with its kind for the next-time
                // unwrapping. We piggyback on a property since extending the enum would
                // touch many places; this is a temporary signal read inside IteratorNext.
                if name != "values" {
                    let kind_key = self.interner.intern("__iter_kind__");
                    let kind_str = self.interner.intern(&name);
                    if let Some(o) = self.heap.get_mut(iter_oid) {
                        o.set_property(kind_key, Value::string(kind_str));
                    }
                }
                Ok(Value::object_id(iter_oid))
            }
            "hasOwnProperty" => {
                let key = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                let has = if let Ok(idx) = key.parse::<usize>() {
                    self.heap.get(oid).map(|o| {
                        if let ObjectKind::Array(ref elems) = o.kind { idx < elems.len() } else { false }
                    }).unwrap_or(false)
                } else {
                    let key_id = self.interner.intern(&key);
                    self.heap.get(oid).map(|o| o.has_own_property(key_id)).unwrap_or(false)
                };
                Ok(Value::boolean(has))
            }
            "propertyIsEnumerable" => {
                let key = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                let is_enum = if let Ok(idx) = key.parse::<usize>() {
                    self.heap.get(oid).map(|o| {
                        if let ObjectKind::Array(ref elems) = o.kind { idx < elems.len() } else { false }
                    }).unwrap_or(false)
                } else {
                    let key_id = self.interner.intern(&key);
                    self.heap.get(oid)
                        .and_then(|o| o.get_property_descriptor(key_id))
                        .map(|p| p.is_enumerable())
                        .unwrap_or(false)
                };
                Ok(Value::boolean(is_enum))
            }
            "isPrototypeOf" => {
                let target = args.first().copied().unwrap_or(Value::undefined());
                let result = self.is_prototype_of(Value::object_id(oid), target);
                Ok(Value::boolean(result))
            }
            "valueOf" => Ok(Value::object_id(oid)),
            "toLocaleString" => {
                // Array toLocaleString: join with ","
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                let parts: Vec<String> = elements.iter().map(|v| {
                    if v.is_null() || v.is_undefined() { String::new() } else { self.value_to_string(*v) }
                }).collect();
                let result = parts.join(",");
                let id = self.interner.intern(&result);
                Ok(Value::string(id))
            }
            _ => Ok(Value::undefined()),
        }
    }

    /// Helper to flatten an array to a given depth.
    /// Walk the prototype chain of `target` to see if `proto` appears.
    pub(crate) fn is_prototype_of(&self, proto: Value, target: Value) -> bool {
        let proto_oid = match proto.as_object_id() {
            Some(oid) => oid,
            None => return false,
        };
        let start_oid = match target.as_object_id() {
            Some(oid) => oid,
            None => return false,
        };
        let mut current = self.heap.get(start_oid).and_then(|o| o.prototype);
        loop {
            match current {
                None => return false,
                Some(oid) if oid == proto_oid => return true,
                Some(oid) => current = self.heap.get(oid).and_then(|o| o.prototype),
            }
        }
    }

    fn flatten_array(&self, elements: &[Value], depth: usize) -> Vec<Value> {
        let mut result = Vec::new();
        for elem in elements {
            if depth > 0
                && let Some(oid) = elem.as_object_id()
                    && let Some(obj) = self.heap.get(oid)
                        && let ObjectKind::Array(ref inner) = obj.kind {
                            result.extend(self.flatten_array(inner, depth - 1));
                            continue;
                        }
            result.push(*elem);
        }
        result
    }

    // ---- Math method dispatch ----
    pub(crate) fn exec_math_method(&mut self, method_name: StringId, args: &[Value]) -> Value {
        let a = || args.first().and_then(|v| v.as_number()).unwrap_or(f64::NAN);
        let b = || args.get(1).and_then(|v| v.as_number()).unwrap_or(f64::NAN);

        // Fast path: compare StringId directly (avoids string allocation)
        let name_str = self.interner.resolve(method_name);
        let result = match name_str {
            "abs" => a().abs(),
            "floor" => a().floor(),
            "ceil" => a().ceil(),
            "round" => a().round(),
            "trunc" => a().trunc(),
            "sqrt" => a().sqrt(),
            "cbrt" => a().cbrt(),
            "sign" => a().signum(),
            "pow" => { let av = a(); let bv = b(); if av.abs() == 1.0 && bv.is_infinite() { f64::NAN } else { av.powf(bv) } },
            "log" => a().ln(),
            "log2" => a().log2(),
            "log10" => a().log10(),
            "exp" => a().exp(),
            "sin" => a().sin(),
            "cos" => a().cos(),
            "tan" => a().tan(),
            "asin" => a().asin(),
            "acos" => a().acos(),
            "atan" => a().atan(),
            "atan2" => a().atan2(b()),
            "random" => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let t = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .subsec_nanos();
                    t as f64 / u32::MAX as f64
                }
                #[cfg(target_arch = "wasm32")]
                {
                    0.42
                }
            }
            "max" => {
                if args.is_empty() { return Value::number(f64::NEG_INFINITY); }
                let mut m = f64::NEG_INFINITY;
                for arg in args {
                    let n = arg.as_number().unwrap_or(f64::NAN);
                    if n.is_nan() { return Value::number(f64::NAN); }
                    if n > m { m = n; }
                }
                m
            }
            "min" => {
                if args.is_empty() { return Value::number(f64::INFINITY); }
                let mut m = f64::INFINITY;
                for arg in args {
                    let n = arg.as_number().unwrap_or(f64::NAN);
                    if n.is_nan() { return Value::number(f64::NAN); }
                    if n < m { m = n; }
                }
                m
            }
            "hypot" => {
                let mut sum = 0.0;
                for arg in args {
                    let n = arg.as_number().unwrap_or(f64::NAN);
                    sum += n * n;
                }
                sum.sqrt()
            }
            "log1p" => a().ln_1p(),
            "expm1" => a().exp_m1(),
            "cosh" => a().cosh(),
            "sinh" => a().sinh(),
            "tanh" => a().tanh(),
            "asinh" => a().asinh(),
            "acosh" => a().acosh(),
            "atanh" => a().atanh(),
            "fround" => (a() as f32) as f64,
            "clz32" => {
                let n = a();
                if n.is_nan() || n.is_infinite() { 32.0 }
                else { (n as u32).leading_zeros() as f64 }
            }
            "imul" => {
                let x = a() as i32 as i64;
                let y = b() as i32 as i64;
                ((x * y) as i32) as f64
            }
            _ => return Value::undefined(),
        };
        Value::number(result)
    }

    // ---- Math sentinel dispatch (-700 to -726) ----
    pub(crate) fn exec_math_sentinel(&mut self, sentinel: i32, args: &[Value]) -> Value {
        let a0 = args.first().map(|v| self.to_f64(*v)).unwrap_or(f64::NAN);
        let a1 = args.get(1).map(|v| self.to_f64(*v)).unwrap_or(f64::NAN);
        let result = match sentinel {
            -700 => a0.sin(),
            -701 => a0.cos(),
            -702 => a0.abs(),
            -703 => a0.floor(),
            -704 => a0.ceil(),
            -705 => a0.round(),
            -706 => a0.sqrt(),
            -707 => { if a0.abs() == 1.0 && a1.is_infinite() { f64::NAN } else { a0.powf(a1) } },
            -708 => { // max
                if args.is_empty() { return Value::number(f64::NEG_INFINITY); }
                let mut m = f64::NEG_INFINITY;
                for arg in args { let n = self.to_f64(*arg); if n.is_nan() { return Value::number(f64::NAN); } else if n > m { m = n; } }
                m
            }
            -709 => { // min
                if args.is_empty() { return Value::number(f64::INFINITY); }
                let mut m = f64::INFINITY;
                for arg in args { let n = self.to_f64(*arg); if n.is_nan() { return Value::number(f64::NAN); } else if n < m { m = n; } }
                m
            }
            -710 => a0.exp(),
            -711 => a0.ln(),
            -712 => a0.log2(),
            -713 => a0.log10(),
            -714 => { // random
                let t = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default().subsec_nanos();
                return Value::number(t as f64 / u32::MAX as f64);
            }
            -715 => a0.trunc(),
            -716 => a0.signum(),
            -717 => a0.cbrt(),
            -718 => { // hypot
                let sum: f64 = args.iter().map(|v| { let n = self.to_f64(*v); n * n }).sum();
                sum.sqrt()
            }
            -719 => a0.atan2(a1),
            -720 => a0.atan(),
            -721 => a0.asin(),
            -722 => a0.acos(),
            -723 => a0.tan(),
            -724 => { // clz32
                if a0.is_nan() || a0.is_infinite() { 32.0 }
                else { (a0 as u32).leading_zeros() as f64 }
            }
            -725 => { // imul
                let x = a0 as i32 as i64; let y = a1 as i32 as i64;
                ((x * y) as i32) as f64
            }
            -726 => { // fround
                a0 as f32 as f64
            }
            _ => return Value::undefined(),
        };
        Value::number(result)
    }

    // ---- Global function dispatch ----
    pub(crate) fn exec_global_fn(&mut self, sentinel: i32, args: &[Value]) -> Value {
        match sentinel {
            -500 => { // parseInt
                let s = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                let radix = args.get(1).and_then(|v| v.as_number()).unwrap_or(10.0) as u32;
                let s = s.trim();
                let (s, neg) = if let Some(stripped) = s.strip_prefix('-') { (stripped, true) } else if let Some(stripped) = s.strip_prefix('+') { (stripped, false) } else { (s, false) };
                let s = if radix == 16 { s.strip_prefix("0x").or(s.strip_prefix("0X")).unwrap_or(s) } else { s };
                // Parse digits for the given radix
                let mut result = 0i64;
                let mut found = false;
                for c in s.chars() {
                    let d = c.to_digit(radix);
                    if let Some(d) = d { result = result * radix as i64 + d as i64; found = true; }
                    else { break; }
                }
                if !found { return Value::number(f64::NAN); }
                let result = if neg { -result } else { result };
                Value::number(result as f64)
            }
            -501 => { // parseFloat
                let s = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                let s = s.trim();
                Value::number(s.parse::<f64>().unwrap_or(f64::NAN))
            }
            -502 => { // isNaN
                let n = args.first().and_then(|v| v.as_number()).unwrap_or(f64::NAN);
                Value::boolean(n.is_nan())
            }
            -503 => { // isFinite
                let n = args.first().and_then(|v| v.as_number()).unwrap_or(f64::NAN);
                Value::boolean(n.is_finite())
            }
            -504 => { // String()
                match args.first().copied() {
                    None => Value::string(crate::util::interner::StringId(0)),
                    Some(v) if v.is_interned_string() || v.is_inline_string() => v,
                    Some(v) if self.is_string_like(v) => {
                        // ConsString/FlatString: flatten once and intern, so that
                        // repeated charAt/substr in a loop resolve in O(1) rather
                        // than re-flattening the cons tree on every access.
                        let s = self.value_to_string(v);
                        Value::string(self.interner.intern(&s))
                    }
                    Some(v) => {
                        let s = self.value_to_string(v);
                        self.new_str(&s)
                    }
                }
            }
            -505 => { // Number()
                let v = args.first().copied().unwrap_or(Value::int(0));
                if let Some(b) = self.as_bigint(v) {
                    // Number(bigint) is an explicit, allowed conversion (unlike ToNumber).
                    Value::number(num_traits::ToPrimitive::to_f64(&b).unwrap_or(f64::INFINITY))
                }
                else if let Some(n) = v.as_number() { Value::number(n) }
                else if v.is_boolean() { Value::number(if v.as_bool().unwrap() { 1.0 } else { 0.0 }) }
                else if v.is_null() { Value::number(0.0) }
                else if v.is_undefined() { Value::number(f64::NAN) }
                else if v.is_string() {
                    let s = self.value_to_string(v);
                    Value::number(s.trim().parse::<f64>().unwrap_or(f64::NAN))
                }
                else { Value::number(f64::NAN) }
            }
            -506 => { // Boolean()
                let v = args.first().copied().unwrap_or(Value::boolean(false));
                Value::boolean(self.truthy(v))
            }
            -517 | -519 => { // decodeURIComponent / decodeURI
                let s = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                // Spec-wise decodeURI preserves the reserved set (;/?:@&=+$,#),
                // decoding only non-reserved escapes; we full-percent-decode for
                // both. In practice decodeURI inputs rarely percent-encode the
                // reserved chars, so the observable difference is negligible.
                // Lenient: malformed escapes are left verbatim rather than
                // throwing URIError (exec_global_fn returns a plain Value and
                // can't signal a throw; lenient decode is safe for real input).
                let decoded = percent_decode_utf8(&s);
                let sid = self.interner.intern(&decoded);
                Value::string(sid)
            }
            -518 | -509 => { // encodeURIComponent / encodeURI
                let s = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                // encodeURI keeps the reserved + unreserved set unescaped;
                // encodeURIComponent keeps only the unreserved set.
                let keep_reserved = sentinel == -509;
                let encoded = percent_encode_uri(&s, keep_reserved);
                let sid = self.interner.intern(&encoded);
                Value::string(sid)
            }
            -530 => { // Number.isNaN
                let v = args.first().copied().unwrap_or(Value::undefined());
                // Number.isNaN does NOT coerce — only true for actual NaN number values
                if v.is_float() { Value::boolean(v.as_float().unwrap().is_nan()) }
                else { Value::boolean(false) }
            }
            -531 => { // Number.isFinite
                let v = args.first().copied().unwrap_or(Value::undefined());
                if let Some(n) = v.as_number() { Value::boolean(n.is_finite()) }
                else { Value::boolean(false) }
            }
            -532 => { // Number.isInteger
                let v = args.first().copied().unwrap_or(Value::undefined());
                if let Some(n) = v.as_number() { Value::boolean(n.fract() == 0.0 && n.is_finite()) }
                else { Value::boolean(false) }
            }
            -533 => { // Number.isSafeInteger
                let v = args.first().copied().unwrap_or(Value::undefined());
                if let Some(n) = v.as_number() {
                    Value::boolean(n.fract() == 0.0 && n.is_finite() && n.abs() <= 9007199254740991.0)
                } else { Value::boolean(false) }
            }
            -534 | -535 => { // String.fromCharCode / String.fromCodePoint
                let mut result = String::new();
                for v in args {
                    let code = self.to_f64(*v) as u32;
                    if let Some(c) = char::from_u32(code) {
                        result.push(c);
                    }
                }
                self.new_str(&result)
            }
            -536 => { // String.raw
                let template = args.first().copied().unwrap_or(Value::undefined());
                let raw_key = self.interner.intern("raw");
                let raw_arr_val = template.as_object_id()
                    .and_then(|oid| self.heap.get(oid))
                    .and_then(|o| o.get_property(raw_key))
                    .unwrap_or(Value::undefined());
                let raw_strs: Vec<Value> = raw_arr_val.as_object_id()
                    .and_then(|oid| self.heap.get(oid))
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                let mut result = String::new();
                for (i, s) in raw_strs.iter().enumerate() {
                    result.push_str(&self.value_to_string(*s));
                    if i + 1 < raw_strs.len() && i + 1 < args.len() {
                        result.push_str(&self.value_to_string(args[i + 1]));
                    }
                }
                self.new_str(&result)
            }
            -507 => { // Array.isArray
                let v = args.first().copied().unwrap_or(Value::undefined());
                let is_arr = v.as_object_id()
                    .and_then(|oid| self.heap.get(oid))
                    .map(|o| matches!(&o.kind, ObjectKind::Array(_)))
                    .unwrap_or(false);
                Value::boolean(is_arr)
            }
            -508 => { // Object(v) — coerce to object
                let arg = args.first().copied().unwrap_or(Value::undefined());
                // A raw BigInt is a primitive (despite living on the heap):
                // ToObject wraps it in a BigInt wrapper object (typeof "object").
                if self.is_bigint(arg) {
                    let proto = self.bigint_prototype_oid();
                    let prim_key = self.interner.intern("__primitive__");
                    let mut obj = crate::runtime::object::JsObject {
                        properties: Vec::new(), prototype: Some(proto),
                        kind: ObjectKind::Wrapper(arg), marked: false, extensible: true };
                    obj.set_property(prim_key, arg);
                    return Value::object_id(self.heap.allocate(obj));
                }
                // ToObject of an object — and functions ARE objects —
                // returns the value unchanged. core-js's toIndexedObject
                // runs every descriptor target through Object(t); minting
                // a fresh wrapper made hasOwn/getOwnPropertyDescriptor on
                // function targets miss everything.
                if arg.is_object() || arg.is_function() {
                    return arg;
                }
                let mut obj = crate::runtime::object::JsObject::ordinary();
                obj.prototype = Some(self.object_prototype);
                if !arg.is_null() && !arg.is_undefined() {
                    let prim_key = self.interner.intern("__primitive__");
                    obj.set_property(prim_key, arg);
                }
                // Chain the wrapper to its primitive's prototype so the inherited
                // valueOf/toString unwrap it (`Object(1) + 0` === 1) and
                // `Object(x) instanceof Ctor` holds.
                let proto_sentinel = if arg.as_bool().is_some() { Some(-506) }
                    else if arg.is_int() || arg.is_number() { Some(-505) }
                    else if arg.is_string() || self.is_cons_string(arg) { Some(-504) }
                    else if arg.is_symbol() { Some(-570) }
                    else { None };
                if let Some(s) = proto_sentinel {
                    let proto = match s {
                        -570 => self.symbol_prototype_oid(),
                        _ => self.func_prototypes.get(&s).copied().unwrap_or(self.object_prototype),
                    };
                    obj.prototype = Some(proto);
                }
                Value::object_id(self.heap.allocate(obj))
            }
            // Error constructors called without `new`
            -516..=-510 => {
                let error_type = match sentinel {
                    -510 => "Error",
                    -511 => "TypeError",
                    -512 => "RangeError",
                    -513 => "ReferenceError",
                    -514 => "SyntaxError",
                    -515 => "EvalError",
                    -516 => "URIError",
                    _ => "Error",
                };
                let msg = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                let mut err_obj = crate::runtime::object::JsObject::ordinary();
                err_obj.prototype = self.func_prototypes.get(&sentinel).copied()
                    .or(Some(self.object_prototype));
                let msg_key = self.interner.intern("message");
                let msg_id = self.interner.intern(&msg);
                err_obj.set_property(msg_key, Value::string(msg_id));
                let stack_key = self.interner.intern("stack");
                let stack_str = format!("{error_type}: {msg}");
                let stack_id = self.interner.intern(&stack_str);
                err_obj.set_property(stack_key, Value::string(stack_id));
                Value::object_id(self.heap.allocate(err_obj))
            }
            _ => Value::undefined(),
        }
    }
    /// Execute a native method sentinel that requires `this` context.
    /// Sentinels -590 to -599: Object.prototype / Function.prototype methods.
    /// Sentinels -600 to -629: Array.prototype methods.
    pub(crate) fn exec_native_method(&mut self, sentinel: i32, this_val: Value, args: &[Value]) -> Value {
        match sentinel {
            -590 => { // Object.prototype.hasOwnProperty — also checks __get_X__/__set_X__
                let key_val = args.first().copied().unwrap_or(Value::undefined());
                let key = if key_val.is_symbol() {
                    format!("__sym_{}__", key_val.as_symbol_id().unwrap())
                } else {
                    self.value_to_string(key_val)
                };
                let key_id = self.interner.intern(&key);
                let getter_key = self.interner.intern(&format!("__get_{key}__"));
                let setter_key = self.interner.intern(&format!("__set_{key}__"));
                if let Some(oid) = this_val.as_object_id() {
                    let has = self.heap.get(oid).map(|o| {
                        // Array/arguments objects also expose numeric indices and "length"
                        // as own properties even though they aren't in `properties`.
                        let array_match = if let ObjectKind::Array(ref elems) = o.kind {
                            if key == "length" { true }
                            else if let Ok(idx) = key.parse::<usize>() { idx < elems.len() }
                            else { false }
                        } else { false };
                        array_match
                            || o.has_own_property(key_id)
                            || o.has_own_property(getter_key)
                            || o.has_own_property(setter_key)
                    }).unwrap_or(false);
                    Value::boolean(has)
                } else if this_val.is_function() {
                    let sentinel = this_val.as_function().unwrap();
                    let has = self.fn_get_own_prop(sentinel, key_id).is_some();
                    Value::boolean(has)
                } else if this_val.is_string() {
                    // String primitives: numeric indices and "length" are own properties.
                    let has = if key == "length" {
                        true
                    } else if let Ok(idx) = key.parse::<usize>() {
                        idx < self.string_char_len(this_val) as usize
                    } else { false };
                    Value::boolean(has)
                } else { Value::boolean(false) }
            }
            -591 => { // Object.prototype.propertyIsEnumerable
                let key_val = args.first().copied().unwrap_or(Value::undefined());
                let key = if key_val.is_symbol() {
                    format!("__sym_{}__", key_val.as_symbol_id().unwrap())
                } else {
                    self.value_to_string(key_val)
                };
                let key_id = self.interner.intern(&key);
                let getter_key = self.interner.intern(&format!("__get_{key}__"));
                let setter_key = self.interner.intern(&format!("__set_{key}__"));
                if let Some(oid) = this_val.as_object_id() {
                    let is_enum = self.heap.get(oid).and_then(|o| {
                        // Accessor properties are stored under __get_/__set_;
                        // their descriptor's enumerable flag is the property's.
                        o.get_property_descriptor(key_id)
                            .or_else(|| o.get_property_descriptor(getter_key))
                            .or_else(|| o.get_property_descriptor(setter_key))
                    })
                    .map(|p| p.is_enumerable())
                    .unwrap_or(false);
                    Value::boolean(is_enum)
                } else { Value::boolean(false) }
            }
            -592 => { // Object.prototype.toString
                if let Some(oid) = this_val.as_object_id()
                    && let Some(obj) = self.heap.get(oid) {
                        let tag = match &obj.kind {
                            ObjectKind::Array(_) => "Array",
                            ObjectKind::Function(_) => "Function",
                            _ => "Object",
                        };
                        let s = self.interner.intern(&format!("[object {tag}]"));
                        return Value::string(s);
                    }
                let s = self.interner.intern("[object Object]");
                Value::string(s)
            }
            -598 => { // Error.prototype.toString — `${name}: ${message}`
                if let Some(oid) = this_val.as_object_id() {
                    let name_key = self.interner.intern("name");
                    let msg_key = self.interner.intern("message");
                    let name_s = self.heap.get_property_chain(oid, name_key)
                        .map(|v| self.value_to_string(v))
                        .unwrap_or_else(|| "Error".to_string());
                    let msg_s = self.heap.get_property_chain(oid, msg_key)
                        .map(|v| self.value_to_string(v))
                        .unwrap_or_default();
                    let s = if msg_s.is_empty() { name_s }
                            else if name_s.is_empty() { msg_s }
                            else { format!("{name_s}: {msg_s}") };
                    Value::string(self.interner.intern(&s))
                } else {
                    Value::string(self.interner.intern("Error"))
                }
            }
            -593 => { // Object.prototype.valueOf
                this_val
            }
            -639 => { // BigInt.prototype.toString(radix)
                let b = self.as_bigint(this_val)
                    .or_else(|| this_val.as_object_id()
                        .and_then(|o| self.heap.get(o))
                        .and_then(|o| if let ObjectKind::Wrapper(inner) = &o.kind { self.as_bigint(*inner) } else { None }));
                if let Some(b) = b {
                    let radix = if let Some(r) = args.first() { self.to_f64(*r) as u32 } else { 10 };
                    let s = if (2..=36).contains(&radix) { b.to_str_radix(radix) } else { b.to_string() };
                    Value::string(self.interner.intern(&s))
                } else {
                    Value::string(self.interner.intern("0"))
                }
            }
            -640 => { // BigInt.prototype.valueOf
                self.as_bigint(this_val).map(|b| self.make_bigint(b))
                    .or_else(|| this_val.as_object_id()
                        .and_then(|o| self.heap.get(o))
                        .and_then(|o| if let ObjectKind::Wrapper(inner) = &o.kind { Some(*inner) } else { None }))
                    .unwrap_or(this_val)
            }
            -594 => { // Object.prototype.isPrototypeOf
                let target = args.first().copied().unwrap_or(Value::undefined());
                if let Some(proto_oid) = this_val.as_object_id() {
                    if let Some(target_oid) = target.as_object_id() {
                        let mut current_proto = self.heap.get(target_oid).and_then(|o| o.prototype);
                        loop {
                            match current_proto {
                                None => break Value::boolean(false),
                                Some(oid) if oid == proto_oid => break Value::boolean(true),
                                Some(oid) => {
                                    current_proto = self.heap.get(oid).and_then(|o| o.prototype);
                                }
                            }
                        }
                    } else { Value::boolean(false) }
                } else { Value::boolean(false) }
            }
            -595 => { // Function.prototype.call — called with this=fn, args=[thisArg, ...rest]
                let this_arg = args.first().copied().unwrap_or(Value::undefined());
                let call_args: Vec<Value> = args.get(1..).unwrap_or_default().to_vec();
                self.call_function_this(this_val, this_arg, &call_args).unwrap_or(Value::undefined())
            }
            -596 => { // Function.prototype.apply — called with this=fn, args=[thisArg, argsArray]
                let this_arg = args.first().copied().unwrap_or(Value::undefined());
                let call_args = if let Some(arr_val) = args.get(1)
                    && let Some(arr_oid) = arr_val.as_object_id()
                    && let Some(obj) = self.heap.get(arr_oid)
                    && let ObjectKind::Array(ref e) = obj.kind { e.clone() } else { vec![] };
                self.call_function_this(this_val, this_arg, &call_args).unwrap_or(Value::undefined())
            }
            -597 => { // Function.prototype.bind — should be intercepted by CallMethod, fallback here
                Value::undefined()
            }
            // Boolean.prototype.toString / valueOf — unwrap a Boolean primitive or wrapper.
            -630 | -631 => {
                let inner = self.unwrap_wrapper_primitive(this_val, |v| v.is_boolean());
                let Some(inner) = inner else { return Value::undefined(); };
                if sentinel == -630 {
                    let s = if inner.to_boolean() { "true" } else { "false" };
                    Value::string(self.interner.intern(s))
                } else {
                    inner
                }
            }
            // Number.prototype.toString / valueOf — unwrap a Number primitive or wrapper.
            -632 | -633 => {
                let inner = self.unwrap_wrapper_primitive(this_val, |v| v.is_int() || v.is_number());
                let Some(inner) = inner else { return Value::undefined(); };
                if sentinel == -632 {
                    let s = self.value_to_string(inner);
                    Value::string(self.interner.intern(&s))
                } else {
                    inner
                }
            }
            // String.prototype.toString / valueOf — unwrap a String primitive or wrapper.
            -634 | -635 => {
                self.unwrap_wrapper_primitive(this_val, |v| v.is_string())
                    .unwrap_or(Value::undefined())
            }
            // Array.prototype methods: dispatch via exec_array_method using this_val as array
            sentinel if (-629..=-600).contains(&sentinel) => {
                let method_name = match sentinel {
                    -600 => "join", -601 => "push", -602 => "pop", -603 => "shift",
                    -604 => "unshift", -605 => "indexOf", -606 => "includes", -607 => "forEach",
                    -608 => "map", -609 => "filter", -610 => "reduce", -611 => "some",
                    -612 => "every", -613 => "find", -614 => "findIndex", -615 => "slice",
                    -616 => "concat", -617 => "reverse", -618 => "sort", -619 => "flat",
                    -620 => "flatMap", -621 => "fill", -622 => "splice", -623 => "reduceRight",
                    -624 => "at", -625 => "keys", -626 => "values", -627 => "entries",
                    -628 => "lastIndexOf", -629 => "toString",
                    _ => return Value::undefined(),
                };
                let method_id = self.interner.intern(method_name);
                // For array-like objects (including actual arrays)
                if let Some(oid) = this_val.as_object_id() {
                    self.exec_array_method(oid, method_id, args).unwrap_or(Value::undefined())
                } else {
                    Value::undefined()
                }
            }
            _ => Value::undefined(),
        }
    }

    /// Check if a value is a String wrapper object.
    /// Unwrap a primitive-wrapper receiver to its inner primitive when it
    /// matches `want`. Handles both the `Wrapper` kind (`new Number(1)`) and the
    /// legacy `__primitive__`-property form (`Object(1)`); a bare matching
    /// primitive is returned as-is.
    pub(crate) fn unwrap_wrapper_primitive(&mut self, this_val: Value, want: fn(Value) -> bool) -> Option<Value> {
        if want(this_val) { return Some(this_val); }
        let oid = this_val.as_object_id()?;
        if let Some(obj) = self.heap.get(oid)
            && let ObjectKind::Wrapper(v) = &obj.kind
        {
            return if want(*v) { Some(*v) } else { None };
        }
        let prim_key = self.interner.intern("__primitive__");
        self.heap.get(oid).and_then(|o| o.get_property(prim_key)).filter(|v| want(*v))
    }

    pub(crate) fn is_string_wrapper(&self, val: Value) -> bool {
        if let Some(oid) = val.as_object_id()
            && let Some(obj) = self.heap.get(oid)
                && let ObjectKind::Wrapper(inner) = &obj.kind {
                    return inner.is_string();
                }
        false
    }
    /// Coerce a value to a primitive, propagating any exception thrown from
    /// the `valueOf`/`toString`/`@@toPrimitive` method as `VmError::Throw(_)`.
    /// The caller is expected to either propagate the throw or re-route it via
    /// `handle_throw` once its own stack is balanced.
    pub(crate) fn try_coerce_to_primitive_hint(&mut self, val: Value, hint_str: &str) -> Result<Value, super::vm::VmError> {
        let prev_protect = self.protect_throw_depth;
        self.protect_throw_depth = self.frames.len() + 1;
        let result = self.try_coerce_to_primitive_hint_inner(val, hint_str);
        self.protect_throw_depth = prev_protect;
        result
    }

    fn try_coerce_to_primitive_hint_inner(&mut self, val: Value, hint_str: &str) -> Result<Value, super::vm::VmError> {
        // A raw BigInt is already a primitive (it lives on the heap only because
        // the Value tag space is full) — ToPrimitive returns it unchanged.
        if self.is_bigint(val) {
            return Ok(val);
        }
        // Function values are tagged primitives in the VM but spec-wise are
        // ordinary objects. Coerce them to a string via the canonical
        // toString form so they participate in '+' / '<' / '==' correctly.
        if val.is_function() {
            let _ = hint_str;
            let sentinel = val.as_function().unwrap();
            let name_id = self.interner.intern("name");
            let name = self.fn_get_own_prop(sentinel, name_id)
                .and_then(|v| v.as_string_id())
                .map(|sid| self.interner.resolve(sid).to_owned())
                .unwrap_or_default();
            let formatted = format!("function {name}() {{ [native code] }}");
            return Ok(Value::string(self.interner.intern(&formatted)));
        }
        if let Some(oid) = val.as_object_id() {
            if let Some(obj) = self.heap.get(oid)
                && let ObjectKind::Wrapper(inner) = &obj.kind {
                    return Ok(*inner);
                }
            // Track whether any method existed so we can distinguish "had no
            // primitive coercion" (return object as-is for back-compat with
            // string contexts) from "all methods returned objects" (throw TypeError).
            let mut tried_method = false;
            // Check for Symbol.toPrimitive method (data or accessor).
            let sym_key_str = format!("__sym_{}__", self.sym_to_primitive);
            let sym_key = self.interner.intern(&sym_key_str);
            // Accessor form: __get___sym_N___ — defineProperty stores accessors
            // under __get_<key>__ where <key> is the Symbol-encoded key.
            let sym_getter_key = self.interner.intern(&format!("__get_{sym_key_str}__"));
            let tp_fn = if let Some(getter) = self.heap.get_property_chain(oid, sym_getter_key)
                && getter.is_function()
            {
                Some(self.call_function_this(getter, val, &[])?)
            } else {
                self.heap.get_property_chain(oid, sym_key)
            };
            if let Some(tp_fn) = tp_fn
                && !tp_fn.is_nullish()
            {
                // GetMethod(@@toPrimitive): a present-but-not-callable value is a
                // TypeError (e.g. `{[Symbol.toPrimitive]: 1}`).
                if !tp_fn.is_function() {
                    let err = self.make_native_error("TypeError", "object[Symbol.toPrimitive] is not a function");
                    return Err(super::vm::VmError::Throw(err));
                }
                let hint = self.interner.intern(hint_str);
                let result = self.call_function_this(tp_fn, val, &[Value::string(hint)])?;
                if !result.is_object() || self.is_bigint(result) { return Ok(result); }
                // @@toPrimitive returning an object always throws.
                let err = self.make_native_error("TypeError", "Cannot convert object to primitive value");
                return Err(super::vm::VmError::Throw(err));
            }
            // Per spec, "string" hint tries toString first, otherwise valueOf first.
            let (try_first, try_second) = if hint_str == "string" {
                ("toString", "valueOf")
            } else {
                ("valueOf", "toString")
            };
            let first_key = self.interner.intern(try_first);
            if let Some(fn1) = self.heap.get_property_chain(oid, first_key)
                && fn1.is_function()
            {
                tried_method = true;
                let result = self.call_function_this(fn1, val, &[])?;
                if !result.is_object() || self.is_bigint(result) { return Ok(result); }
            }
            let second_key = self.interner.intern(try_second);
            if let Some(fn2) = self.heap.get_property_chain(oid, second_key)
                && fn2.is_function()
            {
                tried_method = true;
                let result = self.call_function_this(fn2, val, &[])?;
                if !result.is_object() || self.is_bigint(result) { return Ok(result); }
            }
            // Both methods returned objects (or weren't callable in a way that produced
            // a primitive) — per spec, OrdinaryToPrimitive throws TypeError.
            if tried_method {
                let (line, pc, chunk_name) = if let Some(f) = self.frames.last() {
                    let cn = self.interner.resolve(self.chunks[f.chunk_idx].name).to_owned();
                    (self.chunks[f.chunk_idx].get_line(f.ip as u32), f.ip, cn)
                } else { (0, 0, String::new()) };
                let msg = format!("Cannot convert object to primitive value (at line {line}, pc {pc}, chunk '{chunk_name}')");
                let err = self.make_native_error("TypeError", &msg);
                return Err(super::vm::VmError::Throw(err));
            }
        }
        Ok(val)
    }
}

/// Percent-decode a string into UTF-8 (backs decodeURIComponent / decodeURI).
/// `%XX` byte escapes are collected and interpreted as UTF-8; a malformed or
/// truncated escape is left verbatim (lenient — see call site). Non-escape
/// characters pass through unchanged.
fn percent_decode_utf8(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    // Decoded bytes should be valid UTF-8 for well-formed input; fall back to
    // a lossy conversion rather than dropping the result on stray bytes.
    match String::from_utf8(out) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    }
}

/// Percent-encode a string per encodeURI / encodeURIComponent. The unreserved
/// set (A-Z a-z 0-9 - _ . ! ~ * ' ( )) is always kept; when `keep_reserved`
/// is true (encodeURI) the reserved set (; , / ? : @ & = + $ #) is also kept.
fn percent_encode_uri(s: &str, keep_reserved: bool) -> String {
    const RESERVED: &[u8] = b";,/?:@&=+$#";
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        let unreserved = b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')');
        if unreserved || (keep_reserved && RESERVED.contains(&b)) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(char::from_digit((b >> 4) as u32, 16).unwrap().to_ascii_uppercase());
            out.push(char::from_digit((b & 0xf) as u32, 16).unwrap().to_ascii_uppercase());
        }
    }
    out
}
