//! Assorted VM services shared by several dispatch paths: private-brand
//! checks, Object.defineProperty / Object.assign, the Function constructor,
//! Symbol registries, chunk disassembly hooks, and date / JIT / radix
//! formatting helpers.

use super::*;

impl Vm {
    /// Object.defineProperty(target, key, descriptor). Extracted so that both
    /// the inline `Object.defineProperty(...)` CallMethod path and the
    /// extracted-as-Native-fn path (`var f = Object.defineProperty; f(...)`)
    /// share one implementation.
    /// If `oid` is mid-construction with pending private brands and the private
    /// member `name_str` resolves to a METHOD or ACCESSOR owned by a not-yet-installed
    /// (pending) class prototype, return true (access must throw). Private FIELDS
    /// (own `__priv_` on the instance) and fully-constructed objects return false.
    pub(crate) fn private_brand_not_installed(
        &self,
        oid: ObjectId,
        getter_key: StringId,
        setter_key: StringId,
        method_key: StringId,
    ) -> bool {
        let Some(pending) = self.pending_private_brands.get(&oid) else { return false };
        if pending.is_empty() { return false; }
        // Walk the prototype chain; find the prototype that owns the method/accessor.
        let mut cur = self.heap.get(oid).and_then(|o| o.prototype);
        while let Some(c) = cur {
            let Some(o) = self.heap.get(c) else { break };
            if o.has_own_property(getter_key)
                || o.has_own_property(setter_key)
                || o.has_own_property(method_key)
            {
                return pending.contains(&c);
            }
            cur = o.prototype;
        }
        false
    }

    /// PrivateBrandCheck: returns true when `oid` does NOT carry the private name
    /// `#name`, i.e. accessing it must throw TypeError. The object has the brand
    /// when the private member is reachable as a field (own `__priv_#name__`),
    /// a method (`__priv_#name__` on the prototype chain), or an accessor
    /// (`__get_#name__` / `__set_#name__` on the chain).
    pub(crate) fn private_brand_missing(&mut self, oid: ObjectId, name_str: &str) -> bool {
        let priv_key = self.interner.intern(&format!("__priv_{name_str}__"));
        let getter_key = self.interner.intern(&format!("__get_{name_str}__"));
        let setter_key = self.interner.intern(&format!("__set_{name_str}__"));
        self.heap.get_property_chain(oid, priv_key).is_none()
            && self.heap.get_property_chain(oid, getter_key).is_none()
            && self.heap.get_property_chain(oid, setter_key).is_none()
    }

