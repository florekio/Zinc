use crate::runtime::object::{JsObject, ObjectHeap, ObjectKind};
use crate::runtime::value::Value;
use crate::util::interner::{Interner, StringId};

use super::vm::{Vm, VmError};

impl Vm {
    // ---- JSON method dispatch ----
    pub(crate) fn exec_json_method(&mut self, method_name: StringId, args: &[Value]) -> Result<Value, VmError> {
        let name = self.interner.resolve(method_name).to_owned();
        let result = match name.as_str() {
            "parse" => {
                let s = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                match self.json_parse(&s) {
                    Ok(val) => val,
                    Err(e) => {
                        return Err(VmError::Throw(self.make_native_error(
                            "SyntaxError",
                            &format!("Unexpected token in JSON: {e}"),
                        )));
                    }
                }
            }
            "stringify" => {
                let val = args.first().copied().unwrap_or(Value::undefined());
                let replacer = args.get(1).copied().filter(|v| self.value_callable(*v));
                let indent = args.get(2).map(|v| {
                    if let Some(n) = v.as_number().or_else(|| v.as_int().map(|i| i as f64)) {
                        " ".repeat((n.max(0.0) as usize).min(10))
                    } else if v.is_string() {
                        self.value_to_string(*v).chars().take(10).collect()
                    } else {
                        String::new()
                    }
                }).unwrap_or_default();
                // Top-level holder per spec: { "": value }
                let holder = {
                    let mut h = JsObject::ordinary();
                    h.prototype = Some(self.object_prototype);
                    let ek = self.interner.intern("");
                    h.set_property(ek, val);
                    Value::object_id(self.heap.allocate(h))
                };
                let key = self.new_str("");
                let mut path: Vec<crate::runtime::object::ObjectId> = Vec::new();
                match self.json_serialize_property(val, key, holder, replacer, &indent, 0, &mut path)? {
                    Some(st) => {
                        let id = self.interner.intern(&st);
                        Value::string(id)
                    }
                    None => Value::undefined(),
                }
            }
            _ => Value::undefined(),
        };
        Ok(result)
    }

    // ---- JSON.parse: simple recursive descent ----
    pub(crate) fn json_parse(&mut self, input: &str) -> Result<Value, String> {
        // JSON whitespace is exactly TAB/LF/CR/space; anything else (\v,
        // NBSP, ...) outside a string is a syntax error, as is trailing
        // content after the value.
        let input = json_trim(input);
        let (val, rest) = json_parse_value(input, &mut self.heap, &mut self.interner)?;
        if !json_trim(rest).is_empty() {
            return Err("Unexpected token after JSON value".into());
        }
        Ok(val)
    }

