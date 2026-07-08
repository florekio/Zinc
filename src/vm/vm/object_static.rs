//! The `Object.*` static methods (keys, assign, defineProperty, freeze,
//! …) and function-object property reads shared by several dispatch paths.

use super::*;

impl Vm {
    /// Property GET of an Object static (`Object.create`, `Object.keys`, …)
    /// as a callable value: lazily wraps exec_object_static in a NativeFn,
    /// cached in fn_property_overrides for stable identity. Without this,
    /// `Object.create || shim` feature detection reads undefined — core-js
    /// then activates its ancient-IE iframe shim and dies (DuckDuckGo's
    /// polyfills bundle did exactly that).
    pub(crate) fn object_static_callable(&mut self, name_id: StringId) -> Option<Value> {
        let name_str = self.interner.resolve(name_id).to_owned();
        if !Self::OBJECT_STATIC_NAMES.contains(&name_str.as_str()) {
            return None;
        }
        let func: crate::runtime::object::NativeFn = std::sync::Arc::new(
            move |vm: &mut Vm, _this: Value, args: &[Value]| -> Result<Value, Value> {
                match vm.exec_object_static(&name_str, args) {
                    Ok(v) => Ok(v.unwrap_or(Value::undefined())),
                    Err(VmError::Throw(v)) => Err(v),
                    Err(e) => {
                        let msg = format!("{e:?}");
                        Err(vm.make_native_error("Error", &msg))
                    }
                }
            },
        );
        let fn_obj = JsObject {
            properties: Vec::new(),
            prototype: None,
            kind: ObjectKind::Function(crate::runtime::object::FunctionKind::Native { name: name_id, func }),
            marked: false,
            extensible: true,
        };
        let oid = self.heap.allocate(fn_obj);
        let val = Value::object_id(oid);
        self.fn_property_overrides.insert((-508, name_id), Some(val));
        Some(val)
    }