    pub(crate) fn object_define_property(&mut self, args: &[Value]) -> Value {
        // A canonical array index per ECMAScript: the string is the decimal
        // form of a non-negative integer < 2^32-1, with no leading zeros
        // ("0" itself is fine). "01", "1.5", "-1", "4294967295" are not.
        fn canonical_array_index(s: &str) -> Option<usize> {
            if s == "0" {
                return Some(0);
            }
            if s.is_empty() || s.as_bytes()[0] == b'0' || !s.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            s.parse::<u64>()
                .ok()
                .filter(|&n| n < (u32::MAX as u64))
                .map(|n| n as usize)
        }
        let target = args.first().copied().unwrap_or(Value::undefined());
        let key_val = args.get(1).copied().unwrap_or(Value::undefined());
        let desc_val = args.get(2).copied().unwrap_or(Value::undefined());
        // Function-sentinel targets keep properties in
        // fn_property_overrides — core-js installs well-known symbols
        // with `Object.defineProperty(Symbol, 'asyncDispose', {...})`,
        // which used to silently no-op here.
        if let Some(sentinel) = target.as_function() {
            let key_str = if key_val.is_symbol() {
                format!("__sym_{}__", key_val.as_symbol_id().unwrap())
            } else {
                self.value_to_string(key_val)
            };
            let key_id = self.interner.intern(&key_str);
            // Only a descriptor that carries `value` (or get) changes the
            // stored value. Flags-only defines — core-js does
            // `defineProperty(Ctor, 'prototype', {writable:false})` — must
            // keep the existing value; clobbering it with undefined
            // poisoned `.prototype` of wrapped constructors.
            let value = desc_val.as_object_id().and_then(|doid| {
                let value_key = self.interner.intern("value");
                let get_key = self.interner.intern("get");
                self.heap.get_property_chain(doid, value_key)
                    .or_else(|| self.heap.get_property_chain(doid, get_key)
                        .filter(|v| !v.is_undefined()))
            });
            if let Some(value) = value {
                self.fn_property_overrides.insert((sentinel, key_id), Some(value));
            }
            return target;
        }
        let Some(target_oid) = target.as_object_id() else { return target };
        let key_str = if key_val.is_symbol() {
            format!("__sym_{}__", key_val.as_symbol_id().unwrap())
        } else {
            self.value_to_string(key_val)
        };
        let key_id = self.interner.intern(&key_str);
        let mut flags = Property::ALL;
        let mut value = Value::undefined();
        if let Some(desc_oid) = desc_val.as_object_id() {
            let writable_key = self.interner.intern("writable");
            let enumerable_key = self.interner.intern("enumerable");
            let configurable_key = self.interner.intern("configurable");
            let value_key = self.interner.intern("value");
            let get_key = self.interner.intern("get");
            let set_key = self.interner.intern("set");
            if let Some(v) = self.heap.get_property_chain(desc_oid, value_key) {
                value = v;
            }
            flags = 0;
            if let Some(v) = self.heap.get_property_chain(desc_oid, writable_key)
                && v.to_boolean() { flags |= Property::WRITABLE; }
            if let Some(v) = self.heap.get_property_chain(desc_oid, enumerable_key)
                && v.to_boolean() { flags |= Property::ENUMERABLE; }
            if let Some(v) = self.heap.get_property_chain(desc_oid, configurable_key)
                && v.to_boolean() { flags |= Property::CONFIGURABLE; }
            let accessor_flags = flags & (Property::ENUMERABLE | Property::CONFIGURABLE);
            if let Some(getter) = self.heap.get_property_chain(desc_oid, get_key)
                && getter.is_function() {
                    let getter_key = self.interner.intern(&format!("__get_{key_str}__"));
                    if let Some(obj) = self.heap.get_mut(target_oid) {
                        obj.define_property(getter_key,
                            Property::with_flags(getter, accessor_flags));
                    }
                }
            if let Some(setter) = self.heap.get_property_chain(desc_oid, set_key)
                && setter.is_function() {
                    let setter_key = self.interner.intern(&format!("__set_{key_str}__"));
                    if let Some(obj) = self.heap.get_mut(target_oid) {
                        obj.define_property(setter_key,
                            Property::with_flags(setter, accessor_flags));
                    }
                }
        }
        let has_accessor = self.heap.get(target_oid)
            .map(|o| {
                let gk = self.interner.intern(&format!("__get_{key_str}__"));
                let sk = self.interner.intern(&format!("__set_{key_str}__"));
                o.has_own_property(gk) || o.has_own_property(sk)
            })
            .unwrap_or(false);
        if !has_accessor {
            // Array-index defines must land in the array's element storage,
            // not the property map. Arrays keep elements in a separate Vec
            // (backing `arr[i]` and `.length`); a map-only define is invisible
            // to indexed access and length. core-js's `createProperty` (used by
            // Array.from, Array.of, spread, etc.) builds result arrays via
            // `Object.defineProperty(arr, i, {value})`, so without this every
            // such array came back all-`undefined` — which broke `Array.from`
            // wholesale once core-js's polyfill loaded.
            // Only route to element storage when the descriptor carries the
            // full default flag set (writable+enumerable+configurable) — the
            // shape of a normal array element. core-js's createProperty /
            // Array.from build with exactly these flags. A partial-flag define
            // (e.g. a non-writable index) keeps the property-map path so its
            // descriptor flags are preserved; copper's Vec-backed elements
            // can't carry per-element flags.
            if flags == Property::ALL
                && let Some(idx) = canonical_array_index(&key_str)
            {
                let arr_len = self.heap.get(target_oid).and_then(|o| {
                    if let ObjectKind::Array(ref e) = o.kind { Some(e.len()) } else { None }
                });
                // Only handle in-bounds appends / overwrites (idx <= len):
                // that's exactly the createProperty / Array.from pattern
                // (idx == len each step, or overwriting an existing slot), so
                // the Vec grows by at most one. A large *sparse* index would
                // otherwise balloon the dense Vec to `idx` undefined slots and
                // trip the VM's execution limit (this broke DuckDuckGo's SERP
                // bundles). Out-of-bounds / sparse indices fall through to the
                // property-map path — the original pre-fix behaviour.
                if let Some(len) = arr_len
                    && idx <= len
                {
                    if let Some(obj) = self.heap.get_mut(target_oid)
                        && let ObjectKind::Array(ref mut elements) = obj.kind
                    {
                        while elements.len() <= idx {
                            elements.push(Value::undefined());
                        }
                        elements[idx] = value;
                    }
                    return target;
                }
            }
            if let Some(obj) = self.heap.get_mut(target_oid) {
                obj.define_property(key_id, Property::with_flags(value, flags));
            }
        }
        target
    }

