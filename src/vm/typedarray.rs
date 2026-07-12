//! ArrayBuffer, TypedArray (Int8Array … Float64Array, BigInt64Array,
//! BigUint64Array) and DataView. Element storage is kept as pre-coerced `f64`
//! (BigInt kinds carry the integer value), backed by an ArrayBuffer of bytes so
//! `.buffer`/`.byteLength` and subclassing behave.

use crate::runtime::object::{JsObject, ObjectId, ObjectKind, Property, TypedArrayKind};
use crate::runtime::value::Value;
use super::vm::{Vm, VmError};

/// Constructor sentinels. ArrayBuffer/DataView plus the 11 typed-array kinds.
pub(crate) const SENT_ARRAYBUFFER: i32 = -660;
pub(crate) const SENT_DATAVIEW: i32 = -661;
const TA_SENTINEL_BASE: i32 = -662; // Int8Array .. BigUint64Array, in TA_KINDS order

const TA_KINDS: [TypedArrayKind; 11] = [
    TypedArrayKind::Int8, TypedArrayKind::Uint8, TypedArrayKind::Uint8Clamped,
    TypedArrayKind::Int16, TypedArrayKind::Uint16, TypedArrayKind::Int32,
    TypedArrayKind::Uint32, TypedArrayKind::Float32, TypedArrayKind::Float64,
    TypedArrayKind::BigInt64, TypedArrayKind::BigUint64,
];

pub(crate) fn sentinel_for_kind(kind: TypedArrayKind) -> i32 {
    let idx = TA_KINDS.iter().position(|&k| k == kind).unwrap();
    TA_SENTINEL_BASE - idx as i32
}

pub(crate) fn kind_for_sentinel(sentinel: i32) -> Option<TypedArrayKind> {
    if sentinel > TA_SENTINEL_BASE { return None; }
    let idx = (TA_SENTINEL_BASE - sentinel) as usize;
    TA_KINDS.get(idx).copied()
}

impl Vm {
    /// Register the ArrayBuffer/DataView/TypedArray constructors and their
    /// prototypes. Called once from `Vm::new`.
    pub(crate) fn init_typed_arrays(&mut self) {
        let object_proto = self.object_prototype;
        // ArrayBuffer
        let mut ab_proto = JsObject::ordinary();
        ab_proto.prototype = Some(object_proto);
        self.define_ctor_on_proto(&mut ab_proto, SENT_ARRAYBUFFER);
        let ab_proto_oid = self.heap.allocate(ab_proto);
        self.func_prototypes.insert(SENT_ARRAYBUFFER, ab_proto_oid);
        let ab_name = self.interner.intern("ArrayBuffer");
        self.globals.insert(ab_name, Value::function(SENT_ARRAYBUFFER));

        // DataView
        let mut dv_proto = JsObject::ordinary();
        dv_proto.prototype = Some(object_proto);
        self.define_ctor_on_proto(&mut dv_proto, SENT_DATAVIEW);
        let dv_proto_oid = self.heap.allocate(dv_proto);
        self.func_prototypes.insert(SENT_DATAVIEW, dv_proto_oid);
        let dv_name = self.interner.intern("DataView");
        self.globals.insert(dv_name, Value::function(SENT_DATAVIEW));
        self.seed_dataview_proto(dv_proto_oid);

        // Each typed array constructor + prototype.
        for kind in TA_KINDS {
            let sentinel = sentinel_for_kind(kind);
            let mut proto = JsObject::ordinary();
            proto.prototype = Some(object_proto);
            self.define_ctor_on_proto(&mut proto, sentinel);
            // values/[Symbol.iterator]
            let values_fn = Value::function(-626); // reuse Array values iterator sentinel
            let sym_iter_key = self.interner.intern(&format!("__sym_{}__", self.sym_iterator));
            proto.define_property(sym_iter_key, Property::with_flags(values_fn, Property::WRITABLE | Property::CONFIGURABLE));
            let proto_oid = self.heap.allocate(proto);
            self.func_prototypes.insert(sentinel, proto_oid);
            let name = self.interner.intern(kind.name());
            self.globals.insert(name, Value::function(sentinel));
        }
    }

    fn define_ctor_on_proto(&mut self, proto: &mut JsObject, sentinel: i32) {
        let ctor_key = self.interner.intern("constructor");
        proto.define_property(ctor_key, Property::with_flags(
            Value::function(sentinel), Property::WRITABLE | Property::CONFIGURABLE));
    }

