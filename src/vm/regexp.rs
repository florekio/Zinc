use std::collections::HashMap;
use fancy_regex::Regex;

use crate::runtime::object::{JsObject, ObjectId, ObjectKind};
use crate::runtime::value::Value;
use crate::util::interner::StringId;

use super::vm::{Vm, VmError};

/// Translate a JS regex source into one the Rust regex engine accepts, for the
/// dialect differences the engine is strict about. Currently: an unescaped `[`
/// inside a character class is a literal in JS (`[^()[\]]`) but the Rust engine
/// treats `[` as a nested-class / set operator and errors ("Invalid character
/// class") — Sizzle's selector regexes hit this. Escape it to `\[`.
pub(crate) fn translate_js_regex(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len() + 8);
    let mut chars = pattern.chars().peekable();
    let mut in_class = false;
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                // Copy the escape and its target verbatim.
                out.push('\\');
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            }
            '[' if !in_class => {
                // JS empty classes: `[]` never matches, `[^]` matches
                // anything — the Rust engine rejects both spellings.
                if chars.peek() == Some(&']') {
                    chars.next();
                    out.push_str("[^\\s\\S]");
                    continue;
                }
                in_class = true;
                out.push('[');
                if chars.peek() == Some(&'^') {
                    out.push('^');
                    chars.next();
                    if chars.peek() == Some(&']') {
                        chars.next();
                        out.pop();
                        out.push_str("\\s\\S]");
                        in_class = false;
                    }
                }
            }
            '[' if in_class => {
                // Literal `[` inside a class — escape for the Rust engine.
                out.push_str("\\[");
            }
            ']' if in_class => {
                in_class = false;
                out.push(']');
            }
            _ => out.push(c),
        }
    }
    out
}

/// Cache for compiled regexes, keyed by (pattern, flags).
pub struct RegexCache {
    cache: HashMap<(String, String), Regex>,
}

impl RegexCache {
    pub fn new() -> Self {
        Self { cache: HashMap::new() }
    }

    pub fn get_or_compile(&mut self, pattern: &str, flags: &str) -> Result<(Regex, bool), String> {
        let global = flags.contains('g');
        let key = (pattern.to_string(), flags.to_string());
        if let Some(re) = self.cache.get(&key) {
            return Ok((re.clone(), global));
        }
        let mut prefix = String::new();
        for ch in flags.chars() {
            match ch {
                'i' => prefix.push_str("(?i)"),
                'm' => prefix.push_str("(?m)"),
                's' => prefix.push_str("(?s)"),
                'g' | 'u' | 'y' | 'd' => {} // handled separately or ignored
                _ => return Err(format!("Invalid regex flag: {ch}")),
            }
        }
        let rust_pattern = format!("{prefix}{}", translate_js_regex(pattern));
        let re = Regex::new(&rust_pattern).map_err(|e| {
            if std::env::var("ZINC_REGEX_TRACE").is_ok() {
                eprintln!("[regex] compile failed: {e}\n  pattern: {rust_pattern}");
            }
            format!("Invalid regex: {e}")
        })?;
        self.cache.insert(key, re.clone());
        Ok((re, global))
    }
}

/// Translate JS regex pattern + flags to a compiled Rust Regex.
/// Returns (compiled regex, is_global).
#[allow(dead_code)]
pub fn compile_js_regex(pattern: &str, flags: &str) -> Result<(Regex, bool), String> {
    let mut prefix = String::new();
    let mut global = false;
    for ch in flags.chars() {
        match ch {
            'i' => prefix.push_str("(?i)"),
            'm' => prefix.push_str("(?m)"),
            's' => prefix.push_str("(?s)"),
            'g' => global = true,
            'u' | 'y' | 'd' => {} // ignore for now
            _ => return Err(format!("Invalid regex flag: {ch}")),
        }
    }
    let rust_pattern = format!("{prefix}{}", translate_js_regex(pattern));
    let re = Regex::new(&rust_pattern).map_err(|e| {
        if std::env::var("ZINC_REGEX_TRACE").is_ok() {
            eprintln!("[regex] compile failed: {e}\n  pattern: {rust_pattern}");
        }
        format!("Invalid regex: {e}")
    })?;
    Ok((re, global))
}