    /// Implements `Function(...)` and `new Function(...)`: concatenates params,
    /// compiles `function(p1,p2,...){ body }`, and returns a callable function value.
    pub(crate) fn construct_function(&mut self, args: &[Value]) -> Result<Value, VmError> {
        let params_str = if args.len() > 1 {
            args[..args.len() - 1]
                .iter()
                .map(|v| self.value_to_string(*v))
                .collect::<Vec<_>>()
                .join(",")
        } else {
            String::new()
        };
        let body_str = args.last().map(|v| self.value_to_string(*v)).unwrap_or_default();
        let src = format!("return (function({}){{ {} }})", params_str, body_str);

        // Lex, parse, compile
        let mut lexer = crate::lexer::lexer::Lexer::new(&src, &mut self.interner);
        let tokens = lexer.tokenize();
        let mut parser = crate::parser::parser::Parser::new(tokens, &src, &mut self.interner);
        let program = parser
            .parse_program()
            .map_err(|e| VmError::RuntimeError(format!("Function SyntaxError: {e}")))?;
        let compiler = crate::compiler::compiler::Compiler::new(&mut self.interner);
        let chunk = compiler
            .compile_program(&program)
            .map_err(|e| VmError::RuntimeError(format!("Function CompileError: {e}")))?;
        let base_idx = self.chunks.len();
        let mut flat_chunks = Vec::new();
        Vm::flatten_chunk(chunk, &mut flat_chunks);
        // Adjust children indices to be absolute (flatten_chunk uses indices relative to its output vec).
        for c in &mut flat_chunks {
            for child in &mut c.children {
                *child += base_idx;
            }
        }
        self.maybe_disasm_chunks(&flat_chunks);
        self.chunks.extend(flat_chunks);
        // Run the outer wrapper to evaluate the function expression
        let wrapper_fn = Value::function(base_idx as i32);
        let result = self.call_function(wrapper_fn, &[])?;
        Ok(result)
    }

    /// Embedder-facing: enqueue `callback` on the engine's microtask
    /// queue (drained by `drain_microtasks` — end of eval, end of
    /// tick). Backs `queueMicrotask`: running such callbacks
    /// synchronously breaks run-to-completion, which React's
    /// sync-flush scheduling observes immediately.
    pub fn host_enqueue_microtask(&mut self, callback: Value) {
        let pid = self.allocate_promise();
        self.microtask_queue.push(Microtask::PromiseReaction {
            callback: Some(callback),
            value: Value::undefined(),
            result_promise: pid,
            is_fulfilled: true,
        });
    }

    /// `Symbol.for(key)`: one shared symbol per key, registered globally.
    pub(crate) fn exec_symbol_for(&mut self, args: &[Value]) -> Value {
        let key = args
            .first()
            .map(|v| self.value_to_string(*v))
            .unwrap_or_else(|| "undefined".to_string());
        if let Some(&id) = self.symbol_registry.get(&key) {
            return Value::symbol(id);
        }
        let id = self.next_symbol_id;
        self.next_symbol_id += 1;
        if id as usize >= self.symbol_descriptions.len() {
            self.symbol_descriptions.resize(id as usize + 1, None);
        }
        let desc = self.interner.intern(&key);
        self.symbol_descriptions[id as usize] = Some(desc);
        self.symbol_registry.insert(key, id);
        Value::symbol(id)
    }