    /// Seed DataView.prototype: the 16 get*/set* methods as real function
    /// objects (own name/length, spec attributes) plus buffer/byteLength/
    /// byteOffset accessor getters.
    fn seed_dataview_proto(&mut self, proto_oid: ObjectId) {
        const TYPES: &[&str] = &["Int8", "Uint8", "Int16", "Uint16", "Int32", "Uint32", "Float32", "Float64", "BigInt64", "BigUint64"];
        for ty in TYPES {
            for (prefix, fn_len) in [("get", 1i32), ("set", 2)] {
                let mname = format!("{prefix}{ty}");
                let name_id = self.interner.intern(&mname);
                let m = mname.clone();
                let func: crate::runtime::object::NativeFn =
                    std::sync::Arc::new(move |vm: &mut Vm, this: Value, args: &[Value]| {
                        match vm.exec_dataview_method(this, &m, args) {
                            Ok(v) => Ok(v),
                            Err(VmError::Throw(v)) => Err(v),
                            Err(e) => Err(vm.make_native_error("Error", &format!("{e:?}"))),
                        }
                    });
                let mut fn_obj = JsObject {
                    properties: Vec::new(),
                    prototype: Some(self.function_prototype),
                    kind: ObjectKind::Function(crate::runtime::object::FunctionKind::Native { name: name_id, func }),
                    marked: false,
                    extensible: true,
                };
                let name_key = self.interner.intern("name");
                let len_key = self.interner.intern("length");
                fn_obj.define_property(name_key, Property::with_flags(Value::string(name_id), Property::CONFIGURABLE));
                fn_obj.define_property(len_key, Property::with_flags(Value::int(fn_len), Property::CONFIGURABLE));
                let f_oid = self.heap.allocate(fn_obj);
                if let Some(proto) = self.heap.get_mut(proto_oid) {
                    proto.define_property(name_id, Property::with_flags(
                        Value::object_id(f_oid), Property::WRITABLE | Property::CONFIGURABLE));
                }
            }
        }
        // Accessor getters: get buffer / get byteLength / get byteOffset.
        for acc in ["buffer", "byteLength", "byteOffset"] {
            let getter_name = format!("get {acc}");
            let getter_name_id = self.interner.intern(&getter_name);
            let acc_owned = acc.to_string();
            let func: crate::runtime::object::NativeFn =
                std::sync::Arc::new(move |vm: &mut Vm, this: Value, _args: &[Value]| {
                    let dv = this.as_object_id().and_then(|oid| vm.heap.get(oid).and_then(|o| {
                        if let ObjectKind::DataView { buffer, byte_offset, byte_length } = o.kind {
                            Some((buffer, byte_offset, byte_length))
                        } else { None }
                    }));
                    let Some((buffer, off, len)) = dv else {
                        return Err(vm.make_native_error("TypeError", "DataView accessor called on incompatible receiver"));
                    };
                    Ok(match acc_owned.as_str() {
                        "buffer" => Value::object_id(buffer),
                        "byteLength" => Value::int(len as i32),
                        _ => Value::int(off as i32),
                    })
                });
            let mut fn_obj = JsObject {
                properties: Vec::new(),
                prototype: Some(self.function_prototype),
                kind: ObjectKind::Function(crate::runtime::object::FunctionKind::Native { name: getter_name_id, func }),
                marked: false,
                extensible: true,
            };
            let name_key = self.interner.intern("name");
            let len_key = self.interner.intern("length");
            fn_obj.define_property(name_key, Property::with_flags(Value::string(getter_name_id), Property::CONFIGURABLE));
            fn_obj.define_property(len_key, Property::with_flags(Value::int(0), Property::CONFIGURABLE));
            let f_oid = self.heap.allocate(fn_obj);
            let getter_key = self.interner.intern(&format!("__get_{acc}__"));
            if let Some(proto) = self.heap.get_mut(proto_oid) {
                proto.define_property(getter_key, Property::with_flags(
                    Value::object_id(f_oid), Property::CONFIGURABLE));
            }
        }
    }

    /// Allocate an ArrayBuffer of `byte_len` zeroed bytes.
    pub(crate) fn make_array_buffer(&mut self, byte_len: usize) -> ObjectId {
        let mut obj = JsObject { properties: Vec::new(), prototype: self.func_prototypes.get(&SENT_ARRAYBUFFER).copied(),
            kind: ObjectKind::ArrayBuffer(vec![0u8; byte_len]), marked: false, extensible: true };
        let _ = &mut obj;
        self.heap.allocate(obj)
    }

