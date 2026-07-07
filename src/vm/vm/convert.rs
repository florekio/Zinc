//! Value conversion and comparison: numeric coercion, string
//! representation (flat, inline and cons strings), BigInt helpers, and the
//! abstract/strict equality algorithms.

use super::*;

impl Vm {
    /// Pop two values off the stack, convert each to f64.
    /// JS ToNumber: coerce any value to f64.
    #[inline(always)]
    pub(crate) fn to_f64(&self, val: Value) -> f64 {
        if let Some(n) = val.as_number() { return n; }
        if val.is_boolean() { return if val.as_bool().unwrap() { 1.0 } else { 0.0 }; }
        if val.is_null() { return 0.0; }
        if val.is_undefined() { return f64::NAN; }
        if val.is_string() {
            let inl = val.as_inline_string();
            let raw = if let Some(ref i) = inl {
                i.as_str()
            } else {
                self.interner.resolve(val.as_string_id().unwrap())
            };
            let s = raw.trim();
            if s.is_empty() { return 0.0; }
            // Handle hex literals: 0x, 0X
            if s.starts_with("0x") || s.starts_with("0X") {
                return u64::from_str_radix(&s[2..], 16).map(|v| v as f64).unwrap_or(f64::NAN);
            }
            // Handle octal literals: 0o, 0O
            if s.starts_with("0o") || s.starts_with("0O") {
                return u64::from_str_radix(&s[2..], 8).map(|v| v as f64).unwrap_or(f64::NAN);
            }
            // Handle binary literals: 0b, 0B
            if s.starts_with("0b") || s.starts_with("0B") {
                return u64::from_str_radix(&s[2..], 2).map(|v| v as f64).unwrap_or(f64::NAN);
            }
            // Spec allows only `Infinity`, `+Infinity`, `-Infinity` (case-sensitive).
            // Rust's parser accepts "inf"/"INFINITY" too, so guard against that.
            match s {
                "Infinity" | "+Infinity" => return f64::INFINITY,
                "-Infinity" => return f64::NEG_INFINITY,
                _ => {}
            }
            if s.eq_ignore_ascii_case("inf")
                || s.eq_ignore_ascii_case("infinity")
                || s.eq_ignore_ascii_case("+inf")
                || s.eq_ignore_ascii_case("+infinity")
                || s.eq_ignore_ascii_case("-inf")
                || s.eq_ignore_ascii_case("-infinity")
                || s.eq_ignore_ascii_case("nan")
            {
                return f64::NAN;
            }
            return s.parse::<f64>().unwrap_or(f64::NAN);
        }
        // Wrapper objects: unwrap and coerce the primitive
        if let Some(oid) = val.as_object_id()
            && let Some(obj) = self.heap.get(oid) {
            match &obj.kind {
                ObjectKind::Wrapper(inner) => return self.to_f64(*inner),
                ObjectKind::ConsString { .. } => {
                    let s = self.flatten_cons_to_string(val);
                    let s = s.trim();
                    if s.is_empty() { return 0.0; }
                    return s.parse::<f64>().unwrap_or(f64::NAN);
                }
                _ => {}
            }
        }
        f64::NAN
    }

    /// Convert a Value to i32 for bitwise operations (ECMAScript ToInt32).
    pub(crate) fn to_i32(&self, val: Value) -> Result<i32, VmError> {
        let n = self.to_f64(val);
        if n.is_nan() || n.is_infinite() || n == 0.0 { return Ok(0); }
        let int = n.signum() * n.abs().floor();
        let int32bit = int.rem_euclid(4294967296.0);
        if int32bit >= 2147483648.0 {
            Ok((int32bit - 4294967296.0) as i32)
        } else {
            Ok(int32bit as i32)
        }
    }

    /// Convert a Value to u32 for unsigned right shift (ECMAScript ToUint32).
    pub(crate) fn to_u32(&self, val: Value) -> Result<u32, VmError> {
        let n = self.to_f64(val);
        if n.is_nan() || n.is_infinite() || n == 0.0 { return Ok(0); }
        let int = n.signum() * n.abs().floor();
        Ok(int.rem_euclid(4294967296.0) as u32)
    }