    /// `Symbol.keyFor(sym)`: the registry key, or undefined.
    pub(crate) fn exec_symbol_key_for(&mut self, args: &[Value]) -> Value {
        let Some(sym) = args.first().and_then(|v| v.as_symbol_id()) else {
            return Value::undefined();
        };
        for (k, &id) in &self.symbol_registry {
            if id == sym {
                let sid = self.interner.intern(k);
                return Value::string(sid);
            }
        }
        Value::undefined()
    }

    /// Object.assign(target, ...sources) — shared by the CallMethod
    /// dispatch and the extractable `Object.assign` value (sentinel
    /// -750): minified bundles alias `var assign = Object.assign;`.
    pub(crate) fn exec_object_assign(&mut self, args: &[Value]) -> Value {
        let target = args.first().copied().unwrap_or(Value::undefined());
        if let Some(target_oid) = target.as_object_id() {
            for source_val in args.iter().skip(1) {
                if let Some(src_oid) = source_val.as_object_id() {
                    let props: Vec<(StringId, Value)> = self.heap.get(src_oid)
                        .map(|o| o.properties.iter()
                            .filter(|(k, p)| p.is_enumerable()
                                && !is_internal_key(self.interner.resolve(*k)))
                            .map(|&(k, ref p)| (k, p.value)).collect())
                        .unwrap_or_default();
                    for (k, v) in props {
                        if let Some(obj) = self.heap.get_mut(target_oid) {
                            obj.set_property(k, v);
                        }
                    }
                }
            }
        }
        target
    }

    /// Debug aid: ZINC_DISASM_CHUNK=<name> dumps the bytecode of every
    /// newly-registered chunk with that name to stderr.
    pub(crate) fn maybe_disasm_chunks(&self, flat_chunks: &[crate::compiler::chunk::Chunk]) {
        if let Ok(want) = std::env::var("ZINC_DISASM_CHUNK") {
            for c in flat_chunks {
                if self.interner.resolve(c.name) == want {
                    eprintln!("==== disasm {want} ====");
                    eprintln!("{}", crate::compiler::disassemble::disassemble(c, &self.interner));
                }
            }
        }
    }
}

/// Convert Unix epoch milliseconds to (year, month0, day) in UTC.
pub(crate) fn epoch_to_ymd(ms: f64) -> (i32, i32, i32) {
    let days = (ms / 86_400_000.0).floor() as i64;
    // Civil-from-days (Howard Hinnant)
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as i32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as i32;
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m - 1, d)
}

pub(crate) fn format_date(ms: f64) -> String {
    format_iso(ms)
}

/// StringToBigInt: parse a string (per `==` and `BigInt(str)`) into a BigInt.
/// Allows surrounding whitespace, an optional sign, and 0x/0o/0b prefixes.
/// Empty/whitespace-only yields 0n; anything malformed yields None.
pub(crate) fn string_to_bigint(s: &str) -> Option<num_bigint::BigInt> {
    let t = s.trim();
    if t.is_empty() { return Some(num_bigint::BigInt::default()); }
    let (neg, body) = match t.as_bytes()[0] {
        b'-' => (true, &t[1..]),
        b'+' => (false, &t[1..]),
        _ => (false, t),
    };
    // Only one sign is permitted; the remaining body must be plain digits
    // (parse_bytes would otherwise accept a second leading sign like "++0").
    if body.starts_with(['+', '-']) { return None; }
    let v = parse_bigint_literal(body)?;
    Some(if neg { -v } else { v })
}

/// True iff the BigInt equals the (finite, integral) Number `f`.
pub(crate) fn bigint_eq_f64(big: &num_bigint::BigInt, f: f64) -> bool {
    if !f.is_finite() || f.fract() != 0.0 { return false; }
    matches!(num_traits::FromPrimitive::from_f64(f), Some(bf) if *big == bf)
}

/// Compare a BigInt with a Number for relational operators. None if `f` is NaN.
pub(crate) fn bigint_cmp_f64(big: &num_bigint::BigInt, f: f64) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    if f.is_nan() { return None; }
    if f == f64::INFINITY { return Some(Ordering::Less); }
    if f == f64::NEG_INFINITY { return Some(Ordering::Greater); }
    // Compare exactly: floor(f) splits the integer boundary, fract breaks ties.
    let floor = f.floor();
    let bf: num_bigint::BigInt = num_traits::FromPrimitive::from_f64(floor)
        .unwrap_or_default();
    match big.cmp(&bf) {
        Ordering::Equal => {
            // big == floor(f); if f has a fractional part, f is larger.
            if f.fract() > 0.0 { Some(Ordering::Less) } else { Some(Ordering::Equal) }
        }
        other => Some(other),
    }
}

