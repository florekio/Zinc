use crate::runtime::object::{JsObject, ObjectKind};
use crate::runtime::value::Value;
use crate::util::interner::StringId;

use super::vm::{Vm, VmError};

impl Vm {
    // ---- String method dispatch ----
    pub(crate) fn exec_string_method(&mut self, s: &str, method_name: StringId, args: &[Value]) -> Value {
        let name = self.interner.resolve(method_name).to_owned();
        match name.as_str() {
            "charAt" => {
                let idx = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                let ch = s.chars().nth(idx).map(|c| c.to_string()).unwrap_or_default();
                let id = self.interner.intern(&ch);
                Value::string(id)
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
                let id = self.interner.intern(result);
                Value::string(id)
            }
            "substring" => {
                let len = s.len() as i32;
                let mut start = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
                let mut end = args.get(1).and_then(|v| v.as_number()).map(|n| n as i32).unwrap_or(len);
                start = start.max(0).min(len);
                end = end.max(0).min(len);
                if start > end { std::mem::swap(&mut start, &mut end); }
                let result = &s[start as usize..end as usize];
                let id = self.interner.intern(result);
                Value::string(id)
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
                let id = self.interner.intern(result);
                Value::string(id)
            }
            "toUpperCase" => {
                let result = s.to_uppercase();
                let id = self.interner.intern(&result);
                Value::string(id)
            }
            "toLowerCase" => {
                let result = s.to_lowercase();
                let id = self.interner.intern(&result);
                Value::string(id)
            }
            "trim" => {
                let id = self.interner.intern(s.trim());
                Value::string(id)
            }
            "trimStart" => {
                let id = self.interner.intern(s.trim_start());
                Value::string(id)
            }
            "trimEnd" => {
                let id = self.interner.intern(s.trim_end());
                Value::string(id)
            }
            "normalize" => {
                // No Unicode normalization library: return as-is (sufficient for ASCII)
                let id = self.interner.intern(s);
                Value::string(id)
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
                    let id = self.interner.intern(part);
                    parts.push(Value::string(id));
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
                let replacement = args.get(1).map(|v| self.value_to_string(*v)).unwrap_or_default();
                let result = if name == "replaceAll" {
                    s.replace(&search, &replacement)
                } else {
                    s.replacen(&search, &replacement, 1)
                };
                let id = self.interner.intern(&result);
                Value::string(id)
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
                let id = self.interner.intern(&result);
                Value::string(id)
            }
            "padStart" => {
                let target_len = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                let pad = args.get(1).map(|v| self.value_to_string(*v)).unwrap_or_else(|| " ".into());
                let mut result = s.to_string();
                while result.len() < target_len {
                    result.insert_str(0, &pad);
                }
                if result.len() > target_len { result.truncate(target_len); }
                let id = self.interner.intern(&result);
                Value::string(id)
            }
            "padEnd" => {
                let target_len = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                let pad = args.get(1).map(|v| self.value_to_string(*v)).unwrap_or_else(|| " ".into());
                let mut result = s.to_string();
                while result.len() < target_len {
                    result.push_str(&pad);
                }
                if result.len() > target_len { result.truncate(target_len); }
                let id = self.interner.intern(&result);
                Value::string(id)
            }
            "concat" => {
                let mut result = s.to_string();
                for arg in args {
                    result.push_str(&self.value_to_string(*arg));
                }
                let id = self.interner.intern(&result);
                Value::string(id)
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
                    let id = self.interner.intern(&ch);
                    Value::string(id)
                } else {
                    Value::undefined()
                }
            }
            "toString" | "valueOf" => {
                let id = self.interner.intern(s);
                Value::string(id)
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
            "keys" => {
                let len = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.len() } else { 0 })
                    .unwrap_or(0);
                let keys: Vec<Value> = (0..len).map(|i| Value::int(i as i32)).collect();
                let arr = JsObject::array(keys);
                let new_oid = self.heap.allocate(arr);
                Ok(Value::object_id(new_oid))
            }
            "values" => {
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                let arr = JsObject::array(elements);
                let new_oid = self.heap.allocate(arr);
                Ok(Value::object_id(new_oid))
            }
            "entries" => {
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                let mut entries = Vec::with_capacity(elements.len());
                for (i, elem) in elements.iter().enumerate() {
                    let pair = JsObject::array(vec![Value::int(i as i32), *elem]);
                    let pair_oid = self.heap.allocate(pair);
                    entries.push(Value::object_id(pair_oid));
                }
                let arr = JsObject::array(entries);
                let new_oid = self.heap.allocate(arr);
                Ok(Value::object_id(new_oid))
            }
            _ => Ok(Value::undefined()),
        }
    }

    /// Helper to flatten an array to a given depth.
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
            "pow" => a().powf(b()),
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
                let s = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                let id = self.interner.intern(&s);
                Value::string(id)
            }
            -505 => { // Number()
                let v = args.first().copied().unwrap_or(Value::int(0));
                if let Some(n) = v.as_number() { Value::number(n) }
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
                Value::boolean(v.to_boolean())
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
                let id = self.interner.intern(&result);
                Value::string(id)
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
                let id = self.interner.intern(&result);
                Value::string(id)
            }
            // Error constructors called without `new`
            -514..=-510 => {
                let error_type = match sentinel {
                    -510 => "Error",
                    -511 => "TypeError",
                    -512 => "RangeError",
                    -513 => "ReferenceError",
                    -514 => "SyntaxError",
                    _ => "Error",
                };
                let msg = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                let mut err_obj = crate::runtime::object::JsObject::ordinary();
                let msg_key = self.interner.intern("message");
                let msg_id = self.interner.intern(&msg);
                err_obj.set_property(msg_key, Value::string(msg_id));
                let name_key = self.interner.intern("name");
                let name_id = self.interner.intern(error_type);
                err_obj.set_property(name_key, Value::string(name_id));
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
                let key = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                let key_id = self.interner.intern(&key);
                let getter_key = self.interner.intern(&format!("__get_{key}__"));
                let setter_key = self.interner.intern(&format!("__set_{key}__"));
                if let Some(oid) = this_val.as_object_id() {
                    let has = self.heap.get(oid).map(|o| {
                        o.has_own_property(key_id)
                            || o.has_own_property(getter_key)
                            || o.has_own_property(setter_key)
                    }).unwrap_or(false);
                    Value::boolean(has)
                } else { Value::boolean(false) }
            }
            -591 => { // Object.prototype.propertyIsEnumerable
                let key = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                let key_id = self.interner.intern(&key);
                if let Some(oid) = this_val.as_object_id() {
                    let is_enum = self.heap.get(oid)
                        .and_then(|o| o.get_property_descriptor(key_id))
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
            -593 => { // Object.prototype.valueOf
                this_val
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
    pub(crate) fn is_string_wrapper(&self, val: Value) -> bool {
        if let Some(oid) = val.as_object_id()
            && let Some(obj) = self.heap.get(oid)
                && let ObjectKind::Wrapper(inner) = &obj.kind {
                    return inner.is_string();
                }
        false
    }
    /// Unwrap a wrapper object to its primitive, or return the value as-is.
    /// ToPrimitive with valueOf/toString calls.
    pub(crate) fn coerce_to_primitive(&mut self, val: Value) -> Value {
        self.coerce_to_primitive_hint(val, "default")
    }

    pub(crate) fn coerce_to_number_primitive(&mut self, val: Value) -> Value {
        self.coerce_to_primitive_hint(val, "number")
    }

    fn coerce_to_primitive_hint(&mut self, val: Value, hint_str: &str) -> Value {
        if let Some(oid) = val.as_object_id() {
            if let Some(obj) = self.heap.get(oid)
                && let ObjectKind::Wrapper(inner) = &obj.kind {
                    return *inner;
                }
            // Check for Symbol.toPrimitive method
            let sym_key = self.interner.intern(&format!("__sym_{}__", self.sym_to_primitive));
            if let Some(tp_fn) = self.heap.get_property_chain(oid, sym_key)
                && tp_fn.is_function()
            {
                let hint = self.interner.intern(hint_str);
                if let Ok(result) = self.call_function_this(tp_fn, val, &[Value::string(hint)])
                    && !result.is_object() {
                        return result;
                    }
            }
            // Try valueOf()
            let valueof_key = self.interner.intern("valueOf");
            if let Some(vfn) = self.heap.get_property_chain(oid, valueof_key)
                && vfn.is_function()
                    && let Ok(result) = self.call_function_this(vfn, val, &[])
                        && !result.is_object() {
                            return result;
                        }
            // Try toString()
            let tostring_key = self.interner.intern("toString");
            if let Some(tfn) = self.heap.get_property_chain(oid, tostring_key)
                && tfn.is_function()
                    && let Ok(result) = self.call_function_this(tfn, val, &[])
                        && !result.is_object() {
                            return result;
                        }
        }
        val
    }
}