    pub(crate) fn exec_object_static(&mut self, mn: &str, args: &[Value]) -> Result<Option<Value>, VmError> {
        let result = match mn {
            "keys" => {
                // Function values keep their own properties in
                // fn_property_overrides — webpack's runtime does
                // `Object.keys(a.O)` where a.O is a function used as a
                // registry of chunk-loading handlers.
                if let Some(sentinel) = args.first().and_then(|v| v.as_function()) {
                    let key_ids: Vec<StringId> = self
                        .fn_property_overrides
                        .iter()
                        .filter(|((s, _), v)| *s == sentinel && v.is_some())
                        .map(|((_, k), _)| *k)
                        .collect();
                    // HashMap iteration order is arbitrary — sort by name
                    // for deterministic output.
                    let mut names: Vec<String> = key_ids
                        .iter()
                        .map(|k| self.interner.resolve(*k).to_owned())
                        .collect();
                    names.sort();
                    let values: Vec<Value> = names
                        .iter()
                        .map(|n| Value::string(self.interner.intern(n)))
                        .collect();
                    self.alloc_array(values)
                } else if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
                    let is_array = self.heap.get(oid).map(|o| matches!(&o.kind, ObjectKind::Array(_))).unwrap_or(false);
                    if is_array {
                        let len = self.heap.get(oid).and_then(|o| if let ObjectKind::Array(ref e) = o.kind { Some(e.len()) } else { None }).unwrap_or(0);
                        let keys: Vec<Value> = (0..len).map(|i| {
                            let s = self.interner.intern(&i.to_string());
                            Value::string(s)
                        }).collect();
                        self.alloc_array(keys)
                    } else {
                        let props: Vec<(StringId, bool)> = self.heap.get(oid)
                            .map(|o| o.properties.iter().map(|&(k, ref p)| (k, p.is_enumerable())).collect())
                            .unwrap_or_default();
                        let mut numeric: Vec<(u64, StringId)> = Vec::new();
                        let mut string: Vec<StringId> = Vec::new();
                        let mut seen: std::collections::HashSet<StringId> = std::collections::HashSet::new();
                        for (k, en) in props {
                            if !en { continue; }
                            // Accessor properties are stored under
                            // __get_NAME__ / __set_NAME__ — surface the
                            // bare NAME and dedupe so a paired getter/setter
                            // produces one entry.
                            let accessor_inner = {
                                let name = self.interner.resolve(k);
                                if let Some(rest) = name.strip_prefix("__get_").and_then(|s| s.strip_suffix("__")) {
                                    Some(rest.to_owned())
                                } else if let Some(rest) = name.strip_prefix("__set_").and_then(|s| s.strip_suffix("__")) {
                                    Some(rest.to_owned())
                                } else if is_internal_key(name) {
                                    continue;
                                } else {
                                    None
                                }
                            };
                            let key = match accessor_inner {
                                Some(s) => self.interner.intern(&s),
                                None => k,
                            };
                            if !seen.insert(key) { continue; }
                            let name = self.interner.resolve(key);
                            if let Ok(n) = name.parse::<u64>() {
                                numeric.push((n, key));
                            } else {
                                string.push(key);
                            }
                        }
                        numeric.sort_by_key(|&(n, _)| n);
                        let mut keys: Vec<Value> = numeric.into_iter().map(|(_, k)| Value::string(k)).collect();
                        keys.extend(string.into_iter().map(Value::string));
                        self.alloc_array(keys)
                    }
                } else { Value::undefined() }
            }
            "values" => {
                if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
                    let vals: Vec<Value> = self.heap.get(oid)
                        .map(|o| {
                            if let ObjectKind::Array(ref elems) = o.kind {
                                elems.clone()
                            } else {
                                o.properties.iter()
                                    .filter(|(k, p)| p.is_enumerable()
                                        && !is_internal_key(self.interner.resolve(*k)))
                                    .map(|(_, p)| p.value).collect()
                            }
                        })
                        .unwrap_or_default();
                    self.alloc_array(vals)
                } else { Value::undefined() }
            }
            "entries" => {
                if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
                    let pairs: Vec<(Value, Value)> = self.heap.get(oid)
                        .map(|o| o.properties.iter()
                            .filter(|(k, p)| p.is_enumerable()
                                && !is_internal_key(self.interner.resolve(*k)))
                            .map(|&(k, ref p)| (Value::string(k), p.value)).collect())
                        .unwrap_or_default();
                    let mut entries = Vec::new();
                    for (k, v) in pairs {
                        let pair = self.alloc_array(vec![k, v]);
                        entries.push(pair);
                    }
                    self.alloc_array(entries)
                } else { Value::undefined() }
            }
            "assign" => self.exec_object_assign(args),
            "create" => {
                let proto = args.first().copied().unwrap_or(Value::null());
                let mut obj = JsObject::ordinary();
                obj.prototype = proto.as_object_id();
                // Handle property descriptors argument (2nd arg)
                if let Some(desc_val) = args.get(1)
                    && let Some(desc_oid) = desc_val.as_object_id()
                {
                    let props: Vec<(StringId, Value)> = self.heap.get(desc_oid)
                        .map(|o| o.properties.iter().map(|&(k, ref p)| (k, p.value)).collect())
                        .unwrap_or_default();
                    for (key, desc_obj_val) in props {
                        if let Some(d_oid) = desc_obj_val.as_object_id() {
                            let value_key = self.interner.intern("value");
                            let val = self.heap.get_property_chain(d_oid, value_key)
                                .unwrap_or(Value::undefined());
                            obj.set_property(key, val);
                        }
                    }
                }
                Value::object_id(self.heap.allocate(obj))
            }
            "defineProperty" => self.object_define_property(args),
            "defineProperties" => {
                // Simplified: treat like Object.assign for now
                args.first().copied().unwrap_or(Value::undefined())
            }
            "getOwnPropertyDescriptor" => {
                let first_arg = args.first().copied().unwrap_or(Value::undefined());
                let key_arg = args.get(1).copied().unwrap_or(Value::undefined());
                // For symbol keys, use __sym_N__ encoding; otherwise stringify
                let key_str = if key_arg.is_symbol() {
                    format!("__sym_{}__", key_arg.as_symbol_id().unwrap())
                } else {
                    self.value_to_string(key_arg)
                };
                // Function values keep user-set properties in
                // fn_property_overrides — core-js inspects descriptors of
                // well-known symbols it installed on the Symbol
                // constructor (`getOwnPropertyDescriptor(Symbol,
                // 'asyncDispose').enumerable`).
                if let Some(sentinel) = first_arg.as_function() {
                    let key_id = self.interner.intern(&key_str);
                    let deleted = matches!(self.fn_property_overrides.get(&(sentinel, key_id)), Some(None));
                    let (value, intrinsic) = if let Some(Some(v)) = self.fn_property_overrides.get(&(sentinel, key_id)).copied() {
                        (Some(v), false)
                    } else if deleted {
                        (None, false)
                    } else {
                        // Intrinsic name / length descriptors (spec flags:
                        // writable false, enumerable false, configurable true).
                        (self.fn_get_own_prop(sentinel, key_id), true)
                    };
                    if let Some(v) = value {
                        let mut desc = JsObject::ordinary();
                        desc.prototype = Some(self.object_prototype);
                        let value_key = self.interner.intern("value");
                        let writable_key = self.interner.intern("writable");
                        let enumerable_key = self.interner.intern("enumerable");
                        let configurable_key = self.interner.intern("configurable");
                        // `name`/`length` keep their spec flags (writable
                        // false, enumerable false, configurable true) even
                        // when the VALUE came from an override (e.g.
                        // SetFunctionName on a symbol-keyed function).
                        let spec_flags = intrinsic || key_str == "name" || key_str == "length";
                        desc.set_property(value_key, v);
                        desc.set_property(writable_key, Value::boolean(!spec_flags));
                        desc.set_property(enumerable_key, Value::boolean(!spec_flags));
                        desc.set_property(configurable_key, Value::boolean(true));
                        return Ok(Some(Value::object_id(self.heap.allocate(desc))));
                    }
                    return Ok(Some(Value::undefined()));
                }
                if let Some(oid) = first_arg.as_object_id() {
                    let key_id = self.interner.intern(&key_str);
                    // Check for accessor properties first
                    let getter_key_str = format!("__get_{key_str}__");
                    let setter_key_str = format!("__set_{key_str}__");
                    let getter_key = self.interner.intern(&getter_key_str);
                    let setter_key = self.interner.intern(&setter_key_str);
                    let getter_desc = self.heap.get(oid)
                        .and_then(|o| o.get_property_descriptor(getter_key));
                    let setter_desc = self.heap.get(oid)
                        .and_then(|o| o.get_property_descriptor(setter_key));
                    if getter_desc.is_some() || setter_desc.is_some() {
                        let mut desc = JsObject::ordinary();
                        let get_key = self.interner.intern("get");
                        let set_key = self.interner.intern("set");
                        let en_key = self.interner.intern("enumerable");
                        let cf_key = self.interner.intern("configurable");
                        let getter_v = getter_desc.map(|p| p.value).unwrap_or(Value::undefined());
                        let setter_v = setter_desc.map(|p| p.value).unwrap_or(Value::undefined());
                        // Pull the conceptual descriptor's enumerable/configurable
                        // from the getter's slot (or the setter's if no getter).
                        let flags_src = getter_desc.or(setter_desc).unwrap();
                        desc.set_property(get_key, getter_v);
                        desc.set_property(set_key, setter_v);
                        desc.set_property(en_key, Value::boolean(flags_src.is_enumerable()));
                        desc.set_property(cf_key, Value::boolean(flags_src.is_configurable()));
                        Value::object_id(self.heap.allocate(desc))
                    } else if let Some(obj) = self.heap.get(oid)
                        && let Some(prop) = obj.get_property_descriptor(key_id) {
                            let mut desc = JsObject::ordinary();
                            let val_key = self.interner.intern("value");
                            let wr_key = self.interner.intern("writable");
                            let en_key = self.interner.intern("enumerable");
                            let cf_key = self.interner.intern("configurable");
                            desc.set_property(val_key, prop.value);
                            desc.set_property(wr_key, Value::boolean(prop.is_writable()));
                            desc.set_property(en_key, Value::boolean(prop.is_enumerable()));
                            desc.set_property(cf_key, Value::boolean(prop.is_configurable()));
                            Value::object_id(self.heap.allocate(desc))
                        } else if let Some(arr_info) = self.heap.get(oid).and_then(|o| {
                            // Array index "0", "1", … and "length" descriptors.
                            if let ObjectKind::Array(ref e) = o.kind {
                                Some((e.clone(), e.len()))
                            } else { None }
                        }) {
                            let (elements, len) = arr_info;
                            let val_key = self.interner.intern("value");
                            let wr_key = self.interner.intern("writable");
                            let en_key = self.interner.intern("enumerable");
                            let cf_key = self.interner.intern("configurable");
                            if key_str == "length" {
                                let mut desc = JsObject::ordinary();
                                desc.set_property(val_key, Value::int(len as i32));
                                desc.set_property(wr_key, Value::boolean(true));
                                desc.set_property(en_key, Value::boolean(false));
                                desc.set_property(cf_key, Value::boolean(false));
                                Value::object_id(self.heap.allocate(desc))
                            } else if let Ok(idx) = key_str.parse::<usize>() {
                                if idx < len {
                                    let mut desc = JsObject::ordinary();
                                    desc.set_property(val_key, elements[idx]);
                                    desc.set_property(wr_key, Value::boolean(true));
                                    desc.set_property(en_key, Value::boolean(true));
                                    desc.set_property(cf_key, Value::boolean(true));
                                    Value::object_id(self.heap.allocate(desc))
                                } else { Value::undefined() }
                            } else { Value::undefined() }
                        } else { Value::undefined() }
                } else if first_arg.is_function() {
                    let sentinel = first_arg.as_function().unwrap();
                    let key_id = self.interner.intern(&key_str);
                    let prop_val = if matches!(key_str.as_str(), "name" | "length") {
                        self.fn_get_own_prop(sentinel, key_id)
                            .map(|v| (v, false, false, true))
                    } else if key_str == "prototype" {
                        let proto_val = if let Some(&proto_oid) = self.func_prototypes.get(&sentinel) {
                            Value::object_id(proto_oid)
                        } else {
                            let mut proto = JsObject::ordinary();
                            proto.prototype = Some(self.object_prototype);
                            // Generator functions get a plain empty prototype.
                            let chunk_idx = (sentinel & 0xFFFF) as usize;
                            let is_gen = chunk_idx < self.chunks.len()
                                && self.chunks[chunk_idx].flags.contains(ChunkFlags::GENERATOR);
                            if !is_gen {
                                let ctor_key = self.interner.intern("constructor");
                                proto.define_property(ctor_key, Property::with_flags(
                                    first_arg, Property::WRITABLE | Property::CONFIGURABLE
                                ));
                            }
                            let proto_oid = self.heap.allocate(proto);
                            self.func_prototypes.insert(sentinel, proto_oid);
                            Value::object_id(proto_oid)
                        };
                        Some((proto_val, true, false, false))
                    } else { None };
                    if let Some((val, writable, enumerable, configurable)) = prop_val {
                        let mut desc = JsObject::ordinary();
                        let val_key = self.interner.intern("value");
                        let wr_key = self.interner.intern("writable");
                        let en_key = self.interner.intern("enumerable");
                        let cf_key = self.interner.intern("configurable");
                        desc.set_property(val_key, val);
                        desc.set_property(wr_key, Value::boolean(writable));
                        desc.set_property(en_key, Value::boolean(enumerable));
                        desc.set_property(cf_key, Value::boolean(configurable));
                        Value::object_id(self.heap.allocate(desc))
                    } else { Value::undefined() }
                } else { Value::undefined() }
            }
            "getOwnPropertyNames" => {
                // Native function values (Array, RegExp, etc.) expose
                // their three standard own props: length, name, prototype.
                if let Some(fv) = args.first() && fv.is_function() {
                    let mut names: Vec<Value> = Vec::new();
                    for n in ["length", "name", "prototype"] {
                        let id = self.interner.intern(n);
                        names.push(Value::string(id));
                    }
                    self.alloc_array(names)
                } else if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
                    let mut seen = std::collections::HashSet::new();
                    let mut names: Vec<Value> = Vec::new();
                    // For arrays, integer indices come first in numeric order,
                    // then the `length` property, then named own properties.
                    let array_len = self.heap.get(oid).and_then(|o| {
                        if let ObjectKind::Array(ref e) = o.kind {
                            Some(e.len())
                        } else {
                            None
                        }
                    });
                    if let Some(len) = array_len {
                        for i in 0..len {
                            let s = i.to_string();
                            let id = self.interner.intern(&s);
                            names.push(Value::string(id));
                            seen.insert(s);
                        }
                        let length_str = "length".to_string();
                        if seen.insert(length_str.clone()) {
                            let id = self.interner.intern("length");
                            names.push(Value::string(id));
                        }
                    }
                    let raw_props: Vec<StringId> = self.heap.get(oid)
                        .map(|o| o.properties.iter().map(|(k, _)| *k).collect())
                        .unwrap_or_default();
                    // Per spec OrdinaryOwnPropertyKeys: integer indices
                    // come first in ascending numeric order, then the
                    // remaining string keys in insertion order.
                    let mut numeric: Vec<(u32, String)> = Vec::new();
                    let mut string_keys: Vec<String> = Vec::new();
                    for k in raw_props {
                        let s = self.interner.resolve(k).to_owned();
                        let real = if (s.starts_with("__get_") || s.starts_with("__set_")) && s.ends_with("__") {
                            s[6..s.len()-2].to_owned()
                        } else if is_internal_key(&s) {
                            continue;
                        } else {
                            s
                        };
                        if !seen.insert(real.clone()) { continue; }
                        if let Ok(n) = real.parse::<u32>()
                            && n.to_string() == real
                        {
                            numeric.push((n, real));
                        } else {
                            string_keys.push(real);
                        }
                    }
                    numeric.sort_by_key(|&(n, _)| n);
                    for (_, s) in numeric {
                        let id = self.interner.intern(&s);
                        names.push(Value::string(id));
                    }
                    for s in string_keys {
                        let id = self.interner.intern(&s);
                        names.push(Value::string(id));
                    }
                    self.alloc_array(names)
                } else { Value::undefined() }
            }
            "getOwnPropertyDescriptors" => {
                if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
                    let props: Vec<(StringId, Property)> = self.heap.get(oid)
                        .map(|o| o.properties.iter().map(|(k, p)| (*k, *p)).collect())
                        .unwrap_or_default();
                    let val_key = self.interner.intern("value");
                    let wr_key = self.interner.intern("writable");
                    let en_key = self.interner.intern("enumerable");
                    let cf_key = self.interner.intern("configurable");
                    let mut result = JsObject::ordinary();
                    for (k, prop) in props {
                        let mut desc = JsObject::ordinary();
                        desc.set_property(val_key, prop.value);
                        desc.set_property(wr_key, Value::boolean(prop.is_writable()));
                        desc.set_property(en_key, Value::boolean(prop.is_enumerable()));
                        desc.set_property(cf_key, Value::boolean(prop.is_configurable()));
                        let desc_oid = self.heap.allocate(desc);
                        result.set_property(k, Value::object_id(desc_oid));
                    }
                    Value::object_id(self.heap.allocate(result))
                } else { Value::undefined() }
            }
            "freeze" => {
                let target = args.first().copied().unwrap_or(Value::undefined());
                if let Some(oid) = target.as_object_id()
                    && let Some(obj) = self.heap.get_mut(oid) {
                        obj.extensible = false;
                        for entry in &mut obj.properties {
                            entry.1.flags &= !(Property::WRITABLE | Property::CONFIGURABLE);
                        }
                    }
                target
            }
            "seal" => {
                let target = args.first().copied().unwrap_or(Value::undefined());
                if let Some(oid) = target.as_object_id()
                    && let Some(obj) = self.heap.get_mut(oid) {
                        obj.extensible = false;
                        for entry in &mut obj.properties {
                            entry.1.flags &= !Property::CONFIGURABLE;
                        }
                    }
                target
            }
            "isFrozen" => {
                if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
                    let frozen = self.heap.get(oid)
                        .map(|o| !o.extensible && o.properties.iter().all(|(_, p)| !p.is_writable() && !p.is_configurable()))
                        .unwrap_or(true);
                    Value::boolean(frozen)
                } else { Value::boolean(true) }
            }
            "isSealed" => {
                if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
                    let sealed = self.heap.get(oid)
                        .map(|o| !o.extensible && o.properties.iter().all(|(_, p)| !p.is_configurable()))
                        .unwrap_or(true);
                    Value::boolean(sealed)
                } else { Value::boolean(true) }
            }
            "is" => {
                let a = args.first().copied().unwrap_or(Value::undefined());
                let b = args.get(1).copied().unwrap_or(Value::undefined());
                // Object.is: like === but NaN===NaN is true, +0!==-0 is true
                let result = if a.is_number() && b.is_number() {
                    let na = a.as_number().unwrap();
                    let nb = b.as_number().unwrap();
                    if na.is_nan() && nb.is_nan() { true }
                    else if na == 0.0 && nb == 0.0 { na.to_bits() == nb.to_bits() }
                    else { na == nb }
                } else {
                    self.strict_eq(a, b)
                };
                Value::boolean(result)
            }
            "getPrototypeOf" => {
                let arg = args.first().copied().unwrap_or(Value::undefined());
                if let Some(oid) = arg.as_object_id() {
                    // Class/function objects get Function.prototype as their proto
                    let is_fn_obj = self.heap.get(oid)
                        .map(|o| matches!(&o.kind, ObjectKind::Function(_)))
                        .unwrap_or(false);
                    if is_fn_obj {
                        Value::object_id(self.function_prototype)
                    } else {
                        self.heap.get(oid)
                            .and_then(|o| o.prototype.map(Value::object_id))
                            .unwrap_or(Value::null())
                    }
                } else if arg.is_function() {
                    // Sentinel functions → Function.prototype
                    Value::object_id(self.function_prototype)
                } else { Value::null() }
            }
            "setPrototypeOf" => {
                let target = args.first().copied().unwrap_or(Value::undefined());
                if let Some(oid) = target.as_object_id() {
                    let proto = args.get(1).copied().unwrap_or(Value::null());
                    if let Some(obj) = self.heap.get_mut(oid) {
                        obj.prototype = proto.as_object_id();
                    }
                }
                target
            }
            "preventExtensions" => {
                let target = args.first().copied().unwrap_or(Value::undefined());
                if let Some(oid) = target.as_object_id()
                    && let Some(obj) = self.heap.get_mut(oid) {
                        obj.extensible = false;
                    }
                target
            }
            "isExtensible" => {
                let arg = args.first().copied().unwrap_or(Value::undefined());
                if let Some(oid) = arg.as_object_id() {
                    Value::boolean(self.heap.get(oid).map(|o| o.extensible).unwrap_or(false))
                } else if arg.is_function() {
                    // Function values are extensible by default.
                    Value::boolean(true)
                } else { Value::boolean(false) }
            }
            "hasOwn" => {
                let target = args.first().copied().unwrap_or(Value::undefined());
                let key_val = args.get(1).copied().unwrap_or(Value::undefined());
                if let Some(oid) = target.as_object_id() {
                    let key_str = self.value_to_string(key_val);
                    let key_id = self.interner.intern(&key_str);
                    let getter_key = self.interner.intern(&format!("__get_{key_str}__"));
                    let setter_key = self.interner.intern(&format!("__set_{key_str}__"));
                    let has = self.heap.get(oid).map(|o| {
                        o.has_own_property(key_id)
                            || o.has_own_property(getter_key)
                            || o.has_own_property(setter_key)
                    }).unwrap_or(false);
                    Value::boolean(has)
                } else { Value::boolean(false) }
            }
            "getOwnPropertySymbols" => {
                // Own symbol-keyed properties are stored under __sym_N__
                // names; decode N back into symbol values. core-js's
                // NATIVE_SYMBOL probe requires this function to exist.
                let mut syms: Vec<Value> = Vec::new();
                if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
                    let keys: Vec<StringId> = self.heap.get(oid)
                        .map(|o| o.properties.iter().map(|&(k, _)| k).collect())
                        .unwrap_or_default();
                    for k in keys {
                        let name = self.interner.resolve(k);
                        if let Some(rest) = name.strip_prefix("__sym_").and_then(|s| s.strip_suffix("__"))
                            && let Ok(n) = rest.parse::<u32>()
                        {
                            syms.push(Value::symbol(n));
                        }
                    }
                }
                let arr = JsObject::array(syms);
                Value::object_id(self.heap.allocate(arr))
            }
            "fromEntries" => {
                // Object.fromEntries(iterable)
                let mut obj = JsObject::ordinary();
                if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
                    let entries: Vec<Value> = self.heap.get(oid)
                        .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                        .unwrap_or_default();
                    for entry in entries {
                        if let Some(entry_oid) = entry.as_object_id()
                            && let Some(eobj) = self.heap.get(entry_oid)
                                && let ObjectKind::Array(ref pair) = eobj.kind
                                    && pair.len() >= 2 {
                                        let key_val = pair[0];
                                        let val = pair[1];
                                        let key_str = self.value_to_string(key_val);
                                        let key_id = self.interner.intern(&key_str);
                                        obj.set_property(key_id, val);
                                    }
                    }
                }
                Value::object_id(self.heap.allocate(obj))
            }
            _ => return Ok(None),
        };
        Ok(Some(result))
    }

    /// Get a function's own property value, consulting the override table.
    /// Returns None if the property doesn't exist (or was deleted).
    pub(crate) fn fn_get_own_prop(&mut self, sentinel: i32, key: StringId) -> Option<Value> {
        let key_str = self.interner.resolve(key).to_owned();
        // Check the override table first
        if let Some(ov) = self.fn_property_overrides.get(&(sentinel, key)) {
            return *ov; // None = deleted, Some(v) = overridden
        }
        // Fall back to defaults
        let chunk_idx = (sentinel & 0xFFFF) as usize;
        match key_str.as_str() {
            "name" => {
                if chunk_idx > 0 && chunk_idx < self.chunks.len() {
                    let name_sid = self.chunks[chunk_idx].name;
                    let name_s = self.interner.resolve(name_sid).to_owned();
                    let visible = if name_s.starts_with('<') { String::new() } else { name_s };
                    let vsid = self.interner.intern(&visible);
                    Some(Value::string(vsid))
                } else {
                    let empty = self.interner.intern("");
                    Some(Value::string(empty))
                }
            }
            "length" => {
                if chunk_idx > 0 && chunk_idx < self.chunks.len() {
                    Some(Value::int(self.chunks[chunk_idx].formal_length as i32))
                } else {
                    Some(Value::int(0))
                }
            }
            _ => None,
        }
    }

    /// Comprehensive property lookup on a function-sentinel value (e.g. Number.POSITIVE_INFINITY,
    /// Symbol.iterator, fn.prototype). Returns the resolved value (Value::undefined() if missing).
    /// Used by both dot- and bracket-access paths.
    pub(crate) fn fn_property_get(&mut self, sentinel: i32, name_id: StringId, obj_val: Value) -> Value {
        if let Some(ov) = self.fn_property_overrides.get(&(sentinel, name_id)).copied() {
            return ov.unwrap_or(Value::undefined());
        }
        let name_str = self.interner.resolve(name_id).to_owned();
        match sentinel {
            -505 => match name_str.as_str() {
                "prototype" => Value::object_id(self.number_prototype),
                "NaN" => Value::number(f64::NAN),
                "POSITIVE_INFINITY" => Value::number(f64::INFINITY),
                "NEGATIVE_INFINITY" => Value::number(f64::NEG_INFINITY),
                "MAX_VALUE" => Value::number(f64::MAX),
                // Smallest positive value: the smallest denormal (5e-324),
                // i.e. the double with bit pattern 1 — not the smallest normal.
                "MIN_VALUE" => Value::number(f64::from_bits(1)),
                "MAX_SAFE_INTEGER" => Value::number(9007199254740991.0),
                "MIN_SAFE_INTEGER" => Value::number(-9007199254740991.0),
                "EPSILON" => Value::number(f64::EPSILON),
                "isNaN" => Value::function(-530),
                "isFinite" => Value::function(-531),
                "isInteger" => Value::function(-532),
                "isSafeInteger" => Value::function(-533),
                "parseInt" => Value::function(-500),
                "parseFloat" => Value::function(-501),
                "name" => { let id = self.interner.intern("Number"); Value::string(id) }
                "length" => Value::int(1),
                "call" | "apply" | "bind" => obj_val,
                _ => Value::undefined(),
            },
            -550 => match name_str.as_str() {
                // Extractable Date statics (`var p = Date.parse; p(...)`),
                // identity-cached via fn_property_overrides.
                "now" | "parse" | "UTC" => {
                    let which = name_str.clone();
                    let func: crate::runtime::object::NativeFn = std::sync::Arc::new(
                        move |vm: &mut Vm, _this: Value, args: &[Value]| -> Result<Value, Value> {
                            Ok(match which.as_str() {
                                "now" => Value::number(
                                    std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_millis() as f64)
                                        .unwrap_or(0.0),
                                ),
                                // The VM is timezone-less, so UTC == local
                                // component construction.
                                "UTC" => Value::number(vm.date_ms_from_args(args)),
                                _ => Value::number(f64::NAN), // parse: unsupported formats
                            })
                        },
                    );
                    let fn_obj = JsObject {
                        properties: Vec::new(),
                        prototype: None,
                        kind: ObjectKind::Function(crate::runtime::object::FunctionKind::Native {
                            name: name_id,
                            func,
                        }),
                        marked: false,
                        extensible: true,
                    };
                    let oid = self.heap.allocate(fn_obj);
                    let val = Value::object_id(oid);
                    self.fn_property_overrides.insert((sentinel, name_id), Some(val));
                    val
                }
                "name" => { let id = self.interner.intern("Date"); Value::string(id) }
                "length" => Value::int(7),
                "call" | "apply" | "bind" => obj_val,
                _ => Value::undefined(),
            },
            -570 => match name_str.as_str() {
                "for" => Value::function(-752),
                "keyFor" => Value::function(-753),
                "iterator" => Value::symbol(self.sym_iterator),
                "hasInstance" => Value::symbol(self.sym_has_instance),
                "toPrimitive" => Value::symbol(self.sym_to_primitive),
                "toStringTag" => Value::symbol(self.sym_to_string_tag),
                "species" => Value::symbol(self.sym_species),
                "unscopables" => Value::symbol(self.sym_unscopables),
                "asyncIterator" => Value::symbol(self.sym_async_iterator),
                "matchAll" => Value::symbol(self.sym_match_all),
                "prototype" => Value::object_id(self.symbol_prototype_oid()),
                "name" => { let id = self.interner.intern("Symbol"); Value::string(id) }
                "length" => Value::int(0),
                "call" | "apply" | "bind" => obj_val,
                _ => Value::undefined(),
            },
            -504 => match name_str.as_str() {
                "prototype" => Value::object_id(self.string_prototype),
                "fromCharCode" => Value::function(-534),
                "fromCodePoint" => Value::function(-535),
                "raw" => Value::function(-536),
                "name" => { let id = self.interner.intern("String"); Value::string(id) }
                "length" => Value::int(1),
                "call" | "apply" | "bind" => obj_val,
                _ => Value::undefined(),
            },
            -507 => match name_str.as_str() {
                "prototype" => Value::object_id(self.array_prototype),
                "name" => { let id = self.interner.intern("Array"); Value::string(id) }
                "length" => Value::int(1),
                "call" | "apply" | "bind" => obj_val,
                // Extractable static: `var isArray = Array.isArray;` (react-dom).
                "isArray" => Value::function(-751),
                // Constructors are functions; inherited methods
                // (hasOwnProperty, isPrototypeOf, …) resolve through
                // Function.prototype → Object.prototype. e.g.
                // `Array.hasOwnProperty.call(o, k)` (common in
                // minified libs like Highlight.js).
                _ => self.heap.get_property_chain(self.function_prototype, name_id)
                    .unwrap_or(Value::undefined()),
            },
            -508 => match name_str.as_str() {
                "prototype" => Value::object_id(self.object_prototype),
                "name" => { let id = self.interner.intern("Object"); Value::string(id) }
                "length" => Value::int(1),
                "call" | "apply" | "bind" => obj_val,
                // Extractable static: `var assign = Object.assign` is the
                // standard minified-bundle prologue (React, Preact, ...).
                "assign" => Value::function(-750),
                _ => {
                    // Extractable Object statics — see object_static_callable.
                    if let Some(val) = self.object_static_callable(name_id) {
                        val
                    } else {
                        self.heap.get_property_chain(self.function_prototype, name_id)
                            .unwrap_or(Value::undefined())
                    }
                }
            },
            -551 => match name_str.as_str() {
                "prototype" => Value::object_id(self.function_prototype),
                "name" => { let id = self.interner.intern("Function"); Value::string(id) }
                "length" => Value::int(1),
                "call" | "apply" | "bind" => obj_val,
                _ => self.heap.get_property_chain(self.function_prototype, name_id)
                    .unwrap_or(Value::undefined()),
            },
            _ => match name_str.as_str() {
                "prototype" => {
                    if let Some(&proto_oid) = self.func_prototypes.get(&sentinel) {
                        Value::object_id(proto_oid)
                    } else {
                        let mut proto = JsObject::ordinary();
                        proto.prototype = Some(self.object_prototype);
                        // Generator functions' prototypes are plain empty objects per
                        // spec (`function* f(){}.prototype` has no own properties).
                        let chunk_idx = (sentinel & 0xFFFF) as usize;
                        let is_gen = chunk_idx < self.chunks.len()
                            && self.chunks[chunk_idx].flags.contains(ChunkFlags::GENERATOR);
                        if !is_gen {
                            let ctor_key = self.interner.intern("constructor");
                            proto.define_property(ctor_key, Property::with_flags(
                                obj_val, Property::WRITABLE | Property::CONFIGURABLE
                            ));
                        }
                        let proto_oid = self.heap.allocate(proto);
                        self.func_prototypes.insert(sentinel, proto_oid);
                        Value::object_id(proto_oid)
                    }
                }
                "constructor" => Value::function(-551),
                "name" | "length" => self.fn_get_own_prop(sentinel, name_id).unwrap_or(Value::undefined()),
                "call" | "apply" | "bind" => obj_val,
                // Inherited methods resolve through Function.prototype →
                // Object.prototype so every function value exposes
                // hasOwnProperty / isPrototypeOf / propertyIsEnumerable /
                // toString / valueOf (`fn.hasOwnProperty(...)` etc.).
                _ => self.heap.get_property_chain(self.function_prototype, name_id)
                    .unwrap_or(Value::undefined()),
            },
        }
    }
}
