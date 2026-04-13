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
                let pos = s.find(&search).map(|i| i as i32).unwrap_or(-1);
                Value::int(pos)
            }
            "lastIndexOf" => {
                let search = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                let pos = s.rfind(&search).map(|i| i as i32).unwrap_or(-1);
                Value::int(pos)
            }
            "includes" => {
                let search = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                Value::boolean(s.contains(&search))
            }
            "startsWith" => {
                let search = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                Value::boolean(s.starts_with(&search))
            }
            "endsWith" => {
                let search = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                Value::boolean(s.ends_with(&search))
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
                let sep = args.first().map(|v| self.value_to_string(*v)).unwrap_or_else(|| ",".into());
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
                if let Some(obj) = self.heap.get(oid)
                    && let ObjectKind::Array(ref elements) = obj.kind {
                        for (i, elem) in elements.iter().enumerate() {
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
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                let mut results = Vec::with_capacity(elements.len());
                for (i, elem) in elements.iter().enumerate() {
                    let result = self.call_function(callback, &[*elem, Value::int(i as i32)])?;
                    results.push(result);
                }
                let arr = JsObject::array(results);
                let new_oid = self.heap.allocate(arr);
                Ok(Value::object_id(new_oid))
            }
            "filter" => {
                let callback = args.first().copied().unwrap_or(Value::undefined());
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                let mut results = Vec::new();
                for (i, elem) in elements.iter().enumerate() {
                    let result = self.call_function(callback, &[*elem, Value::int(i as i32)])?;
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
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                for (i, elem) in elements.iter().enumerate() {
                    self.call_function(callback, &[*elem, Value::int(i as i32)])?;
                }
                Ok(Value::undefined())
            }
            "find" => {
                let callback = args.first().copied().unwrap_or(Value::undefined());
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                for (i, elem) in elements.iter().enumerate() {
                    let result = self.call_function(callback, &[*elem, Value::int(i as i32)])?;
                    if result.to_boolean() { return Ok(*elem); }
                }
                Ok(Value::undefined())
            }
            "some" => {
                let callback = args.first().copied().unwrap_or(Value::undefined());
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                for (i, elem) in elements.iter().enumerate() {
                    let result = self.call_function(callback, &[*elem, Value::int(i as i32)])?;
                    if result.to_boolean() { return Ok(Value::boolean(true)); }
                }
                Ok(Value::boolean(false))
            }
            "every" => {
                let callback = args.first().copied().unwrap_or(Value::undefined());
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                for (i, elem) in elements.iter().enumerate() {
                    let result = self.call_function(callback, &[*elem, Value::int(i as i32)])?;
                    if !result.to_boolean() { return Ok(Value::boolean(false)); }
                }
                Ok(Value::boolean(true))
            }
            "findIndex" => {
                let callback = args.first().copied().unwrap_or(Value::undefined());
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                for (i, elem) in elements.iter().enumerate() {
                    let result = self.call_function(callback, &[*elem, Value::int(i as i32)])?;
                    if result.to_boolean() { return Ok(Value::int(i as i32)); }
                }
                Ok(Value::int(-1))
            }
            "findLast" => {
                let callback = args.first().copied().unwrap_or(Value::undefined());
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                for (i, elem) in elements.iter().enumerate().rev() {
                    let result = self.call_function(callback, &[*elem, Value::int(i as i32)])?;
                    if result.to_boolean() { return Ok(*elem); }
                }
                Ok(Value::undefined())
            }
            "findLastIndex" => {
                let callback = args.first().copied().unwrap_or(Value::undefined());
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                for (i, elem) in elements.iter().enumerate().rev() {
                    let result = self.call_function(callback, &[*elem, Value::int(i as i32)])?;
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
        if let Some(oid) = val.as_object_id() {
            if let Some(obj) = self.heap.get(oid)
                && let ObjectKind::Wrapper(inner) = &obj.kind {
                    return *inner;
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