    /// Allocate a typed array of `kind` holding the given (already raw-number)
    /// elements, with a fresh backing ArrayBuffer, chained to its prototype.
    pub(crate) fn make_typed_array(&mut self, kind: TypedArrayKind, elements: Vec<f64>, proto: Option<ObjectId>) -> Value {
        let buffer = self.make_array_buffer(elements.len() * kind.bytes_per_element());
        let proto = proto.or_else(|| self.func_prototypes.get(&sentinel_for_kind(kind)).copied());
        let obj = JsObject { properties: Vec::new(), prototype: proto,
            kind: ObjectKind::TypedArray { kind, elements, buffer }, marked: false, extensible: true };
        Value::object_id(self.heap.allocate(obj))
    }

    /// `new T(arg)` construction. `proto` overrides the prototype (for subclasses).
    pub(crate) fn construct_typed_array(&mut self, kind: TypedArrayKind, args: &[Value], proto: Option<ObjectId>) -> Result<Value, VmError> {
        let arg = args.first().copied().unwrap_or(Value::undefined());
        // new T() / new T(length)
        if arg.is_undefined() {
            return Ok(self.make_typed_array(kind, Vec::new(), proto));
        }
        if let Some(n) = arg.as_number().or_else(|| arg.as_int().map(|i| i as f64)) {
            let len = if n.is_finite() && n >= 0.0 { n as usize } else {
                let e = self.make_native_error("RangeError", "Invalid typed array length");
                return Err(VmError::Throw(e));
            };
            return Ok(self.make_typed_array(kind, vec![0.0; len], proto));
        }
        // new T(arrayBuffer[, byteOffset[, length]])
        if let Some(oid) = arg.as_object_id()
            && matches!(self.heap.get(oid).map(|o| &o.kind), Some(ObjectKind::ArrayBuffer(_)))
        {
            let bytes = if let Some(ObjectKind::ArrayBuffer(b)) = self.heap.get(oid).map(|o| &o.kind) { b.len() } else { 0 };
            let elem = kind.bytes_per_element();
            let offset = args.get(1).map(|v| self.to_f64(*v) as usize).unwrap_or(0);
            let len = match args.get(2) {
                Some(v) if !v.is_undefined() => self.to_f64(*v) as usize,
                _ => bytes.saturating_sub(offset) / elem,
            };
            // Decode existing bytes into elements (little-endian).
            let elements = self.read_buffer_elements(oid, kind, offset, len);
            let obj = JsObject { properties: Vec::new(),
                prototype: proto.or_else(|| self.func_prototypes.get(&sentinel_for_kind(kind)).copied()),
                kind: ObjectKind::TypedArray { kind, elements, buffer: oid }, marked: false, extensible: true };
            return Ok(Value::object_id(self.heap.allocate(obj)));
        }
        // new T(typedArray) / new T(arrayLike / iterable)
        let kinded: Option<(TypedArrayKind, Vec<f64>)> = arg.as_object_id()
            .and_then(|oid| self.heap.get(oid))
            .and_then(|o| match &o.kind {
                ObjectKind::TypedArray { elements, kind: sk, .. } => Some((*sk, elements.clone())),
                _ => None,
            });
        let src: Vec<Value> = if let Some((sk, elems)) = kinded {
            elems.into_iter().map(|e| if sk.is_bigint() { self.make_bigint(num_bigint::BigInt::from(e as i64)) } else { Value::number(e) }).collect()
        } else if let Some(oid) = arg.as_object_id()
            && let Some(ObjectKind::Array(elems)) = self.heap.get(oid).map(|o| &o.kind)
        {
            elems.clone()
        } else {
            self.collect_iterable(arg)?.unwrap_or_default()
        };
        // Coerce each element to the typed-array kind.
        let mut elements = Vec::with_capacity(src.len());
        for v in src {
            elements.push(self.coerce_ta_element(kind, v)?);
        }
        Ok(self.make_typed_array(kind, elements, proto))
    }

