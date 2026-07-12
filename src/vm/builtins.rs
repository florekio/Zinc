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

    /// Fast path for cheap indexing methods (`charAt`/`charCodeAt`/`substr`) on
    /// an **interned ASCII** receiver. The generic dispatch clones the whole
    /// receiver into an owned `String` (a borrow-checker workaround so the
    /// method body can call `&mut self`); here we instead read only the small
    /// *result* out of the borrowed `&str`, drop the borrow, then materialize
    /// it — so a `charAt` on an 88-char string no longer copies 88 bytes.
    ///
    /// Returns `None` (fall through to the generic path) for any other method
    /// or a non-ASCII receiver. Semantics mirror `exec_string_method` exactly.
    #[inline]
    pub(crate) fn try_fast_string_index_method(
        &mut self,
        rid: StringId,
        method_name: StringId,
        arg0: Option<f64>,
        arg1: Option<f64>,
    ) -> Option<Value> {
        if !self.interner.is_ascii(rid) {
            return None;
        }
        let kind = match self.interner.resolve(method_name) {
            "charAt" => 1u8,
            "charCodeAt" => 2,
            "substr" => 3,
            _ => return None,
        };
        enum R {
            Num(f64),
            Str(String),
        }
        // Borrow ends with this block; only the (small) result escapes.
        let r = {
            let s = self.interner.resolve(rid);
            let b = s.as_bytes();
            let n = b.len();
            match kind {
                1 => {
                    let i = arg0.unwrap_or(0.0) as usize;
                    R::Str(if i < n { s[i..i + 1].to_owned() } else { String::new() })
                }
                2 => {
                    let i = arg0.unwrap_or(0.0) as usize;
                    R::Num(if i < n { b[i] as f64 } else { f64::NAN })
                }
                _ => {
                    let len = n as i32;
                    let mut start = arg0.unwrap_or(0.0) as i32;
                    if start < 0 {
                        start = (start + len).max(0);
                    }
                    let start = start.min(len) as usize;
                    let length = arg1.map(|x| x as i32).unwrap_or(len - start as i32).max(0) as usize;
                    let end = (start + length).min(n);
                    R::Str(s[start..end].to_owned())
                }
            }
        };
        Some(match r {
            R::Num(x) => Value::number(x),
            R::Str(st) => self.new_str(&st),
        })
    }

    // ---- String method dispatch ----
    pub(crate) fn exec_string_method(&mut self, s: &str, method_name: StringId, args: &[Value], ascii: bool) -> Result<Value, VmError> {
        let name = self.interner.resolve(method_name).to_owned();
        match name.as_str() {
            "repeat" => {
                let count = match args.first().filter(|v| !v.is_undefined()) {
                    Some(v) => self.coerce_to_f64(*v)?,
                    None => 0.0,
                };
                if count < 0.0 || !count.is_finite() && count > 0.0 {
                    return Err(VmError::Throw(self.make_native_error(
                        "RangeError",
                        "Invalid count value",
                    )));
                }
                let count = count.max(0.0) as usize;
                // Bound the result size (matches the padStart cap).
                if s.len().saturating_mul(count) > 10_000_000 {
                    return Err(VmError::Throw(self.make_native_error(
                        "RangeError",
                        "Invalid string length",
                    )));
                }
                return Ok(self.new_str(&s.repeat(count)));
            }
            "normalize" => {
                if let Some(f) = args.first().filter(|v| !v.is_undefined()) {
                    let form = self.value_to_string(*f);
                    if !matches!(form.as_str(), "NFC" | "NFD" | "NFKC" | "NFKD") {
                        return Err(VmError::Throw(self.make_native_error(
                            "RangeError",
                            "The normalization form should be one of NFC, NFD, NFKC, NFKD",
                        )));
                    }
                }
            }
            "includes" | "startsWith" | "endsWith" => {
                // A RegExp search argument throws (IsRegExp check).
                if let Some(a) = args.first()
                    && a.as_object_id().and_then(|o| self.heap.get(o))
                        .is_some_and(|o| matches!(o.kind, ObjectKind::RegExp { .. }))
                {
                    return Err(VmError::Throw(self.make_native_error(
                        "TypeError",
                        "First argument must not be a regular expression",
                    )));
                }
            }
            "charAt" | "charCodeAt" | "codePointAt" | "at" | "indexOf" | "lastIndexOf"
            | "slice" | "substring" | "substr" | "padStart" | "padEnd" => {
                // Numeric arguments: Symbols throw, objects coerce observably.
                for a in args.iter().take(2) {
                    if a.is_symbol() {
                        return Err(VmError::Throw(self.make_native_error(
                            "TypeError",
                            "Cannot convert a Symbol value to a number",
                        )));
                    }
                    if a.is_object() {
                        self.try_coerce_to_primitive_hint(*a, "number")?;
                    }
                }
            }
            _ => {}
        }
        Ok(self.exec_string_method_inner(s, method_name, args, ascii))
    }

    fn exec_string_method_inner(&mut self, s: &str, method_name: StringId, args: &[Value], ascii: bool) -> Value {
        let name = self.interner.resolve(method_name).to_owned();
        match name.as_str() {
            "charAt" => {
                let idx = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                if ascii {
                    // byte index == char index for ASCII → O(1), no chars() walk
                    return if idx < s.len() { self.new_str(&s[idx..idx + 1]) } else { self.new_str("") };
                }
                let ch = s.chars().nth(idx).map(|c| c.to_string()).unwrap_or_default();
                self.new_str(&ch)
            }
            "charCodeAt" => {
                let idx = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                if ascii {
                    return if idx < s.len() { Value::number(s.as_bytes()[idx] as f64) } else { Value::number(f64::NAN) };
                }
                let code = s.chars().nth(idx).map(|c| c as u32 as f64).unwrap_or(f64::NAN);
                Value::number(code)
            }
            "indexOf" => {
                let search = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                let from = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0).max(0.0) as usize;
                if from >= s.len() {
                    return Value::int(if search.is_empty() { s.len() as i32 } else { -1 });
                }
                if ascii {
                    // char index == byte index → slice directly, no chars() walk
                    return Value::int(s[from..].find(&search).map(|i| (from + i) as i32).unwrap_or(-1));
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
                if ascii { return Value::boolean(s[from..].contains(&search)); }
                let sub: String = s.chars().skip(from).collect();
                Value::boolean(sub.contains(&search))
            }
            "startsWith" => {
                let search = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                let from = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0).max(0.0) as usize;
                if ascii {
                    return Value::boolean(from <= s.len() && s[from..].starts_with(&search));
                }
                let sub: String = s.chars().skip(from).collect();
                Value::boolean(sub.starts_with(&search))
            }
            "endsWith" => {
                let search = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                let end_pos = args.get(1).and_then(|v| v.as_number()).map(|n| n as usize).unwrap_or(s.chars().count());
                if ascii {
                    let end = end_pos.min(s.len());
                    return Value::boolean(s[..end].ends_with(&search));
                }
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
            "toLocaleUpperCase" | "toUpperCase" => {
                let result = s.to_uppercase();
                self.new_str(&result)
            }
            "localeCompare" => {
                // ToString(undefined) is "undefined" — a missing argument
                // compares like the literal string.
                let that = args.first()
                    .map(|v| self.value_to_string(*v))
                    .unwrap_or_else(|| "undefined".to_string());
                let ord = match s.cmp(that.as_str()) {
                    std::cmp::Ordering::Less => -1,
                    std::cmp::Ordering::Equal => 0,
                    std::cmp::Ordering::Greater => 1,
                };
                Value::int(ord)
            }
            "toLocaleLowerCase" | "toLowerCase" => {
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
                let sep_arg = args.first().copied().unwrap_or(Value::undefined());
                let limit = args.get(1)
                    .filter(|v| !v.is_undefined())
                    .map(|v| {
                        let prim = if v.is_object() {
                            self.try_coerce_to_primitive_hint(*v, "number").unwrap_or(*v)
                        } else {
                            *v
                        };
                        let n = self.to_f64(prim);
                        super::vm::f64_to_int32(n) as u32 as usize
                    });
                let mut parts: Vec<Value> = Vec::new();
                if limit != Some(0) {
                    if sep_arg.is_undefined() {
                        // No separator: the whole string is the only element.
                        parts.push(self.new_str(s));
                    } else {
                        // ToString(separator) runs user toString/valueOf.
                        let sep_prim = if sep_arg.is_object() {
                            self.try_coerce_to_primitive_hint(sep_arg, "string").unwrap_or(sep_arg)
                        } else {
                            sep_arg
                        };
                        let sep = self.value_to_string(sep_prim);
                        if sep.is_empty() {
                            // Per-char split (Rust's split("") adds boundary
                            // empties that JS doesn't have).
                            for c in s.chars() {
                                if let Some(lim) = limit && parts.len() >= lim { break; }
                                let v = self.new_str(&c.to_string());
                                parts.push(v);
                            }
                        } else {
                            for part in s.split(&sep) {
                                if let Some(lim) = limit && parts.len() >= lim { break; }
                                let v = self.new_str(part);
                                parts.push(v);
                            }
                        }
                    }
                }
                let mut arr = JsObject::array(parts);
                arr.prototype = Some(self.array_prototype);
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
                // Non-RegExp pattern: coerce to a RegExp source per spec
                // (match/search build RegExp(pattern)).
                let pat = match args.first().filter(|v| !v.is_undefined()) {
                    // The string IS the pattern (RegExp(pattern)) — verbatim.
                    Some(v) => self.value_to_string(*v),
                    None => String::new(),
                };
                let re_obj = JsObject {
                    properties: Vec::new(),
                    prototype: self.func_prototypes.get(&-580).copied(),
                    kind: ObjectKind::RegExp { pattern: pat, flags: String::new() },
                    marked: false,
                    extensible: true,
                };
                let roid = self.heap.allocate(re_obj);
                if let Some(result) = self.exec_string_regex_method(s, &name, &[Value::object_id(roid)]) {
                    return result;
                }
                Value::null()
            }
            "matchAll" => {
                if let Some(result) = self.exec_string_regex_method(s, &name, args) {
                    return result;
                }
                // Non-regex / no matches: an empty (iterable) array.
                self.alloc_array(vec![])
            }
            "repeat" => {
                let count = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize;
                let result = s.repeat(count);
                self.new_str(&result)
            }
            "padStart" | "padEnd" => {
                // Cap the target length: real engines RangeError past the max
                // string length; a dense multi-GB fill would OOM the process.
                let target_len = (args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as usize)
                    .min(1_000_000);
                let pad = args.get(1)
                    .filter(|v| !v.is_undefined())
                    .map(|v| self.value_to_string(*v))
                    .unwrap_or_else(|| " ".into());
                // Pad math is in UTF-16-ish char units, not bytes — byte
                // truncation split multibyte fillers mid-char (panic).
                let s_chars = s.chars().count();
                // An empty filler pads nothing (spec: return the string as-is)
                // — without this check the fill loop below never terminates.
                if pad.is_empty() || s_chars >= target_len {
                    return self.new_str(s);
                }
                let need = target_len - s_chars;
                let mut fill = String::with_capacity(need + pad.len());
                let mut filled = 0;
                'outer: loop {
                    for c in pad.chars() {
                        if filled == need { break 'outer; }
                        fill.push(c);
                        filled += 1;
                    }
                }
                let result = if name == "padStart" {
                    format!("{fill}{s}")
                } else {
                    format!("{s}{fill}")
                };
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
                if ascii {
                    return if idx < s.len() { Value::number(s.as_bytes()[idx] as f64) } else { Value::undefined() };
                }
                match s.chars().nth(idx) {
                    Some(c) => Value::number(c as u32 as f64),
                    None => Value::undefined(),
                }
            }
            "at" => {
                let idx = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as i32;
                let len = if ascii { s.len() as i32 } else { s.chars().count() as i32 };
                let actual = if idx < 0 { len + idx } else { idx };
                if actual >= 0 && (actual as usize) < len as usize {
                    if ascii {
                        let a = actual as usize;
                        return self.new_str(&s[a..a + 1]);
                    }
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


    /// Get(O, "length") for an array-like, getter-aware, clamped to the
    /// iteration cap so a poisoned length can't spin the VM.
    fn array_like_length(&mut self, oid: crate::runtime::object::ObjectId) -> Result<u64, VmError> {
        Ok(self.array_like_length_raw(oid)?.min(1_000_000))
    }

    /// Crate-visible fronts for the array-like protocol (Array.from wrapper).
    pub(crate) fn array_like_len_public(&mut self, oid: crate::runtime::object::ObjectId) -> Result<u64, VmError> {
        self.array_like_length(oid)
    }

    pub(crate) fn array_like_get_public(
        &mut self,
        oid: crate::runtime::object::ObjectId,
        idx: u64,
    ) -> Result<Option<Value>, VmError> {
        self.array_like_get(oid, idx)
    }

    /// ToLength(Get(O, "length")) without the iteration cap — for RangeError
    /// checks that need the spec value (ArrayCreate limits).
    fn array_like_length_raw(&mut self, oid: crate::runtime::object::ObjectId) -> Result<u64, VmError> {
        // Dense arrays without a shadowing named length use the element count.
        let len_key = self.interner.intern("length");
        let (is_array, elems_len, named) = match self.heap.get(oid) {
            Some(o) => (
                matches!(o.kind, ObjectKind::Array(_)),
                if let ObjectKind::Array(ref e) = o.kind { e.len() } else { 0 },
                o.get_property(len_key),
            ),
            None => (false, 0, None),
        };
        if is_array && named.is_none() {
            return Ok(elems_len as u64);
        }
        // Per-level walk: an own property of ANY form (data, getter, or
        // setter-only accessor — whose Get is undefined) shadows inherited
        // ones. A flat chain lookup of the getter key would wrongly see an
        // inherited getter past an own setter-only accessor.
        let getter_key = self.interner.intern("__get_length__");
        let setter_key = self.interner.intern("__set_length__");
        let mut raw = Value::undefined();
        let mut cur = Some(oid);
        let mut hops = 0;
        while let Some(c) = cur {
            let (getter, data, has_setter, proto) = match self.heap.get(c) {
                Some(o) => (
                    o.get_property(getter_key).filter(|v| v.is_function()),
                    o.get_property(len_key),
                    o.get_property(setter_key).is_some(),
                    o.prototype,
                ),
                None => (None, None, false, None),
            };
            if let Some(gfn) = getter {
                raw = self.call_function_this(gfn, Value::object_id(oid), &[])?;
                break;
            }
            if let Some(v) = data {
                raw = v;
                break;
            }
            if has_setter {
                break; // accessor without a getter: Get returns undefined
            }
            hops += 1;
            if hops > 64 {
                break;
            }
            cur = proto;
        }
        let n = self.coerce_to_f64(raw)?;
        if n.is_nan() || n <= 0.0 {
            return Ok(0);
        }
        Ok(n.min(9_007_199_254_740_991.0) as u64)
    }

    /// HasProperty + Get for an array-like index: own accessors, dense
    /// elements, named data properties, then the prototype chain. Getters run
    /// with the receiver as `this`. None = absent (hole).
    fn array_like_get(
        &mut self,
        oid: crate::runtime::object::ObjectId,
        idx: u64,
    ) -> Result<Option<Value>, VmError> {
        let key = self.interner.intern(&idx.to_string());
        let getter_key = self.interner.intern(&format!("__get_{idx}__"));
        let setter_key = self.interner.intern(&format!("__set_{idx}__"));
        let receiver = Value::object_id(oid);
        let mut cur = Some(oid);
        let mut hops = 0;
        while let Some(c) = cur {
            let (getter, data, has_setter, in_elems, proto) = match self.heap.get(c) {
                Some(o) => (
                    o.get_property(getter_key).filter(|v| v.is_function()),
                    o.get_property(key),
                    o.get_property(setter_key).is_some(),
                    if let ObjectKind::Array(ref e) = o.kind {
                        ((idx as usize) < e.len())
                            .then(|| e[idx as usize])
                            .filter(|v| !v.is_empty_marker())
                    } else {
                        None
                    },
                    o.prototype,
                ),
                None => (None, None, false, None, None),
            };
            if let Some(gfn) = getter {
                let prev = self.protect_throw_depth;
                self.protect_throw_depth = self.frames.len() + 1;
                let r = self.call_function_this(gfn, receiver, &[]);
                self.protect_throw_depth = prev;
                return r.map(Some);
            }
            if let Some(v) = data {
                return Ok(Some(v));
            }
            if has_setter {
                // Accessor without a getter: the property exists, Get is undefined.
                return Ok(Some(Value::undefined()));
            }
            if let Some(v) = in_elems {
                return Ok(Some(v));
            }
            hops += 1;
            if hops > 64 {
                break;
            }
            cur = proto;
        }
        // String wrapper receivers expose char indices.
        if let Some(o) = self.heap.get(oid)
            && let ObjectKind::Wrapper(inner) = o.kind
            && inner.is_string()
        {
            let s = self.value_to_string(inner);
            if let Some(ch) = s.chars().nth(idx as usize) {
                return Ok(Some(self.new_str(&ch.to_string())));
            }
        }
        Ok(None)
    }

    /// ToNumber that runs user valueOf/toString (ordinary `to_f64` is
    /// non-mutating and can't) — index arguments coerce observably per spec.
    /// Symbols throw TypeError.
    fn coerce_to_f64(&mut self, v: Value) -> Result<f64, VmError> {
        if v.is_symbol() {
            return Err(VmError::Throw(self.make_native_error(
                "TypeError",
                "Cannot convert a Symbol value to a number",
            )));
        }
        let p = if v.is_object() {
            self.try_coerce_to_primitive_hint(v, "number")?
        } else {
            v
        };
        if p.is_symbol() {
            return Err(VmError::Throw(self.make_native_error(
                "TypeError",
                "Cannot convert a Symbol value to a number",
            )));
        }
        Ok(self.to_f64(p))
    }

    /// Set(O, idx, v) for array-likes: chain setters run; dense arrays grow
    /// (hole-filled) as needed; other receivers store named properties.
    fn array_like_set(&mut self, oid: crate::runtime::object::ObjectId, idx: u64, v: Value) -> Result<(), VmError> {
        let setter_key = self.interner.intern(&format!("__set_{idx}__"));
        if let Some(sfn) = self.heap.get_property_chain(oid, setter_key)
            && self.value_callable(sfn)
        {
            self.call_function_this(sfn, Value::object_id(oid), &[v])?;
            return Ok(());
        }
        let is_array = self.heap.get(oid).is_some_and(|o| matches!(o.kind, ObjectKind::Array(_)));
        if is_array && idx < 1_000_000 {
            if let Some(o) = self.heap.get_mut(oid)
                && let ObjectKind::Array(ref mut e) = o.kind
            {
                while e.len() <= idx as usize {
                    e.push(Value::empty());
                }
                e[idx as usize] = v;
            }
            return Ok(());
        }
        let key = self.interner.intern(&idx.to_string());
        if let Some(o) = self.heap.get_mut(oid) {
            o.set_property(key, v);
        }
        Ok(())
    }

    /// DeletePropertyOrThrow-ish for array-like indices (sort's tail cleanup).
    fn array_like_delete(&mut self, oid: crate::runtime::object::ObjectId, idx: u64) {
        let key = self.interner.intern(&idx.to_string());
        let gk = self.interner.intern(&format!("__get_{idx}__"));
        let sk = self.interner.intern(&format!("__set_{idx}__"));
        if let Some(o) = self.heap.get_mut(oid) {
            o.delete_property(key);
            o.delete_property(gk);
            o.delete_property(sk);
            if let ObjectKind::Array(ref mut e) = o.kind
                && (idx as usize) < e.len()
            {
                e[idx as usize] = Value::empty();
            }
        }
    }

    /// SortCompare: comparator (protected) or default ToString comparison.
    fn sort_compare_pair(&mut self, a: Value, b: Value, comparator: Option<Value>) -> Result<std::cmp::Ordering, VmError> {
        use std::cmp::Ordering;
        if let Some(f) = comparator {
            let r = self.call_function_this(f, Value::undefined(), &[a, b])?;
            let n = self.to_f64(r);
            return Ok(if n < 0.0 {
                Ordering::Less
            } else if n > 0.0 {
                Ordering::Greater
            } else {
                Ordering::Equal
            });
        }
        let sa = {
            let p = if a.is_object() && !a.is_symbol() {
                self.try_coerce_to_primitive_hint(a, "string")?
            } else { a };
            self.value_to_string(p)
        };
        let sb = {
            let p = if b.is_object() && !b.is_symbol() {
                self.try_coerce_to_primitive_hint(b, "string")?
            } else { b };
            self.value_to_string(p)
        };
        Ok(sa.cmp(&sb))
    }

    /// Whether an array carries reconfigured index properties (accessors or
    /// named data entries) that the dense fast paths would miss.
    fn array_has_index_props(&self, oid: crate::runtime::object::ObjectId) -> bool {
        self.heap.get(oid).is_some_and(|o| {
            o.properties.iter().any(|(k, _)| {
                let ks = self.interner.resolve(*k);
                ks.bytes().all(|b| b.is_ascii_digit())
                    || (ks.starts_with("__get_") && ks[6..ks.len().saturating_sub(2)].bytes().all(|b| b.is_ascii_digit()))
                    || ks == "length"
            })
        })
    }

    /// Spec-shaped implementations of the iteration-family Array.prototype
    /// methods for GENERIC receivers (array-likes, arrays with reconfigured
    /// indices): length via Get, elements via HasProperty/Get with holes.
    /// Returns Ok(None) for methods without a generic form (caller falls
    /// back to the dense implementation).
    fn exec_array_method_generic(
        &mut self,
        oid: crate::runtime::object::ObjectId,
        name: &str,
        args: &[Value],
    ) -> Result<Option<Value>, VmError> {
        let obj_val = Value::object_id(oid);
        let callback = args.first().copied().unwrap_or(Value::undefined());
        let this_arg = args.get(1).copied().unwrap_or(Value::undefined());
        let require_callback = |vm: &mut Self| -> Result<(), VmError> {
            let callable = callback.is_function()
                || callback.as_object_id()
                    .and_then(|o| vm.heap.get(o))
                    .is_some_and(|o| matches!(o.kind, ObjectKind::Function(_)));
            if callable {
                Ok(())
            } else {
                Err(VmError::Throw(vm.make_native_error(
                    "TypeError",
                    "callback is not a function",
                )))
            }
        };
        match name {
            "forEach" | "map" | "filter" | "every" | "some"
            | "find" | "findIndex" | "findLast" | "findLastIndex" => {
                let len = if name == "map" {
                    // ArrayCreate(len) rejects lengths beyond 2^32-1.
                    let raw = self.array_like_length_raw(oid)?;
                    if raw > 4_294_967_295 {
                        return Err(VmError::Throw(
                            self.make_native_error("RangeError", "Invalid array length"),
                        ));
                    }
                    raw.min(1_000_000)
                } else {
                    self.array_like_length(oid)?
                };
                require_callback(self)?;
                let mut mapped: Vec<Value> = Vec::new();
                let mut filtered: Vec<Value> = Vec::new();
                let forward = !matches!(name, "findLast" | "findLastIndex");
                let indices: Vec<u64> = if forward { (0..len).collect() } else { (0..len).rev().collect() };
                for k in indices {
                    let elem = self.array_like_get(oid, k)?;
                    // find-family visits holes as undefined; the others skip.
                    let visits_holes = matches!(name, "find" | "findIndex" | "findLast" | "findLastIndex");
                    let Some(v) = elem.or(visits_holes.then(Value::undefined)) else {
                        if name == "map" {
                            // map preserves holes.
                            mapped.push(Value::empty());
                        }
                        continue;
                    };
                    let r = self.call_function_this(
                        callback,
                        this_arg,
                        &[v, Value::number(k as f64), obj_val],
                    )?;
                    match name {
                        "forEach" => {}
                        "map" => mapped.push(r),
                        "filter" => {
                            if r.to_boolean() {
                                filtered.push(v);
                            }
                        }
                        "every" => {
                            if !r.to_boolean() {
                                return Ok(Some(Value::boolean(false)));
                            }
                        }
                        "some" => {
                            if r.to_boolean() {
                                return Ok(Some(Value::boolean(true)));
                            }
                        }
                        "find" | "findLast" => {
                            if r.to_boolean() {
                                return Ok(Some(v));
                            }
                        }
                        "findIndex" | "findLastIndex" => {
                            if r.to_boolean() {
                                return Ok(Some(Value::number(k as f64)));
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                Ok(Some(match name {
                    "forEach" => Value::undefined(),
                    "map" => {
                        let mut arr = JsObject::array(mapped);
                        arr.prototype = Some(self.array_prototype);
                        Value::object_id(self.heap.allocate(arr))
                    }
                    "filter" => {
                        let mut arr = JsObject::array(filtered);
                        arr.prototype = Some(self.array_prototype);
                        Value::object_id(self.heap.allocate(arr))
                    }
                    "every" => Value::boolean(true),
                    "some" => Value::boolean(false),
                    "find" | "findLast" => Value::undefined(),
                    _ => Value::number(-1.0),
                }))
            }
            "reduce" | "reduceRight" => {
                let len = self.array_like_length(oid)?;
                require_callback(self)?;
                let has_init = args.len() > 1;
                let mut acc = args.get(1).copied();
                let indices: Vec<u64> = if name == "reduce" {
                    (0..len).collect()
                } else {
                    (0..len).rev().collect()
                };
                let mut it = indices.into_iter();
                if !has_init {
                    // Seed from the first PRESENT element (holes skipped).
                    for k in it.by_ref() {
                        if let Some(v) = self.array_like_get(oid, k)? {
                            acc = Some(v);
                            break;
                        }
                    }
                    if acc.is_none() {
                        return Err(VmError::Throw(self.make_native_error(
                            "TypeError",
                            "Reduce of empty array with no initial value",
                        )));
                    }
                }
                let mut acc = acc.unwrap_or(Value::undefined());
                for k in it {
                    if let Some(v) = self.array_like_get(oid, k)? {
                        acc = self.call_function_this(
                            callback,
                            Value::undefined(),
                            &[acc, v, Value::number(k as f64), obj_val],
                        )?;
                    }
                }
                Ok(Some(acc))
            }
            "indexOf" | "lastIndexOf" | "includes" => {
                let len = self.array_like_length(oid)? as i64;
                let search = args.first().copied().unwrap_or(Value::undefined());
                let from = match args.get(1) {
                    Some(v) => self.coerce_to_f64(*v)?,
                    None if name == "lastIndexOf" => (len - 1) as f64,
                    None => 0.0,
                };
                let norm = |f: f64, len: i64| -> i64 {
                    if f.is_nan() { 0 } else if f < 0.0 { (len as f64 + f).max(0.0) as i64 } else { f.min(len as f64) as i64 }
                };
                let result = if name == "lastIndexOf" {
                    let start = if from.is_nan() { -1 } else if from < 0.0 { len + from as i64 } else { (from as i64).min(len - 1) };
                    let mut found = -1i64;
                    let mut k = start;
                    while k >= 0 {
                        if let Some(v) = self.array_like_get(oid, k as u64)?
                            && self.strict_eq(v, search)
                        {
                            found = k;
                            break;
                        }
                        k -= 1;
                    }
                    found
                } else {
                    let start = norm(from, len);
                    let mut found = -1i64;
                    for k in start..len {
                        let elem = self.array_like_get(oid, k as u64)?;
                        let matched = match elem {
                            Some(v) => {
                                if name == "includes" {
                                    // SameValueZero: NaN matches NaN.
                                    self.strict_eq(v, search)
                                        || (self.to_f64(v).is_nan()
                                            && self.to_f64(search).is_nan()
                                            && (v.is_number() || v.is_int())
                                            && (search.is_number() || search.is_int()))
                                } else {
                                    self.strict_eq(v, search)
                                }
                            }
                            // includes treats holes as undefined.
                            None => name == "includes" && search.is_undefined(),
                        };
                        if matched {
                            found = k;
                            break;
                        }
                    }
                    found
                };
                Ok(Some(if name == "includes" {
                    Value::boolean(result >= 0)
                } else {
                    Value::number(result as f64)
                }))
            }
            "join" => {
                let len = self.array_like_length(oid)?;
                let sep = args.first()
                    .filter(|v| !v.is_undefined())
                    .map(|v| self.value_to_string(*v))
                    .unwrap_or_else(|| ",".into());
                let mut parts: Vec<String> = Vec::with_capacity(len as usize);
                for k in 0..len {
                    let v = self.array_like_get(oid, k)?;
                    parts.push(match v {
                        Some(v) if !v.is_undefined() && !v.is_null() => self.value_to_string(v),
                        _ => String::new(),
                    });
                }
                Ok(Some(self.new_str(&parts.join(&sep))))
            }
            "at" => {
                let len = self.array_like_length(oid)? as i64;
                let rel = match args.first() {
                    Some(v) => self.coerce_to_f64(*v)? as i64,
                    None => 0,
                };
                let k = if rel < 0 { len + rel } else { rel };
                if k < 0 || k >= len {
                    return Ok(Some(Value::undefined()));
                }
                Ok(Some(self.array_like_get(oid, k as u64)?.unwrap_or(Value::undefined())))
            }
            "slice" => {
                let len = self.array_like_length(oid)? as i64;
                let norm = |f: f64| -> i64 {
                    if f.is_nan() { 0 } else if f < 0.0 { (len + f as i64).max(0) } else { (f as i64).min(len) }
                };
                let start = match args.first() {
                    Some(v) => norm(self.coerce_to_f64(*v)?),
                    None => 0,
                };
                let end = match args.get(1).filter(|v| !v.is_undefined()) {
                    Some(v) => norm(self.coerce_to_f64(*v)?),
                    None => len,
                };
                let mut out = Vec::new();
                for k in start..end {
                    out.push(self.array_like_get(oid, k as u64)?.unwrap_or(Value::undefined()));
                }
                let mut arr = JsObject::array(out);
                arr.prototype = Some(self.array_prototype);
                Ok(Some(Value::object_id(self.heap.allocate(arr))))
            }
            "sort" => {
                let len = self.array_like_length(oid)?;
                let comparator = args.first().copied().filter(|v| self.value_callable(*v));
                // Collect: values, undefineds, holes (spec order after sort).
                let mut present: Vec<Value> = Vec::new();
                let mut undefs = 0usize;
                let mut holes = 0usize;
                for i in 0..len {
                    match self.array_like_get(oid, i)? {
                        None => holes += 1,
                        Some(v) if v.is_undefined() => undefs += 1,
                        Some(v) => present.push(v),
                    }
                }
                // Insertion sort with an observable comparator.
                for i in 1..present.len() {
                    let mut j = i;
                    while j > 0 {
                        if self.sort_compare_pair(present[j - 1], present[j], comparator)?
                            == std::cmp::Ordering::Greater
                        {
                            present.swap(j - 1, j);
                            j -= 1;
                        } else {
                            break;
                        }
                    }
                }
                let n_present = present.len();
                for (i, v) in present.into_iter().enumerate() {
                    self.array_like_set(oid, i as u64, v)?;
                }
                for i in n_present..n_present + undefs {
                    self.array_like_set(oid, i as u64, Value::undefined())?;
                }
                let _ = holes;
                for i in (n_present + undefs)..len as usize {
                    self.array_like_delete(oid, i as u64);
                }
                Ok(Some(obj_val))
            }
            _ => Ok(None),
        }
    }

    // ---- Date method dispatch ----
    /// All Date.prototype methods for a Date receiver, shared by the
    /// CallMethod fast path and the reified prototype wrappers. The engine is
    /// timezone-less (offset 0), so UTC and local accessors alias.
    pub(crate) fn exec_date_method(
        &mut self,
        oid: crate::runtime::object::ObjectId,
        name: &str,
        args: &[Value],
    ) -> Result<Value, VmError> {
        use super::vm::{epoch_to_ymd, epoch_weekday, format_iso, ymd_hms_to_ms, DAY_NAMES, MONTH_NAMES};
        let ms = match self.heap.get(oid).map(|o| &o.kind) {
            Some(&ObjectKind::Date(ms)) => ms,
            _ => {
                return Err(VmError::Throw(self.make_native_error(
                    "TypeError",
                    "this is not a Date object",
                )))
            }
        };
        // Time-of-day components (valid only when ms is finite).
        let hour = (ms / 3_600_000.0).rem_euclid(24.0).floor();
        let min = (ms / 60_000.0).rem_euclid(60.0).floor();
        let sec = (ms / 1000.0).rem_euclid(60.0).floor();
        let milli = ms.rem_euclid(1000.0).floor();
        let nan_guarded = |v: f64| -> Value {
            if ms.is_nan() { Value::number(f64::NAN) } else { Value::number(v) }
        };
        let result = match name {
            "getTime" | "valueOf" => Value::number(ms),
            "getFullYear" | "getUTCFullYear" => nan_guarded(epoch_to_ymd(ms).0 as f64),
            "getMonth" | "getUTCMonth" => nan_guarded(epoch_to_ymd(ms).1 as f64),
            "getDate" | "getUTCDate" => nan_guarded(epoch_to_ymd(ms).2 as f64),
            "getDay" | "getUTCDay" => nan_guarded(epoch_weekday(ms) as f64),
            "getHours" | "getUTCHours" => nan_guarded(hour),
            "getMinutes" | "getUTCMinutes" => nan_guarded(min),
            "getSeconds" | "getUTCSeconds" => nan_guarded(sec),
            "getMilliseconds" | "getUTCMilliseconds" => nan_guarded(milli),
            "getTimezoneOffset" => nan_guarded(0.0),
            "getYear" => nan_guarded((epoch_to_ymd(ms).0 - 1900) as f64),
            "setTime" => {
                let t = match args.first() {
                    Some(v) => self.coerce_to_f64(*v)?,
                    None => f64::NAN,
                };
                let t = if t.is_finite() && t.abs() <= 8.64e15 { t.trunc() } else { f64::NAN };
                self.store_date_ms(oid, t);
                Value::number(t)
            }
            "setMilliseconds" | "setUTCMilliseconds"
            | "setSeconds" | "setUTCSeconds"
            | "setMinutes" | "setUTCMinutes"
            | "setHours" | "setUTCHours"
            | "setDate" | "setUTCDate"
            | "setMonth" | "setUTCMonth"
            | "setFullYear" | "setUTCFullYear"
            | "setYear" => {
                let (y0, mon0, d0) = epoch_to_ymd(ms);
                // Current components; a NaN receiver poisons every slot except
                // the ones being written (setFullYear on an invalid date is
                // still defined for the fields it sets — the rest stay NaN).
                let cur = if ms.is_nan() {
                    // setFullYear treats an invalid date as +0 time-of-year
                    // (month 0, day 1, midnight); the others stay poisoned.
                    if matches!(name, "setFullYear" | "setUTCFullYear" | "setYear") {
                        [f64::NAN, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0]
                    } else {
                        [f64::NAN; 7]
                    }
                } else {
                    [y0 as f64, mon0 as f64, d0 as f64, hour, min, sec, milli]
                };
                // Which component slot the first argument targets, and how
                // many trailing slots the argument list may fill.
                let first_slot = match name {
                    "setFullYear" | "setUTCFullYear" | "setYear" => 0,
                    "setMonth" | "setUTCMonth" => 1,
                    "setDate" | "setUTCDate" => 2,
                    "setHours" | "setUTCHours" => 3,
                    "setMinutes" | "setUTCMinutes" => 4,
                    "setSeconds" | "setUTCSeconds" => 5,
                    _ => 6, // setMilliseconds
                };
                // setMonth's optional 2nd arg is the day; setHours' extras run
                // through ms — i.e. args fill consecutive slots from first_slot,
                // but date-part setters stop at the day slot.
                let max_slots = if name == "setYear" {
                    1
                } else if first_slot <= 2 {
                    3 - first_slot
                } else {
                    7 - first_slot
                };
                let mut comps = cur;
                for (i, a) in args.iter().take(max_slots).enumerate() {
                    let mut v = self.coerce_to_f64(*a)?;
                    if name == "setYear" && i == 0 && (0.0..=99.0).contains(&v.trunc()) {
                        v = v.trunc() + 1900.0;
                    }
                    comps[first_slot + i] = v;
                }
                let new_ms =
                    ymd_hms_to_ms(comps[0], comps[1], comps[2], comps[3], comps[4], comps[5], comps[6]);
                self.store_date_ms(oid, new_ms);
                Value::number(new_ms)
            }
            "toISOString" => {
                if !ms.is_finite() {
                    return Err(VmError::Throw(
                        self.make_native_error("RangeError", "Invalid time value"),
                    ));
                }
                let s = format_iso(ms);
                self.new_str(&s)
            }
            "toJSON" => {
                if !ms.is_finite() {
                    Value::null()
                } else {
                    let s = format_iso(ms);
                    self.new_str(&s)
                }
            }
            "toString" | "toLocaleString" => {
                let s = super::vm::format_date_tostring(ms);
                self.new_str(&s)
            }
            "toDateString" | "toLocaleDateString" => {
                if ms.is_nan() {
                    self.new_str("Invalid Date")
                } else {
                    let (y, m0, d) = epoch_to_ymd(ms);
                    let s = format!(
                        "{} {} {:02} {:04}",
                        DAY_NAMES[epoch_weekday(ms) as usize], MONTH_NAMES[m0 as usize], d, y
                    );
                    self.new_str(&s)
                }
            }
            "toTimeString" | "toLocaleTimeString" => {
                if ms.is_nan() {
                    self.new_str("Invalid Date")
                } else {
                    let s = format!(
                        "{:02}:{:02}:{:02} GMT+0000 (Coordinated Universal Time)",
                        hour as i32, min as i32, sec as i32
                    );
                    self.new_str(&s)
                }
            }
            "toUTCString" => {
                if ms.is_nan() {
                    self.new_str("Invalid Date")
                } else {
                    let (y, m0, d) = epoch_to_ymd(ms);
                    let s = format!(
                        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
                        DAY_NAMES[epoch_weekday(ms) as usize], d, MONTH_NAMES[m0 as usize], y,
                        hour as i32, min as i32, sec as i32
                    );
                    self.new_str(&s)
                }
            }
            _ => Value::undefined(),
        };
        Ok(result)
    }

    fn store_date_ms(&mut self, oid: crate::runtime::object::ObjectId, ms: f64) {
        if let Some(o) = self.heap.get_mut(oid) {
            o.kind = ObjectKind::Date(ms);
        }
    }

    // ---- Array method dispatch ----
    pub(crate) fn exec_array_method(&mut self, oid: crate::runtime::object::ObjectId, method_name: StringId, args: &[Value]) -> Result<Value, VmError> {
        let name = self.interner.resolve(method_name).to_owned();
        // Generic receivers (array-likes, arrays with reconfigured index
        // properties) take the spec-shaped path; dense arrays stay fast.
        let has_holes = self.heap.get(oid).is_some_and(|o| {
            if let ObjectKind::Array(ref e) = o.kind {
                e.iter().any(|v| v.is_empty_marker())
            } else {
                false
            }
        });
        let needs_generic = has_holes
            || match self.heap.get(oid).map(|o| (matches!(o.kind, ObjectKind::Array(_)), o.properties.is_empty())) {
                Some((true, true)) => false,
                Some((true, false)) => self.array_has_index_props(oid),
                _ => true,
            };
        if needs_generic
            && let Some(v) = self.exec_array_method_generic(oid, &name, args)?
        {
            return Ok(v);
        }
        // sort's comparator must be undefined or callable.
        if matches!(name.as_str(), "sort" | "toSorted")
            && let Some(c) = args.first()
            && !c.is_undefined()
            && !self.value_callable(*c)
        {
            return Err(VmError::Throw(self.make_native_error(
                "TypeError",
                "The comparison function must be either a function or undefined",
            )));
        }
        // The callback must be callable even on an empty receiver (the dense
        // loops below would never touch it).
        if matches!(
            name.as_str(),
            "forEach" | "map" | "filter" | "every" | "some" | "find" | "findIndex"
                | "findLast" | "findLastIndex" | "reduce" | "reduceRight" | "flatMap"
        ) {
            let cb = args.first().copied().unwrap_or(Value::undefined());
            let callable = cb.is_function()
                || cb.as_object_id()
                    .and_then(|o| self.heap.get(o))
                    .is_some_and(|o| matches!(o.kind, ObjectKind::Function(_)));
            if !callable {
                return Err(VmError::Throw(self.make_native_error(
                    "TypeError",
                    "callback is not a function",
                )));
            }
        }
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
                        let v = elements.pop().unwrap_or(Value::undefined());
                        return Ok(if v.is_empty_marker() { Value::undefined() } else { v });
                    }
                Ok(Value::undefined())
            }
            "join" => {
                let sep = args.first()
                    .filter(|v| !v.is_undefined())
                    .map(|v| self.value_to_string(*v))
                    .unwrap_or_else(|| ",".into());
                let elements: Option<Vec<Value>> = self.heap.get(oid).and_then(|o| {
                    if let ObjectKind::Array(ref e) = o.kind { Some(e.clone()) } else { None }
                });
                if let Some(elements) = elements {
                        // Per spec (Array.prototype.join step 7c): undefined,
                        // null, and holes stringify to the empty string — NOT
                        // "undefined"/"null". `Array(n).join(x)` relies on this
                        // to produce a run of separators (a common zero-pad
                        // idiom); rendering holes as "undefined" corrupted it.
                        let mut parts: Vec<String> = Vec::with_capacity(elements.len());
                        for v in &elements {
                            if v.is_undefined() || v.is_null() || v.is_empty_marker() {
                                parts.push(String::new());
                            } else if v.is_object() && !v.is_symbol() {
                                // ToString runs ToPrimitive observably.
                                let p = self.try_coerce_to_primitive_hint(*v, "string")?;
                                parts.push(self.value_to_string(p));
                            } else {
                                parts.push(self.value_to_string(*v));
                            }
                        }
                        let result = parts.join(&sep);
                        let id = self.interner.intern(&result);
                        return Ok(Value::string(id));
                    }
                Ok(Value::undefined())
            }
            "indexOf" => {
                let search = args.first().copied().unwrap_or(Value::undefined());
                // Length is checked before ToInteger(fromIndex) per spec.
                let empty = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.is_empty() } else { true })
                    .unwrap_or(true);
                if empty {
                    return Ok(Value::int(-1));
                }
                let from_idx = match args.get(1) {
                    Some(v) => self.coerce_to_f64(*v)?,
                    None => 0.0,
                };
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
                            let v = elements.remove(0);
                            return Ok(if v.is_empty_marker() { Value::undefined() } else { v });
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
                let mut arr = JsObject::array(results);
                arr.prototype = Some(self.array_prototype);
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
                let mut arr = JsObject::array(results);
                arr.prototype = Some(self.array_prototype);
                let new_oid = self.heap.allocate(arr);
                Ok(Value::object_id(new_oid))
            }
            "reduce" => {
                let callback = args.first().copied().unwrap_or(Value::undefined());
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                if args.len() < 2 && elements.is_empty() {
                    return Err(VmError::Throw(self.make_native_error(
                        "TypeError",
                        "Reduce of empty array with no initial value",
                    )));
                }
                let mut acc = if args.len() > 1 { args[1] } else { elements[0] };
                let start = if args.len() > 1 { 0 } else { 1 };
                for (i, elem) in elements.iter().enumerate().skip(start) {
                    acc = self.call_function(callback, &[acc, *elem, Value::int(i as i32), Value::object_id(oid)])?;
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
                    return Err(VmError::Throw(self.make_native_error(
                        "TypeError",
                        "Reduce of empty array with no initial value",
                    )));
                }
                let mut acc = if args.len() > 1 { args[1] } else { *elements.last().unwrap() };
                let end = if args.len() > 1 { elements.len() } else { elements.len() - 1 };
                for i in (0..end).rev() {
                    acc = self.call_function(callback, &[acc, elements[i], Value::int(i as i32), Value::object_id(oid)])?;
                }
                Ok(acc)
            }
            "splice" => {
                let len = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.len() } else { 0 })
                    .unwrap_or(0);
                let raw_start = match args.first() {
                    Some(v) => self.coerce_to_f64(*v)? as i32,
                    None => 0,
                };
                let start = if raw_start < 0 { (len as i32 + raw_start).max(0) as usize } else { (raw_start as usize).min(len) };
                let delete_count = if args.len() >= 2 {
                    (self.coerce_to_f64(args[1])? as i32).max(0) as usize
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

                let mut arr = JsObject::array(deleted);
                arr.prototype = Some(self.array_prototype);
                let new_oid = self.heap.allocate(arr);
                Ok(Value::object_id(new_oid))
            }
            "slice" => {
                let elements: Vec<Value> = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                    .unwrap_or_default();
                let len = elements.len() as i32;
                let raw_start = match args.first() {
                    Some(v) => self.coerce_to_f64(*v)? as i32,
                    None => 0,
                };
                let raw_end = match args.get(1).filter(|v| !v.is_undefined()) {
                    Some(v) => self.coerce_to_f64(*v)? as i32,
                    None => len,
                };
                let start = if raw_start < 0 { (len + raw_start).max(0) as usize } else { raw_start.min(len) as usize };
                let end = if raw_end < 0 { (len + raw_end).max(0) as usize } else { raw_end.min(len) as usize };
                let sliced = if start < end { elements[start..end].to_vec() } else { vec![] };
                let mut arr = JsObject::array(sliced);
                arr.prototype = Some(self.array_prototype);
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
                let mut arr = JsObject::array(result);
                arr.prototype = Some(self.array_prototype);
                let new_oid = self.heap.allocate(arr);
                Ok(Value::object_id(new_oid))
            }
            "fill" => {
                let fill_val = args.first().copied().unwrap_or(Value::undefined());
                let len = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.len() } else { 0 })
                    .unwrap_or(0) as i32;
                let raw_start = match args.get(1) {
                    Some(v) => self.coerce_to_f64(*v)? as i32,
                    None => 0,
                };
                let raw_end = match args.get(2).filter(|v| !v.is_undefined()) {
                    Some(v) => self.coerce_to_f64(*v)? as i32,
                    None => len,
                };
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
                let raw_target = match args.first() {
                    Some(v) => self.coerce_to_f64(*v)? as i32,
                    None => 0,
                };
                let raw_start = match args.get(1) {
                    Some(v) => self.coerce_to_f64(*v)? as i32,
                    None => 0,
                };
                let raw_end = match args.get(2).filter(|v| !v.is_undefined()) {
                    Some(v) => self.coerce_to_f64(*v)? as i32,
                    None => len,
                };
                let target = if raw_target < 0 { (len + raw_target).max(0) as usize } else { raw_target.min(len) as usize };
                let start = if raw_start < 0 { (len + raw_start).max(0) as usize } else { raw_start.min(len) as usize };
                let end = if raw_end < 0 { (len + raw_end).max(0) as usize } else { raw_end.min(len) as usize };
                let end = end.max(start);
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
                let mut arr = JsObject::array(result);
                arr.prototype = Some(self.array_prototype);
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
                let mut arr = JsObject::array(mapped);
                arr.prototype = Some(self.array_prototype);
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
                let len = self.heap.get(oid)
                    .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.len() as i64 } else { 0 })
                    .unwrap_or(0);
                // Length is checked before ToInteger(fromIndex) per spec.
                if len == 0 {
                    return Ok(Value::int(-1));
                }
                let from = match args.get(1) {
                    Some(v) => self.coerce_to_f64(*v)?,
                    None => (len - 1) as f64,
                };
                let start = if from.is_nan() {
                    -1
                } else if from < 0.0 {
                    len + from as i64
                } else {
                    (from as i64).min(len - 1)
                };
                if start >= 0 && let Some(obj) = self.heap.get(oid)
                    && let ObjectKind::Array(ref elements) = obj.kind {
                        let elements = elements.clone();
                        for i in (0..=(start as usize).min(elements.len().saturating_sub(1))).rev() {
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
                let mut arr = JsObject::array(elements);
                arr.prototype = Some(self.array_prototype);
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
                let mut arr = JsObject::array(elements);
                arr.prototype = Some(self.array_prototype);
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
                let mut arr = JsObject::array(elements);
                arr.prototype = Some(self.array_prototype);
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
                let mut arr = JsObject::array(elements);
                arr.prototype = Some(self.array_prototype);
                Ok(Value::object_id(self.heap.allocate(arr)))
            }
            "toString" => {
                // Array.prototype.toString is equivalent to join(",")
                if let Some(obj) = self.heap.get(oid)
                    && let ObjectKind::Array(ref elements) = obj.kind {
                        let parts: Vec<String> = elements.iter().map(|v| {
                            if v.is_undefined() || v.is_null() || v.is_empty_marker() {
                                String::new()
                            } else {
                                self.value_to_string(*v)
                            }
                        }).collect();
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
                let key = match args.first() {
                    Some(v) if v.is_symbol() => format!("__sym_{}__", v.as_symbol_id().unwrap()),
                    Some(v) => self.value_to_string(*v),
                    None => String::new(),
                };
                let key_id = self.interner.intern(&key);
                let getter_key = self.interner.intern(&format!("__get_{key}__"));
                let setter_key = self.interner.intern(&format!("__set_{key}__"));
                self.ensure_builtin_proto_method(oid, key_id);
                let has = self.heap.get(oid).map(|o| {
                    let idx_own = key.parse::<usize>().ok().is_some_and(|idx| match &o.kind {
                        ObjectKind::Array(elems) => idx < elems.len() && !elems[idx].is_empty_marker(),
                        ObjectKind::Wrapper(inner) if inner.is_string() => {
                            inner.as_string_id()
                                .map(|sid| idx < self.interner.resolve(sid).chars().count())
                                .unwrap_or(false)
                        }
                        _ => false,
                    });
                    let len_own = key == "length"
                        && matches!(&o.kind, ObjectKind::Array(_) | ObjectKind::Wrapper(_))
                        && !matches!(&o.kind, ObjectKind::Wrapper(inner) if !inner.is_string());
                    idx_own
                        || len_own
                        || o.has_own_property(key_id)
                        || o.has_own_property(getter_key)
                        || o.has_own_property(setter_key)
                }).unwrap_or(false);
                Ok(Value::boolean(has))
            }
            "propertyIsEnumerable" => {
                let key = match args.first() {
                    Some(v) if v.is_symbol() => format!("__sym_{}__", v.as_symbol_id().unwrap()),
                    Some(v) => self.value_to_string(*v),
                    None => String::new(),
                };
                let key_id = self.interner.intern(&key);
                let getter_key = self.interner.intern(&format!("__get_{key}__"));
                let setter_key = self.interner.intern(&format!("__set_{key}__"));
                let is_enum = self.heap.get(oid).map(|o| {
                    let idx_enum = key.parse::<usize>().ok().is_some_and(|idx| match &o.kind {
                        ObjectKind::Array(elems) => idx < elems.len() && !elems[idx].is_empty_marker(),
                        ObjectKind::Wrapper(inner) if inner.is_string() => {
                            inner.as_string_id()
                                .map(|sid| idx < self.interner.resolve(sid).chars().count())
                                .unwrap_or(false)
                        }
                        _ => false,
                    });
                    idx_enum
                        || o.get_property_descriptor(key_id)
                            .or_else(|| o.get_property_descriptor(getter_key))
                            .or_else(|| o.get_property_descriptor(setter_key))
                            .map(|p| p.is_enumerable())
                            .unwrap_or(false)
                }).unwrap_or(false);
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
            -727 => a0.ln_1p(),
            -728 => a0.exp_m1(),
            -729 => a0.sinh(),
            -730 => a0.cosh(),
            -731 => a0.tanh(),
            -732 => a0.asinh(),
            -733 => a0.acosh(),
            -734 => a0.atanh(),
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
                let radix_arg = args.get(1)
                    .map(|v| self.to_f64(*v))
                    .filter(|n| !n.is_nan())
                    .map(|n| n as i64)
                    .unwrap_or(0);
                let s = s.trim_start();
                let (s, neg) = if let Some(stripped) = s.strip_prefix('-') { (stripped, true) }
                    else if let Some(stripped) = s.strip_prefix('+') { (stripped, false) }
                    else { (s, false) };
                // Radix 0/undefined: auto-detect 16 via 0x prefix, else 10.
                let (s, radix) = if radix_arg == 0 || radix_arg == 16 {
                    if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                        (rest, 16u32)
                    } else {
                        (s, if radix_arg == 0 { 10 } else { 16 })
                    }
                } else {
                    if !(2..=36).contains(&radix_arg) {
                        return Value::number(f64::NAN);
                    }
                    (s, radix_arg as u32)
                };
                // Longest valid-digit prefix, accumulated in f64 (parseInt
                // handles arbitrarily long digit strings).
                let mut result = 0f64;
                let mut found = false;
                for c in s.chars() {
                    match c.to_digit(radix) {
                        Some(d) => {
                            result = result * radix as f64 + d as f64;
                            found = true;
                        }
                        None => break,
                    }
                }
                if !found { return Value::number(f64::NAN); }
                Value::number(if neg { -result } else { result })
            }
            -501 => { // parseFloat
                let s = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                let t = s.trim_start();
                // Longest prefix matching StrDecimalLiteral:
                // [+-]? (Infinity | digits[.digits?][eE[+-]?digits] | .digits[eE...])
                let b: Vec<char> = t.chars().collect();
                let mut i = 0usize;
                if i < b.len() && (b[i] == '+' || b[i] == '-') { i += 1; }
                let after_sign = i;
                if t[after_sign..].starts_with("Infinity") {
                    let v = if b.first() == Some(&'-') { f64::NEG_INFINITY } else { f64::INFINITY };
                    return Value::number(v);
                }
                let mut saw_digit = false;
                while i < b.len() && b[i].is_ascii_digit() { i += 1; saw_digit = true; }
                if i < b.len() && b[i] == '.' {
                    i += 1;
                    while i < b.len() && b[i].is_ascii_digit() { i += 1; saw_digit = true; }
                }
                if !saw_digit { return Value::number(f64::NAN); }
                let mantissa_end = i;
                let mut exp_end = i;
                if i < b.len() && (b[i] == 'e' || b[i] == 'E') {
                    let mut j = i + 1;
                    if j < b.len() && (b[j] == '+' || b[j] == '-') { j += 1; }
                    let ds = j;
                    while j < b.len() && b[j].is_ascii_digit() { j += 1; }
                    if j > ds { exp_end = j; }
                }
                let end = exp_end.max(mantissa_end);
                let prefix: String = b[..end].iter().collect();
                Value::number(prefix.parse::<f64>().unwrap_or(f64::NAN))
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
                        // Full ToString: objects go through ToPrimitive with
                        // the string hint (toString → valueOf fallback).
                        let prim = if v.is_object() && !v.is_symbol() {
                            self.try_coerce_to_primitive_hint(v, "string").unwrap_or(v)
                        } else {
                            v
                        };
                        let s = self.value_to_string(prim);
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
                    let t = s.trim();
                    let parsed = if let Some(rest) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
                        i64::from_str_radix(rest, 2).map(|i| i as f64).unwrap_or(f64::NAN)
                    } else if let Some(rest) = t.strip_prefix("0o").or_else(|| t.strip_prefix("0O")) {
                        i64::from_str_radix(rest, 8).map(|i| i as f64).unwrap_or(f64::NAN)
                    } else if let Some(rest) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                        i64::from_str_radix(rest, 16).map(|i| i as f64).unwrap_or(f64::NAN)
                    } else if t.is_empty() {
                        0.0
                    } else if t == "Infinity" || t == "+Infinity" {
                        f64::INFINITY
                    } else if t == "-Infinity" {
                        f64::NEG_INFINITY
                    } else {
                        t.parse::<f64>().unwrap_or(f64::NAN)
                    };
                    Value::number(parsed)
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
            // AggregateError called without `new`: (errors, message)
            -539 => {
                match self.simple_iterable_to_list(args.first().copied().unwrap_or(Value::undefined())) {
                    Ok(errors) => {
                        let msg = args.get(1).copied().unwrap_or(Value::undefined());
                        self.make_aggregate_error(errors, msg)
                    }
                    Err(VmError::Throw(err)) => {
                        let _ = self.handle_throw(err);
                        Value::undefined()
                    }
                    Err(_) => Value::undefined(),
                }
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

    /// Number.prototype methods (toString/valueOf/toFixed/toPrecision/
    /// toExponential), shared by receiver dispatch and reified method values.
    pub(crate) fn exec_number_method(&mut self, effective_val: Value, method_name: StringId, args: &[Value]) -> Result<Value, VmError> {
        let mn = self.interner.resolve(method_name).to_owned();
        let n = self.to_f64(effective_val);
        let args: Vec<Value> = args.to_vec();
        let result = match mn.as_str() {
                            "toString" => {
                                let radix = match args.first().filter(|v| !v.is_undefined()) {
                                    Some(v) => {
                                        let r = self.to_f64(*v).trunc();
                                        if !(2.0..=36.0).contains(&r) {
                                            return Err(VmError::Throw(self.make_native_error(
                                                "RangeError",
                                                "toString() radix must be between 2 and 36",
                                            )));
                                        }
                                        r as u32
                                    }
                                    None => 10,
                                };
                                let s = if radix == 10 || !n.is_finite() {
                                    self.value_to_string(effective_val)
                                } else {
                                    crate::vm::vm::f64_to_radix(n, radix)
                                };
                                let id = self.interner.intern(&s);
                                Value::string(id)
                            }
                            "valueOf" => effective_val,
                            "toFixed" => {
                                let d = args.first()
                                    .filter(|v| !v.is_undefined())
                                    .map(|v| self.to_f64(*v).trunc())
                                    .unwrap_or(0.0);
                                if !(0.0..=100.0).contains(&d) {
                                    return Err(VmError::Throw(self.make_native_error(
                                        "RangeError",
                                        "toFixed() digits argument must be between 0 and 100",
                                    )));
                                }
                                let s = if n.is_nan() {
                                    "NaN".to_string()
                                } else if n.is_infinite() {
                                    (if n > 0.0 { "Infinity" } else { "-Infinity" }).to_string()
                                } else if n.abs() >= 1e21 {
                                    self.value_to_string(effective_val)
                                } else {
                                    format!("{:.prec$}", n, prec = d as usize)
                                };
                                let id = self.interner.intern(&s);
                                Value::string(id)
                            }
                            "toPrecision" => {
                                // Spec: NaN/Infinity receivers stringify BEFORE
                                // the precision range check.
                                if args.first().is_some_and(|a| !a.is_undefined()) && !n.is_finite() {
                                    let s = if n.is_nan() {
                                        "NaN".to_string()
                                    } else if n > 0.0 {
                                        "Infinity".to_string()
                                    } else {
                                        "-Infinity".to_string()
                                    };
                                    let id = self.interner.intern(&s);
                                    return Ok(Value::string(id));
                                }
                                // ToIntegerOrInfinity(precision) must land in
                                // [1, 100] — NaN and non-numbers coerce to 0
                                // and throw RangeError.
                                if let Some(arg) = args.first()
                                    && !arg.is_undefined()
                                {
                                    let p = self.to_f64(*arg);
                                    if !(1.0..=100.0).contains(&p.trunc()) {
                                        let err = self.make_native_error(
                                            "RangeError",
                                            "toPrecision() argument must be between 1 and 100",
                                        );
                                        return Err(VmError::Throw(err));
                                    }
                                }
                                let s = if let Some(p) = args.first().and_then(|v| v.as_number()) {
                                    let p = p as usize;
                                    if n == 0.0 {
                                        format!("{:.prec$}", 0.0, prec = p.saturating_sub(1))
                                    } else {
                                        let mag = n.abs().log10().floor() as i32;
                                        if mag >= -6 && mag < p as i32 {
                                            let decimals = (p as i32 - 1 - mag).max(0) as usize;
                                            format!("{:.prec$}", n, prec = decimals)
                                        } else {
                                            let mantissa = n / 10f64.powi(mag);
                                            let decimals = p.saturating_sub(1);
                                            let sign = if mag >= 0 { "+" } else { "-" };
                                            format!("{:.prec$}e{}{}", mantissa, sign, mag.abs(), prec = decimals)
                                        }
                                    }
                                } else {
                                    self.value_to_string(effective_val)
                                };
                                let id = self.interner.intern(&s);
                                Value::string(id)
                            }
                            "toExponential" => {
                                // Spec order: f = ToIntegerOrInfinity(digits);
                                // NaN receiver returns "NaN" before the range
                                // check; Infinity likewise.
                                if n.is_nan() {
                                    let id = self.interner.intern("NaN");
                                    return Ok(Value::string(id));
                                }
                                if n.is_infinite() {
                                    let id = self.interner.intern(if n > 0.0 { "Infinity" } else { "-Infinity" });
                                    return Ok(Value::string(id));
                                }
                                if let Some(v) = args.first().filter(|v| !v.is_undefined()) {
                                    let d = self.to_f64(*v).trunc();
                                    if !(0.0..=100.0).contains(&d) {
                                        return Err(VmError::Throw(self.make_native_error(
                                            "RangeError",
                                            "toExponential() argument must be between 0 and 100",
                                        )));
                                    }
                                }
                                let digits = args.first()
                                    .filter(|v| !v.is_undefined())
                                    .and_then(|v| v.as_number())
                                    .map(|d| (d.trunc().clamp(0.0, 100.0)) as usize);
                                let s = if n == 0.0 {
                                    let decimals = digits.unwrap_or(0);
                                    if decimals == 0 { "0e+0".to_string() }
                                    else { format!("{:.prec$}e+0", 0.0, prec = decimals) }
                                } else {
                                    let mag = n.abs().log10().floor() as i32;
                                    let mantissa = n / 10f64.powi(mag);
                                    let sign = if mag >= 0 { "+" } else { "-" };
                                    match digits {
                                        Some(d) => format!("{:.prec$}e{}{}", mantissa, sign, mag.abs(), prec = d),
                                        None => {
                                            // Minimum digits: default formatting, trim trailing zeros
                                            let mut m = format!("{mantissa}");
                                            if m.contains('.') {
                                                m = m.trim_end_matches('0').trim_end_matches('.').to_string();
                                            }
                                            format!("{}e{}{}", m, sign, mag.abs())
                                        }
                                    }
                                };
                                let id = self.interner.intern(&s);
                                Value::string(id)
                            }
                            _ => Value::undefined(),
                        };
        Ok(result)
    }

    /// Execute a native method sentinel that requires `this` context.
    /// Sentinels -590 to -599: Object.prototype / Function.prototype methods.
    /// Sentinels -600 to -629: Array.prototype methods.
    pub(crate) fn exec_native_method(&mut self, sentinel: i32, this_val: Value, args: &[Value]) -> Result<Value, VmError> {
        let result = match sentinel {
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
                // Per spec, the tag comes from the receiver's type — the classic
                // `Object.prototype.toString.call(x)` type-detection idiom relies
                // on this (Handlebars, jQuery, lodash, …). Previously every
                // primitive returned "[object Object]".
                let tag = if this_val.is_undefined() {
                    "Undefined"
                } else if this_val.is_null() {
                    "Null"
                } else if this_val.is_boolean() {
                    "Boolean"
                } else if this_val.is_number() || this_val.is_int() {
                    "Number"
                } else if this_val.is_string() {
                    "String"
                } else if this_val.is_symbol() {
                    "Symbol"
                } else if this_val.is_function() {
                    "Function"
                } else if let Some(oid) = this_val.as_object_id() {
                    match self.heap.get(oid).map(|o| &o.kind) {
                        Some(ObjectKind::Array(_)) => "Array",
                        Some(ObjectKind::Function(_)) => "Function",
                        Some(ObjectKind::RegExp { .. }) => "RegExp",
                        Some(ObjectKind::Promise { .. }) => "Promise",
                        Some(ObjectKind::Map { .. }) => "Map",
                        Some(ObjectKind::Set { .. }) => "Set",
                        Some(ObjectKind::WeakMap { .. }) => "WeakMap",
                        Some(ObjectKind::WeakSet { .. }) => "WeakSet",
                        Some(ObjectKind::Date(_)) => "Date",
                        Some(ObjectKind::ConsString { .. } | ObjectKind::FlatString { .. }) => "String",
                        Some(ObjectKind::BigInt(_)) => "BigInt",
                        Some(ObjectKind::Wrapper(inner)) => {
                            // Boxed primitive (new String/Number/Boolean).
                            let inner = *inner;
                            if inner.is_boolean() { "Boolean" }
                            else if inner.is_number() || inner.is_int() { "Number" }
                            else if inner.is_string() { "String" }
                            else { "Object" }
                        }
                        _ => "Object",
                    }
                } else {
                    "Object"
                };
                // Get(O, @@toStringTag): a string-valued tag overrides the
                // builtin one (Math, JSON, user objects).
                let tag_override = this_val.as_object_id().and_then(|oid| {
                    let tag_key = self.interner.intern(&format!("__sym_{}__", self.sym_to_string_tag));
                    self.heap.get_property_chain(oid, tag_key)
                        .filter(|v| v.is_string())
                        .map(|v| self.value_to_string(v))
                });
                let s = match tag_override {
                    Some(t) => self.interner.intern(&format!("[object {t}]")),
                    None => self.interner.intern(&format!("[object {tag}]")),
                };
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
                let Some(inner) = inner else { return Ok(Value::undefined()); };
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
                let Some(inner) = inner else {
                    return Err(VmError::Throw(self.make_native_error(
                        "TypeError",
                        "Number.prototype method called on incompatible receiver",
                    )));
                };
                if sentinel == -632 {
                    // Full toString semantics (radix argument included).
                    let ts = self.interner.intern("toString");
                    return self.exec_number_method(inner, ts, args);
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
                    _ => return Ok(Value::undefined()),
                };
                // ToObject(this): null/undefined receivers throw.
                if this_val.is_nullish() {
                    return Err(VmError::Throw(self.make_native_error(
                        "TypeError",
                        "Array.prototype method called on null or undefined",
                    )));
                }
                let method_id = self.interner.intern(method_name);
                let recv = if let Some(oid) = this_val.as_object_id() {
                    Some(oid)
                } else {
                    self.box_primitive(this_val).as_object_id()
                };
                match recv {
                    Some(oid) => return self.exec_array_method(oid, method_id, args),
                    None => Value::undefined(),
                }
            }
            _ => Value::undefined(),
        };
        Ok(result)
    }

    /// Check if a value is a String wrapper object.
    /// Unwrap a primitive-wrapper receiver to its inner primitive when it
    /// matches `want`. Handles both the `Wrapper` kind (`new Number(1)`) and the
    /// legacy `__primitive__`-property form (`Object(1)`); a bare matching
    /// primitive is returned as-is.
    pub(crate) fn unwrap_wrapper_primitive(&mut self, this_val: Value, want: fn(Value) -> bool) -> Option<Value> {
        if want(this_val) { return Some(this_val); }
        // The primordial prototypes carry primitive data slots per spec:
        // Number.prototype +0, Boolean.prototype false, String.prototype "".
        if let Some(oid) = this_val.as_object_id() {
            if oid == self.number_prototype && want(Value::number(0.0)) {
                return Some(Value::number(0.0));
            }
            if oid == self.boolean_prototype && want(Value::boolean(false)) {
                return Some(Value::boolean(false));
            }
            if oid == self.string_prototype {
                let empty = self.new_str("");
                if want(empty) {
                    return Some(empty);
                }
            }
        }
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
            // Cons/flat strings are string PRIMITIVES that happen to live on
            // the heap — they coerce to themselves, never via toString.
            if self.heap.get(oid).is_some_and(|o| matches!(
                o.kind,
                ObjectKind::ConsString { .. } | ObjectKind::FlatString { .. }
            )) {
                return Ok(val);
            }
            // Wrapper objects: only shortcut when no OWN toString/valueOf
            // override exists — overrides must run observably.
            if let Some(obj) = self.heap.get(oid)
                && let ObjectKind::Wrapper(inner) = &obj.kind {
                    let inner = *inner;
                    let ts = self.interner.intern("toString");
                    let vo = self.interner.intern("valueOf");
                    let has_override = self.heap.get(oid).is_some_and(|o| {
                        o.has_own_property(ts) || o.has_own_property(vo)
                    });
                    if !has_override {
                        return Ok(inner);
                    }
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
            let callable = |vm: &Self, v: Value| {
                v.is_function()
                    || v.as_object_id()
                        .and_then(|o| vm.heap.get(o))
                        .is_some_and(|o| matches!(o.kind, ObjectKind::Function(_)))
            };
            let first_key = self.interner.intern(try_first);
            self.ensure_chain_method(oid, first_key);
            if let Some(fn1) = self.heap.get_property_chain(oid, first_key)
                && callable(self, fn1)
            {
                tried_method = true;
                let result = self.call_function_this(fn1, val, &[])?;
                if !result.is_object() || self.is_bigint(result) { return Ok(result); }
            }
            let second_key = self.interner.intern(try_second);
            self.ensure_chain_method(oid, second_key);
            if let Some(fn2) = self.heap.get_property_chain(oid, second_key)
                && callable(self, fn2)
            {
                tried_method = true;
                let result = self.call_function_this(fn2, val, &[])?;
                if !result.is_object() || self.is_bigint(result) { return Ok(result); }
            }
            // Neither method produced a primitive (returned objects, were
            // shadowed with non-callables, or don't exist) — per spec,
            // OrdinaryToPrimitive throws TypeError.
            let _ = tried_method;
            let (line, pc, chunk_name) = if let Some(f) = self.frames.last() {
                let cn = self.interner.resolve(self.chunks[f.chunk_idx].name).to_owned();
                (self.chunks[f.chunk_idx].get_line(f.ip as u32), f.ip, cn)
            } else { (0, 0, String::new()) };
            let msg = format!("Cannot convert object to primitive value (at line {line}, pc {pc}, chunk '{chunk_name}')");
            let err = self.make_native_error("TypeError", &msg);
            return Err(super::vm::VmError::Throw(err));
        }
        Ok(val)
    }
}

/// Percent-decode a string into UTF-8 (backs decodeURIComponent / decodeURI).
/// `%XX` byte escapes are collected and interpreted as UTF-8; a malformed or
/// truncated escape is left verbatim (lenient — see call site). Non-escape
/// characters pass through unchanged.
/// URIError validation for decodeURI[Component]: every '%' must introduce
/// two hex digits and the decoded byte sequence must be well-formed UTF-8
/// (surrogate code points are invalid).
pub(crate) fn uri_escapes_valid(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut decoded: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return false;
            }
            let h1 = (bytes[i + 1] as char).to_digit(16);
            let h2 = (bytes[i + 2] as char).to_digit(16);
            match (h1, h2) {
                (Some(a), Some(b)) => decoded.push(((a << 4) | b) as u8),
                _ => return false,
            }
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    std::str::from_utf8(&decoded).is_ok()
}

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