    /// SerializeJSONProperty: toJSON, replacer, wrapper unwrapping,
    /// cycle detection on the live path, observable Gets, indentation.
    #[allow(clippy::too_many_arguments)]
    fn json_serialize_property(
        &mut self,
        mut value: Value,
        key: Value,
        holder: Value,
        replacer: Option<Value>,
        indent: &str,
        depth: usize,
        path: &mut Vec<crate::runtime::object::ObjectId>,
    ) -> Result<Option<String>, VmError> {
        if depth > 200 {
            return Err(VmError::Throw(self.make_native_error(
                "RangeError",
                "Maximum call stack size exceeded",
            )));
        }
        // 1. toJSON (reify lazily-materialized prototype methods first —
        // Date.prototype.toJSON lives behind the reification hook).
        if let Some(oid) = value.as_object_id() {
            let tj_key = self.interner.intern("toJSON");
            self.ensure_chain_method(oid, tj_key);
            let tj = self.getter_aware_get(oid, "toJSON")?;
            if let Some(f) = tj.filter(|f| self.value_callable(*f)) {
                let prev = self.protect_throw_depth;
                self.protect_throw_depth = self.frames.len() + 1;
                let r = self.call_function_this(f, value, &[key]);
                self.protect_throw_depth = prev;
                value = r?;
            }
        }
        // 2. replacer
        if let Some(r) = replacer {
            let prev = self.protect_throw_depth;
            self.protect_throw_depth = self.frames.len() + 1;
            let rv = self.call_function_this(r, holder, &[key, value]);
            self.protect_throw_depth = prev;
            value = rv?;
        }
        // 3. wrappers coerce observably
        if let Some(oid) = value.as_object_id() {
            let inner = self.heap.get(oid).and_then(|o| {
                if let ObjectKind::Wrapper(i) = &o.kind { Some(*i) } else { None }
            });
            if let Some(inner) = inner {
                value = if inner.is_boolean() {
                    inner
                } else if inner.is_number() || inner.is_int() {
                    let pv = self.try_coerce_to_primitive_hint(value, "number")?;
                    Value::number(self.to_f64(pv))
                } else if inner.is_string() {
                    let pv = self.try_coerce_to_primitive_hint(value, "string")?;
                    let st = self.value_to_string(pv);
                    self.new_str(&st)
                } else {
                    inner
                };
            }
        }
        // 4. primitives
        if value.is_null() {
            return Ok(Some("null".into()));
        }
        if value.is_boolean() {
            return Ok(Some(if value.to_boolean() { "true".into() } else { "false".into() }));
        }
        if self.is_bigint(value)
            || value.as_object_id().and_then(|o| self.heap.get(o))
                .is_some_and(|o| matches!(o.kind, ObjectKind::BigInt(_)))
        {
            return Err(VmError::Throw(self.make_native_error(
                "TypeError",
                "Do not know how to serialize a BigInt",
            )));
        }
        if value.is_string() || self.is_string_like(value) {
            let st = self.value_to_string(value);
            return Ok(Some(json_quote(&st)));
        }
        if value.is_int() {
            return Ok(Some(format!("{}", value.as_int().unwrap())));
        }
        if value.is_number() {
            let n = self.to_f64(value);
            if !n.is_finite() {
                return Ok(Some("null".into()));
            }
            if n == 0.0 {
                return Ok(Some("0".into()));
            }
            return Ok(Some(format!("{n}")));
        }
        if value.is_undefined() || value.is_symbol() || value.is_function() {
            return Ok(None);
        }
        let Some(oid) = value.as_object_id() else { return Ok(None) };
        // callable objects serialize like functions: skipped
        if self.heap.get(oid).is_some_and(|o| matches!(o.kind, ObjectKind::Function(_))) {
            return Ok(None);
        }
        if path.contains(&oid) {
            return Err(VmError::Throw(self.make_native_error(
                "TypeError",
                "Converting circular structure to JSON",
            )));
        }
        path.push(oid);
        let nl_open;
        let nl_close;
        let sep;
        if indent.is_empty() {
            nl_open = String::new();
            nl_close = String::new();
            sep = ",".to_string();
        } else {
            nl_open = format!("\n{}", indent.repeat(depth + 1));
            nl_close = format!("\n{}", indent.repeat(depth));
            sep = format!(",\n{}", indent.repeat(depth + 1));
        }
        let is_array = self.heap.get(oid).is_some_and(|o| matches!(o.kind, ObjectKind::Array(_)));
        let result = if is_array {
            let len = self.array_like_len_public(oid)?;
            let mut parts: Vec<String> = Vec::with_capacity(len as usize);
            for i in 0..len {
                let elem = self.array_like_get_public(oid, i)?.unwrap_or(Value::undefined());
                let k = self.new_str(&i.to_string());
                let part = self.json_serialize_property(elem, k, value, replacer, indent, depth + 1, path)?;
                parts.push(part.unwrap_or_else(|| "null".into()));
            }
            if parts.is_empty() {
                "[]".to_string()
            } else {
                format!("[{}{}{}]", nl_open, parts.join(&sep), nl_close)
            }
        } else {
            let keys = self.enumerable_own_string_keys(oid);
            let mut parts: Vec<String> = Vec::new();
            for kstr in keys {
                let v = self.getter_aware_get(oid, &kstr)?.unwrap_or(Value::undefined());
                let kv = self.new_str(&kstr);
                if let Some(part) =
                    self.json_serialize_property(v, kv, value, replacer, indent, depth + 1, path)?
                {
                    let colon = if indent.is_empty() { ":" } else { ": " };
                    parts.push(format!("{}{}{}", json_quote(&kstr), colon, part));
                }
            }
            if parts.is_empty() {
                "{}".to_string()
            } else {
                format!("{{{}{}{}}}", nl_open, parts.join(&sep), nl_close)
            }
        };
        path.pop();
        Ok(Some(result))
    }

}