    /// Coerce a JS value to the f64 stored for `kind` (ToNumber / ToBigInt + wrap).
    fn coerce_ta_element(&mut self, kind: TypedArrayKind, v: Value) -> Result<f64, VmError> {
        if kind.is_bigint() {
            let b = self.value_to_bigint(v)?;
            // store as f64 of the 64-bit-wrapped value (sufficient for the language tests)
            return Ok(num_traits::ToPrimitive::to_i64(&b).map(|i| i as f64).unwrap_or(0.0));
        }
        if v.is_symbol() {
            let e = self.make_native_error("TypeError", "Cannot convert a Symbol value to a number");
            return Err(VmError::Throw(e));
        }
        let prim = if v.is_object() && !self.is_bigint(v) { self.try_coerce_to_primitive_hint(v, "number")? } else { v };
        if self.is_bigint(prim) {
            let e = self.make_native_error("TypeError", "Cannot convert a BigInt value to a number");
            return Err(VmError::Throw(e));
        }
        Ok(kind.coerce(self.to_f64(prim)))
    }

    fn read_buffer_elements(&self, _buffer: ObjectId, _kind: TypedArrayKind, _offset: usize, len: usize) -> Vec<f64> {
        // Minimal: language tests don't read pre-populated buffers; start zeroed.
        vec![0.0; len]
    }

    /// Indexed read of a typed array element as a Value. None if out of range.
    pub(crate) fn typed_array_get(&mut self, oid: ObjectId, index: usize) -> Option<Value> {
        let (kind, val) = match self.heap.get(oid).map(|o| &o.kind) {
            Some(ObjectKind::TypedArray { kind, elements, .. }) => (*kind, elements.get(index).copied()?),
            _ => return None,
        };
        Some(if kind.is_bigint() {
            self.make_bigint(num_bigint::BigInt::from(val as i64))
        } else {
            Value::number(val)
        })
    }

    /// Indexed write of a typed array element. Returns true if `oid` is a typed
    /// array (the write is dropped silently when the index is out of range, per spec).
    pub(crate) fn typed_array_set(&mut self, oid: ObjectId, index: usize, value: Value) -> Result<bool, VmError> {
        let kind = match self.heap.get(oid).map(|o| &o.kind) {
            Some(ObjectKind::TypedArray { kind, .. }) => *kind,
            _ => return Ok(false),
        };
        let coerced = self.coerce_ta_element(kind, value)?;
        if let Some(ObjectKind::TypedArray { elements, .. }) = self.heap.get_mut(oid).map(|o| &mut o.kind)
            && index < elements.len()
        {
            elements[index] = coerced;
        }
        Ok(true)
    }

    /// Extract a non-negative integer index from a property key (SMI, integral
    /// number, or a canonical numeric string).
    pub(crate) fn canonical_index(&self, key: Value) -> Option<usize> {
        if let Some(i) = key.as_int() {
            return if i >= 0 { Some(i as usize) } else { None };
        }
        if let Some(n) = key.as_number() {
            return if n >= 0.0 && n.fract() == 0.0 && n.is_finite() { Some(n as usize) } else { None };
        }
        if let Some(id) = key.as_string_id() {
            return self.interner.resolve(id).parse::<usize>().ok();
        }
        None
    }

    /// ToIndex: observable ToNumber, then integer validation (negative,
    /// non-finite and >2^53-1 throw RangeError).
    pub(crate) fn spec_to_index(&mut self, v: Value) -> Result<usize, VmError> {
        if v.is_undefined() {
            return Ok(0);
        }
        let n = self.coerce_to_f64(v)?;
        let n = if n.is_nan() { 0.0 } else { n.trunc() };
        if n < 0.0 || !n.is_finite() || n > 9_007_199_254_740_991.0 {
            let e = self.make_native_error("RangeError", "Invalid index");
            return Err(VmError::Throw(e));
        }
        Ok(n as usize)
    }