impl Vm {
    /// Execute a RegExp method (.test, .exec, .toString).
    pub(crate) fn exec_regexp_method(
        &mut self,
        oid: ObjectId,
        method_name: StringId,
        args: &[Value],
    ) -> Result<Value, VmError> {
        let (pattern, flags) = {
            let obj = self.heap.get(oid).ok_or_else(|| {
                VmError::RuntimeError("RegExp object not found".into())
            })?;
            match &obj.kind {
                ObjectKind::RegExp { pattern, flags } => (pattern.clone(), flags.clone()),
                _ => return Ok(Value::undefined()),
            }
        };
        let name = self.interner.resolve(method_name).to_owned();

        match name.as_str() {
            "test" => {
                let input = args
                    .first()
                    .map(|v| self.value_to_string(*v))
                    .unwrap_or_default();
                let (re, _global) = self.regex_cache.get_or_compile(&pattern, &flags)
                    .map_err(VmError::RuntimeError)?;
                // fancy-regex matching is fallible (backtrack limit);
                // treat a blown limit as "no match" rather than a throw.
                Ok(Value::boolean(re.is_match(&input).unwrap_or(false)))
            }
            "exec" => {
                let input = args
                    .first()
                    .map(|v| self.value_to_string(*v))
                    .unwrap_or_default();
                let (re, global) = self.regex_cache.get_or_compile(&pattern, &flags)
                    .map_err(VmError::RuntimeError)?;
                // Global and sticky regexes are stateful: matching resumes from
                // `lastIndex` and advances it, so `while ((m = re.exec(s)))`
                // walks the string and terminates. Without this it always
                // re-matched from 0 → an infinite loop.
                let sticky = flags.contains('y');
                let stateful = global || sticky;
                let li_key = self.interner.intern("lastIndex");
                let start = if stateful {
                    self.heap.get_property_chain(oid, li_key)
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0)
                        .max(0.0) as usize
                } else {
                    0
                };
                let caps_opt = if start > input.len() {
                    None
                } else {
                    re.captures_from_pos(&input, start).ok().flatten()
                };
                // Sticky requires the match to begin exactly at lastIndex.
                let caps_opt = match caps_opt {
                    Some(c) if sticky && c.get(0).map(|m| m.start()) != Some(start) => None,
                    other => other,
                };
                if stateful {
                    let next = match &caps_opt {
                        Some(c) => c.get(0).map(|m| m.end()).unwrap_or(start) as i32,
                        None => 0,
                    };
                    if let Some(o) = self.heap.get_mut(oid) {
                        o.set_property(li_key, Value::int(next));
                    }
                }
                match caps_opt {
                    Some(caps) => {
                        // Build result array: [full_match, ...groups]
                        let mut elements = Vec::new();
                        for i in 0..caps.len() {
                            if let Some(m) = caps.get(i) {
                                let id = self.interner.intern(m.as_str());
                                elements.push(Value::string(id));
                            } else {
                                elements.push(Value::undefined());
                            }
                        }
                        // Collect named groups into a `groups` object (or null if none).
                        let named: Vec<(String, Option<String>)> = re.capture_names()
                            .filter_map(|n| n.map(|name| (
                                name.to_string(),
                                caps.name(name).map(|m| m.as_str().to_string()),
                            )))
                            .collect();
                        let mut arr = JsObject::array(elements);
                        // Set .index property
                        let index_key = self.interner.intern("index");
                        if let Some(m) = caps.get(0) {
                            arr.set_property(index_key, Value::int(m.start() as i32));
                        }
                        // Set .input property
                        let input_key = self.interner.intern("input");
                        let input_id = self.interner.intern(&input);
                        arr.set_property(input_key, Value::string(input_id));
                        // Set .groups property: object with named-group captures, or undefined.
                        let groups_key = self.interner.intern("groups");
                        let groups_val = if named.is_empty() {
                            Value::undefined()
                        } else {
                            let mut g = JsObject::ordinary();
                            g.prototype = Some(self.object_prototype);
                            for (name, capture) in &named {
                                let key = self.interner.intern(name);
                                let v = match capture {
                                    Some(s) => Value::string(self.interner.intern(s)),
                                    None => Value::undefined(),
                                };
                                g.set_property(key, v);
                            }
                            Value::object_id(self.heap.allocate(g))
                        };
                        arr.set_property(groups_key, groups_val);
                        let arr_oid = self.heap.allocate(arr);
                        Ok(Value::object_id(arr_oid))
                    }
                    None => Ok(Value::null()),
                }
            }
            "toString" => {
                let s = format!("/{pattern}/{flags}");
                let id = self.interner.intern(&s);
                Ok(Value::string(id))
            }
            _ => Ok(Value::undefined()),
        }
    }

    /// Execute a string method that takes a regex argument.
    /// Returns Some(result) if handled, None if the arg is not a RegExp.
    pub(crate) fn exec_string_regex_method(
        &mut self,
        s: &str,
        method: &str,
        args: &[Value],
    ) -> Option<Value> {
        // Check if first arg is a RegExp
        let first_arg = args.first().copied()?;
        let (pattern, flags) = {
            let oid = first_arg.as_object_id()?;
            let obj = self.heap.get(oid)?;
            match &obj.kind {
                ObjectKind::RegExp { pattern, flags } => (pattern.clone(), flags.clone()),
                _ => return None,
            }
        };

        let (re, global) = self.regex_cache.get_or_compile(&pattern, &flags).ok()?;

        match method {
            "replace" | "replaceAll" => {
                let repl = args.get(1).copied().unwrap_or(Value::undefined());
                let do_all = global || method == "replaceAll";
                if repl.is_function() {
                    // Function replacement: call fn(match, p1..pN, offset, string)
                    // per match and substitute the returned string.
                    let mut out = String::new();
                    let mut last_end = 0usize;
                    for caps in re.captures_iter(s) {
                        let caps = match caps { Ok(c) => c, Err(_) => break };
                        let m0 = match caps.get(0) { Some(m) => m, None => continue };
                        out.push_str(&s[last_end..m0.start()]);
                        let mut cb_args: Vec<Value> = Vec::with_capacity(caps.len() + 2);
                        for i in 0..caps.len() {
                            cb_args.push(match caps.get(i) {
                                Some(m) => Value::string(self.interner.intern(m.as_str())),
                                None => Value::undefined(),
                            });
                        }
                        cb_args.push(Value::int(m0.start() as i32));
                        cb_args.push(Value::string(self.interner.intern(s)));
                        let r = self
                            .call_function_this(repl, Value::undefined(), &cb_args)
                            .unwrap_or(Value::undefined());
                        let rs = self.value_to_string(r);
                        out.push_str(&rs);
                        last_end = m0.end();
                        if !do_all { break; }
                    }
                    out.push_str(&s[last_end..]);
                    let id = self.interner.intern(&out);
                    return Some(Value::string(id));
                }
                let replacement = self.value_to_string(repl);
                let result = if do_all {
                    re.replace_all(s, replacement.as_str()).to_string()
                } else {
                    re.replace(s, replacement.as_str()).to_string()
                };
                let id = self.interner.intern(&result);
                Some(Value::string(id))
            }
            "match" => {
                if global {
                    // Return array of all matches
                    let matches: Vec<Value> = re
                        .find_iter(s)
                        .filter_map(Result::ok)
                        .map(|m| {
                            let id = self.interner.intern(m.as_str());
                            Value::string(id)
                        })
                        .collect();
                    if matches.is_empty() {
                        Some(Value::null())
                    } else {
                        Some(self.alloc_array(matches))
                    }
                } else {
                    // Return single match result (like exec), including the
                    // spec .index and .input properties.
                    match re.captures(s).ok().flatten() {
                        Some(caps) => {
                            let mut elements = Vec::new();
                            for i in 0..caps.len() {
                                if let Some(m) = caps.get(i) {
                                    let id = self.interner.intern(m.as_str());
                                    elements.push(Value::string(id));
                                } else {
                                    elements.push(Value::undefined());
                                }
                            }
                            let start = caps.get(0).map(|m| m.start()).unwrap_or(0);
                            // char index, not byte index
                            let char_index = s[..start].chars().count();
                            let arr_val = self.alloc_array(elements);
                            if let Some(aid) = arr_val.as_object_id()
                                && let Some(o) = self.heap.get_mut(aid)
                            {
                                let ik = self.interner.intern("index");
                                o.set_property(ik, Value::int(char_index as i32));
                            }
                            let input_id = self.interner.intern(s);
                            if let Some(aid) = arr_val.as_object_id()
                                && let Some(o) = self.heap.get_mut(aid)
                            {
                                let nk = self.interner.intern("input");
                                o.set_property(nk, Value::string(input_id));
                            }
                            Some(arr_val)
                        }
                        None => Some(Value::null()),
                    }
                }
            }
            "search" => {
                match re.find(s).ok().flatten() {
                    Some(m) => Some(Value::int(m.start() as i32)),
                    None => Some(Value::int(-1)),
                }
            }
            "split" => {
                let raw: Vec<&str> = re.split(s).filter_map(Result::ok).collect();
                // JS SplitMatch skips zero-width matches at the string's
                // boundaries: an empty-matching regex must not produce
                // leading/trailing "" the way the Rust engine does.
                let empty_width = re.find(s).ok().flatten().is_some_and(|m| m.start() == m.end());
                let mut slice: &[&str] = &raw;
                if empty_width && !s.is_empty() {
                    if slice.first() == Some(&"") {
                        slice = &slice[1..];
                    }
                    if slice.last() == Some(&"") {
                        slice = &slice[..slice.len() - 1];
                    }
                }
                // ToUint32 limit; NaN/absent → unlimited (2^32-1).
                let limit = args.get(1)
                    .filter(|v| !v.is_undefined())
                    .map(|v| {
                        let n = self.to_f64(*v);
                        super::vm::f64_to_int32(n) as u32 as usize
                    });
                let parts: Vec<Value> = slice
                    .iter()
                    .take(limit.unwrap_or(usize::MAX))
                    .map(|part| {
                        let id = self.interner.intern(part);
                        Value::string(id)
                    })
                    .collect();
                Some(self.alloc_array(parts))
            }
            "matchAll" => {
                // Return an array of match-result arrays (iterable via for-of).
                // Spec returns an iterator; an array covers the common usage.
                let mut results: Vec<Value> = Vec::new();
                for caps in re.captures_iter(s) {
                    let caps = match caps { Ok(c) => c, Err(_) => break };
                    let mut elements = Vec::new();
                    for i in 0..caps.len() {
                        elements.push(match caps.get(i) {
                            Some(m) => Value::string(self.interner.intern(m.as_str())),
                            None => Value::undefined(),
                        });
                    }
                    let mut arr = JsObject::array(elements);
                    arr.prototype = Some(self.array_prototype);
                    if let Some(m) = caps.get(0) {
                        let idx_key = self.interner.intern("index");
                        arr.set_property(idx_key, Value::int(m.start() as i32));
                    }
                    let input_key = self.interner.intern("input");
                    let input_id = self.interner.intern(s);
                    arr.set_property(input_key, Value::string(input_id));
                    results.push(Value::object_id(self.heap.allocate(arr)));
                }
                Some(self.alloc_array(results))
            }
            _ => None,
        }
    }
}

