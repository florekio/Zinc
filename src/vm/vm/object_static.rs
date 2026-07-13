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
        // Spec length, captured before name_str moves into the closure.
        let mlen: i32 = match name_str.as_str() {
            "assign" => 2,
            "defineProperty" => 3,
            "defineProperties" | "create" | "getOwnPropertyDescriptor" | "setPrototypeOf" | "is"
            | "groupBy" => 2,
            _ => 1,
        };
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
        let mut fn_obj = JsObject {
            properties: Vec::new(),
            prototype: None,
            kind: ObjectKind::Function(crate::runtime::object::FunctionKind::Native { name: name_id, func }),
            marked: false,
            extensible: true,
        };
        // Spec own name/length for the Object statics.
        let name_key = self.interner.intern("name");
        let len_key = self.interner.intern("length");
        fn_obj.define_property(name_key, Property::with_flags(Value::string(name_id), Property::CONFIGURABLE));
        fn_obj.define_property(len_key, Property::with_flags(Value::int(mlen), Property::CONFIGURABLE));
        let oid = self.heap.allocate(fn_obj);
        let val = Value::object_id(oid);
        self.fn_property_overrides.insert((-508, name_id), Some(val));
        Some(val)
    }

    pub(crate) fn exec_object_static(&mut self, mn: &str, args: &[Value]) -> Result<Option<Value>, VmError> {
        // ToObject / RequireObjectCoercible on the first argument: nullish
        // receivers throw TypeError for these statics.
        if matches!(
            mn,
            "keys" | "values" | "entries" | "getPrototypeOf" | "setPrototypeOf"
                | "getOwnPropertyDescriptor" | "getOwnPropertyDescriptors"
                | "getOwnPropertyNames" | "getOwnPropertySymbols" | "defineProperty"
                | "defineProperties"
        ) && args.first().copied().unwrap_or(Value::undefined()).is_nullish()
        {
            return Err(VmError::Throw(self.make_native_error(
                "TypeError",
                &format!("Object.{mn} called on null or undefined"),
            )));
        }
        // defineProperty requires an actual object target.
        if mn == "defineProperty" {
            let t = args.first().copied().unwrap_or(Value::undefined());
            if t.as_object_id().is_none() && !t.is_function() {
                return Err(VmError::Throw(self.make_native_error(
                    "TypeError",
                    "Object.defineProperty called on non-object",
                )));
            }
        }
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
                    let is_array = self.heap.get(oid).map(|o| matches!(&o.kind, ObjectKind::Array(_) | ObjectKind::Wrapper(_))).unwrap_or(false);
                    if is_array {
                        // Hole indices are not own properties.
                        let idxs: Vec<usize> = self.heap.get(oid).map(|o| match &o.kind {
                            ObjectKind::Array(e) => e.iter().enumerate()
                                .filter(|(_, v)| !v.is_empty_marker())
                                .map(|(i, _)| i)
                                .collect(),
                            ObjectKind::Wrapper(inner) if inner.is_string() => {
                                inner.as_string_id()
                                    .map(|sid| (0..self.interner.resolve(sid).chars().count()).collect())
                                    .unwrap_or_default()
                            }
                            _ => Vec::new(),
                        }).unwrap_or_default();
                        let len = idxs.last().map(|i| i + 1).unwrap_or(0);
                        let mut keys: Vec<Value> = idxs.into_iter().map(|i| {
                            let s = self.interner.intern(&i.to_string());
                            Value::string(s)
                        }).collect();
                        // Named enumerable properties (index defines beyond the
                        // dense length, accessors) enumerate as well.
                        for k in self.enumerable_own_string_keys(oid) {
                            if k.parse::<usize>().is_ok_and(|i| i < len) {
                                continue;
                            }
                            let sid = self.interner.intern(&k);
                            keys.push(Value::string(sid));
                        }
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
                } else if let Some(sv) = args.first().copied()
                    .filter(|v| v.is_string() || self.is_cons_string(*v))
                {
                    // String receiver: enumerable own keys are the indices.
                    let st = self.value_to_string(sv);
                    let keys: Vec<Value> = (0..st.chars().count())
                        .map(|i| Value::string(self.interner.intern(&i.to_string())))
                        .collect();
                    self.alloc_array(keys)
                } else { Value::undefined() }
            }
            "values" => {
                if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
                    let elems: Vec<Value> = self.heap.get(oid)
                        .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                        .unwrap_or_default();
                    let mut vals = elems;
                    for key in self.enumerable_own_string_keys(oid) {
                        let v = self.getter_aware_get(oid, &key)?.unwrap_or(Value::undefined());
                        vals.push(v);
                    }
                    self.alloc_array(vals)
                } else { Value::undefined() }
            }
            "entries" => {
                if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
                    let elems: Vec<Value> = self.heap.get(oid)
                        .map(|o| if let ObjectKind::Array(ref e) = o.kind { e.clone() } else { vec![] })
                        .unwrap_or_default();
                    let mut entries = Vec::new();
                    for (i, v) in elems.into_iter().enumerate() {
                        let k = self.new_str(&i.to_string());
                        let pair = self.alloc_array(vec![k, v]);
                        entries.push(pair);
                    }
                    for key in self.enumerable_own_string_keys(oid) {
                        let v = self.getter_aware_get(oid, &key)?.unwrap_or(Value::undefined());
                        let k_id = self.interner.intern(&key);
                        let pair = self.alloc_array(vec![Value::string(k_id), v]);
                        entries.push(pair);
                    }
                    self.alloc_array(entries)
                } else { Value::undefined() }
            }
            "assign" => {
                if args.first().copied().unwrap_or(Value::undefined()).is_nullish() {
                    return Err(VmError::Throw(self.make_native_error(
                        "TypeError",
                        "Cannot convert undefined or null to object",
                    )));
                }
                self.exec_object_assign(args)?
            }
            "create" => {
                let proto = args.first().copied().unwrap_or(Value::undefined());
                if !(proto.is_null() || proto.as_object_id().is_some() || proto.is_function()) {
                    return Err(VmError::Throw(self.make_native_error(
                        "TypeError",
                        "Object prototype may only be an Object or null",
                    )));
                }
                let mut obj = JsObject::ordinary();
                obj.prototype = proto.as_object_id();
                let target = Value::object_id(self.heap.allocate(obj));
                if let Some(descs) = args.get(1).copied().filter(|v| !v.is_undefined()) {
                    self.apply_property_descriptors(target, descs)?;
                }
                target
            }
            "defineProperty" => self.object_define_property(args)?,
            "defineProperties" => {
                let target = args.first().copied().unwrap_or(Value::undefined());
                if target.as_object_id().is_none() && !target.is_function() {
                    return Err(VmError::Throw(self.make_native_error(
                        "TypeError",
                        "Object.defineProperties called on non-object",
                    )));
                }
                let descs = args.get(1).copied().unwrap_or(Value::undefined());
                self.apply_property_descriptors(target, descs)?;
                target
            }
            "getOwnPropertyDescriptor" => {
                let first_arg = args.first().copied().unwrap_or(Value::undefined());
                let key_arg = args.get(1).copied().unwrap_or(Value::undefined());
                // ToPropertyKey: symbol keys use __sym_N__ encoding; objects
                // coerce through their own toString observably.
                let key_str = if key_arg.is_symbol() {
                    format!("__sym_{}__", key_arg.as_symbol_id().unwrap())
                } else {
                    let prim = if key_arg.is_object() {
                        self.try_coerce_to_primitive_hint(key_arg, "string")?
                    } else {
                        key_arg
                    };
                    self.value_to_string(prim)
                };
                // Function values keep user-set properties in
                // fn_property_overrides — core-js inspects descriptors of
                // well-known symbols it installed on the Symbol
                // constructor (`getOwnPropertyDescriptor(Symbol,
                // 'asyncDispose').enumerable`).
                if let Some(sentinel) = first_arg.as_function() {
                    let key_id = self.interner.intern(&key_str);
                    let deleted = matches!(self.fn_property_overrides.get(&(sentinel, key_id)), Some(None));
                    // Source of the value decides the flags: user override
                    // (plain assignment) → w/e/c true; intrinsic name/length →
                    // w false, e false, c true; built-in static (Object.keys,
                    // Promise.resolve, …) → w true, e false, c true;
                    // .prototype → all false.
                    let (value, from_override) = if let Some(Some(v)) = self.fn_property_overrides.get(&(sentinel, key_id)).copied() {
                        (Some(v), true)
                    } else if deleted {
                        (None, false)
                    } else {
                        let own = self.fn_get_own_prop(sentinel, key_id);
                        match own {
                            Some(v) if key_str == "name" || key_str == "length" => (Some(v), false),
                            _ => {
                                let v = self.fn_property_get(sentinel, key_id, first_arg);
                                if v.is_undefined() {
                                    (own, false)
                                } else {
                                    (Some(v), false)
                                }
                            }
                        }
                    };
                    if let Some(v) = value {
                        let mut desc = JsObject::ordinary();
                        desc.prototype = Some(self.object_prototype);
                        let value_key = self.interner.intern("value");
                        let writable_key = self.interner.intern("writable");
                        let enumerable_key = self.interner.intern("enumerable");
                        let configurable_key = self.interner.intern("configurable");
                        // `name`/`length` keep their spec flags even when the
                        // VALUE came from an override (SetFunctionName).
                        let spec_flags = key_str == "name" || key_str == "length";
                        let is_proto = key_str == "prototype" || key_str == "BYTES_PER_ELEMENT";
                        desc.set_property(value_key, v);
                        desc.set_property(writable_key, Value::boolean(!spec_flags && !is_proto));
                        desc.set_property(enumerable_key, Value::boolean(from_override && !spec_flags && !is_proto));
                        desc.set_property(configurable_key, Value::boolean(!is_proto));
                        return Ok(Some(Value::object_id(self.heap.allocate(desc))));
                    }
                    return Ok(Some(Value::undefined()));
                }
                // String receivers (primitive or wrapper): char-index and
                // length descriptors.
                let str_recv: Option<String> = if first_arg.is_string() || self.is_cons_string(first_arg) {
                    Some(self.value_to_string(first_arg))
                } else {
                    first_arg.as_object_id().and_then(|o| self.heap.get(o)).and_then(|o| {
                        if let ObjectKind::Wrapper(inner) = &o.kind {
                            if inner.is_string() {
                                Some(self.interner.resolve(inner.as_string_id()?).to_owned())
                            } else { None }
                        } else { None }
                    })
                };
                if let Some(st) = str_recv {
                    let n = st.chars().count();
                    let (val, flags): (Option<Value>, u8) = if key_str == "length" {
                        (Some(Value::int(n as i32)), 0)
                    } else if let Ok(i) = key_str.parse::<usize>() {
                        match st.chars().nth(i) {
                            Some(c) => (Some(self.new_str(&c.to_string())), Property::ENUMERABLE),
                            None => (None, 0),
                        }
                    } else {
                        (None, 0)
                    };
                    if let Some(v) = val {
                        let mut desc = JsObject::ordinary();
                        desc.prototype = Some(self.object_prototype);
                        let vk = self.interner.intern("value");
                        let wk = self.interner.intern("writable");
                        let ek = self.interner.intern("enumerable");
                        let ck = self.interner.intern("configurable");
                        desc.set_property(vk, v);
                        desc.set_property(wk, Value::boolean(false));
                        desc.set_property(ek, Value::boolean(flags & Property::ENUMERABLE != 0));
                        desc.set_property(ck, Value::boolean(false));
                        return Ok(Some(Value::object_id(self.heap.allocate(desc))));
                    }
                    if first_arg.as_object_id().is_none() {
                        return Ok(Some(Value::undefined()));
                    }
                    // Wrapper: fall through for named props.
                }
                if let Some(oid) = first_arg.as_object_id() {
                    let key_id = self.interner.intern(&key_str);
                    // The global object proxies misses to the globals map;
                    // its built-ins get spec global-property descriptors.
                    if oid == self.global_this_oid
                        && !self.heap.get(oid).is_some_and(|o| o.has_own_property(key_id))
                        && let Some(&gv) = self.globals.get(&key_id)
                    {
                        let mut desc = JsObject::ordinary();
                        desc.prototype = Some(self.object_prototype);
                        let vk = self.interner.intern("value");
                        let wk = self.interner.intern("writable");
                        let ek = self.interner.intern("enumerable");
                        let ck = self.interner.intern("configurable");
                        desc.set_property(vk, gv);
                        desc.set_property(wk, Value::boolean(true));
                        desc.set_property(ek, Value::boolean(false));
                        desc.set_property(ck, Value::boolean(true));
                        return Ok(Some(Value::object_id(self.heap.allocate(desc))));
                    }
                    self.ensure_builtin_proto_method(oid, key_id);
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
                                let ro_key = self.interner.intern("__len_ro__");
                                let len_ro = self.heap.get(oid).is_some_and(|o| o.has_own_property(ro_key));
                                let mut desc = JsObject::ordinary();
                                desc.set_property(val_key, Value::int(len as i32));
                                desc.set_property(wr_key, Value::boolean(!len_ro));
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
                            let chunk_idx = Value::fn_chunk_idx(sentinel);
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
                } else if let Some(sv) = args.first().copied()
                    .filter(|v| v.is_string() || self.is_cons_string(*v))
                {
                    // String primitive: indices then length.
                    let st = self.value_to_string(sv);
                    let mut names: Vec<Value> = (0..st.chars().count())
                        .map(|i| Value::string(self.interner.intern(&i.to_string())))
                        .collect();
                    names.push(Value::string(self.interner.intern("length")));
                    self.alloc_array(names)
                } else if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
                    let mut seen = std::collections::HashSet::new();
                    let mut names: Vec<Value> = Vec::new();
                    // For arrays, integer indices come first in numeric order,
                    // then the `length` property, then named own properties.
                    // String wrappers expose their char indices the same way.
                    let array_len = self.heap.get(oid).and_then(|o| {
                        match &o.kind {
                            ObjectKind::Array(e) => Some(e.len()),
                            ObjectKind::Wrapper(inner) if inner.is_string() => {
                                let st = self.interner.resolve(inner.as_string_id()?);
                                Some(st.chars().count())
                            }
                            _ => None,
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
                if let Some(oid) = target.as_object_id() {
                    // Dense array elements have no per-slot flags; a marker
                    // property records that they froze.
                    let is_array = self.heap.get(oid)
                        .is_some_and(|o| matches!(o.kind, ObjectKind::Array(_)));
                    if let Some(obj) = self.heap.get_mut(oid) {
                        obj.extensible = false;
                        for entry in &mut obj.properties {
                            entry.1.flags &= !(Property::WRITABLE | Property::CONFIGURABLE);
                        }
                        if is_array {
                            let mk = self.interner.intern("__frozen_elems__");
                            obj.define_property(mk, Property::with_flags(Value::boolean(true), 0));
                        }
                    }
                }
                // Packed function values track their frozen state in the
                // override table (they have no heap object to flag).
                if let Some(packed) = target.as_function() {
                    let k = self.interner.intern("__frozen__");
                    self.fn_property_overrides.insert((packed, k), Some(Value::boolean(true)));
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
                if let Some(packed) = target.as_function() {
                    let k = self.interner.intern("__sealed__");
                    self.fn_property_overrides.insert((packed, k), Some(Value::boolean(true)));
                }
                target
            }
            "isFrozen" => {
                if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
                    let frozen = self.heap.get(oid)
                        .map(|o| !o.extensible && o.properties.iter().all(|(_, p)| !p.is_writable() && !p.is_configurable()))
                        .unwrap_or(true);
                    Value::boolean(frozen)
                } else if let Some(packed) = args.first().and_then(|v| v.as_function()) {
                    // Frozen only if explicitly frozen.
                    let k = self.interner.intern("__frozen__");
                    Value::boolean(self.fn_property_overrides.contains_key(&(packed, k)))
                } else { Value::boolean(true) }
            }
            "isSealed" => {
                if let Some(oid) = args.first().and_then(|v| v.as_object_id()) {
                    let sealed = self.heap.get(oid)
                        .map(|o| !o.extensible && o.properties.iter().all(|(_, p)| !p.is_configurable()))
                        .unwrap_or(true);
                    Value::boolean(sealed)
                } else if let Some(packed) = args.first().and_then(|v| v.as_function()) {
                    let ks = self.interner.intern("__sealed__");
                    let kf = self.interner.intern("__frozen__");
                    Value::boolean(
                        self.fn_property_overrides.contains_key(&(packed, ks))
                            || self.fn_property_overrides.contains_key(&(packed, kf)),
                    )
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
                    // %GeneratorFunction%/%AsyncFunction% are subclasses of
                    // Function: their [[Prototype]] is %Function% itself.
                    let ck = self.interner.intern("constructor");
                    let is_intrinsic_ctor = [self.generator_function_proto, self.async_function_proto]
                        .iter()
                        .any(|p| p.and_then(|po| self.heap.get(po).and_then(|o| o.get_property(ck)))
                            .and_then(|v| v.as_object_id()) == Some(oid));
                    if is_intrinsic_ctor {
                        return Ok(Some(Value::function(-551)));
                    }
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
                    // Generator functions chain to %GeneratorFunction.prototype%.
                    let sentinel = arg.as_function().unwrap();
                    let chunk_idx = Value::fn_chunk_idx(sentinel);
                    let flags = if sentinel >= 0 && chunk_idx < self.chunks.len() {
                        self.chunks[chunk_idx].flags
                    } else {
                        crate::compiler::chunk::ChunkFlags::empty()
                    };
                    if flags.contains(crate::compiler::chunk::ChunkFlags::GENERATOR) {
                        Value::object_id(self.generator_function_proto_oid())
                    } else if flags.contains(crate::compiler::chunk::ChunkFlags::ASYNC) {
                        Value::object_id(self.async_function_proto_oid())
                    } else {
                        // Sentinel functions → Function.prototype
                        Value::object_id(self.function_prototype)
                    }
                } else if arg.is_number() || arg.is_int() {
                    Value::object_id(self.number_prototype)
                } else if arg.is_string() || self.is_cons_string(arg) {
                    Value::object_id(self.string_prototype)
                } else if arg.is_boolean() {
                    Value::object_id(self.boolean_prototype)
                } else if arg.is_symbol() {
                    Value::object_id(self.symbol_prototype_oid())
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
                    let key_str = if key_val.is_symbol() {
                        format!("__sym_{}__", key_val.as_symbol_id().unwrap())
                    } else {
                        self.value_to_string(key_val)
                    };
                    // Array elements / wrapper chars / length are own too.
                    if let Ok(idx) = key_str.parse::<usize>() {
                        let idx_own = self.heap.get(oid).is_some_and(|o| match &o.kind {
                            ObjectKind::Array(e) => idx < e.len() && !e[idx].is_empty_marker(),
                            ObjectKind::Wrapper(inner) if inner.is_string() => {
                                inner.as_string_id().map(|sid| idx < self.interner.resolve(sid).chars().count()).unwrap_or(false)
                            }
                            _ => false,
                        });
                        if idx_own {
                            return Ok(Some(Value::boolean(true)));
                        }
                    }
                    if key_str == "length"
                        && self.heap.get(oid).is_some_and(|o| matches!(o.kind, ObjectKind::Array(_)))
                    {
                        return Ok(Some(Value::boolean(true)));
                    }
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
                    let mut seen = std::collections::HashSet::new();
                    for k in keys {
                        let name = self.interner.resolve(k);
                        // Data props are "__sym_N__"; accessor halves are
                        // "__get___sym_N__" / "__set___sym_N__".
                        let body = name
                            .strip_prefix("__get_")
                            .or_else(|| name.strip_prefix("__set_"))
                            .map(|s| s.strip_suffix("__").unwrap_or(s))
                            .unwrap_or(name);
                        if let Some(rest) = body.strip_prefix("__sym_")
                            && let Some(rest) = rest.strip_suffix("__").or(Some(rest))
                            && let Ok(n) = rest.parse::<u32>()
                            && seen.insert(n)
                        {
                            syms.push(Value::symbol(n));
                        }
                    }
                }
                let mut arr = JsObject::array(syms);
                arr.prototype = Some(self.array_prototype);
                Value::object_id(self.heap.allocate(arr))
            }
            "groupBy" => {
                let items = args.first().copied().unwrap_or(Value::undefined());
                let cb = args.get(1).copied().unwrap_or(Value::undefined());
                if items.is_nullish() || !self.value_callable(cb) {
                    return Err(VmError::Throw(self.make_native_error(
                        "TypeError",
                        "Object.groupBy requires an iterable and a callback",
                    )));
                }
                let elems: Vec<Value> = if let Some(ioid) = items.as_object_id() {
                    match self.heap.get(ioid).map(|o| &o.kind) {
                        Some(ObjectKind::Array(e)) => e.iter()
                            .map(|v| if v.is_empty_marker() { Value::undefined() } else { *v })
                            .collect(),
                        Some(ObjectKind::Set { entries }) => entries.clone(),
                        _ => {
                            let len = self.array_like_len_public(ioid)?;
                            let mut out = Vec::new();
                            for i in 0..len {
                                out.push(self.array_like_get_public(ioid, i)?.unwrap_or(Value::undefined()));
                            }
                            out
                        }
                    }
                } else if items.is_string() || self.is_cons_string(items) {
                    let st = self.value_to_string(items);
                    st.chars().map(|c| self.new_str(&c.to_string())).collect()
                } else {
                    Vec::new()
                };
                let mut groups = JsObject::ordinary();
                groups.prototype = None; // null-prototype result per spec
                let groups_oid = self.heap.allocate(groups);
                for (i, item) in elems.into_iter().enumerate() {
                    let k = self.call_function_this(cb, Value::undefined(), &[item, Value::number(i as f64)])?;
                    let key_str = if k.is_symbol() {
                        format!("__sym_{}__", k.as_symbol_id().unwrap())
                    } else {
                        let p = if k.is_object() {
                            self.try_coerce_to_primitive_hint(k, "string")?
                        } else { k };
                        self.value_to_string(p)
                    };
                    let kid = self.interner.intern(&key_str);
                    let arr = self.heap.get(groups_oid)
                        .and_then(|o| o.get_property(kid))
                        .and_then(|v| v.as_object_id());
                    match arr {
                        Some(aid) => {
                            if let Some(o) = self.heap.get_mut(aid)
                                && let ObjectKind::Array(ref mut e) = o.kind {
                                    e.push(item);
                                }
                        }
                        None => {
                            let a = self.alloc_array(vec![item]);
                            if let Some(o) = self.heap.get_mut(groups_oid) {
                                o.set_property(kid, a);
                            }
                        }
                    }
                }
                Value::object_id(groups_oid)
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

    /// Spec `name`/`length` for the well-known negative sentinels (global
    /// constructors, global functions, extractable statics, Math methods).
    pub(crate) fn sentinel_fn_meta(sentinel: i64) -> Option<(&'static str, i32)> {
        const T: &[(i64, &str, i32)] = &[
            (-500, "parseInt", 2), (-501, "parseFloat", 1), (-502, "isNaN", 1), (-503, "isFinite", 1),
            (-504, "String", 1), (-505, "Number", 1), (-506, "Boolean", 1), (-507, "Array", 1), (-508, "Object", 1),
            (-509, "encodeURI", 1), (-517, "decodeURIComponent", 1), (-518, "encodeURIComponent", 1), (-519, "decodeURI", 1),
            (-510, "Error", 1), (-511, "TypeError", 1), (-512, "RangeError", 1), (-513, "ReferenceError", 1),
            (-514, "SyntaxError", 1), (-515, "EvalError", 1), (-516, "URIError", 1),
            (-539, "AggregateError", 2),
            (-520, "Promise", 1), (-540, "Map", 0), (-541, "Set", 0), (-542, "WeakMap", 0), (-543, "WeakSet", 0),
            (-550, "Date", 7), (-551, "Function", 1), (-560, "eval", 1), (-570, "Symbol", 0),
            (-580, "RegExp", 2), (-638, "BigInt", 1),
            (-530, "isNaN", 1), (-531, "isFinite", 1), (-532, "isInteger", 1), (-533, "isSafeInteger", 1),
            (-534, "fromCharCode", 1), (-535, "fromCodePoint", 1), (-536, "raw", 1),
            (-751, "isArray", 1), (-752, "for", 1), (-753, "keyFor", 1),
            // ArrayBuffer/DataView/typed-array constructors (-660..-672).
            (-660, "ArrayBuffer", 1), (-661, "DataView", 1),
            (-662, "Int8Array", 3), (-663, "Uint8Array", 3), (-664, "Uint8ClampedArray", 3),
            (-665, "Int16Array", 3), (-666, "Uint16Array", 3), (-667, "Int32Array", 3),
            (-668, "Uint32Array", 3), (-669, "Float32Array", 3), (-670, "Float64Array", 3),
            (-671, "BigInt64Array", 3), (-672, "BigUint64Array", 3),
            (-630, "toString", 0), (-631, "valueOf", 0), (-632, "toString", 1),
            (-633, "valueOf", 0), (-634, "toString", 0), (-635, "valueOf", 0),
            (-590, "hasOwnProperty", 1), (-591, "propertyIsEnumerable", 1),
            (-592, "toString", 0), (-593, "valueOf", 0), (-594, "isPrototypeOf", 1),
            // Array.prototype method sentinels (seeded in init).
            (-600, "join", 1), (-601, "push", 1), (-602, "pop", 0), (-603, "shift", 0),
            (-604, "unshift", 1), (-605, "indexOf", 1), (-606, "includes", 1), (-607, "forEach", 1),
            (-608, "map", 1), (-609, "filter", 1), (-610, "reduce", 1), (-611, "some", 1),
            (-612, "every", 1), (-613, "find", 1), (-614, "findIndex", 1), (-615, "slice", 2),
            (-616, "concat", 1), (-617, "reverse", 0), (-618, "sort", 1), (-619, "flat", 0),
            (-620, "flatMap", 1), (-621, "fill", 1), (-622, "splice", 2), (-623, "reduceRight", 1),
            (-624, "at", 1), (-625, "keys", 0), (-626, "values", 0), (-627, "entries", 0),
            (-628, "lastIndexOf", 1), (-629, "toString", 0),
            (-700, "sin", 1), (-701, "cos", 1), (-702, "abs", 1), (-703, "floor", 1),
            (-704, "ceil", 1), (-705, "round", 1), (-706, "sqrt", 1), (-707, "pow", 2),
            (-708, "max", 2), (-709, "min", 2), (-710, "exp", 1), (-711, "log", 1),
            (-712, "log2", 1), (-713, "log10", 1), (-714, "random", 0), (-715, "trunc", 1),
            (-716, "sign", 1), (-717, "cbrt", 1), (-718, "hypot", 2), (-719, "atan2", 2),
            (-720, "atan", 1), (-721, "asin", 1), (-722, "acos", 1), (-723, "tan", 1),
            (-724, "clz32", 1), (-725, "imul", 2), (-726, "fround", 1),
            (-727, "log1p", 1), (-728, "expm1", 1), (-729, "sinh", 1), (-730, "cosh", 1),
            (-731, "tanh", 1), (-732, "asinh", 1), (-733, "acosh", 1), (-734, "atanh", 1),
        ];
        T.iter().find(|(s, _, _)| *s == sentinel).map(|(_, n, l)| (*n, *l))
    }

    /// Get a function's own property value, consulting the override table.
    /// Returns None if the property doesn't exist (or was deleted).
    pub(crate) fn fn_get_own_prop(&mut self, sentinel: i64, key: StringId) -> Option<Value> {
        let key_str = self.interner.resolve(key).to_owned();
        // Check the override table first
        if let Some(ov) = self.fn_property_overrides.get(&(sentinel, key)) {
            return *ov; // None = deleted, Some(v) = overridden
        }
        // Promise resolving/combinator element functions (deep negative
        // encodings): anonymous, length 1.
        if sentinel <= -600_000 {
            match key_str.as_str() {
                "name" => {
                    let sid = self.interner.intern("");
                    return Some(Value::string(sid));
                }
                "length" => return Some(Value::int(1)),
                _ => return None,
            }
        }
        // Fall back to defaults
        let chunk_idx = Value::fn_chunk_idx(sentinel);
        match key_str.as_str() {
            "name" => {
                if chunk_idx > 0 && chunk_idx < self.chunks.len() {
                    let name_sid = self.chunks[chunk_idx].name;
                    let name_s = self.interner.resolve(name_sid).to_owned();
                    let visible = if name_s.starts_with('<') { String::new() } else { name_s };
                    let vsid = self.interner.intern(&visible);
                    Some(Value::string(vsid))
                } else if let Some((n, _)) = Self::sentinel_fn_meta(sentinel) {
                    let sid = self.interner.intern(n);
                    Some(Value::string(sid))
                } else {
                    let empty = self.interner.intern("");
                    Some(Value::string(empty))
                }
            }
            "length" => {
                if chunk_idx > 0 && chunk_idx < self.chunks.len() {
                    Some(Value::int(self.chunks[chunk_idx].formal_length as i32))
                } else if let Some((_, l)) = Self::sentinel_fn_meta(sentinel) {
                    Some(Value::int(l))
                } else {
                    Some(Value::int(0))
                }
            }
            "BYTES_PER_ELEMENT" => {
                crate::vm::typedarray::kind_for_sentinel(sentinel)
                    .map(|k| Value::int(k.bytes_per_element() as i32))
            }
            _ => None,
        }
    }

    /// Comprehensive property lookup on a function-sentinel value (e.g. Number.POSITIVE_INFINITY,
    /// Symbol.iterator, fn.prototype). Returns the resolved value (Value::undefined() if missing).
    /// Used by both dot- and bracket-access paths.
    pub(crate) fn fn_property_get(&mut self, sentinel: i64, name_id: StringId, obj_val: Value) -> Value {
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
                                _ => {
                                    let s = args
                                        .first()
                                        .map(|v| vm.value_to_string(*v))
                                        .unwrap_or_default();
                                    Value::number(super::parse_date_string(&s))
                                }
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
                "prototype" => Value::object_id(self.date_prototype),
                "call" | "apply" | "bind" => obj_val,
                _ => Value::undefined(),
            },
            -520 => match name_str.as_str() {
                // Extractable Promise statics (`var r = Promise.resolve;`),
                // identity-cached like the Date statics.
                "resolve" | "reject" | "all" | "race" | "allSettled" | "any" => {
                    let is_settle = matches!(name_str.as_str(), "resolve" | "reject");
                    let func: crate::runtime::object::NativeFn = std::sync::Arc::new(
                        move |vm: &mut Vm, this: Value, args: &[Value]| -> Result<Value, Value> {
                            // NewPromiseCapability(this): the receiver must be a
                            // constructor; custom ones are actually constructed and
                            // their capability resolve/reject invoked.
                            if let Some((instance, res, rej)) = vm.promise_new_capability(this)?
                                && is_settle
                            {
                                let settle = if vm.interner.resolve(name_id) == "reject" { rej } else { res };
                                let arg = args.first().copied().unwrap_or(Value::undefined());
                                match vm.call_function_this(settle, Value::undefined(), &[arg]) {
                                    Ok(_) => {}
                                    Err(VmError::Throw(v)) => return Err(v),
                                    Err(e) => return Err(vm.make_native_error("Error", &format!("{e:?}"))),
                                }
                                return Ok(instance);
                            }
                            // Combinators keep native semantics after the
                            // capability handshake.
                            match vm.exec_promise_static(name_id, args) {
                                Ok(v) => Ok(v),
                                Err(VmError::Throw(v)) => Err(v),
                                Err(e) => Err(vm.make_native_error("Error", &format!("{e:?}"))),
                            }
                        },
                    );
                    let mut fn_obj = JsObject {
                        properties: Vec::new(),
                        prototype: Some(self.function_prototype),
                        kind: ObjectKind::Function(crate::runtime::object::FunctionKind::Native {
                            name: name_id,
                            func,
                        }),
                        marked: false,
                        extensible: true,
                    };
                    let name_key = self.interner.intern("name");
                    let len_key = self.interner.intern("length");
                    fn_obj.define_property(name_key, Property::with_flags(Value::string(name_id), Property::CONFIGURABLE));
                    fn_obj.define_property(len_key, Property::with_flags(Value::int(1), Property::CONFIGURABLE));
                    let oid = self.heap.allocate(fn_obj);
                    let val = Value::object_id(oid);
                    self.fn_property_overrides.insert((sentinel, name_id), Some(val));
                    val
                }
                "prototype" => self.func_prototypes.get(&-520).copied()
                    .map(Value::object_id).unwrap_or(Value::undefined()),
                "name" => { let id = self.interner.intern("Promise"); Value::string(id) }
                "length" => Value::int(1),
                "call" | "apply" | "bind" => obj_val,
                _ => Value::undefined(),
            },
            -638 => match name_str.as_str() {
                // Extractable BigInt statics, identity-cached like Date's.
                "asIntN" | "asUintN" => {
                    let signed = name_str == "asIntN";
                    let func: crate::runtime::object::NativeFn = std::sync::Arc::new(
                        move |vm: &mut Vm, _this: Value, args: &[Value]| -> Result<Value, Value> {
                            // bits = ToIndex(bits); bigint = ToBigInt(bigint)
                            let bits_arg = args.first().copied().unwrap_or(Value::undefined());
                            let bits = match vm.spec_to_index(bits_arg) {
                                Ok(b) => b,
                                Err(VmError::Throw(e)) => return Err(e),
                                Err(_) => return Err(vm.make_native_error("Error", "internal")),
                            };
                            let big_arg = args.get(1).copied().unwrap_or(Value::undefined());
                            let b = match vm.value_to_bigint(big_arg) {
                                Ok(b) => b,
                                Err(VmError::Throw(e)) => return Err(e),
                                Err(_) => return Err(vm.make_native_error("Error", "internal")),
                            };
                            use num_bigint::BigInt;
                            let modulus = BigInt::from(1) << bits;
                            let mut r = ((b % &modulus) + &modulus) % &modulus;
                            if signed && bits > 0 && r >= (BigInt::from(1) << (bits - 1)) {
                                r -= &modulus;
                            }
                            Ok(vm.make_bigint(r))
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
                "name" => { let id = self.interner.intern("BigInt"); Value::string(id) }
                "length" => Value::int(1),
                "prototype" => self.func_prototypes.get(&-638).map(|o| Value::object_id(*o)).unwrap_or(Value::undefined()),
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
                "replace" => Value::symbol(8),
                "match" => Value::symbol(9),
                "search" => Value::symbol(10),
                "split" => Value::symbol(11),
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
                // Extractable Array.from / Array.of, identity-cached. The
                // wrapper covers arrays, strings, collections, and generic
                // array-likes with an optional mapFn; the method-call path
                // keeps the full iterator protocol.
                "from" | "of" => {
                    let is_of = name_str == "of";
                    let func: crate::runtime::object::NativeFn = std::sync::Arc::new(
                        move |vm: &mut Vm, _this: Value, args: &[Value]| -> Result<Value, Value> {
                            let unwrap_throw = |vm: &mut Vm, e: VmError| match e {
                                VmError::Throw(v) => v,
                                e => vm.make_native_error("Error", &format!("{e:?}")),
                            };
                            let items: Vec<Value> = if is_of {
                                args.to_vec()
                            } else {
                                let source = args.first().copied().unwrap_or(Value::undefined());
                                if source.is_nullish() {
                                    return Err(vm.make_native_error(
                                        "TypeError",
                                        "Array.from requires an array-like object",
                                    ));
                                }
                                if source.is_string() || vm.is_cons_string(source) {
                                    let st = vm.value_to_string(source);
                                    st.chars().map(|c| vm.new_str(&c.to_string())).collect()
                                } else if let Some(soid) = source.as_object_id() {
                                    match vm.heap.get(soid).map(|o| &o.kind) {
                                        Some(ObjectKind::Array(e)) => e.iter()
                                            .map(|v| if v.is_empty_marker() { Value::undefined() } else { *v })
                                            .collect(),
                                        Some(ObjectKind::Set { entries }) => entries.clone(),
                                        _ => {
                                            let len = vm.array_like_len_public(soid)
                                                .map_err(|e| unwrap_throw(vm, e))?;
                                            let mut out = Vec::new();
                                            for i in 0..len {
                                                let v = vm.array_like_get_public(soid, i)
                                                    .map_err(|e| unwrap_throw(vm, e))?
                                                    .unwrap_or(Value::undefined());
                                                out.push(v);
                                            }
                                            out
                                        }
                                    }
                                } else {
                                    Vec::new()
                                }
                            };
                            // mapFn (Array.from only)
                            let map_fn = if is_of { None } else { args.get(1).copied().filter(|v| !v.is_undefined()) };
                            let this_arg = args.get(2).copied().unwrap_or(Value::undefined());
                            let mut result = Vec::with_capacity(items.len());
                            for (i, item) in items.into_iter().enumerate() {
                                let v = if let Some(f) = map_fn {
                                    vm.call_function_this(f, this_arg, &[item, Value::number(i as f64)])
                                        .map_err(|e| unwrap_throw(vm, e))?
                                } else {
                                    item
                                };
                                result.push(v);
                            }
                            // A constructor receiver builds via Construct(this,
                            // [len]) + CreateDataPropertyOrThrow + Set(length) —
                            // every step's abrupt completion propagates.
                            let ctor_like = _this.as_function().is_some_and(|p| p >= 0 && p != -507);
                            if ctor_like {
                                // The instance links to the constructor's
                                // .prototype (setters there are observable).
                                let proto_oid = _this.as_function().and_then(|packed| {
                                    let pk = vm.interner.intern("prototype");
                                    vm.fn_property_overrides.get(&(packed, pk)).copied().flatten()
                                        .and_then(|v| v.as_object_id())
                                        .or_else(|| vm.func_prototypes.get(&packed).copied())
                                        .or_else(|| {
                                            // Create + cache, mirroring Construct.
                                            let mut proto = JsObject::ordinary();
                                            proto.prototype = Some(vm.object_prototype);
                                            let ck = vm.interner.intern("constructor");
                                            proto.define_property(ck, Property::with_flags(
                                                _this, Property::WRITABLE | Property::CONFIGURABLE,
                                            ));
                                            let po = vm.heap.allocate(proto);
                                            vm.func_prototypes.insert(packed, po);
                                            Some(po)
                                        })
                                });
                                let mut fresh_obj = JsObject::ordinary();
                                fresh_obj.prototype = proto_oid;
                                let fresh = vm.heap.allocate(fresh_obj);
                                let ret = vm.call_function_this(
                                    _this,
                                    Value::object_id(fresh),
                                    &[Value::number(result.len() as f64)],
                                ).map_err(|e| unwrap_throw(vm, e))?;
                                let target = if ret.as_object_id().is_some() { ret } else { Value::object_id(fresh) };
                                // Reusable data descriptor {value, all true}.
                                let mut desc = JsObject::ordinary();
                                let vk = vm.interner.intern("value");
                                let wk = vm.interner.intern("writable");
                                let ek = vm.interner.intern("enumerable");
                                let ck = vm.interner.intern("configurable");
                                desc.set_property(wk, Value::boolean(true));
                                desc.set_property(ek, Value::boolean(true));
                                desc.set_property(ck, Value::boolean(true));
                                let desc_oid = vm.heap.allocate(desc);
                                for (i, v) in result.iter().enumerate() {
                                    if let Some(d) = vm.heap.get_mut(desc_oid) {
                                        d.set_property(vk, *v);
                                    }
                                    let key_id = vm.interner.intern(&i.to_string());
                                    vm.object_define_property(&[
                                        target,
                                        Value::string(key_id),
                                        Value::object_id(desc_oid),
                                    ]).map_err(|e| unwrap_throw(vm, e))?;
                                }
                                // Set(target, "length", len, true)
                                if let Some(toid) = target.as_object_id() {
                                    let lk = vm.interner.intern("length");
                                    let sk = vm.interner.intern("__set_length__");
                                    let gk = vm.interner.intern("__get_length__");
                                    // Setters anywhere on the chain run (the
                                    // spec Set walks the prototype chain).
                                    let (setter, has_acc, nonwritable) = (
                                        vm.heap.get_property_chain(toid, sk).filter(|v| vm.value_callable(*v)),
                                        vm.heap.get_property_chain(toid, sk).is_some()
                                            || vm.heap.get_property_chain(toid, gk).is_some(),
                                        vm.heap.get(toid)
                                            .and_then(|o| o.get_property_descriptor(lk))
                                            .is_some_and(|pr| !pr.is_writable()),
                                    );
                                    if let Some(sfn) = setter {
                                        vm.call_function_this(sfn, target, &[Value::number(result.len() as f64)])
                                            .map_err(|e| unwrap_throw(vm, e))?;
                                    } else if has_acc || nonwritable {
                                        return Err(vm.make_native_error(
                                            "TypeError",
                                            "Cannot assign to read only property 'length'",
                                        ));
                                    } else if let Some(o) = vm.heap.get_mut(toid) {
                                        o.set_property(lk, Value::number(result.len() as f64));
                                    }
                                }
                                return Ok(target);
                            }
                            Ok(vm.alloc_array(result))
                        },
                    );
                    let mut fn_obj = JsObject {
                        properties: Vec::new(),
                        prototype: Some(self.function_prototype),
                        kind: ObjectKind::Function(crate::runtime::object::FunctionKind::Native {
                            name: name_id,
                            func,
                        }),
                        marked: false,
                        extensible: true,
                    };
                    let name_key = self.interner.intern("name");
                    let len_key = self.interner.intern("length");
                    fn_obj.define_property(name_key, Property::with_flags(Value::string(name_id), Property::CONFIGURABLE));
                    fn_obj.define_property(len_key, Property::with_flags(Value::int(if is_of { 0 } else { 1 }), Property::CONFIGURABLE));
                    let oid = self.heap.allocate(fn_obj);
                    let val = Value::object_id(oid);
                    self.fn_property_overrides.insert((sentinel, name_id), Some(val));
                    val
                }
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
                        let chunk_idx = Value::fn_chunk_idx(sentinel);
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
