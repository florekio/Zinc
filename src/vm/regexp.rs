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
                in_class = true;
                out.push('[');
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
                    // Return single match result (like exec)
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
                            Some(self.alloc_array(elements))
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
                let parts: Vec<Value> = re
                    .split(s)
                    .filter_map(Result::ok)
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