/// Structural validation of a JS RegExp pattern: rejects definite grammar
/// violations (unbalanced groups/classes, dangling quantifiers, reversed
/// {m,n} ranges, trailing backslash) while staying permissive about
/// constructs the backing engine may or may not support.
pub fn validate_js_pattern(pattern: &str, unicode: bool) -> Result<(), String> {
    let b: Vec<char> = pattern.chars().collect();
    let n = b.len();
    let mut i = 0usize;
    let mut depth = 0i32;
    let mut prev_quantifiable = false;
    while i < n {
        match b[i] {
            '\\' => {
                if i + 1 >= n {
                    return Err("\\ at end of pattern".into());
                }
                // Unicode property escapes \p{...} / \P{...} consume their
                // braces as part of the escape.
                if matches!(b[i + 1], 'p' | 'P') && i + 2 < n && b[i + 2] == '{' {
                    let mut j = i + 3;
                    while j < n && b[j] != '}' {
                        j += 1;
                    }
                    if j >= n {
                        return Err("invalid property escape".into());
                    }
                    i = j + 1;
                } else {
                    i += 2;
                }
                prev_quantifiable = true;
            }
            '(' => {
                depth += 1;
                if i + 1 < n && b[i + 1] == '?' {
                    if i + 2 >= n {
                        return Err("unterminated group".into());
                    }
                    match b[i + 2] {
                        ':' | '=' | '!' => i += 3,
                        // Modifier groups (?ims-ims:...) — ES2025.
                        'i' | 'm' | 's' | '-' => {
                            let mut j = i + 2;
                            while j < n && matches!(b[j], 'i' | 'm' | 's') {
                                j += 1;
                            }
                            if j < n && b[j] == '-' {
                                j += 1;
                                while j < n && matches!(b[j], 'i' | 'm' | 's') {
                                    j += 1;
                                }
                            }
                            if j >= n || b[j] != ':' {
                                return Err("invalid group".into());
                            }
                            i = j + 1;
                        }
                        '<' => {
                            if i + 3 < n && (b[i + 3] == '=' || b[i + 3] == '!') {
                                i += 4;
                            } else {
                                let mut j = i + 3;
                                while j < n && b[j] != '>' {
                                    j += 1;
                                }
                                if j >= n || j == i + 3 {
                                    return Err("invalid named capture group".into());
                                }
                                i = j + 1;
                            }
                        }
                        _ => return Err("invalid group".into()),
                    }
                } else {
                    i += 1;
                }
                prev_quantifiable = false;
            }
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return Err("unmatched )".into());
                }
                i += 1;
                prev_quantifiable = true;
            }
            '[' => {
                // JS classes close on the FIRST unescaped ']' — '[]' is a
                // valid empty class (unlike POSIX, no literal-] rule).
                let mut j = i + 1;
                if j < n && b[j] == '^' {
                    j += 1;
                }
                let mut closed = false;
                while j < n {
                    match b[j] {
                        '\\' => j += 2,
                        ']' => {
                            closed = true;
                            break;
                        }
                        _ => j += 1,
                    }
                }
                if !closed {
                    return Err("unterminated character class".into());
                }
                i = j + 1;
                prev_quantifiable = true;
            }
            '*' | '+' | '?' => {
                if !prev_quantifiable {
                    return Err("nothing to repeat".into());
                }
                i += 1;
                if i < n && b[i] == '?' {
                    i += 1;
                }
                prev_quantifiable = false;
            }
            '{' => {
                // Try to parse {n} / {n,} / {n,m}
                let mut j = i + 1;
                let d1_start = j;
                while j < n && b[j].is_ascii_digit() {
                    j += 1;
                }
                let d1 = j - d1_start;
                let mut d2: Option<(usize, usize)> = None;
                if d1 > 0 && j < n && b[j] == ',' {
                    j += 1;
                    let s = j;
                    while j < n && b[j].is_ascii_digit() {
                        j += 1;
                    }
                    if j > s {
                        d2 = Some((s, j));
                    }
                }
                if d1 > 0 && j < n && b[j] == '}' {
                    if !prev_quantifiable {
                        return Err("nothing to repeat".into());
                    }
                    if let Some((s, e)) = d2 {
                        let lo: String = b[d1_start..d1_start + d1].iter().collect();
                        let hi: String = b[s..e].iter().collect();
                        if let (Ok(a), Ok(z)) = (lo.parse::<u64>(), hi.parse::<u64>())
                            && a > z
                        {
                            return Err("numbers out of order in {} quantifier".into());
                        }
                    }
                    i = j + 1;
                    if i < n && b[i] == '?' {
                        i += 1;
                    }
                    prev_quantifiable = false;
                } else {
                    if unicode {
                        return Err("lone quantifier brackets".into());
                    }
                    i += 1;
                    prev_quantifiable = true;
                }
            }
            ']' | '}' => {
                if unicode {
                    return Err("lone quantifier brackets".into());
                }
                i += 1;
                prev_quantifiable = true;
            }
            '|' => {
                i += 1;
                prev_quantifiable = false;
            }
            '^' | '$' => {
                i += 1;
                prev_quantifiable = false;
            }
            _ => {
                i += 1;
                prev_quantifiable = true;
            }
        }
    }
    if depth != 0 {
        return Err("unterminated group".into());
    }
    Ok(())
}