    /// Push a number result, using SMI when the value fits in i32 with no
    /// fractional part.  Preserves -0.0 as a float (JS distinguishes it).
    #[inline]
    /// Pop two operands for an arithmetic/bitwise/shift op, applying ToPrimitive
    /// (number hint) exactly once each. Classifies the pair so the caller can run
    /// BigInt vs Number semantics; mixing the two is a TypeError (Mixed).
    pub(crate) fn pop_arith_operands(&mut self) -> Result<ArithOperands, VmError> {
        let b = self.pop()?;
        let a = self.pop()?;
        // Raw BigInts are already primitive — don't run ToPrimitive on them.
        let a = if a.is_object() && !self.is_bigint(a) { self.try_coerce_to_primitive_hint(a, "number")? } else { a };
        if a.is_symbol() {
            let err = self.make_native_error("TypeError", "Cannot convert a Symbol value to a number");
            return Err(VmError::Throw(err));
        }
        let b = if b.is_object() && !self.is_bigint(b) { self.try_coerce_to_primitive_hint(b, "number")? } else { b };
        if b.is_symbol() {
            let err = self.make_native_error("TypeError", "Cannot convert a Symbol value to a number");
            return Err(VmError::Throw(err));
        }
        match (self.as_bigint(a), self.as_bigint(b)) {
            (Some(x), Some(y)) => Ok(ArithOperands::BigInts(x, y)),
            (None, None) => Ok(ArithOperands::Numbers(self.to_f64(a), self.to_f64(b))),
            _ => Ok(ArithOperands::Mixed),
        }
    }

    /// Throw the standard "Cannot mix BigInt and other types" TypeError.
    pub(crate) fn throw_mix_bigint(&mut self) -> Result<(), VmError> {
        let err = self.make_native_error("TypeError", "Cannot mix BigInt and other types, use explicit conversions");
        self.handle_throw(err)
    }

    pub(crate) fn push_number(&mut self, n: f64) {
        if n == 0.0 && n.is_sign_negative() {
            // -0.0 must stay as a float
            self.push(Value::number(n));
        } else if n.fract() == 0.0
            && n >= i32::MIN as f64
            && n <= i32::MAX as f64
            && !n.is_nan()
        {
            self.push(Value::int(n as i32));
        } else {
            self.push(Value::number(n));
        }
    }