/// Top-level `var` and function-declaration names of an eval program — the
/// names EvalDeclarationInstantiation would install in the variable environment.
pub(crate) fn collect_eval_hoisted_names(program: &crate::ast::node::Program) -> Vec<crate::util::interner::StringId> {
    use crate::ast::node::Statement;
    let mut out = Vec::new();
    crate::compiler::compiler::collect_program_var_names(&program.body, &mut out);
    for stmt in &program.body {
        if let Statement::Function(f) = stmt
            && let Some(name) = f.id
        {
            out.push(name);
        }
    }
    out
}

/// ECMAScript ToInt32 on an f64.
pub(crate) fn f64_to_int32(n: f64) -> i32 {
    if n.is_nan() || n.is_infinite() || n == 0.0 { return 0; }
    let int = n.signum() * n.abs().floor();
    let int32bit = int.rem_euclid(4294967296.0);
    if int32bit >= 2147483648.0 { (int32bit - 4294967296.0) as i32 } else { int32bit as i32 }
}

/// Parse a BigInt literal's digit string (no trailing `n`, separators already
/// stripped) into a BigInt, honoring 0x/0o/0b radix prefixes. Returns None on
/// malformed input.
pub(crate) fn parse_bigint_literal(s: &str) -> Option<num_bigint::BigInt> {
    use num_bigint::BigInt;
    let s = s.trim();
    let (radix, digits) = if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        (16, rest)
    } else if let Some(rest) = s.strip_prefix("0o").or_else(|| s.strip_prefix("0O")) {
        (8, rest)
    } else if let Some(rest) = s.strip_prefix("0b").or_else(|| s.strip_prefix("0B")) {
        (2, rest)
    } else {
        (10, s)
    };
    if digits.is_empty() { return if radix == 10 { Some(BigInt::default()) } else { None }; }
    BigInt::parse_bytes(digits.as_bytes(), radix)
}

pub(crate) fn format_iso(ms: f64) -> String {
    let (y, m0, d) = epoch_to_ymd(ms);
    let hour = (ms / 3_600_000.0).rem_euclid(24.0) as i32;
    let min = (ms / 60_000.0).rem_euclid(60.0) as i32;
    let sec = (ms / 1000.0).rem_euclid(60.0) as i32;
    let msec = ms.rem_euclid(1000.0) as i32;
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z", y, m0 + 1, d, hour, min, sec, msec)
}

// ---- Standalone JSON parser (avoids &mut self borrow issues) ----

/// Unbox a NaN-tagged Value to a raw i64 for the globals JIT buffer.
#[cfg(any(all(target_arch = "aarch64", target_os = "macos"), target_arch = "x86_64"))]
pub(crate) fn jit_unbox(val: Value) -> i64 {
    if val.is_int() {
        val.as_int().unwrap_or(0) as i64
    } else if val.is_boolean() {
        if val.as_bool().unwrap_or(false) { 1 } else { 0 }
    } else if let Some(f) = val.as_number() {
        f as i64
    } else {
        0  // null, undefined, object → 0
    }
}

/// Rebox a raw i64 from the globals JIT buffer back to a NaN-tagged Value.
#[cfg(any(all(target_arch = "aarch64", target_os = "macos"), target_arch = "x86_64"))]
pub(crate) fn jit_rebox(v: i64) -> Value {
    if v >= i32::MIN as i64 && v <= i32::MAX as i64 {
        Value::int(v as i32)
    } else {
        Value::number(v as f64)
    }
}

/// Format an unsigned integer in a given radix (2-36).
pub(crate) fn radix_fmt(mut n: u64, radix: u32) -> String {
    if n == 0 { return "0".to_string(); }
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut result = Vec::new();
    while n > 0 {
        result.push(digits[(n % radix as u64) as usize]);
        n /= radix as u64;
    }
    result.reverse();
    String::from_utf8(result).unwrap()
}

// Tests
// ---------------------------------------------------------------------------