/// Validate a RegExp flags string: only dgimsuvy, no duplicates.
pub fn validate_js_flags(flags: &str) -> Result<(), String> {
    let mut seen = [false; 8];
    for c in flags.chars() {
        let idx = match c {
            'd' => 0, 'g' => 1, 'i' => 2, 'm' => 3,
            's' => 4, 'u' => 5, 'v' => 6, 'y' => 7,
            _ => return Err(format!("invalid flag '{c}'")),
        };
        if seen[idx] {
            return Err(format!("duplicate flag '{c}'"));
        }
        seen[idx] = true;
    }
    Ok(())
}

impl Vm {
    /// Set(this, "lastIndex", n) with real descriptor semantics: accessor
    /// setters run, setter-less accessors and non-writable data properties
    /// throw TypeError, plain properties update.
    pub(crate) fn set_lastindex_checked(&mut self, oid: ObjectId, n: f64) -> Result<(), Value> {
        let li = self.interner.intern("lastIndex");
        let gk = self.interner.intern("__get_lastIndex__");
        let sk = self.interner.intern("__set_lastIndex__");
        let (setter, has_accessor, named_nonwritable) = match self.heap.get(oid) {
            Some(o) => (
                o.get_property(sk).filter(|v| v.is_function()),
                o.has_own_property(gk) || o.has_own_property(sk),
                o.get_property_descriptor(li).is_some_and(|p| !p.is_writable()),
            ),
            None => (None, false, false),
        };
        if let Some(sfn) = setter {
            return match self.call_function_this(sfn, Value::object_id(oid), &[Value::number(n)]) {
                Ok(_) => Ok(()),
                Err(VmError::Throw(v)) => Err(v),
                Err(e) => Err(self.make_native_error("Error", &format!("{e:?}"))),
            };
        }
        if has_accessor || named_nonwritable {
            return Err(self.make_native_error(
                "TypeError",
                "Cannot assign to read only property 'lastIndex'",
            ));
        }
        if let Some(o) = self.heap.get_mut(oid) {
            o.set_property(li, Value::number(n));
        }
        Ok(())
    }