fn json_parse_value<'s>(s: &'s str, heap: &mut ObjectHeap, interner: &mut Interner) -> Result<(Value, &'s str), String> {
    let s = json_trim_start(s);
    if s.is_empty() { return Err("unexpected end of JSON".into()); }
    match s.as_bytes()[0] {
        b'"' => json_parse_string(s, interner),
        b'{' => json_parse_object(s, heap, interner),
        b'[' => json_parse_array(s, heap, interner),
        b't' if s.starts_with("true") => Ok((Value::boolean(true), &s[4..])),
        b'f' if s.starts_with("false") => Ok((Value::boolean(false), &s[5..])),
        b'n' if s.starts_with("null") => Ok((Value::null(), &s[4..])),
        b'-' | b'0'..=b'9' => json_parse_number(s),
        _ => Err(format!("unexpected char in JSON: {}", s.chars().next().unwrap())),
    }
}

/// JSON whitespace: exactly TAB, LF, CR, space.
fn json_trim(s: &str) -> &str {
    s.trim_matches([' ', '\t', '\n', '\r'].as_slice())
}

fn json_trim_start(s: &str) -> &str {
    s.trim_start_matches([' ', '\t', '\n', '\r'].as_slice())
}

fn json_parse_string<'s>(s: &'s str, interner: &mut Interner) -> Result<(Value, &'s str), String> {
    if !s.starts_with('"') {
        return Err("expected string".into());
    }
    let s = &s[1..];
    let mut result = String::new();
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => { let id = interner.intern(&result); return Ok((Value::string(id), &s[i + 1..])); }
            '\\' => {
                let Some((_, esc)) = chars.next() else { return Err("unterminated escape".into()) };
                match esc {
                    '"' => result.push('"'),
                    '\\' => result.push('\\'),
                    '/' => result.push('/'),
                    'n' => result.push('\n'),
                    'r' => result.push('\r'),
                    't' => result.push('\t'),
                    'b' => result.push('\u{8}'),
                    'f' => result.push('\u{c}'),
                    'u' => {
                        let mut code: u32 = 0;
                        for _ in 0..4 {
                            let Some((_, h)) = chars.next() else { return Err("bad \\u escape".into()) };
                            let d = h.to_digit(16).ok_or_else(|| "bad \\u escape".to_string())?;
                            code = code * 16 + d;
                        }
                        // Surrogate pair: combine when a low surrogate follows.
                        if (0xD800..0xDC00).contains(&code) {
                            let mut la = chars.clone();
                            if let (Some((_, '\\')), Some((_, 'u'))) = (la.next(), la.next()) {
                                let mut low: u32 = 0;
                                let mut ok = true;
                                for _ in 0..4 {
                                    match la.next().and_then(|(_, h)| h.to_digit(16)) {
                                        Some(d) => low = low * 16 + d,
                                        None => { ok = false; break; }
                                    }
                                }
                                if ok && (0xDC00..0xE000).contains(&low) {
                                    chars = la;
                                    let combined = 0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                                    result.push(char::from_u32(combined).unwrap_or('\u{FFFD}'));
                                    continue;
                                }
                            }
                            // Lone surrogate: not representable; substitute.
                            result.push('\u{FFFD}');
                            continue;
                        }
                        result.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                    }
                    _ => return Err(format!("invalid escape \\{esc}")),
                }
            }
            c if (c as u32) < 0x20 => return Err("control character in string".into()),
            _ => result.push(c),
        }
    }
    Err("unterminated string".into())
}