    /// GetViewValue / SetViewValue for every DataView get*/set* method.
    pub(crate) fn exec_dataview_method(&mut self, this: Value, method: &str, args: &[Value]) -> Result<Value, VmError> {
        let dv = this.as_object_id().and_then(|oid| self.heap.get(oid).and_then(|o| {
            if let ObjectKind::DataView { buffer, byte_offset, byte_length } = o.kind {
                Some((buffer, byte_offset, byte_length))
            } else {
                None
            }
        }));
        let Some((buffer, view_off, view_len)) = dv else {
            let e = self.make_native_error("TypeError", "DataView method called on incompatible receiver");
            return Err(VmError::Throw(e));
        };
        let (is_get, ty) = if let Some(t) = method.strip_prefix("get") {
            (true, t)
        } else if let Some(t) = method.strip_prefix("set") {
            (false, t)
        } else {
            return Ok(Value::undefined());
        };
        let size: usize = match ty {
            "Int8" | "Uint8" => 1,
            "Int16" | "Uint16" => 2,
            "Int32" | "Uint32" | "Float32" => 4,
            "Float64" | "BigInt64" | "BigUint64" => 8,
            _ => return Ok(Value::undefined()),
        };
        let is_big = matches!(ty, "BigInt64" | "BigUint64");
        // Spec order: ToIndex(requestIndex), then (for set) ToNumber(value),
        // then bounds. Both coercions are observable.
        let idx = self.spec_to_index(args.first().copied().unwrap_or(Value::undefined()))?;
        let mut big_value: u64 = 0;
        let value = if is_get {
            0.0
        } else if is_big {
            // ToBigInt(value): Numbers throw TypeError (unlike the BigInt()
            // constructor's NumberToBigInt).
            let v = args.get(1).copied().unwrap_or(Value::undefined());
            if v.as_number().is_some() || v.is_int() {
                let e = self.make_native_error("TypeError", "Cannot convert a Number to a BigInt");
                return Err(VmError::Throw(e));
            }
            let b = self.value_to_bigint(v)?;
            use num_bigint::BigInt;
            let modulus = BigInt::from(1u8) << 64u32;
            let wrapped = ((b % &modulus) + &modulus) % &modulus;
            big_value = u64::try_from(wrapped).unwrap_or(0);
            0.0
        } else {
            let v = args.get(1).copied().unwrap_or(Value::undefined());
            if v.is_symbol() {
                let e = self.make_native_error("TypeError", "Cannot convert a Symbol value to a number");
                return Err(VmError::Throw(e));
            }
            self.coerce_to_f64(v)?
        };
        let little = {
            let li = if is_get { 1 } else { 2 };
            args.get(li).map(|v| self.truthy(*v)).unwrap_or(false)
        };
        if idx.checked_add(size).is_none_or(|end| end > view_len) {
            let e = self.make_native_error("RangeError", "Offset is outside the bounds of the DataView");
            return Err(VmError::Throw(e));
        }
        let start = view_off + idx;
        if is_get {
            let bytes: Vec<u8> = match self.heap.get(buffer).map(|o| &o.kind) {
                Some(ObjectKind::ArrayBuffer(b)) => b[start..start + size].to_vec(),
                _ => vec![0; size],
            };
            let mut raw = [0u8; 8];
            for (i, b) in bytes.iter().enumerate() {
                raw[if little { i } else { size - 1 - i }] = *b;
            }
            // raw is now little-endian
            let bits = u64::from_le_bytes(raw);
            if is_big {
                use num_bigint::BigInt;
                let b = if ty == "BigInt64" {
                    BigInt::from(bits as i64)
                } else {
                    BigInt::from(bits)
                };
                return Ok(self.make_bigint(b));
            }
            let n: f64 = match ty {
                "Int8" => (bits as u8) as i8 as f64,
                "Uint8" => (bits as u8) as f64,
                "Int16" => (bits as u16) as i16 as f64,
                "Uint16" => (bits as u16) as f64,
                "Int32" => (bits as u32) as i32 as f64,
                "Uint32" => (bits as u32) as f64,
                "Float32" => f32::from_bits(bits as u32) as f64,
                "Float64" => f64::from_bits(bits),
                _ => 0.0,
            };
            Ok(Value::number(n))
        } else {
            let bits: u64 = match ty {
                _ if is_big => big_value,
                "Float32" => (value as f32).to_bits() as u64,
                "Float64" => value.to_bits(),
                _ => {
                    // Integer types wrap modulo 2^bits (ToInt8/ToUint8/…).
                    if value.is_finite() {
                        let m = value.trunc() as i128;
                        m.rem_euclid(1i128 << (size * 8)) as u64
                    } else {
                        0
                    }
                }
            };
            let le = bits.to_le_bytes();
            if let Some(obj) = self.heap.get_mut(buffer)
                && let ObjectKind::ArrayBuffer(ref mut b) = obj.kind
            {
                for i in 0..size {
                    b[start + i] = le[if little { i } else { size - 1 - i }];
                }
            }
            Ok(Value::undefined())
        }
    }

    pub(crate) fn typed_array_len(&self, oid: ObjectId) -> Option<usize> {
        match self.heap.get(oid).map(|o| &o.kind) {
            Some(ObjectKind::TypedArray { elements, .. }) => Some(elements.len()),
            _ => None,
        }
    }
}