    /// ToLength(Get(this, "lastIndex")) with observable getter and valueOf.
    fn read_lastindex_coerced(&mut self, oid: ObjectId) -> Result<f64, Value> {
        let v = self.getter_aware_get(oid, "lastIndex")
            .map_err(|e| match e {
                VmError::Throw(v) => v,
                e => self.make_native_error("Error", &format!("{e:?}")),
            })?
            .unwrap_or(Value::undefined());
        let prim = if v.is_object() && !v.is_symbol() {
            match self.try_coerce_to_primitive_hint(v, "number") {
                Ok(p) => p,
                Err(VmError::Throw(t)) => return Err(t),
                Err(_) => v,
            }
        } else {
            v
        };
        Ok(self.to_f64(prim))
    }

    /// RegExp.prototype[@@replace/@@match/@@search/@@split] — protocol-aware
    /// front door. Native RegExp receivers surface lastIndex descriptor
    /// semantics then delegate to the string machinery; other objects run
    /// the generic RegExpExec protocol (custom `exec`, observable Gets).
    pub(crate) fn regexp_symbol_method(
        &mut self,
        mname: &str,
        this: Value,
        args: &[Value],
    ) -> Result<Value, Value> {
        let Some(oid) = this.as_object_id() else {
            return Err(self.make_native_error(
                "TypeError",
                "RegExp.prototype method called on incompatible receiver",
            ));
        };
        // ToString(string argument), observable; Symbols throw.
        let sv = args.first().copied().unwrap_or(Value::undefined());
        if sv.is_symbol() {
            return Err(self.make_native_error(
                "TypeError",
                "Cannot convert a Symbol value to a string",
            ));
        }
        let prim = if sv.is_object() && !sv.is_symbol() {
            match self.try_coerce_to_primitive_hint(sv, "string") {
                Ok(p) => p,
                Err(VmError::Throw(v)) => return Err(v),
                Err(_) => sv,
            }
        } else {
            sv
        };
        let s = self.value_to_string(prim);
        let native_flags = self.heap.get(oid).and_then(|o| {
            if let ObjectKind::RegExp { ref flags, .. } = o.kind { Some(flags.clone()) } else { None }
        });
        // @@split runs SpeciesConstructor(this): an own constructor override
        // is read observably, its @@species fetched and constructed — abrupt
        // completions propagate (the constructed splitter is approximated by
        // the native machinery afterwards).
        if mname == "split" {
            let ck = self.interner.intern("constructor");
            let gck = self.interner.intern("__get_constructor__");
            let has_own_ctor = self.heap.get(oid)
                .is_some_and(|o| o.has_own_property(ck) || o.has_own_property(gck));
            if has_own_ctor {
                let ctor = self.getter_aware_get(oid, "constructor")
                    .map_err(|e| match e {
                        VmError::Throw(v) => v,
                        e => self.make_native_error("Error", &format!("{e:?}")),
                    })?
                    .unwrap_or(Value::undefined());
                let species = if let Some(packed) = ctor.as_function() {
                    let sk = self.interner.intern("__sym_4__");
                    self.fn_property_overrides.get(&(packed, sk)).copied().flatten()
                } else if let Some(coid) = ctor.as_object_id() {
                    self.getter_aware_get(coid, "__sym_4__")
                        .map_err(|e| match e {
                            VmError::Throw(v) => v,
                            e => self.make_native_error("Error", &format!("{e:?}")),
                        })?
                } else {
                    None
                };
                if let Some(sp) = species.filter(|v| !v.is_nullish()) {
                    let fresh = self.heap.allocate(JsObject::ordinary());
                    let flags_arg = self.new_str(native_flags.as_deref().unwrap_or(""));
                    if let Err(VmError::Throw(v)) =
                        self.call_function_this(sp, Value::object_id(fresh), &[this, flags_arg])
                    {
                        return Err(v);
                    }
                }
            }
        }
        // A receiver with its OWN exec (data or accessor) uses the generic
        // RegExpExec protocol even when it is a native RegExp — custom exec
        // overrides are spec-observable.
        let has_custom_exec = {
            let ek = self.interner.intern("exec");
            let gk = self.interner.intern("__get_exec__");
            self.heap.get(oid).is_some_and(|o| o.has_own_property(ek) || o.has_own_property(gk))
        };
        let repl_is_fn = mname == "replace" && {
            let r = args.get(1).copied().unwrap_or(Value::undefined());
            r.is_function()
                || r.as_object_id().and_then(|o| self.heap.get(o))
                    .is_some_and(|o| matches!(o.kind, ObjectKind::Function(_)))
        };
        if let Some(flags) = native_flags.clone().filter(|_| !has_custom_exec && !repl_is_fn) {
            let global = flags.contains('g');
            let sticky = flags.contains('y');
            // Own accessor overrides of the flag properties are observable
            // Gets per spec (poisoned getters throw before any matching).
            for prop in ["flags", "global", "unicode", "sticky"] {
                let gk = self.interner.intern(&format!("__get_{prop}__"));
                let has_override = self.heap.get(oid).is_some_and(|o| o.has_own_property(gk));
                if has_override {
                    let v = self.getter_aware_get(oid, prop)
                        .map_err(|e| match e {
                            VmError::Throw(v) => v,
                            e => self.make_native_error("Error", &format!("{e:?}")),
                        })?
                        .unwrap_or(Value::undefined());
                    if prop == "flags" && v.is_object() && !v.is_symbol()
                        && let Err(VmError::Throw(t)) = self.try_coerce_to_primitive_hint(v, "string")
                    {
                        return Err(t);
                    }
                }
            }
            // replace: a function replacement routes through the generic
            // protocol (protected callback calls); an object replacement
            // coerces observably.
            if mname == "replace" {
                let repl = args.get(1).copied().unwrap_or(Value::undefined());
                let repl_is_fn = repl.is_function()
                    || repl.as_object_id().and_then(|o| self.heap.get(o))
                        .is_some_and(|o| matches!(o.kind, ObjectKind::Function(_)));
                if !repl_is_fn && repl.is_object() && !repl.is_symbol()
                    && let Err(VmError::Throw(t)) = self.try_coerce_to_primitive_hint(repl, "string")
                {
                    return Err(t);
                }
            }
            // split: the limit coerces observably before any matching.
            if mname == "split"
                && let Some(lim) = args.get(1).copied()
            {
                if lim.is_symbol() {
                    return Err(self.make_native_error(
                        "TypeError",
                        "Cannot convert a Symbol value to a number",
                    ));
                }
                if lim.is_object()
                    && let Err(VmError::Throw(t)) = self.try_coerce_to_primitive_hint(lim, "number")
                {
                    return Err(t);
                }
            }
            // Observable lastIndex protocol on the receiver itself.
            if mname == "search" {
                let prev = self.read_lastindex_coerced(oid)?;
                self.set_lastindex_checked(oid, 0.0)?;
                let ascii = s.is_ascii();
                let name_id = self.interner.intern(mname);
                let mut call_args: Vec<Value> = vec![this];
                call_args.extend(args.iter().skip(1).copied());
                let r = self.exec_string_method(&s, name_id, &call_args, ascii)
                    .map_err(|e| match e {
                        VmError::Throw(v) => v,
                        e => self.make_native_error("Error", &format!("{e:?}")),
                    })?;
                self.set_lastindex_checked(oid, prev)?;
                return Ok(r);
            }
            if (global && matches!(mname, "match" | "replace")) || sticky {
                // Global match/replace reset lastIndex; sticky ops read and
                // rewrite it — either way the read and the write (with
                // descriptor checks) are observable.
                let cur = self.read_lastindex_coerced(oid)?;
                let target = if global { 0.0 } else { cur };
                self.set_lastindex_checked(oid, target)?;
            }
            let ascii = s.is_ascii();
            let name_id = self.interner.intern(mname);
            let mut call_args: Vec<Value> = vec![this];
            call_args.extend(args.iter().skip(1).copied());
            return self.exec_string_method(&s, name_id, &call_args, ascii)
                .map_err(|e| match e {
                    VmError::Throw(v) => v,
                    e => self.make_native_error("Error", &format!("{e:?}")),
                });
        }
        // Generic (non-RegExp) receiver: RegExpExec protocol.
        let unwrap_throw = |vm: &mut Self, e: VmError| match e {
            VmError::Throw(v) => v,
            e => vm.make_native_error("Error", &format!("{e:?}")),
        };
        // Observable flags read (ToString runs) for the methods that use it.
        if matches!(mname, "match" | "replace" | "split") {
            let fl = self.getter_aware_get(oid, "flags")
                .map_err(|e| unwrap_throw(self, e))?;
            if let Some(f) = fl
                && f.is_object() && !f.is_symbol()
                && let Err(VmError::Throw(v)) = self.try_coerce_to_primitive_hint(f, "string")
            {
                return Err(v);
            }
        }
        // split coerces its limit before running.
        if mname == "split"
            && let Some(lim) = args.get(1).copied()
        {
            if lim.is_symbol() {
                return Err(self.make_native_error(
                    "TypeError",
                    "Cannot convert a Symbol value to a number",
                ));
            }
            if lim.is_object()
                && let Err(VmError::Throw(v)) = self.try_coerce_to_primitive_hint(lim, "number")
            {
                return Err(v);
            }
        }
        if mname == "search" {
            let prev = self.read_lastindex_coerced(oid)?;
            self.set_lastindex_checked(oid, 0.0)?;
            let result = self.regexp_exec_generic(oid, this, &s)?;
            self.set_lastindex_checked(oid, prev)?;
            return if result.is_null() {
                Ok(Value::number(-1.0))
            } else if let Some(roid) = result.as_object_id() {
                let idx = self.getter_aware_get(roid, "index")
                    .map_err(|e| unwrap_throw(self, e))?
                    .unwrap_or(Value::undefined());
                Ok(idx)
            } else {
                Ok(Value::number(-1.0))
            };
        }
        // Global comes from native flags or the (already-read) flags string.
        let global = if let Some(f) = &native_flags {
            f.contains('g')
        } else {
            let fl = self.getter_aware_get(oid, "flags")
                .map_err(|e| unwrap_throw(self, e))?
                .unwrap_or(Value::undefined());
            self.value_to_string(fl).contains('g')
        };
        if global && matches!(mname, "match" | "replace") {
            self.set_lastindex_checked(oid, 0.0)?;
        }
        // Exec loop: single-shot for non-global, repeat-until-null (with
        // observable empty-match lastIndex advancement) for global.
        let mut results: Vec<Value> = Vec::new();
        loop {
            let r = self.regexp_exec_generic(oid, this, &s)?;
            if r.is_null() {
                break;
            }
            results.push(r);
            if !global || matches!(mname, "split") {
                break;
            }
            // AdvanceStringIndex on empty matches (observable read + write).
            let m0 = r.as_object_id()
                .map(|roid| self.getter_aware_get(roid, "0"))
                .transpose()
                .map_err(|e| unwrap_throw(self, e))?
                .flatten()
                .unwrap_or(Value::undefined());
            let m0p = if m0.is_object() && !m0.is_symbol() {
                match self.try_coerce_to_primitive_hint(m0, "string") {
                    Ok(p) => p,
                    Err(VmError::Throw(v)) => return Err(v),
                    Err(_) => m0,
                }
            } else { m0 };
            if self.value_to_string(m0p).is_empty() {
                let li = self.read_lastindex_coerced(oid)?;
                self.set_lastindex_checked(oid, li + 1.0)?;
                if li + 1.0 > s.len() as f64 {
                    break;
                }
            }
            if results.len() > 10_000 {
                break; // runaway custom exec
            }
        }
        let result = results.first().copied().unwrap_or(Value::null());
        match mname {
            "match" => {
                if !global {
                    return Ok(result);
                }
                // Global match: array of ToString(Get(result, "0")).
                let mut out = Vec::new();
                for r in &results {
                    let Some(roid) = r.as_object_id() else { continue };
                    let m0 = self.getter_aware_get(roid, "0")
                        .map_err(|e| unwrap_throw(self, e))?
                        .unwrap_or(Value::undefined());
                    let m0p = if m0.is_object() && !m0.is_symbol() {
                        match self.try_coerce_to_primitive_hint(m0, "string") {
                            Ok(p) => p,
                            Err(VmError::Throw(v)) => return Err(v),
                            Err(_) => m0,
                        }
                    } else { m0 };
                    let ms = self.value_to_string(m0p);
                    out.push(self.new_str(&ms));
                }
                if out.is_empty() {
                    return Ok(Value::null());
                }
                Ok(self.alloc_array(out))
            }
            "replace" => {
                if results.is_empty() {
                    return Ok(self.new_str(&s));
                }
                let repl = args.get(1).copied().unwrap_or(Value::undefined());
                let repl_is_fn = repl.is_function()
                    || repl.as_object_id().and_then(|o| self.heap.get(o))
                        .is_some_and(|o| matches!(o.kind, ObjectKind::Function(_)));
                let mut subs: Vec<(usize, String, String)> = Vec::new();
                for r in results.clone() {
                    let Some(roid) = r.as_object_id() else { continue };
                    // Observable reads of the exec result: length (ToLength),
                    // capture slots, and groups.
                    let len_v = self.getter_aware_get(roid, "length")
                        .map_err(|e| unwrap_throw(self, e))?
                        .unwrap_or(Value::undefined());
                    let len_p = if len_v.is_object() && !len_v.is_symbol() {
                        match self.try_coerce_to_primitive_hint(len_v, "number") {
                            Ok(p) => p,
                            Err(VmError::Throw(v)) => return Err(v),
                            Err(_) => len_v,
                        }
                    } else { len_v };
                    let ncaps = self.to_f64(len_p).clamp(0.0, 64.0) as usize;
                    let mut caps: Vec<Value> = Vec::new();
                    for ci in 1..ncaps {
                        let cv = self.getter_aware_get(roid, &ci.to_string())
                            .map_err(|e| unwrap_throw(self, e))?
                            .unwrap_or(Value::undefined());
                        if cv.is_object() && !cv.is_symbol()
                            && let Err(VmError::Throw(v)) = self.try_coerce_to_primitive_hint(cv, "string")
                        {
                            return Err(v);
                        }
                        caps.push(cv);
                    }
                    let groups_v = self.getter_aware_get(roid, "groups")
                        .map_err(|e| unwrap_throw(self, e))?
                        .unwrap_or(Value::undefined());
                    if groups_v.is_null() {
                        // ToObject(null) — namedCaptures coercion is abrupt.
                        return Err(self.make_native_error(
                            "TypeError",
                            "Cannot convert null to object",
                        ));
                    }
                    if let Some(goid) = groups_v.as_object_id() {
                        for key in self.enumerable_own_string_keys(goid) {
                            let gv = self.getter_aware_get(goid, &key)
                                .map_err(|e| unwrap_throw(self, e))?
                                .unwrap_or(Value::undefined());
                            if gv.is_object() && !gv.is_symbol()
                                && let Err(VmError::Throw(v)) = self.try_coerce_to_primitive_hint(gv, "string")
                            {
                                return Err(v);
                            }
                        }
                    }
                    let m0 = self.getter_aware_get(roid, "0")
                        .map_err(|e| unwrap_throw(self, e))?
                        .unwrap_or(Value::undefined());
                    let m0p = if m0.is_object() && !m0.is_symbol() {
                        match self.try_coerce_to_primitive_hint(m0, "string") {
                            Ok(p) => p,
                            Err(VmError::Throw(v)) => return Err(v),
                            Err(_) => m0,
                        }
                    } else { m0 };
                    let matched = self.value_to_string(m0p);
                    let idx_v = self.getter_aware_get(roid, "index")
                        .map_err(|e| unwrap_throw(self, e))?
                        .unwrap_or(Value::undefined());
                    let idxp = if idx_v.is_object() && !idx_v.is_symbol() {
                        match self.try_coerce_to_primitive_hint(idx_v, "number") {
                            Ok(p) => p,
                            Err(VmError::Throw(v)) => return Err(v),
                            Err(_) => idx_v,
                        }
                    } else { idx_v };
                    let pos = (self.to_f64(idxp).max(0.0) as usize).min(s.len());
                    let pos = (0..=pos).rev().find(|p| s.is_char_boundary(*p)).unwrap_or(0);
                    let repl_str = if repl_is_fn {
                        let m_id = self.new_str(&matched);
                        let s_id = self.new_str(&s);
                        let mut cb_args = vec![m_id];
                        cb_args.extend(caps.iter().copied());
                        cb_args.push(Value::number(pos as f64));
                        cb_args.push(s_id);
                        let rv = self.call_function_this(repl, Value::undefined(), &cb_args)
                            .map_err(|e| unwrap_throw(self, e))?;
                        let rp = if rv.is_object() && !rv.is_symbol() {
                            match self.try_coerce_to_primitive_hint(rv, "string") {
                                Ok(p) => p,
                                Err(VmError::Throw(v)) => return Err(v),
                                Err(_) => rv,
                            }
                        } else { rv };
                        self.value_to_string(rp)
                    } else {
                        let rp = if repl.is_object() && !repl.is_symbol() {
                            match self.try_coerce_to_primitive_hint(repl, "string") {
                                Ok(p) => p,
                                Err(VmError::Throw(v)) => return Err(v),
                                Err(_) => repl,
                            }
                        } else { repl };
                        self.value_to_string(rp)
                    };
                    subs.push((pos, matched, repl_str));
                }
                // Build the output left to right; overlapping/regressing
                // positions keep the untouched text.
                let mut out = String::new();
                let mut cursor = 0usize;
                for (pos, matched, repl_str) in subs {
                    if pos < cursor {
                        continue;
                    }
                    out.push_str(&s[cursor..pos]);
                    out.push_str(&repl_str);
                    let tail = (pos + matched.len()).min(s.len());
                    cursor = (tail..=s.len()).find(|p| s.is_char_boundary(*p)).unwrap_or(s.len());
                }
                out.push_str(&s[cursor..]);
                Ok(self.new_str(&out))
            }
            // Generic @@split approximation: no species machinery — the
            // observable coercions above ran; return the unsplit string.
            _ => {
                let one = self.new_str(&s);
                Ok(self.alloc_array(vec![one]))
            }
        }
    }

