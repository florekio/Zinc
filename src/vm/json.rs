use crate::runtime::object::{JsObject, ObjectHeap, ObjectKind};
use crate::runtime::value::Value;
use crate::util::interner::{Interner, StringId};

use super::vm::Vm;

impl Vm {
    // ---- JSON method dispatch ----
    pub(crate) fn exec_json_method(&mut self, method_name: StringId, args: &[Value]) -> Value {
        let name = self.interner.resolve(method_name).to_owned();
        match name.as_str() {
            "parse" => {
                let s = args.first().map(|v| self.value_to_string(*v)).unwrap_or_default();
                match self.json_parse(&s) {
                    Ok(val) => val,
                    Err(_) => Value::undefined(), // SyntaxError in real JS
                }
            }
            "stringify" => {
                let val = args.first().copied().unwrap_or(Value::undefined());
                let s = self.json_stringify(val);
                let id = self.interner.intern(&s);
                Value::string(id)
            }
            _ => Value::undefined(),
        }
    }

    // ---- JSON.parse: simple recursive descent ----
    pub(crate) fn json_parse(&mut self, input: &str) -> Result<Value, String> {
        let input = input.trim();
        let (val, _) = json_parse_value(input, &mut self.heap, &mut self.interner)?;
        Ok(val)
    }

    // ---- JSON.stringify ----
    pub(crate) fn json_stringify(&self, val: Value) -> String {
        if val.is_undefined() { return "undefined".into(); }
        if val.is_null() { return "null".into(); }
        if val.is_boolean() { return format!("{}", val.as_bool().unwrap()); }
        if val.is_int() { return format!("{}", val.as_int().unwrap()); }
        if val.is_float() {
            let n = val.as_float().unwrap();
            if n.is_nan() || n.is_infinite() { return "null".into(); }
            return format!("{n}");
        }
        if val.is_string() {
            let id = val.as_string_id().unwrap();
            let s = self.interner.resolve(id);
            return format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n").replace('\r', "\\r").replace('\t', "\\t"));
        }
        if let Some(oid) = val.as_object_id()
            && let Some(obj) = self.heap.get(oid) {
                match &obj.kind {
                    ObjectKind::Array(elements) => {
                        let parts: Vec<String> = elements.iter().map(|v| self.json_stringify(*v)).collect();
                        return format!("[{}]", parts.join(","));
                    }
                    _ => {
                        let parts: Vec<String> = obj.properties.iter().map(|(k, v)| {
                            let key = self.interner.resolve(*k);
                            format!("\"{}\":{}", key, self.json_stringify(*v))
                        }).collect();
                        return format!("{{{}}}", parts.join(","));
                    }
                }
            }
        "null".into()
    }
}

fn json_parse_value<'s>(s: &'s str, heap: &mut ObjectHeap, interner: &mut Interner) -> Result<(Value, &'s str), String> {
    let s = s.trim_start();
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

fn json_parse_string<'s>(s: &'s str, interner: &mut Interner) -> Result<(Value, &'s str), String> {
    let s = &s[1..];
    let mut result = String::new();
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => { let id = interner.intern(&result); return Ok((Value::string(id), &s[i + 1..])); }
            '\\' => { if let Some((_, esc)) = chars.next() { match esc {
                '"' => result.push('"'), '\\' => result.push('\\'), '/' => result.push('/'),
                'n' => result.push('\n'), 'r' => result.push('\r'), 't' => result.push('\t'),
                _ => { result.push('\\'); result.push(esc); }
            }}}
            _ => result.push(c),
        }
    }
    Err("unterminated string".into())
}

fn json_parse_number(s: &str) -> Result<(Value, &str), String> {
    let mut end = 0;
    let b = s.as_bytes();
    if end < b.len() && b[end] == b'-' { end += 1; }
    while end < b.len() && b[end].is_ascii_digit() { end += 1; }
    if end < b.len() && b[end] == b'.' { end += 1; while end < b.len() && b[end].is_ascii_digit() { end += 1; } }
    if end < b.len() && (b[end] == b'e' || b[end] == b'E') { end += 1; if end < b.len() && (b[end] == b'+' || b[end] == b'-') { end += 1; } while end < b.len() && b[end].is_ascii_digit() { end += 1; } }
    let n: f64 = s[..end].parse().map_err(|_| "invalid number".to_string())?;
    Ok((Value::number(n), &s[end..]))
}

fn json_parse_object<'s>(s: &'s str, heap: &mut ObjectHeap, interner: &mut Interner) -> Result<(Value, &'s str), String> {
    let mut s = &s[1..];
    let mut obj = JsObject::ordinary();
    s = s.trim_start();
    if let Some(rest) = s.strip_prefix('}') { let oid = heap.allocate(obj); return Ok((Value::object_id(oid), rest)); }
    loop {
        s = s.trim_start();
        let (key, rest) = json_parse_string(s, interner)?;
        s = rest.trim_start();
        if let Some(rest) = s.strip_prefix(':') { s = rest; } else { return Err("expected ':'".into()); }
        let (val, rest) = json_parse_value(s, heap, interner)?;
        s = rest;
        if let Some(kid) = key.as_string_id() { obj.set_property(kid, val); }
        s = s.trim_start();
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
    s = s.trim_start();
    if let Some(rest) = s.strip_prefix(']') { let o = JsObject::array(elems); let oid = heap.allocate(o); return Ok((Value::object_id(oid), rest)); }
    loop {
        let (val, rest) = json_parse_value(s, heap, interner)?;
        s = rest; elems.push(val);
        s = s.trim_start();
        if let Some(rest) = s.strip_prefix(',') { s = rest; continue; }
        if let Some(rest) = s.strip_prefix(']') { s = rest; break; }
        return Err("expected ',' or ']'".into());
    }
    let o = JsObject::array(elems);
    let oid = heap.allocate(o);
    Ok((Value::object_id(oid), s))
}

// ---------------------------------------------------------------------------
