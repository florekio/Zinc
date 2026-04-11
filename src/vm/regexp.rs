use std::collections::HashMap;
use regex::Regex;

use crate::runtime::object::{JsObject, ObjectId, ObjectKind};
use crate::runtime::value::Value;
use crate::util::interner::StringId;

use super::vm::{Vm, VmError};

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
        let rust_pattern = format!("{prefix}{pattern}");
        let re = Regex::new(&rust_pattern).map_err(|e| format!("Invalid regex: {e}"))?;
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
    let rust_pattern = format!("{prefix}{pattern}");
    let re = Regex::new(&rust_pattern).map_err(|e| format!("Invalid regex: {e}"))?;
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
                Ok(Value::boolean(re.is_match(&input)))
            }
            "exec" => {
                let input = args
                    .first()
                    .map(|v| self.value_to_string(*v))
                    .unwrap_or_default();
                let (re, _global) = self.regex_cache.get_or_compile(&pattern, &flags)
                    .map_err(VmError::RuntimeError)?;
                match re.captures(&input) {
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
            "replace" => {
                let replacement = args
                    .get(1)
                    .map(|v| self.value_to_string(*v))
                    .unwrap_or_default();
                let result = if global {
                    re.replace_all(s, replacement.as_str()).to_string()
                } else {
                    re.replace(s, replacement.as_str()).to_string()
                };
                let id = self.interner.intern(&result);
                Some(Value::string(id))
            }
            "replaceAll" => {
                let replacement = args
                    .get(1)
                    .map(|v| self.value_to_string(*v))
                    .unwrap_or_default();
                let result = re.replace_all(s, replacement.as_str()).to_string();
                let id = self.interner.intern(&result);
                Some(Value::string(id))
            }
            "match" => {
                if global {
                    // Return array of all matches
                    let matches: Vec<Value> = re
                        .find_iter(s)
                        .map(|m| {
                            let id = self.interner.intern(m.as_str());
                            Value::string(id)
                        })
                        .collect();
                    if matches.is_empty() {
                        Some(Value::null())
                    } else {
                        let arr = JsObject::array(matches);
                        let oid = self.heap.allocate(arr);
                        Some(Value::object_id(oid))
                    }
                } else {
                    // Return single match result (like exec)
                    match re.captures(s) {
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
                            let arr = JsObject::array(elements);
                            let oid = self.heap.allocate(arr);
                            Some(Value::object_id(oid))
                        }
                        None => Some(Value::null()),
                    }
                }
            }
            "search" => {
                match re.find(s) {
                    Some(m) => Some(Value::int(m.start() as i32)),
                    None => Some(Value::int(-1)),
                }
            }
            "split" => {
                let parts: Vec<Value> = re
                    .split(s)
                    .map(|part| {
                        let id = self.interner.intern(part);
                        Value::string(id)
                    })
                    .collect();
                let arr = JsObject::array(parts);
                let oid = self.heap.allocate(arr);
                Some(Value::object_id(oid))
            }
            _ => None,
        }
    }
}