    /// RegExpExec on a generic receiver: Get(this, "exec"), call it when
    /// callable (result must be Object or null), otherwise TypeError.
    fn regexp_exec_generic(&mut self, oid: ObjectId, this: Value, s: &str) -> Result<Value, Value> {
        let exec = self.getter_aware_get(oid, "exec")
            .map_err(|e| match e {
                VmError::Throw(v) => v,
                e => self.make_native_error("Error", &format!("{e:?}")),
            })?
            .unwrap_or(Value::undefined());
        let callable = exec.is_function()
            || exec.as_object_id().and_then(|o| self.heap.get(o))
                .is_some_and(|o| matches!(o.kind, ObjectKind::Function(_)));
        if !callable {
            return Err(self.make_native_error(
                "TypeError",
                "Receiver is not a RegExp and has no callable exec",
            ));
        }
        let s_val = self.new_str(s);
        let r = self.call_function_this(exec, this, &[s_val])
            .map_err(|e| match e {
                VmError::Throw(v) => v,
                e => self.make_native_error("Error", &format!("{e:?}")),
            })?;
        if !r.is_null() && r.as_object_id().is_none() {
            return Err(self.make_native_error(
                "TypeError",
                "RegExp exec method returned something other than an Object or null",
            ));
        }
        Ok(r)
    }
}