fn json_parse_number(s: &str) -> Result<(Value, &str), String> {
    let mut end = 0;
    let b = s.as_bytes();
    if end < b.len() && b[end] == b'-' { end += 1; }
    // Integer part: 0, or [1-9] digits — leading zeros are a syntax error.
    let int_start = end;
    while end < b.len() && b[end].is_ascii_digit() { end += 1; }
    if end == int_start {
        return Err("invalid number".into());
    }
    if end - int_start > 1 && b[int_start] == b'0' {
        return Err("leading zeros are not allowed".into());
    }
    if end < b.len() && b[end] == b'.' {
        end += 1;
        let frac_start = end;
        while end < b.len() && b[end].is_ascii_digit() { end += 1; }
        if end == frac_start {
            return Err("missing fraction digits".into());
        }
    }
    if end < b.len() && (b[end] == b'e' || b[end] == b'E') {
        end += 1;
        if end < b.len() && (b[end] == b'+' || b[end] == b'-') { end += 1; }
        let exp_start = end;
        while end < b.len() && b[end].is_ascii_digit() { end += 1; }
        if end == exp_start {
            return Err("missing exponent digits".into());
        }
    }
    let n: f64 = s[..end].parse().map_err(|_| "invalid number".to_string())?;
    Ok((Value::number(n), &s[end..]))
}

fn json_parse_object<'s>(s: &'s str, heap: &mut ObjectHeap, interner: &mut Interner) -> Result<(Value, &'s str), String> {
    let mut s = &s[1..];
    let mut obj = JsObject::ordinary();
    s = json_trim_start(s);
    if let Some(rest) = s.strip_prefix('}') { let oid = heap.allocate(obj); return Ok((Value::object_id(oid), rest)); }
    loop {
        s = json_trim_start(s);
        let (key, rest) = json_parse_string(s, interner)?;
        s = json_trim_start(rest);
        if let Some(rest) = s.strip_prefix(':') { s = rest; } else { return Err("expected ':'".into()); }
        let (val, rest) = json_parse_value(s, heap, interner)?;
        s = rest;
        if let Some(kid) = key.as_string_id() { obj.set_property(kid, val); }
        s = json_trim_start(s);
        if let Some(rest) = s.strip_prefix(',') { s = rest; continue; }
        if let Some(rest) = s.strip_prefix('}') { s = rest; break; }
        return Err("expected ',' or '}'".into());
    }
    let oid = heap.allocate(obj);
    Ok((Value::object_id(oid), s))
}

fn json_parse_array<'s>(s: &'s str, heap: &mut ObjectHeap, interner: &mut Interner) -> Result<(Value, &'s str), String> {
    let mut s = &s[1..];
    let mut elems = Vec::new();
    s = json_trim_start(s);
    if let Some(rest) = s.strip_prefix(']') { let o = JsObject::array(elems); let oid = heap.allocate(o); return Ok((Value::object_id(oid), rest)); }
    loop {
        let (val, rest) = json_parse_value(s, heap, interner)?;
        s = rest; elems.push(val);
        s = json_trim_start(s);
        if let Some(rest) = s.strip_prefix(',') { s = rest; continue; }
        if let Some(rest) = s.strip_prefix(']') { s = rest; break; }
        return Err("expected ',' or ']'".into());
    }
    let o = JsObject::array(elems);
    let oid = heap.allocate(o);
    Ok((Value::object_id(oid), s))
}

// ---------------------------------------------------------------------------

/// JSON string quoting with full control-character and surrogate escapes.
fn json_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