    /// Convert a Value to its string representation, using the interner for
    /// string values.
    pub(crate) fn value_to_string(&self, val: Value) -> String {
        if let Some(inl) = val.as_inline_string() {
            inl.as_str().to_owned()
        } else if let Some(id) = val.as_string_id() {
            self.interner.resolve(id).to_owned()
        } else if val.is_undefined() {
            "undefined".into()
        } else if val.is_null() {
            "null".into()
        } else if let Some(b) = val.as_bool() {
            if b { "true".into() } else { "false".into() }
        } else if let Some(i) = val.as_int() {
            i.to_string()
        } else if let Some(f) = val.as_number() {
            js_format_number(f)
        } else if val.is_symbol() {
            let id = val.as_symbol_id().unwrap();
            if let Some(Some(desc)) = self.symbol_descriptions.get(id as usize) {
                format!("Symbol({})", self.interner.resolve(*desc))
            } else {
                "Symbol()".into()
            }
        } else if val.is_function() {
            "function() { [native code] }".into()
        } else if let Some(oid) = val.as_object_id() {
            if let Some(obj) = self.heap.get(oid) {
                match &obj.kind {
                    ObjectKind::ConsString { .. } => {
                        self.flatten_cons_to_string(val)
                    }
                    ObjectKind::FlatString { data, .. } => data.to_string(),
                    ObjectKind::Array(elements) => {
                        let parts: Vec<String> = elements.iter().map(|v| self.value_to_string(*v)).collect();
                        parts.join(",")
                    }
                    ObjectKind::Wrapper(inner) => self.value_to_string(*inner),
                    ObjectKind::BigInt(b) => b.to_string(),
                    ObjectKind::RegExp { pattern, flags } => {
                        format!("/{pattern}/{flags}")
                    }
                    _ => {
                        // Check for Error-like objects: scan properties for "message" string
                        if let Some(obj) = self.heap.get(oid) {
                            let mut msg_val: Option<Value> = None;
                            let mut name_val: Option<Value> = None;
                            let proto = obj.prototype;
                            for &(k, ref prop) in &obj.properties {
                                let ks = self.interner.resolve(k);
                                if ks == "message" { msg_val = Some(prop.value); }
                                else if ks == "name" { name_val = Some(prop.value); }
                            }
                            // Walk prototype chain for "name" if not an own property
                            if name_val.is_none() {
                                let mut cur = proto;
                                'proto_walk: while let Some(p_oid) = cur {
                                    if let Some(p_obj) = self.heap.get(p_oid) {
                                        for &(k, ref prop) in &p_obj.properties {
                                            if self.interner.resolve(k) == "name" {
                                                name_val = Some(prop.value);
                                                break 'proto_walk;
                                            }
                                        }
                                        cur = p_obj.prototype;
                                    } else {
                                        break;
                                    }
                                }
                            }
                            if let Some(mv) = msg_val
                                && (mv.is_string() || self.is_cons_string(mv)) {
                                    let msg = self.flatten_cons_to_string(mv);
                                    let name = name_val
                                        .map(|v| self.flatten_cons_to_string(v))
                                        .filter(|s| !s.is_empty())
                                        .unwrap_or_else(|| "Error".to_owned());
                                    return format!("{name}: {msg}");
                                }
                        }
                        "[object Object]".into()
                    }
                }
            } else {
                "[object Object]".into()
            }
        } else {
            "???".into()
        }
    }

    /// Return the typeof string for a value.
    pub(crate) fn type_of_value(&self, val: Value) -> &'static str {
        if val.is_undefined() {
            "undefined"
        } else if val.is_null() {
            "object"
        } else if val.is_boolean() {
            "boolean"
        } else if val.is_function() {
            "function"
        } else if val.is_int() || val.is_number() {
            "number"
        } else if val.is_string() {
            "string"
        } else if val.is_symbol() {
            "symbol"
        } else if val.is_object() {
            if let Some(oid) = val.as_object_id()
                && let Some(obj) = self.heap.get(oid) {
                    if matches!(obj.kind, ObjectKind::ConsString { .. } | ObjectKind::FlatString { .. }) { return "string"; }
                    if matches!(obj.kind, ObjectKind::BigInt(_)) { return "bigint"; }
                    // Function-kind objects (bound functions, native sentinels wrapped
                    // as objects, bytecode functions stored as objects) report "function".
                    if matches!(obj.kind, ObjectKind::Function(_)) { return "function"; }
                    // Classes have __constructor__ — typeof should be "function"
                    for &(k, _) in &obj.properties {
                        if self.interner.resolve(k) == "__constructor__" { return "function"; }
                    }
                }
            "object"
        } else {
            "undefined"
        }
    }

    /// ToBigInt(value) per spec. Returns the BigInt value, or Err(thrown).
    pub(crate) fn value_to_bigint(&mut self, val: Value) -> Result<num_bigint::BigInt, VmError> {
        // Unwrap a primitive-wrapper object first (Object(1n)).
        let v = if val.is_object() && !self.is_bigint(val) {
            self.try_coerce_to_primitive_hint(val, "number")?
        } else { val };
        if let Some(b) = self.as_bigint(v) { return Ok(b); }
        if let Some(boolean) = v.as_bool() {
            return Ok(num_bigint::BigInt::from(if boolean { 1 } else { 0 }));
        }
        if v.is_number() || v.is_int() {
            let f = self.to_f64(v);
            if !f.is_finite() || f.fract() != 0.0 {
                let err = self.make_native_error("RangeError", "The number is not a safe integer");
                return Err(VmError::Throw(err));
            }
            return Ok(num_traits::FromPrimitive::from_f64(f).unwrap_or_default());
        }
        if self.is_string_like(v) {
            let s = self.flatten_cons_to_string(v);
            return match string_to_bigint(&s) {
                Some(b) => Ok(b),
                None => {
                    let err = self.make_native_error("SyntaxError", "Cannot convert string to a BigInt");
                    Err(VmError::Throw(err))
                }
            };
        }
        let err = self.make_native_error("TypeError", "Cannot convert value to a BigInt");
        Err(VmError::Throw(err))
    }

    /// Allocate a heap BigInt and return it as a Value. Its prototype is
    /// BigInt.prototype so property reads (`.constructor`, `.toString`) resolve;
    /// `typeof`/arithmetic still classify it by its BigInt kind, not the proto.
    pub(crate) fn make_bigint(&mut self, v: num_bigint::BigInt) -> Value {
        let proto = self.bigint_prototype_oid();
        let mut obj = crate::runtime::object::JsObject::bigint(v);
        obj.prototype = Some(proto);
        let oid = self.heap.allocate(obj);
        Value::object_id(oid)
    }

    /// ToBoolean, heap-aware: `0n` is falsy (other BigInts truthy); everything
    /// else defers to the bit-level `Value::to_boolean`.
    pub(crate) fn truthy(&self, val: Value) -> bool {
        if let Some(b) = self.as_bigint(val) {
            return !num_traits::Zero::is_zero(&b);
        }
        val.to_boolean()
    }

    /// True if `val` is a BigInt primitive (heap object of kind BigInt).
    pub(crate) fn is_bigint(&self, val: Value) -> bool {
        val.as_object_id()
            .and_then(|oid| self.heap.get(oid))
            .map(|o| matches!(o.kind, crate::runtime::object::ObjectKind::BigInt(_)))
            .unwrap_or(false)
    }

    /// BigInt shift: `a << b` (left=true) or `a >> b` (left=false). A negative
    /// shift count reverses direction (`a << -n` == `a >> n`). `>>` is an
    /// arithmetic shift (rounds toward -∞), matching BigInt semantics.
    pub(crate) fn bigint_shift(&mut self, a: num_bigint::BigInt, b: num_bigint::BigInt, left: bool) -> Value {
        use num_bigint::{BigInt, Sign};
        use num_traits::{ToPrimitive, Zero};
        // Normalize to "shift left by `n`" where n may be negative.
        let n = b.to_i64().map(|v| if left { v } else { -v });
        let result = match n {
            Some(n) if n >= 0 => a << (n as usize),
            Some(n) => a >> ((-n) as usize),
            None => {
                // Shift count doesn't fit i64. Either direction is infeasible to
                // materialize for a nonzero value; approximate the limit results.
                let shifting_left = (b.sign() == Sign::Plus) == left;
                if a.is_zero() {
                    BigInt::zero()
                } else if shifting_left {
                    a << usize::MAX // effectively unbounded; only reachable with a==0 in practice
                } else if a.sign() == Sign::Minus {
                    BigInt::from(-1)
                } else {
                    BigInt::zero()
                }
            }
        };
        self.make_bigint(result)
    }

    /// Clone the BigInt value out of `val`, if it is one.
    pub(crate) fn as_bigint(&self, val: Value) -> Option<num_bigint::BigInt> {
        val.as_object_id()
            .and_then(|oid| self.heap.get(oid))
            .and_then(|o| match &o.kind {
                crate::runtime::object::ObjectKind::BigInt(b) => Some(b.clone()),
                _ => None,
            })
    }

    /// Returns true if val is a heap object of kind ConsString.
    #[inline(always)]
    /// Embedder-facing: the string content of `val` when it IS a
    /// string (flat/interned or a runtime-concatenation ConsString),
    /// else None. DOM bindings read text arguments through this —
    /// resolving only interned ids silently turned every
    /// `'prefix' + variable` argument into "".
    pub fn string_content(&self, val: Value) -> Option<String> {
        if self.is_string_like(val) {
            // is_string_like covers interned, inline (SSO), ConsString and
            // FlatString; value_to_string decodes each correctly.
            Some(self.value_to_string(val))
        } else {
            None
        }
    }

    pub(crate) fn is_cons_string(&self, val: Value) -> bool {
        if let Some(oid) = val.as_object_id()
            && let Some(obj) = self.heap.get(oid) {
            matches!(obj.kind, ObjectKind::ConsString { .. })
        } else {
            false
        }
    }

    /// Returns true if val is string-like: either a TAG_STRING or a ConsString object.
    #[inline(always)]
    pub(crate) fn is_string_like(&self, val: Value) -> bool {
        val.is_string() || self.is_cons_string(val) || self.is_flat_string(val)
    }

    /// Whether a string value's content is all-ASCII, enabling O(1) byte-indexed
    /// codepoint access in string methods. Interned strings use the cached flag;
    /// inline strings check their (≤5) bytes; ConsStrings conservatively return
    /// false (the slow char-walk path stays correct).
    #[inline]
    pub(crate) fn string_is_ascii(&self, val: Value) -> bool {
        if let Some(inl) = val.as_inline_string() {
            inl.as_str().is_ascii()
        } else if let Some(id) = val.as_string_id() {
            self.interner.is_ascii(id)
        } else {
            false
        }
    }

    /// True if `val` is a non-interned flat heap string (ObjectKind::FlatString).
    pub(crate) fn is_flat_string(&self, val: Value) -> bool {
        val.as_object_id()
            .and_then(|oid| self.heap.get(oid))
            .map(|o| matches!(o.kind, ObjectKind::FlatString { .. }))
            .unwrap_or(false)
    }

    /// Create a string Value from a `&str`, preferring an inline (small-string)
    /// encoding so transient short strings cost no heap allocation and no
    /// interner growth. String-producing operations use this instead of
    /// `Value::string(self.interner.intern(s))`.
    ///
    /// - empty           -> the interned StringId(0) singleton
    /// - <= 5 UTF-8 bytes -> inline (NaN-box payload; no allocation)
    /// - longer          -> interned (deduplicated and GC-free; the FlatString
    ///   heap path measured slower than interning on the decode benchmark)
    pub(crate) fn new_str(&mut self, s: &str) -> Value {
        if s.is_empty() {
            return Value::string(crate::util::interner::StringId(0));
        }
        if let Some(v) = Value::inline_string(s) {
            return v;
        }
        Value::string(self.interner.intern(s))
    }

    /// Returns the character length of a string-like value in O(1) for ConsString.
    pub(crate) fn string_char_len(&self, val: Value) -> u32 {
        if let Some(inl) = val.as_inline_string() {
            inl.as_str().chars().count() as u32
        } else if let Some(id) = val.as_string_id() {
            self.interner.char_len(id)
        } else if let Some(oid) = val.as_object_id()
            && let Some(obj) = self.heap.get(oid) {
            match &obj.kind {
                ObjectKind::ConsString { len, .. } => *len,
                ObjectKind::FlatString { char_len, .. } => *char_len,
                _ => 0,
            }
        } else {
            0
        }
    }

    /// Flatten a ConsString (or regular string) to a plain String without interning.
    /// Uses an iterative traversal to avoid stack overflow on deep trees.
    pub(crate) fn flatten_cons_to_string(&self, val: Value) -> String {
        if let Some(inl) = val.as_inline_string() {
            return inl.as_str().to_owned();
        }
        if let Some(id) = val.as_string_id() {
            return self.interner.resolve(id).to_owned();
        }
        let capacity = self.string_char_len(val) as usize;
        let mut result = String::with_capacity(capacity);
        let mut stack = Vec::new();
        stack.push(val);
        while let Some(cur) = stack.pop() {
            if let Some(inl) = cur.as_inline_string() {
                result.push_str(inl.as_str());
            } else if let Some(id) = cur.as_string_id() {
                result.push_str(self.interner.resolve(id));
            } else if let Some(oid) = cur.as_object_id()
                && let Some(obj) = self.heap.get(oid) {
                match &obj.kind {
                    ObjectKind::ConsString { left, right, .. } => {
                        // Push right first so left is processed first (LIFO)
                        stack.push(*right);
                        stack.push(*left);
                    }
                    ObjectKind::FlatString { data, .. } => result.push_str(data),
                    _ => {}
                }
            }
        }
        result
    }

    /// Flatten a ConsString to an interned StringId.
    pub(crate) fn flatten_to_string_id(&mut self, val: Value) -> crate::util::interner::StringId {
        if let Some(id) = val.as_string_id() {
            return id;
        }
        let flat = self.flatten_cons_to_string(val);
        self.interner.intern(&flat)
    }

    /// Simplified abstract equality (==). Handles the most common cases:
    /// Abstract equality (==) that propagates throws from valueOf/toString
    /// during ToPrimitive as `Err(VmError::Throw(_))`.
    ///
    ///   - same type: strict equality
    ///   - null == undefined (and vice versa)
    ///   - number == string: coerce string to number
    pub(crate) fn try_abstract_eq(&mut self, a: Value, b: Value) -> Result<bool, VmError> {
        // Fast path: identical bits
        if a.raw() == b.raw() {
            // NaN !== NaN
            if a.is_float() {
                let f = a.as_number().unwrap();
                return Ok(!f.is_nan());
            }
            return Ok(true);
        }

        // null == undefined
        if a.is_nullish() && b.is_nullish() {
            return Ok(true);
        }
        // null/undefined equals NOTHING else, with no coercion — per
        // spec §7.2.13. Falling through coerced the object operand via
        // ToPrimitive, which can THROW (core-js's `null == t` isNullOrUndefined
        // helper died on objects with object-returning toString).
        if a.is_nullish() || b.is_nullish() {
            return Ok(false);
        }

        // Both numbers (int/float mix)
        if a.is_number() && b.is_number() {
            return Ok(a.as_number() == b.as_number());
        }

        // Both strings (including ConsString)
        if self.is_string_like(a) && self.is_string_like(b) {
            return Ok(self.str_eq(a, b));
        }

        // Both booleans
        if a.is_boolean() && b.is_boolean() {
            return Ok(false); // already handled by raw() check
        }

        // number == string: coerce string to number
        if a.is_number() && b.is_string() {
            if let Some(n) = self.string_to_number(b) {
                return Ok(a.as_number() == Some(n));
            }
            return Ok(false);
        }
        if a.is_string() && b.is_number() {
            if let Some(n) = self.string_to_number(a) {
                return Ok(b.as_number() == Some(n));
            }
            return Ok(false);
        }

        // boolean vs other: coerce boolean to number, retry
        if a.is_boolean() {
            let num_a = if a.as_bool().unwrap() { 1.0 } else { 0.0 };
            return self.try_abstract_eq(Value::number(num_a), b);
        }
        if b.is_boolean() {
            let num_b = if b.as_bool().unwrap() { 1.0 } else { 0.0 };
            return self.try_abstract_eq(a, Value::number(num_b));
        }

        // BigInt comparisons. A BigInt is a heap object, so this must run before
        // the generic object-vs-primitive branch below.
        if self.is_bigint(a) || self.is_bigint(b) {
            // If the other operand is a (non-BigInt) object, coerce it first.
            if self.is_bigint(a) && b.is_object() && !self.is_bigint(b) {
                let pb = self.try_coerce_to_primitive_hint(b, "default")?;
                return self.try_abstract_eq(a, pb);
            }
            if self.is_bigint(b) && a.is_object() && !self.is_bigint(a) {
                let pa = self.try_coerce_to_primitive_hint(a, "default")?;
                return self.try_abstract_eq(pa, b);
            }
            let (big, other) = if let Some(x) = self.as_bigint(a) { (x, b) } else { (self.as_bigint(b).unwrap(), a) };
            if let Some(y) = self.as_bigint(other) {
                return Ok(big == y);
            }
            if other.is_number() {
                return Ok(bigint_eq_f64(&big, self.to_f64(other)));
            }
            if self.is_string_like(other) {
                let s = self.flatten_cons_to_string(other);
                return Ok(matches!(string_to_bigint(&s), Some(y) if big == y));
            }
            return Ok(false);
        }

        // object vs primitive: unwrap wrapper only when the OTHER side is primitive
        // (object == object compares references, not values)
        if a.is_object() && !b.is_object() {
            let pa = self.try_coerce_to_primitive_hint(a, "default")?;
            if pa.raw() != a.raw() {
                return self.try_abstract_eq(pa, b);
            }
        }
        if b.is_object() && !a.is_object() {
            let pb = self.try_coerce_to_primitive_hint(b, "default")?;
            if pb.raw() != b.raw() {
                return self.try_abstract_eq(a, pb);
            }
        }

        Ok(false)
    }

    /// Strict equality (===).
    /// Content equality for two string-like values, avoiding allocation on the
    /// hot path. Both args MUST be string-like (caller checks `is_string_like`).
    ///
    /// The decode loop's dominant comparison is `charAt(i) === '%'`: an *inline*
    /// string vs an *interned* literal. Their NaN-box bits differ, so the
    /// fast identity check misses — but we can still compare the inline bytes
    /// against the interned `&str` directly, with no `String` allocation.
    pub(crate) fn str_eq(&self, a: Value, b: Value) -> bool {
        // Identical encoding: inline==inline (same bytes) or interned==interned
        // (interning dedupes, so equal content => equal id => equal bits).
        if a.raw() == b.raw() {
            return true;
        }
        match (a.as_inline_string(), b.as_inline_string()) {
            (Some(x), Some(y)) => x.as_str() == y.as_str(),
            (Some(x), None) => self.value_eq_str(b, x.as_str()),
            (None, Some(y)) => self.value_eq_str(a, y.as_str()),
            (None, None) => {
                // Neither is inline. Two distinct interned ids => distinct content
                // (dedup guarantee; identical bits already handled above). Anything
                // involving a ConsString needs a flattening compare.
                if let (Some(ia), Some(ib)) = (a.as_string_id(), b.as_string_id()) {
                    return ia == ib;
                }
                self.flatten_cons_to_string(a) == self.flatten_cons_to_string(b)
            }
        }
    }

    /// Compare a string-like value against a known `&str` without allocating
    /// when `val` is interned; flattens only for ConsString.
    fn value_eq_str(&self, val: Value, s: &str) -> bool {
        if let Some(id) = val.as_string_id() {
            self.interner.resolve(id) == s
        } else if self.is_cons_string(val) || self.is_flat_string(val) {
            self.flatten_cons_to_string(val) == s
        } else {
            false
        }
    }

    pub(crate) fn strict_eq(&self, a: Value, b: Value) -> bool {
        if a.raw() == b.raw() {
            if a.is_float() {
                let f = a.as_number().unwrap();
                return !f.is_nan();
            }
            return true;
        }
        // Handle int == float comparison: 1 === 1.0 should be true
        if a.is_number() && b.is_number() {
            return a.as_number() == b.as_number();
        }
        // BigInt === BigInt compares by mathematical value; a BigInt is never
        // strictly equal to a value of any other type.
        match (self.as_bigint(a), self.as_bigint(b)) {
            (Some(x), Some(y)) => return x == y,
            (Some(_), None) | (None, Some(_)) => return false,
            (None, None) => {}
        }
        // ConsString equality: compare flattened content
        let a_str = self.is_string_like(a);
        let b_str = self.is_string_like(b);
        if a_str && b_str {
            return self.str_eq(a, b);
        }
        false
    }

    /// Try to parse a string value as a number (for == coercion).
    pub(crate) fn string_to_number(&self, val: Value) -> Option<f64> {
        let inl = val.as_inline_string();
        let raw = if let Some(ref i) = inl {
            i.as_str()
        } else {
            let id = val.as_string_id()?;
            self.interner.resolve(id)
        };
        let s = raw.trim();
        if s.is_empty() {
            return Some(0.0);
        }
        // Handle hex/octal/binary literal strings: "0xff", "0o17", "0b1010".
        if s.len() > 2 {
            let (sign, body) = match s.as_bytes()[0] {
                b'+' => (1.0, &s[1..]),
                b'-' => (-1.0, &s[1..]),
                _ => (1.0, s),
            };
            if body.len() > 2 && body.as_bytes()[0] == b'0' {
                let radix = match body.as_bytes()[1] {
                    b'x' | b'X' => Some(16),
                    b'o' | b'O' => Some(8),
                    b'b' | b'B' => Some(2),
                    _ => None,
                };
                if let Some(r) = radix
                    && let Ok(n) = u64::from_str_radix(&body[2..], r)
                {
                    return Some(sign * n as f64);
                }
            }
        }
        match s {
            "Infinity" | "+Infinity" => return Some(f64::INFINITY),
            "-Infinity" => return Some(f64::NEG_INFINITY),
            _ => {}
        }
        s.parse::<f64>().ok()
    }
}
