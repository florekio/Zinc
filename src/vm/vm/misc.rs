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

    /// ObjectDefineProperties: apply every own enumerable key of `descs` to
    /// `target` as a property descriptor. Shared by Object.create and
    /// Object.defineProperties.
    pub(crate) fn apply_property_descriptors(&mut self, target: Value, descs: Value) -> Result<(), VmError> {
        let Some(doid) = descs.as_object_id() else {
            // ToObject(Properties): null/undefined throw; other primitives
            // box to objects with no own enumerable string keys.
            if descs.is_nullish() {
                return Err(VmError::Throw(self.make_native_error(
                    "TypeError",
                    "Cannot convert undefined or null to object",
                )));
            }
            return Ok(());
        };
        // Own enumerable keys, with accessor halves (__get_X__/__set_X__)
        // reduced to X; symbol keys keep their storage encoding.
        let mut keys: Vec<String> = Vec::new();
        if let Some(o) = self.heap.get(doid) {
            for (k, p) in &o.properties {
                if !p.is_enumerable() {
                    continue;
                }
                let ks = self.interner.resolve(*k);
                let base = if ks.starts_with("__sym_") && ks.ends_with("__") {
                    ks
                } else if let Some(rest) = ks
                    .strip_prefix("__get_")
                    .and_then(|r| r.strip_suffix("__"))
                    .or_else(|| ks.strip_prefix("__set_").and_then(|r| r.strip_suffix("__")))
                {
                    rest
                } else if is_internal_key(ks) {
                    continue;
                } else {
                    ks
                };
                if !keys.iter().any(|e| e == base) {
                    keys.push(base.to_string());
                }
            }
        }
        for kstr in keys {
            let key_id = self.interner.intern(&kstr);
            // Get(descObj, key) — getter-aware, receiver = the desc object.
            let getter_key = self.interner.intern(&format!("__get_{kstr}__"));
            let dval = if let Some(g) = self
                .heap
                .get(doid)
                .and_then(|o| o.get_property(getter_key))
                .filter(|v| v.is_function())
            {
                self.call_function_this(g, descs, &[])?
            } else {
                self.heap
                    .get(doid)
                    .and_then(|o| o.get_property(key_id))
                    .unwrap_or(Value::undefined())
            };
            if dval.as_object_id().is_none() && !dval.is_function() {
                return Err(VmError::Throw(self.make_native_error(
                    "TypeError",
                    "Property description must be an object",
                )));
            }
            self.object_define_property(&[target, Value::string(key_id), dval])?;
        }
        Ok(())
    }

    /// Own enumerable string keys in spec order (numeric ascending, then
    /// insertion order), with accessor halves reduced to their base name and
    /// internal/symbol keys skipped. Shared by Object.values / entries.
    pub(crate) fn enumerable_own_string_keys(&mut self, oid: ObjectId) -> Vec<String> {
        let raw: Vec<(String, bool)> = self.heap.get(oid)
            .map(|o| o.properties.iter()
                .map(|(k, p)| (self.interner.resolve(*k).to_owned(), p.is_enumerable()))
                .collect())
            .unwrap_or_default();
        let mut ordered: Vec<String> = Vec::new();
        for (ks, enumerable) in raw {
            if !enumerable {
                continue;
            }
            let base = if let Some(rest) = ks
                .strip_prefix("__get_")
                .and_then(|r| r.strip_suffix("__"))
                .or_else(|| ks.strip_prefix("__set_").and_then(|r| r.strip_suffix("__")))
            {
                rest.to_string()
            } else if is_internal_key(&ks) {
                continue;
            } else {
                ks
            };
            if base.starts_with("__sym_") {
                continue;
            }
            if !ordered.contains(&base) {
                ordered.push(base);
            }
        }
        let (mut numeric, rest): (Vec<String>, Vec<String>) =
            ordered.into_iter().partition(|k| k.parse::<u64>().is_ok());
        numeric.sort_by_key(|k| k.parse::<u64>().unwrap());
        numeric.extend(rest);
        numeric
    }

    /// Callable check that covers packed function values AND heap function
    /// objects (reified built-ins, bound functions, NativeFns).
    pub(crate) fn value_callable(&self, v: Value) -> bool {
        v.is_function()
            || v.as_object_id()
                .and_then(|o| self.heap.get(o))
                .is_some_and(|o| matches!(o.kind, ObjectKind::Function(_)))
    }

    /// Get(O, key) for a named key: getter-aware, per-level shadowing
    /// (own setter-only accessors read as undefined), chain-walking.
    pub(crate) fn getter_aware_get(&mut self, oid: ObjectId, key: &str) -> Result<Option<Value>, VmError> {
        let key_id = self.interner.intern(key);
        let getter_key = self.interner.intern(&format!("__get_{key}__"));
        let setter_key = self.interner.intern(&format!("__set_{key}__"));
        let receiver = Value::object_id(oid);
        let mut cur = Some(oid);
        let mut hops = 0;
        while let Some(c) = cur {
            let (g, d, has_set, proto) = match self.heap.get(c) {
                Some(o) => (
                    o.get_property(getter_key),
                    o.get_property(key_id),
                    o.get_property(setter_key).is_some(),
                    o.prototype,
                ),
                None => (None, None, false, None),
            };
            if let Some(g) = g {
                if self.value_callable(g) {
                    // Protect: a throw inside the getter must return to THIS
                    // native caller, not unwind the JS frames beneath it.
                    let prev = self.protect_throw_depth;
                    self.protect_throw_depth = self.frames.len() + 1;
                    let r = self.call_function_this(g, receiver, &[]);
                    self.protect_throw_depth = prev;
                    return r.map(Some);
                }
                // Accessor half stored without a callable getter: the
                // property exists, Get is undefined.
                return Ok(Some(Value::undefined()));
            }
            if let Some(d) = d {
                return Ok(Some(d));
            }
            if has_set {
                return Ok(Some(Value::undefined()));
            }
            hops += 1;
            if hops > 64 {
                break;
            }
            cur = proto;
        }
        Ok(None)
    }

    /// ArraySetLength shrink: remove named index props and dense elements
    /// from the top down, stopping at the first non-configurable index.
    /// Returns true when the full shrink to `req` succeeded.
    pub(crate) fn array_shrink_length(&mut self, oid: ObjectId, req: usize) -> bool {
        fn canon(s: &str) -> Option<usize> {
            if s == "0" { return Some(0); }
            if s.is_empty() || s.as_bytes()[0] == b'0' || !s.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            s.parse::<u64>().ok().filter(|&n| n < (u32::MAX as u64)).map(|n| n as usize)
        }
        let cur_len = self.heap.get(oid).map(|o| {
            if let ObjectKind::Array(ref e) = o.kind { e.len() } else { 0 }
        }).unwrap_or(0);
        let barrier = self.heap.get(oid).and_then(|o| o.properties.iter()
            .filter_map(|(k, p)| {
                let ks = self.interner.resolve(*k);
                // Accessor halves live under __get_N__ / __set_N__.
                let ks = ks.strip_prefix("__get_")
                    .or_else(|| ks.strip_prefix("__set_"))
                    .and_then(|t| t.strip_suffix("__"))
                    .unwrap_or(ks);
                canon(ks).filter(|i| *i >= req && *i < cur_len && !p.is_configurable())
            })
            .max());
        let stop = barrier.map(|b| b + 1).unwrap_or(req);
        let len_key = self.interner.intern("length");
        let doomed: Vec<StringId> = self.heap.get(oid)
            .map(|o| o.properties.iter()
                .filter(|(k, _)| {
                    *k == len_key || {
                        let ks = self.interner.resolve(*k);
                        canon(ks).is_some_and(|i| i >= stop)
                    }
                })
                .map(|(k, _)| *k)
                .collect())
            .unwrap_or_default();
        if let Some(obj) = self.heap.get_mut(oid) {
            obj.properties.retain(|(k, _)| !doomed.contains(k));
            if let ObjectKind::Array(ref mut elements) = obj.kind {
                elements.truncate(stop);
            }
        }
        barrier.is_none()
    }

    pub(crate) fn object_define_property(&mut self, args: &[Value]) -> Result<Value, VmError> {
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
            let value_key = self.interner.intern("value");
            let get_key = self.interner.intern("get");
            let data_val = desc_val.as_object_id()
                .and_then(|doid| self.heap.get_property_chain(doid, value_key));
            let getter_val = desc_val.as_object_id()
                .and_then(|doid| self.heap.get_property_chain(doid, get_key))
                .filter(|v| !v.is_undefined());
            if let Some(value) = data_val {
                self.fn_property_overrides.insert((sentinel, key_id), Some(value));
            } else if let Some(g) = getter_val {
                // Accessor define on a builtin: keep the getter under the
                // __get_<key>__ convention so reads can distinguish it.
                let getter_id = self.interner.intern(&format!("__get_{key_str}__"));
                self.fn_property_overrides.insert((sentinel, getter_id), Some(g));
            }
            return Ok(target);
        }
        let Some(target_oid) = target.as_object_id() else { return Ok(target) };
        let key_str = if key_val.is_symbol() {
            format!("__sym_{}__", key_val.as_symbol_id().unwrap())
        } else {
            self.value_to_string(key_val)
        };
        let key_id = self.interner.intern(&key_str);
        // Creating a NEW property on a non-extensible object throws.
        {
            let gk = self.interner.intern(&format!("__get_{key_str}__"));
            let sk = self.interner.intern(&format!("__set_{key_str}__"));
            let (exists, extensible) = match self.heap.get(target_oid) {
                Some(o) => (
                    o.has_own_property(key_id)
                        || o.has_own_property(gk)
                        || o.has_own_property(sk)
                        || canonical_array_index(&key_str).is_some_and(|idx| {
                            if let ObjectKind::Array(ref e) = o.kind {
                                idx < e.len() && !e[idx].is_empty_marker()
                            } else {
                                false
                            }
                        }),
                    o.extensible,
                ),
                None => (false, true),
            };
            if !exists && !extensible {
                return Err(VmError::Throw(self.make_native_error(
                    "TypeError",
                    &format!("Cannot define property {key_str}, object is not extensible"),
                )));
            }
        }
        let mut flags = Property::ALL;
        let mut value = Value::undefined();
        let mut has_value = false;
        let mut present: u8 = 0;
        let mut prior_accessor_flags: Option<u8> = None;
        // ToPropertyDescriptor: the descriptor must be an object.
        if desc_val.as_object_id().is_none() && !desc_val.is_function() {
            return Err(VmError::Throw(self.make_native_error(
                "TypeError",
                "Property description must be an object",
            )));
        }
        if let Some(desc_oid) = desc_val.as_object_id() {
            // ToPropertyDescriptor: fields read via Get (getters run) in
            // spec order — enumerable, configurable, value, writable, get, set.
            flags = 0;
            if let Some(v) = self.getter_aware_get(desc_oid, "enumerable")? {
                present |= Property::ENUMERABLE;
                if v.to_boolean() { flags |= Property::ENUMERABLE; }
            }
            if let Some(v) = self.getter_aware_get(desc_oid, "configurable")? {
                present |= Property::CONFIGURABLE;
                if v.to_boolean() { flags |= Property::CONFIGURABLE; }
            }
            if let Some(v) = self.getter_aware_get(desc_oid, "value")? {
                value = v;
                has_value = true;
            }
            if let Some(v) = self.getter_aware_get(desc_oid, "writable")? {
                present |= Property::WRITABLE;
                if v.to_boolean() { flags |= Property::WRITABLE; }
            }
            let accessor_flags = flags & (Property::ENUMERABLE | Property::CONFIGURABLE);
            // Parse get/set without applying yet — validation against an
            // existing non-configurable property must happen first.
            let mut new_getter: Option<Value> = None;
            let mut new_setter: Option<Value> = None;
            let mut desc_has_get = false;
            let mut desc_has_set = false;
            if let Some(g) = self.getter_aware_get(desc_oid, "get")? {
                desc_has_get = true;
                if self.value_callable(g) {
                    new_getter = Some(g);
                } else if !g.is_undefined() {
                    return Err(VmError::Throw(self.make_native_error(
                        "TypeError",
                        "Getter must be a function",
                    )));
                }
            }
            if let Some(sv) = self.getter_aware_get(desc_oid, "set")? {
                desc_has_set = true;
                if self.value_callable(sv) {
                    new_setter = Some(sv);
                } else if !sv.is_undefined() {
                    return Err(VmError::Throw(self.make_native_error(
                        "TypeError",
                        "Setter must be a function",
                    )));
                }
            }
            let desc_is_accessor = desc_has_get || desc_has_set;
            let desc_is_data = has_value || present & Property::WRITABLE != 0;
            // A descriptor may not mix data and accessor fields.
            if desc_is_accessor && desc_is_data {
                return Err(VmError::Throw(self.make_native_error(
                    "TypeError",
                    "Invalid property descriptor. Cannot both specify accessors and a value or writable attribute",
                )));
            }
            // Array length is a non-configurable DATA property: accessor
            // redefinition always rejects.
            if desc_is_accessor
                && key_str == "length"
                && self.heap.get(target_oid).is_some_and(|o| matches!(o.kind, ObjectKind::Array(_)))
            {
                return Err(VmError::Throw(self.make_native_error(
                    "TypeError",
                    "Cannot redefine property: length",
                )));
            }

            // ValidateAndApplyPropertyDescriptor: an existing
            // non-configurable property rejects incompatible redefines.
            let gk = self.interner.intern(&format!("__get_{key_str}__"));
            let sk = self.interner.intern(&format!("__set_{key_str}__"));
            let (ex_named, ex_get, ex_set) = match self.heap.get(target_oid) {
                Some(o) => (
                    o.get_property_descriptor(key_id).map(|p| (p.value, p.flags)),
                    o.get_property_descriptor(gk).map(|p| (p.value, p.flags)),
                    o.get_property_descriptor(sk).map(|p| (p.value, p.flags)),
                ),
                None => (None, None, None),
            };
            let ex_is_accessor = ex_get.is_some() || ex_set.is_some();
            let ex_flags = if ex_is_accessor {
                ex_get.or(ex_set).map(|(_, f)| f)
            } else {
                ex_named.map(|(_, f)| f)
            };
            if let Some(exf) = ex_flags
                && exf & Property::CONFIGURABLE == 0
            {
                let same_fn = |a: Option<Value>, b: Option<(Value, u8)>| -> bool {
                    match (a, b) {
                        (None, None) => true,
                        // `get: undefined` in the descriptor matches a stored
                        // undefined half.
                        (None, Some((y, _))) => y.is_undefined(),
                        (Some(x), Some((y, _))) => x == y,
                        (Some(_), None) => false,
                    }
                };
                let reject =
                    // Can't make it configurable again
                    (present & Property::CONFIGURABLE != 0 && flags & Property::CONFIGURABLE != 0)
                    // Can't flip enumerable
                    || (present & Property::ENUMERABLE != 0
                        && (flags ^ exf) & Property::ENUMERABLE != 0)
                    // Can't convert between data and accessor
                    || (desc_is_data && ex_is_accessor)
                    || (desc_is_accessor && !ex_is_accessor)
                    // Accessor: get/set must match exactly
                    || (ex_is_accessor
                        && ((desc_has_get && !same_fn(new_getter, ex_get))
                            || (desc_has_set && !same_fn(new_setter, ex_set))))
                    // Non-writable data: no value change, no re-enabling write
                    || (!ex_is_accessor
                        && exf & Property::WRITABLE == 0
                        && ((present & Property::WRITABLE != 0 && flags & Property::WRITABLE != 0)
                            || (has_value
                                && !ex_named.map(|(v, _)| {
                                    v == value
                                        || (v.as_number().is_some_and(f64::is_nan)
                                            && value.as_number().is_some_and(f64::is_nan))
                                }).unwrap_or(false))));
                if reject {
                    return Err(VmError::Throw(self.make_native_error(
                        "TypeError",
                        &format!("Cannot redefine property: {key_str}"),
                    )));
                }
            }

            // Attributes absent from the descriptor keep the existing
            // accessor's values (re-installing just a getter must not reset
            // enumerable/configurable).
            let mut acc_flags = accessor_flags;
            if let Some((_, exf)) = ex_get.or(ex_set) {
                for bit in [Property::ENUMERABLE, Property::CONFIGURABLE] {
                    if present & bit == 0 {
                        acc_flags = (acc_flags & !bit) | (exf & bit);
                    }
                }
            }
            // Explicit `get: undefined` / `set: undefined` still creates the
            // accessor half (an accessor property with no functions exists,
            // reads as undefined, and is redefinable with identical halves).
            // Converting an existing data entry renames it in place so the
            // property keeps its creation-order position.
            if (desc_has_get || desc_has_set)
                && let Some(obj) = self.heap.get_mut(target_oid)
            {
                let first_half = if desc_has_get { gk } else { sk };
                if !obj.has_own_property(first_half)
                    && let Some(e) = obj.properties.iter_mut().find(|e| e.0 == key_id)
                {
                    e.0 = first_half;
                }
            }
            let mut installed_accessor = false;
            if desc_has_get
                && let Some(obj) = self.heap.get_mut(target_oid) {
                    obj.define_property(
                        gk,
                        Property::with_flags(new_getter.unwrap_or(Value::undefined()), acc_flags),
                    );
                    installed_accessor = true;
                }
            if desc_has_set
                && let Some(obj) = self.heap.get_mut(target_oid) {
                    obj.define_property(
                        sk,
                        Property::with_flags(new_setter.unwrap_or(Value::undefined()), acc_flags),
                    );
                    installed_accessor = true;
                }
            // An accessor define replaces an existing data property.
            if installed_accessor
                && let Some(obj) = self.heap.get_mut(target_oid) {
                    obj.delete_property(key_id);
                }
            // A data define ({value} / {writable}) replaces an existing
            // accessor property — drop the stale halves so the new value
            // isn't shadowed (and the old getter can't resurface). The
            // converted property inherits the accessor's enumerable /
            // configurable where the descriptor is silent, and keeps the
            // accessor's POSITION (creation order survives redefinition).
            if !installed_accessor && desc_is_data && ex_is_accessor {
                prior_accessor_flags = ex_get.or(ex_set).map(|(_, f)| f);
                if let Some(obj) = self.heap.get_mut(target_oid) {
                    if let Some(entry) = obj.properties.iter_mut()
                        .find(|e| e.0 == gk || e.0 == sk)
                    {
                        entry.0 = key_id; // later define_property updates in place
                    }
                    obj.delete_property(gk);
                    obj.delete_property(sk);
                }
            }
            // A flags-only redefine of an existing accessor updates the
            // halves' attributes in place (nothing else changes).
            if !installed_accessor && !desc_is_data && !desc_is_accessor && ex_is_accessor
                && present & (Property::ENUMERABLE | Property::CONFIGURABLE) != 0
                && let Some(obj) = self.heap.get_mut(target_oid)
            {
                for k in [gk, sk] {
                    if let Some(e) = obj.properties.iter_mut().find(|e| e.0 == k) {
                        e.1 = Property::with_flags(e.1.value, acc_flags);
                    }
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
            // Attributes omitted from the descriptor keep the property's
            // CURRENT values when it already exists ({configurable: false}
            // on a normal array element leaves it writable+enumerable).
            // Array elements without a map entry default to all-true.
            let existing_flags = self.heap.get(target_oid)
                .and_then(|o| o.get_property_descriptor(key_id))
                .map(|p| p.flags)
                .or(prior_accessor_flags)
                .or_else(|| {
                    canonical_array_index(&key_str).and_then(|idx| {
                        self.heap.get(target_oid).and_then(|o| {
                            if let ObjectKind::Array(ref e) = o.kind {
                                (idx < e.len()).then_some(Property::ALL)
                            } else {
                                None
                            }
                        })
                    })
                });
            if let Some(ex) = existing_flags {
                for bit in [Property::WRITABLE, Property::ENUMERABLE, Property::CONFIGURABLE] {
                    if present & bit == 0 {
                        flags = (flags & !bit) | (ex & bit);
                    }
                }
            }
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
                    // Appending (idx == len) grows length — rejected when the
                    // length is non-writable.
                    if idx == len {
                        let ro_key = self.interner.intern("__len_ro__");
                        if self.heap.get(target_oid).is_some_and(|o| o.has_own_property(ro_key)) {
                            return Err(VmError::Throw(self.make_native_error(
                                "TypeError",
                                "Cannot add property, array length is not writable",
                            )));
                        }
                    }
                    if let Some(obj) = self.heap.get_mut(target_oid)
                        && let ObjectKind::Array(ref mut elements) = obj.kind
                    {
                        while elements.len() <= idx && elements.len() < 1_000_000 {
                            elements.push(Value::undefined());
                        }
                        if idx < elements.len() { elements[idx] = value; }
                    }
                    return Ok(target);
                }
            }
            // Defining "length" on an Array resizes element storage; a named
            // "length" property would shadow the live element count, so never
            // create one (flags-only defines leave the length as-is).
            if key_str == "length"
                && self.heap.get(target_oid).is_some_and(|o| matches!(o.kind, ObjectKind::Array(_)))
            {
                // length is a data property; accessor descriptors reject.
                let has_acc_field = desc_val.as_object_id().is_some_and(|doid| {
                    self.heap.get(doid).is_some_and(|o| {
                        let gk = self.interner.get("get");
                        let sk = self.interner.get("set");
                        gk.is_some_and(|k| o.has_own_property(k))
                            || sk.is_some_and(|k| o.has_own_property(k))
                    })
                });
                if has_acc_field {
                    return Err(VmError::Throw(self.make_native_error(
                        "TypeError",
                        "Cannot redefine property: length",
                    )));
                }
                let ro_key = self.interner.intern("__len_ro__");
                let len_ro = self.heap.get(target_oid).is_some_and(|o| o.has_own_property(ro_key));
                // length is non-configurable and non-enumerable; a descriptor
                // asking otherwise is rejected by ValidateAndApply.
                if (present & Property::CONFIGURABLE != 0 && flags & Property::CONFIGURABLE != 0)
                    || (present & Property::ENUMERABLE != 0 && flags & Property::ENUMERABLE != 0)
                {
                    return Err(VmError::Throw(self.make_native_error(
                        "TypeError",
                        "Cannot redefine property: length",
                    )));
                }
                // Non-writable length: can't be made writable again, and
                // can't change value.
                if len_ro && present & Property::WRITABLE != 0 && flags & Property::WRITABLE != 0 {
                    return Err(VmError::Throw(self.make_native_error(
                        "TypeError",
                        "Cannot redefine property: length",
                    )));
                }
                if has_value {
                    // ToNumber runs ToPrimitive observably (objects with
                    // toString/valueOf, throws propagate).
                    let prim = if value.is_object() && !value.is_symbol() {
                        self.try_coerce_to_primitive_hint(value, "number")?
                    } else {
                        value
                    };
                    let n = self.to_f64(prim);
                    let n32 = n as u32;
                    if !(n.is_finite() && n >= 0.0 && n.fract() == 0.0 && (n32 as f64) == n) {
                        return Err(VmError::Throw(
                            self.make_native_error("RangeError", "Invalid array length"),
                        ));
                    }
                    let req = n32 as usize;
                    let (cur_len, shadow_len) = self.heap.get(target_oid).map(|o| {
                        let dense = if let ObjectKind::Array(ref e) = o.kind { e.len() } else { 0 };
                        let sh = o.get_property(key_id)
                            .and_then(|v| v.as_number().or_else(|| v.as_int().map(|i| i as f64)));
                        (dense, sh)
                    }).unwrap_or((0, None));
                    let effective_len = shadow_len.map(|f| f as usize).unwrap_or(cur_len);
                    if len_ro && req != effective_len {
                        return Err(VmError::Throw(self.make_native_error(
                            "TypeError",
                            "Cannot assign to read only property 'length' of object",
                        )));
                    }
                    if req > cur_len {
                        // Growing creates HOLES, which dense storage can't
                        // represent (undefined-filling would fabricate own
                        // index properties). Store a shadowing named length
                        // instead — reads observe it, elements stay sparse.
                        if let Some(obj) = self.heap.get_mut(target_oid) {
                            obj.define_property(
                                key_id,
                                Property::with_flags(Value::number(n32 as f64), Property::WRITABLE),
                            );
                        }
                    } else if !self.array_shrink_length(target_oid, req) {
                        // A non-configurable index stopped the shrink: length
                        // lands at barrier+1 and the define reports failure.
                        // writable:false still applies on failure, per spec.
                        if present & Property::WRITABLE != 0 && flags & Property::WRITABLE == 0
                            && let Some(obj) = self.heap.get_mut(target_oid)
                        {
                            obj.define_property(ro_key, Property::with_flags(Value::boolean(true), 0));
                        }
                        return Err(VmError::Throw(self.make_native_error(
                            "TypeError",
                            "Cannot delete property of array while setting length",
                        )));
                    }
                }
                if present & Property::WRITABLE != 0 && flags & Property::WRITABLE == 0
                    && let Some(obj) = self.heap.get_mut(target_oid)
                {
                    obj.define_property(ro_key, Property::with_flags(Value::boolean(true), 0));
                }
                return Ok(target);
            }
            // Partial-flag define on an array index: flags live in the
            // property map, the VALUE stays in element storage. Keep the two
            // in sync — write the element when the descriptor carries a
            // value, and store the CURRENT element value in the map entry
            // when it doesn't (so reads via either path agree).
            let mut map_value = value;
            let mut value_resolved = has_value;
            if let Some(idx) = canonical_array_index(&key_str)
                && let Some(obj) = self.heap.get_mut(target_oid)
                && let ObjectKind::Array(ref mut elements) = obj.kind
                && idx < elements.len()
            {
                if has_value {
                    elements[idx] = value;
                } else {
                    map_value = elements[idx];
                    value_resolved = true;
                }
            }
            // A descriptor without `value` keeps the CURRENT value of an
            // existing property ({} or flags-only must not clobber it).
            if !value_resolved
                && let Some(p) = self.heap.get(target_oid).and_then(|o| o.get_property_descriptor(key_id))
            {
                map_value = p.value;
            }
            if let Some(obj) = self.heap.get_mut(target_oid) {
                obj.define_property(key_id, Property::with_flags(map_value, flags));
            }
        }
            // Defining an array index at or beyond the dense length bumps
            // the array's length per ArraySetLength (kept in the shadow
            // property so reads observe it).
            if let Some(idx) = canonical_array_index(&key_str) {
                let (is_arr, elems_len, shadow) = match self.heap.get(target_oid) {
                    Some(o) => (
                        matches!(o.kind, ObjectKind::Array(_)),
                        if let ObjectKind::Array(ref e) = o.kind { e.len() } else { 0 },
                        {
                            let lk = self.interner.intern("length");
                            o.get_property(lk).and_then(|v| v.as_number().or_else(|| v.as_int().map(|i| i as f64)))
                        },
                    ),
                    None => (false, 0, None),
                };
                let effective = shadow.unwrap_or(elems_len as f64);
                if is_arr && (idx as f64) >= effective {
                    let ro_key = self.interner.intern("__len_ro__");
                    if self.heap.get(target_oid).is_some_and(|o| o.has_own_property(ro_key)) {
                        return Err(VmError::Throw(self.make_native_error(
                            "TypeError",
                            "Cannot add property, array length is not writable",
                        )));
                    }
                    let lk = self.interner.intern("length");
                    if let Some(o) = self.heap.get_mut(target_oid) {
                        o.define_property(
                            lk,
                            Property::with_flags(Value::number((idx + 1) as f64), Property::WRITABLE),
                        );
                    }
                }
            }
        Ok(target)
    }

    /// Implements `Function(...)` and `new Function(...)`: concatenates params,
    /// compiles `function(p1,p2,...){ body }`, and returns a callable function value.
    pub(crate) fn construct_function(&mut self, args: &[Value]) -> Result<Value, VmError> {
        self.construct_function_kind(args, "function")
    }

    /// Shared Function/GeneratorFunction dynamic compilation; `keyword` is
    /// "function" or "function*".
    pub(crate) fn construct_function_kind(&mut self, args: &[Value], keyword: &str) -> Result<Value, VmError> {
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
        let src = format!("return ({}({}){{ {} }})", keyword, params_str, body_str);

        // Lex, parse, compile
        let mut lexer = crate::lexer::lexer::Lexer::new(&src, &mut self.interner);
        let tokens = lexer.tokenize();
        let mut parser = crate::parser::parser::Parser::new(tokens, &src, &mut self.interner);
        let program = parser
            .parse_program()
            .map_err(|e| format!("{e:?}"))
            .and_then(|prog| {
                // The parser error-recovers; collected errors still mean the
                // source is invalid.
                if parser.errors.is_empty() {
                    Ok(prog)
                } else {
                    Err(format!("{:?}", parser.errors[0]))
                }
            })
            .map_err(|e| {
                let err = self.make_native_error("SyntaxError", &format!("Invalid function body: {e}"));
                VmError::Throw(err)
            })?;
        let compiler = crate::compiler::compiler::Compiler::new(&mut self.interner);
        let chunk = compiler
            .compile_program(&program)
            .map_err(|e| {
                let err = self.make_native_error("SyntaxError", &format!("Invalid function body: {e}"));
                VmError::Throw(err)
            })?;
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

/// MakeDay+MakeTime+TimeClip: components (0-based month, fractional values
/// allowed per spec ToNumber results) → epoch ms. NaN in any slot → NaN.
pub(crate) fn ymd_hms_to_ms(year: f64, month0: f64, day: f64, hour: f64, min: f64, sec: f64, milli: f64) -> f64 {
    if !year.is_finite() || !month0.is_finite() || !day.is_finite()
        || !hour.is_finite() || !min.is_finite() || !sec.is_finite() || !milli.is_finite()
    {
        return f64::NAN;
    }
    // Normalize month overflow into years (days-from-civil, Howard Hinnant).
    let total_months = year.trunc() as i64 * 12 + month0.trunc() as i64;
    let y = total_months.div_euclid(12);
    let m = total_months.rem_euclid(12);
    let (yy, mm) = (y, m + 1);
    let a = if mm <= 2 { yy - 1 } else { yy };
    let era = if a >= 0 { a } else { a - 399 } / 400;
    let yoe = a - era * 400;
    let mp = (mm + 9) % 12;
    let doy = (153 * mp + 2) / 5;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days_civil = era * 146_097 + doe - 719_468;
    let ms = (days_civil as f64 + (day.trunc() - 1.0)) * 86_400_000.0
        + hour.trunc() * 3_600_000.0
        + min.trunc() * 60_000.0
        + sec.trunc() * 1000.0
        + milli.trunc();
    if ms.abs() > 8.64e15 { f64::NAN } else { ms } // TimeClip
}

pub(crate) const DAY_NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
pub(crate) const MONTH_NAMES: [&str; 12] =
    ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// Day of week for an epoch-ms instant (0 = Sunday).
pub(crate) fn epoch_weekday(ms: f64) -> i32 {
    let days = (ms / 86_400_000.0).floor() as i64;
    (((days + 4) % 7 + 7) % 7) as i32
}

/// Date.prototype.toString format (the engine is UTC-only).
pub(crate) fn format_date_tostring(ms: f64) -> String {
    if ms.is_nan() {
        return "Invalid Date".to_string();
    }
    let (y, m0, d) = epoch_to_ymd(ms);
    format!(
        "{} {} {:02} {:04} {:02}:{:02}:{:02} GMT+0000 (Coordinated Universal Time)",
        DAY_NAMES[epoch_weekday(ms) as usize],
        MONTH_NAMES[m0 as usize],
        d,
        y,
        (ms / 3_600_000.0).rem_euclid(24.0) as i32,
        (ms / 60_000.0).rem_euclid(60.0) as i32,
        (ms / 1000.0).rem_euclid(60.0) as i32,
    )
}

/// Date.parse: ISO 8601 plus the engine's own toString / toUTCString
/// formats. Unrecognized input → NaN.
pub(crate) fn parse_date_string(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        return f64::NAN;
    }
    if let Some(ms) = parse_iso_date(t) {
        return ms;
    }
    parse_textual_date(t).unwrap_or(f64::NAN)
}

/// "YYYY-MM-DD", "±YYYYYY-MM-DD", with optional "THH:MM[:SS[.mmm]]" and
/// optional "Z" / "±HH:MM" offset.
fn parse_iso_date(t: &str) -> Option<f64> {
    let (date_part, time_part) = match t.split_once('T') {
        Some((d, tm)) => (d, Some(tm)),
        None => (t, None),
    };
    // Extended years carry a mandatory sign and six digits.
    let (sign, rest) = match date_part.as_bytes().first()? {
        b'+' => (1i64, &date_part[1..]),
        b'-' => (-1i64, &date_part[1..]),
        _ => (1, date_part),
    };
    let mut dit = rest.split('-');
    let ys = dit.next()?;
    let has_sign = date_part.starts_with(['+', '-']);
    if !(ys.len() == 4 || (has_sign && ys.len() == 6)) || !ys.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let year = sign * ys.parse::<i64>().ok()?;
    // Year zero is positive by definition: "-000000" is invalid.
    if sign == -1 && year == 0 {
        return None;
    }
    let month = match dit.next() {
        Some(m) if m.len() == 2 && m.bytes().all(|b| b.is_ascii_digit()) => m.parse::<i64>().ok()?,
        Some(_) => return None,
        None => 1,
    };
    let day = match dit.next() {
        Some(d) if d.len() == 2 && d.bytes().all(|b| b.is_ascii_digit()) => d.parse::<i64>().ok()?,
        Some(_) => return None,
        None => 1,
    };
    if dit.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (mut hour, mut min, mut sec, mut milli, mut offset_min) = (0i64, 0i64, 0i64, 0i64, 0i64);
    if let Some(tm) = time_part {
        // Strip the timezone designator.
        let (clock, off): (&str, Option<(i64, &str)>) = if let Some(c) = tm.strip_suffix('Z') {
            (c, None)
        } else if let Some(pos) = tm.rfind(['+', '-']) {
            let (c, o) = tm.split_at(pos);
            let sign = if o.starts_with('-') { -1 } else { 1 };
            (c, Some((sign, &o[1..])))
        } else {
            (tm, None)
        };
        if let Some((osign, ostr)) = off {
            let (oh, om) = ostr.split_once(':')?;
            if oh.len() != 2 || om.len() != 2 {
                return None;
            }
            offset_min = osign * (oh.parse::<i64>().ok()? * 60 + om.parse::<i64>().ok()?);
        }
        let mut cit = clock.split(':');
        let hs = cit.next()?;
        if hs.len() != 2 {
            return None;
        }
        hour = hs.parse().ok()?;
        min = cit.next()?.parse().ok()?;
        if let Some(ss) = cit.next() {
            match ss.split_once('.') {
                Some((sw, sf)) => {
                    sec = sw.parse().ok()?;
                    let frac: String = sf.chars().take(3).collect();
                    if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
                        return None;
                    }
                    milli = format!("{frac:0<3}").parse().ok()?;
                }
                None => sec = ss.parse().ok()?,
            }
        }
        if cit.next().is_some() || hour > 24 || min > 59 || sec > 59 {
            return None;
        }
    }
    let ms = ymd_hms_to_ms(
        year as f64,
        (month - 1) as f64,
        day as f64,
        hour as f64,
        min as f64,
        sec as f64,
        milli as f64,
    ) - (offset_min * 60_000) as f64;
    Some(ms)
}

/// "Thu Jan 01 1970 00:00:00 GMT+0000 (...)" and
/// "Thu, 01 Jan 1970 00:00:00 GMT".
fn parse_textual_date(t: &str) -> Option<f64> {
    let toks: Vec<&str> = t.split_whitespace().collect();
    if toks.len() < 4 {
        return None;
    }
    let utc_style = toks[0].ends_with(',');
    let (mon_tok, day_tok, year_tok) = if utc_style {
        (toks[2], toks[1], toks[3])
    } else {
        (toks[1], toks[2], toks[3])
    };
    let month = MONTH_NAMES.iter().position(|m| *m == mon_tok)? as i64;
    let day: i64 = day_tok.parse().ok()?;
    let year: i64 = year_tok.parse().ok()?;
    let (mut hour, mut min, mut sec) = (0i64, 0i64, 0i64);
    let mut offset_min = 0i64;
    if let Some(clock) = toks.get(4) {
        let mut cit = clock.split(':');
        hour = cit.next()?.parse().ok()?;
        min = cit.next()?.parse().ok()?;
        sec = cit.next().unwrap_or("0").parse().ok()?;
    }
    if let Some(tz) = toks.get(5)
        && let Some(o) = tz.strip_prefix("GMT")
        && o.len() == 5
    {
        let sign = if o.starts_with('-') { -1 } else { 1 };
        let oh: i64 = o[1..3].parse().ok()?;
        let om: i64 = o[3..5].parse().ok()?;
        offset_min = sign * (oh * 60 + om);
    }
    Some(
        ymd_hms_to_ms(
            year as f64,
            month as f64,
            day as f64,
            hour as f64,
            min as f64,
            sec as f64,
            0.0,
        ) - (offset_min * 60_000) as f64,
    )
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

/// Contextually-restricted constructs found in eval code. Whether each is a
/// SyntaxError depends on where the eval was called from (EarlyErrors for
/// eval via the direct-eval additional rules).
#[derive(Default, Clone)]
pub(crate) struct EvalRestrictions {
    pub super_call: bool,
    pub super_prop: bool,
    pub new_target: bool,
    pub arguments_ref: bool,
    /// Private names (#x) referenced outside any class body in the eval
    /// code: legal only when the calling context's class chain declares them.
    pub private_names: Vec<String>,
}

/// Scan eval code for super() / super.prop / new.target / `arguments` in
/// positions that inherit the CALLING context: top-level code and arrow
/// bodies. Ordinary function, class and object-method bodies re-scope these
/// constructs, so they are not descended into.
pub(crate) fn scan_eval_restrictions(
    program: &crate::ast::node::Program,
    interner: &crate::util::interner::Interner,
) -> EvalRestrictions {
    use crate::ast::node::*;

    struct Scan<'a> {
        r: EvalRestrictions,
        it: &'a crate::util::interner::Interner,
    }

    impl Scan<'_> {
        fn stmt(&mut self, s: &Statement) {
            match s {
                Statement::Block(b) => b.body.iter().for_each(|s| self.stmt(s)),
                Statement::Variable(v) => {
                    for d in &v.declarations {
                        self.pat(&d.id);
                        if let Some(init) = &d.init { self.expr(init); }
                    }
                }
                Statement::Expression(e) => self.expr(&e.expression),
                Statement::If(i) => {
                    self.expr(&i.test);
                    self.stmt(&i.consequent);
                    if let Some(a) = &i.alternate { self.stmt(a); }
                }
                Statement::While(w) => { self.expr(&w.test); self.stmt(&w.body); }
                Statement::DoWhile(d) => { self.stmt(&d.body); self.expr(&d.test); }
                Statement::For(f) => {
                    if let Some(init) = &f.init {
                        match init {
                            ForInit::Variable(v) => {
                                for d in &v.declarations {
                                    self.pat(&d.id);
                                    if let Some(e) = &d.init { self.expr(e); }
                                }
                            }
                            ForInit::Expression(e) => self.expr(e),
                        }
                    }
                    if let Some(t) = &f.test { self.expr(t); }
                    if let Some(u) = &f.update { self.expr(u); }
                    self.stmt(&f.body);
                }
                Statement::ForIn(f) => { self.for_left(&f.left); self.expr(&f.right); self.stmt(&f.body); }
                Statement::ForOf(f) => { self.for_left(&f.left); self.expr(&f.right); self.stmt(&f.body); }
                Statement::Switch(sw) => {
                    self.expr(&sw.discriminant);
                    for c in &sw.cases {
                        if let Some(t) = &c.test { self.expr(t); }
                        c.consequent.iter().for_each(|s| self.stmt(s));
                    }
                }
                Statement::Return(r) => { if let Some(a) = &r.argument { self.expr(a); } }
                Statement::Throw(t) => self.expr(&t.argument),
                Statement::Try(t) => {
                    t.block.body.iter().for_each(|s| self.stmt(s));
                    if let Some(h) = &t.handler { h.body.body.iter().for_each(|s| self.stmt(s)); }
                    if let Some(f) = &t.finalizer { f.body.iter().for_each(|s| self.stmt(s)); }
                }
                Statement::With(w) => { self.expr(&w.object); self.stmt(&w.body); }
                Statement::Labeled(l) => self.stmt(&l.body),
                // Function/class bodies establish their own context for these
                // constructs — do not descend. (A class heritage clause does
                // evaluate in the outer context; rare enough to skip.)
                Statement::Function(_) | Statement::Class(_) => {}
                _ => {}
            }
        }

        fn for_left(&mut self, l: &ForInOfLeft) {
            match l {
                ForInOfLeft::Variable(v) => {
                    for d in &v.declarations { self.pat(&d.id); }
                }
                ForInOfLeft::Pattern(p) => self.pat(p),
                ForInOfLeft::Expression(e) => self.expr(e),
            }
        }

        fn pat(&mut self, p: &Pattern) {
            match p {
                Pattern::Assignment(a) => { self.pat(&a.left); self.expr(&a.right); }
                Pattern::Array(arr) => {
                    for e in arr.elements.iter().flatten() { self.pat(e); }
                }
                Pattern::Rest(r) => self.pat(&r.argument),
                _ => {}
            }
        }

        fn expr(&mut self, e: &Expression) {
            match e {
                Expression::Identifier(id) => {
                    let name = self.it.resolve(id.name);
                    if name == "arguments" {
                        self.r.arguments_ref = true;
                    } else if name.starts_with('#') {
                        // `#x in o` parses the private name as an identifier.
                        self.r.private_names.push(name.to_owned());
                    }
                }
                Expression::Call(c) => {
                    if matches!(&c.callee, Expression::Super(_)) {
                        self.r.super_call = true;
                    } else {
                        self.expr(&c.callee);
                    }
                    c.arguments.iter().for_each(|a| self.expr(a));
                }
                Expression::New(n) => {
                    self.expr(&n.callee);
                    n.arguments.iter().for_each(|a| self.expr(a));
                }
                Expression::Member(m) => {
                    if matches!(&m.object, Expression::Super(_)) {
                        self.r.super_prop = true;
                    } else {
                        self.expr(&m.object);
                    }
                    match &m.property {
                        MemberProperty::Expression(k) => self.expr(k),
                        MemberProperty::PrivateIdentifier(sid) => {
                            self.r.private_names.push(self.it.resolve(*sid).to_owned());
                        }
                        MemberProperty::Identifier(_) => {}
                    }
                }
                Expression::MetaProperty(mp) => {
                    if self.it.resolve(mp.meta) == "new" && self.it.resolve(mp.property) == "target" {
                        self.r.new_target = true;
                    }
                }
                Expression::ArrowFunction(a) => {
                    // Arrows inherit super/new.target/arguments from the
                    // surrounding (eval) context — descend.
                    for p in &a.params { self.pat(p); }
                    match &a.body {
                        ArrowBody::Expression(e) => self.expr(e),
                        ArrowBody::Block(b) => b.body.iter().for_each(|s| self.stmt(s)),
                    }
                }
                Expression::Unary(u) => self.expr(&u.argument),
                Expression::Update(u) => self.expr(&u.argument),
                Expression::Binary(b) => { self.expr(&b.left); self.expr(&b.right); }
                Expression::Logical(l) => { self.expr(&l.left); self.expr(&l.right); }
                Expression::Conditional(c) => { self.expr(&c.test); self.expr(&c.consequent); self.expr(&c.alternate); }
                Expression::Assignment(a) => {
                    match &a.left {
                        AssignmentTarget::Identifier(id) => {
                            if self.it.resolve(id.name) == "arguments" { self.r.arguments_ref = true; }
                        }
                        AssignmentTarget::Member(m) => {
                            if matches!(&m.object, Expression::Super(_)) {
                                self.r.super_prop = true;
                            } else {
                                self.expr(&m.object);
                            }
                            if let MemberProperty::Expression(k) = &m.property { self.expr(k); }
                        }
                        AssignmentTarget::Pattern(p) => self.pat(p),
                    }
                    self.expr(&a.right);
                }
                Expression::Sequence(s) => s.expressions.iter().for_each(|e| self.expr(e)),
                Expression::Array(arr) => {
                    for el in arr.elements.iter().flatten() { self.expr(el); }
                }
                Expression::Object(o) => {
                    for p in &o.properties {
                        match p {
                            ObjectProperty::Property(prop) => {
                                if let PropertyKey::Computed(k) = &prop.key { self.expr(k); }
                                // Method values (function exprs) are skipped by
                                // the Function arm below; plain values descend.
                                self.expr(&prop.value);
                            }
                            ObjectProperty::SpreadElement(sp) => self.expr(&sp.argument),
                        }
                    }
                }
                Expression::Spread(sp) => self.expr(&sp.argument),
                Expression::Yield(y) => { if let Some(a) = &y.argument { self.expr(a); } }
                Expression::Await(a) => self.expr(&a.argument),
                Expression::TemplateLiteral(t) => t.expressions.iter().for_each(|e| self.expr(e)),
                Expression::TaggedTemplate(t) => {
                    self.expr(&t.tag);
                    t.quasi.expressions.iter().for_each(|e| self.expr(e));
                }
                Expression::OptionalChain(oc) => {
                    if matches!(&oc.base, Expression::Super(_)) {
                        self.r.super_prop = true;
                    } else {
                        self.expr(&oc.base);
                    }
                    for el in &oc.chain {
                        match el {
                            OptionalChainElement::Member { property, .. } => {
                                if let MemberProperty::Expression(k) = property { self.expr(k); }
                            }
                            OptionalChainElement::Call { arguments, .. } => {
                                arguments.iter().for_each(|a| self.expr(a));
                            }
                        }
                    }
                }
                // Function/class/method bodies re-scope these constructs.
                Expression::Function(_) | Expression::Class(_) => {}
                _ => {}
            }
        }
    }

    let mut scan = Scan { r: EvalRestrictions::default(), it: interner };
    for s in &program.body {
        scan.stmt(s);
    }
    scan.r
}

impl Vm {

    /// Storage key for an instance-field initializer on the class object.
    /// A redeclared field (`class C { y = a; y = b; }`) must keep BOTH
    /// initializers (each runs, in order, the last value wins), so duplicates
    /// get an ordinal suffix separated by \u{1} that install-time strips.
    pub(crate) fn ifield_store_key(
        &mut self,
        class_oid: ObjectId,
        field_name: &str,
    ) -> crate::util::interner::StringId {
        let base = self.interner.intern(&format!("__ifield_{field_name}__"));
        let taken = self.heap.get(class_oid)
            .is_some_and(|o| o.get_property(base).is_some());
        if !taken {
            return base;
        }
        let mut n = 1usize;
        loop {
            let key = self.interner.intern(&format!("__ifield_{field_name}{SEP}{n}__", SEP = '\u{1}'));
            let used = self.heap.get(class_oid)
                .is_some_and(|o| o.get_property(key).is_some());
            if !used {
                return key;
            }
            n += 1;
        }
    }

    /// Record that `method_val` (a closure installed on a class: method,
    /// accessor, constructor or field-initializer thunk) belongs to the class
    /// evaluation `class_oid`: prepend that class to the closure's inherited
    /// private-environment chain.
    pub(crate) fn register_class_closure(&mut self, method_val: Value, class_oid: ObjectId) {
        if let Some(packed) = method_val.as_function()
            && packed >= 0
        {
            let cid = ((packed as u32) >> 16) as usize;
            if cid != 0 {
                let mut env = vec![class_oid];
                if let Some(existing) = self.closure_private_env.get(&cid) {
                    env.extend(existing.iter().copied().filter(|c| *c != class_oid));
                }
                self.closure_private_env.insert(cid, std::rc::Rc::new(env));
            }
        }
    }

    /// Whether the class evaluation `class_oid` declares the private name
    /// `name` (e.g. "#x"): any own instance-field initializer, private
    /// method/field or private accessor stored under the mangled keys on the
    /// class object or its prototype.
    pub(crate) fn class_declares_private(&mut self, class_oid: ObjectId, name: &str) -> bool {
        let priv_key = self.interner.intern(&format!("__priv_{name}__"));
        let get_key = self.interner.intern(&format!("__get_{name}__"));
        let set_key = self.interner.intern(&format!("__set_{name}__"));
        let ifield_prefix = format!("__ifield_{name}");
        let proto_key = self.interner.intern("prototype");
        let on_class = self.heap.get(class_oid).is_some_and(|o| {
            o.get_property(priv_key).is_some()
                || o.get_property(get_key).is_some()
                || o.get_property(set_key).is_some()
                || o.properties.iter().any(|(k, _)| {
                    let ks = self.interner.resolve(*k);
                    ks.starts_with(&ifield_prefix)
                        && (ks.len() == ifield_prefix.len() + 2 // __ifield_#x__
                            || ks.as_bytes().get(ifield_prefix.len()) == Some(&1)) // ordinal
                })
        });
        if on_class {
            return true;
        }
        let proto_oid = self.heap.get(class_oid)
            .and_then(|o| o.get_property(proto_key))
            .and_then(|v| v.as_object_id());
        proto_oid
            .and_then(|p| self.heap.get(p))
            .is_some_and(|o| {
                o.get_property(priv_key).is_some()
                    || o.get_property(get_key).is_some()
                    || o.get_property(set_key).is_some()
            })
    }

    /// Evaluation-identity brand check for the currently executing closure.
    /// The private name binds to the INNERMOST class evaluation on the
    /// closure's lexical private-environment chain that declares it (an
    /// inner class's #x shadows the outer's); the receiver must carry that
    /// exact evaluation's brand. Returns true when the executing code has no
    /// chain or no chain class declares the name (fall back to key-presence
    /// semantics).
    pub(crate) fn private_access_allowed(&mut self, oid: ObjectId, name: &str) -> bool {
        let env = self.frames.last()
            .filter(|f| f.base > 0)
            .and_then(|f| self.stack.get(f.base - 1))
            .and_then(|v| v.as_function())
            .filter(|p| *p >= 0)
            .map(|p| ((p as u32) >> 16) as usize)
            .filter(|cid| *cid != 0)
            .and_then(|cid| self.closure_private_env.get(&cid).cloned());
        let Some(env) = env else { return true };
        for &c in env.iter() {
            if self.class_declares_private(c, name) {
                return self.object_branded_by(oid, c);
            }
        }
        true
    }

    /// Whether `oid` carries the private brand of the class evaluation
    /// `class_oid`: it is the class itself (static access), or an instance
    /// whose construction chain (__class__ then __super__ links) includes
    /// that exact class object.
    pub(crate) fn object_branded_by(&mut self, oid: ObjectId, class_oid: ObjectId) -> bool {
        if oid == class_oid {
            return true;
        }
        let class_key = self.interner.intern("__class__");
        let super_key = self.interner.intern("__super__");
        let mut cur = self.heap.get(oid)
            .and_then(|o| o.get_property(class_key))
            .and_then(|v| v.as_object_id());
        let mut hops = 0;
        while let Some(c) = cur {
            if c == class_oid {
                return true;
            }
            hops += 1;
            if hops > 128 {
                break;
            }
            cur = self.heap.get(c)
                .and_then(|o| o.get_property(super_key))
                .and_then(|v| v.as_object_id());
        }
        false
    }
}

impl Vm {
    /// Milliseconds since epoch from Date constructor arguments:
    /// () → now, (ms) → ms, (year, month[, day, h, m, s, ms]) → local
    /// components (treated as UTC — the VM has no timezone).
    pub(crate) fn date_ms_from_args(&mut self, args: &[Value]) -> f64 {
        match args.len() {
            0 => std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as f64)
                .unwrap_or(0.0),
            1 => {
                // ToPrimitive; a string parses like Date.parse, anything
                // else goes through ToNumber + TimeClip.
                let prim = self
                    .try_coerce_to_primitive_hint(args[0], "default")
                    .unwrap_or(args[0]);
                if prim.is_string() || self.is_cons_string(prim) {
                    let s = self.value_to_string(prim);
                    parse_date_string(&s)
                } else {
                    let n = self.to_f64(prim);
                    if n.is_finite() && n.abs() <= 8.64e15 { n.trunc() } else { f64::NAN }
                }
            }
            _ => {
                let num = |vm: &mut Self, i: usize, default: f64| -> f64 {
                    args.get(i).map(|v| vm.to_f64(*v)).unwrap_or(default)
                };
                let mut year = num(self, 0, 0.0);
                let month = num(self, 1, 0.0);
                let day = num(self, 2, 1.0);
                let hour = num(self, 3, 0.0);
                let min = num(self, 4, 0.0);
                let sec = num(self, 5, 0.0);
                let ms = num(self, 6, 0.0);
                if !year.is_finite() || !month.is_finite() || !day.is_finite() {
                    return f64::NAN;
                }
                if (0.0..=99.0).contains(&year) {
                    year += 1900.0;
                }
                // Normalize month overflow into years.
                let total_months = year as i64 * 12 + month as i64;
                let y = total_months.div_euclid(12);
                let m = total_months.rem_euclid(12); // 0-based month
                // days-from-civil (Howard Hinnant's algorithm), month 1-based
                let (yy, mm) = (y, m + 1);
                let a = if mm <= 2 { yy - 1 } else { yy };
                let era = if a >= 0 { a } else { a - 399 } / 400;
                let yoe = a - era * 400;
                let mp = (mm + 9) % 12;
                let doy = (153 * mp + 2) / 5;
                let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
                let days_civil = era * 146_097 + doe - 719_468; // days since 1970-01-01 for day 1
                let days = days_civil as f64 + (day - 1.0);
                days * 86_400_000.0
                    + hour * 3_600_000.0
                    + min * 60_000.0
                    + sec * 1000.0
                    + ms
            }
        }
    }

    /// Internal kind for `this` when a class extends a native built-in:
    /// mirrors what `new Builtin(...args)` would produce. Used by both the
    /// explicit super(...) path and the implicit default constructor.
    pub(crate) fn native_subclass_kind(&mut self, sentinel: i32, args: &[Value]) -> Option<ObjectKind> {
        match sentinel {
            // Array
            -507 => {
                let elements: Vec<Value> = if args.len() == 1 {
                    let only = args[0];
                    if let Some(n) = only.as_number()
                        && n.is_finite() && n.fract() == 0.0 && (0.0..=u32::MAX as f64).contains(&n)
                    {
                        vec![Value::empty(); (n as usize).min(1_000_000)]
                    } else if let Some(n) = only.as_int() {
                        if n >= 0 { vec![Value::empty(); (n as usize).min(1_000_000)] } else { vec![only] }
                    } else {
                        vec![only]
                    }
                } else {
                    args.to_vec()
                };
                Some(ObjectKind::Array(elements))
            }
            // String / Number / Boolean wrappers
            -504 => {
                let s = if args.is_empty() { String::new() } else { self.value_to_string(args[0]) };
                let id = self.interner.intern(&s);
                Some(ObjectKind::Wrapper(Value::string(id)))
            }
            -505 => {
                let n = if args.is_empty() { 0.0 } else { self.to_f64(args[0]) };
                Some(ObjectKind::Wrapper(Value::number(n)))
            }
            -506 => {
                let b = if args.is_empty() { false } else { self.truthy(args[0]) };
                Some(ObjectKind::Wrapper(Value::boolean(b)))
            }
            // Date
            -550 => Some(ObjectKind::Date(self.date_ms_from_args(args))),
            // RegExp
            -580 => {
                let pattern = if !args.is_empty() { self.value_to_string(args[0]) } else { String::new() };
                let flags = if args.len() > 1 { self.value_to_string(args[1]) } else { String::new() };
                Some(ObjectKind::RegExp { pattern, flags })
            }
            // Map: entries iterable — array of entry OBJECTS, each read via
            // its "0" / "1" properties (array pairs included).
            -540 => {
                let mut entries = Vec::new();
                if let Some(arr_oid) = args.first().and_then(|v| v.as_object_id())
                    && let Some(obj) = self.heap.get(arr_oid)
                    && let ObjectKind::Array(ref elems) = obj.kind
                {
                    let elems = elems.clone();
                    let zero = self.interner.intern("0");
                    let one = self.interner.intern("1");
                    for elem in &elems {
                        if let Some(pair_oid) = elem.as_object_id() {
                            if let Some(pair_obj) = self.heap.get(pair_oid)
                                && let ObjectKind::Array(ref pair) = pair_obj.kind
                            {
                                let k = pair.first().copied().unwrap_or(Value::undefined());
                                let v = pair.get(1).copied().unwrap_or(Value::undefined());
                                entries.push((k, v));
                            } else {
                                let k = self.heap.get(pair_oid)
                                    .and_then(|o| o.get_property(zero))
                                    .unwrap_or(Value::undefined());
                                let v = self.heap.get(pair_oid)
                                    .and_then(|o| o.get_property(one))
                                    .unwrap_or(Value::undefined());
                                entries.push((k, v));
                            }
                        }
                    }
                }
                Some(ObjectKind::Map { entries })
            }
            -541 => {
                let mut entries = Vec::new();
                if let Some(arr_oid) = args.first().and_then(|v| v.as_object_id())
                    && let Some(obj) = self.heap.get(arr_oid)
                    && let ObjectKind::Array(ref elems) = obj.kind
                {
                    entries = elems.clone();
                }
                Some(ObjectKind::Set { entries })
            }
            -542 => Some(ObjectKind::WeakMap { entries: Vec::new() }),
            -543 => Some(ObjectKind::WeakSet { entries: Vec::new() }),
            _ => None,
        }
    }
}

impl Vm {
    /// Materialize (or reuse) the `arguments` object of the nearest
    /// non-arrow frame, caching it on that frame so identity holds.
    pub(crate) fn materialize_enclosing_arguments(&mut self) -> Value {
        let mut frame_idx = self.frames.len() - 1;
        while frame_idx > 0
            && self.chunks[self.frames[frame_idx].chunk_idx]
                .flags
                .contains(crate::compiler::chunk::ChunkFlags::ARROW)
        {
            frame_idx -= 1;
        }
        let cached = self.frames[frame_idx].arguments_oid;
        let oid = if let Some(oid) = cached {
            oid
        } else {
            let mut args = self.frames[frame_idx].saved_args.clone();
            let chunk_idx = self.frames[frame_idx].chunk_idx;
            // Mapped portion (sloppy functions): parameters may have been
            // reassigned before `arguments` materialized — read the LIVE
            // slots, not the call-time snapshot.
            if !self.chunks[chunk_idx].flags.contains(crate::compiler::chunk::ChunkFlags::STRICT)
                && self.chunks[chunk_idx].simple_params
            {
                let base = self.frames[frame_idx].base;
                let pc = self.chunks[chunk_idx].param_count as usize;
                for (i, slot) in args.iter_mut().enumerate().take(pc) {
                    if let Some(v) = self.stack.get(base + i)
                        && !v.is_empty_marker()
                    {
                        *slot = *v;
                    }
                }
            }
            let is_strict = self.chunks[chunk_idx]
                .flags
                .contains(crate::compiler::chunk::ChunkFlags::STRICT);
            let mut arr = JsObject::array(args);
            // Per spec, arguments has Object.prototype (not Array.prototype).
            arr.prototype = Some(self.object_prototype);
            // arguments[Symbol.iterator] is Array.prototype.values, so
            // `for (x of arguments)` and `[...arguments]` work.
            let sym_iter_key = self.interner.intern(&format!("__sym_{}__", self.sym_iterator));
            arr.define_property(
                sym_iter_key,
                crate::runtime::object::Property::with_flags(
                    Value::function(-626),
                    crate::runtime::object::Property::WRITABLE
                        | crate::runtime::object::Property::CONFIGURABLE,
                ),
            );
            if !is_strict {
                let callee_key = self.interner.intern("callee");
                arr.define_property(
                    callee_key,
                    crate::runtime::object::Property::with_flags(
                        Value::function(chunk_idx as i32),
                        crate::runtime::object::Property::WRITABLE
                            | crate::runtime::object::Property::CONFIGURABLE,
                    ),
                );
            } else {
                // Strict arguments: callee is the %ThrowTypeError% poison
                // accessor (get === set, non-enumerable, non-configurable).
                let tte = self.throw_type_error_fn();
                let get_key = self.interner.intern("__get_callee__");
                let set_key = self.interner.intern("__set_callee__");
                arr.define_property(get_key, crate::runtime::object::Property::with_flags(tte, 0));
                arr.define_property(set_key, crate::runtime::object::Property::with_flags(tte, 0));
            }
            let oid = self.heap.allocate(arr);
            self.frames[frame_idx].arguments_oid = Some(oid);
            oid
        };
        Value::object_id(oid)
    }
}

impl Vm {
    /// Mapped-arguments aliasing (sloppy functions): writing `arguments[idx]`
    /// while its creating frame is live also writes the parameter slot.
    /// Returns true when the object is a live frame's arguments object.
    pub(crate) fn sync_mapped_argument_to_param(
        &mut self,
        oid: ObjectId,
        idx: usize,
        val: Value,
    ) -> bool {
        let Some(fi) = (0..self.frames.len())
            .rev()
            .find(|&i| self.frames[i].arguments_oid == Some(oid))
        else {
            return false;
        };
        let chunk_idx = self.frames[fi].chunk_idx;
        if self.chunks[chunk_idx].flags.contains(crate::compiler::chunk::ChunkFlags::STRICT)
            || !self.chunks[chunk_idx].simple_params
        {
            return false;
        }
        if idx >= self.chunks[chunk_idx].param_count as usize {
            return true; // arguments object, but index beyond the mapping
        }
        // An index redefined non-writable — or deleted — is unmapped.
        let key_id = self.interner.intern(&idx.to_string());
        let tombstone = self.interner.intern(&format!("__argmap_del_{idx}__"));
        let unmapped = self.heap.get(oid).is_some_and(|o| {
            o.get_property(tombstone).is_some()
                || o.get_property_descriptor(key_id)
                    .is_some_and(|p| !p.is_writable())
        });
        if unmapped {
            return true;
        }
        let pos = self.frames[fi].base + idx;
        if pos < self.stack.len() {
            self.stack[pos] = val;
        }
        true
    }

    /// Mapped-arguments aliasing, parameter side: writing a parameter slot of
    /// a sloppy function with a materialized arguments object also writes
    /// `arguments[slot]` (unless that index was unmapped by defineProperty).
    pub(crate) fn sync_param_to_mapped_argument(&mut self, slot: usize, val: Value) {
        let Some(f) = self.frames.last() else { return };
        let Some(aoid) = f.arguments_oid else { return };
        let chunk_idx = f.chunk_idx;
        if slot >= self.chunks[chunk_idx].param_count as usize
            || self.chunks[chunk_idx].flags.contains(crate::compiler::chunk::ChunkFlags::STRICT)
            || !self.chunks[chunk_idx].simple_params
        {
            return;
        }
        let key_id = self.interner.intern(&slot.to_string());
        let tombstone = self.interner.intern(&format!("__argmap_del_{slot}__"));
        if self.heap.get(aoid).is_some_and(|o| o.get_property(tombstone).is_some()) {
            return; // deleted → unmapped
        }
        let named = self.heap.get(aoid).and_then(|o| o.get_property_descriptor(key_id));
        if named.is_some_and(|p| !p.is_writable()) {
            return; // unmapped
        }
        if let Some(obj) = self.heap.get_mut(aoid) {
            if let ObjectKind::Array(ref mut e) = obj.kind
                && slot < e.len()
            {
                e[slot] = val;
            }
            if named.is_some() {
                obj.set_property(key_id, val);
            }
        }
    }
}

impl Vm {
    /// Lazily materialize a builtin prototype method as a real, identity-
    /// cached function property. Instance calls keep dispatching natively;
    /// this covers VALUE reads — `typeof String.prototype.charAt`,
    /// `var push = Array.prototype.push; push.call(arr, x)` — which
    /// otherwise saw undefined because the prototypes are empty objects.
    /// Reflection helper: before hasOwnProperty / getOwnPropertyDescriptor
    /// inspect a tracked builtin prototype, materialize the queried method so
    /// lazily-reified prototypes (Date) answer like eagerly-populated ones.
    pub(crate) fn ensure_builtin_proto_method(&mut self, oid: ObjectId, name_id: StringId) {
        if (oid == self.string_prototype
            || oid == self.array_prototype
            || oid == self.object_prototype
            || oid == self.number_prototype
            || oid == self.boolean_prototype
            || oid == self.date_prototype)
            && !self.heap.get(oid).is_some_and(|o| o.has_own_property(name_id))
        {
            let _ = self.reify_builtin_proto_method(oid, name_id);
        }
    }

    /// ensure_builtin_proto_method along the whole prototype chain — for
    /// lookups (ToPrimitive's toString/valueOf) that use get_property_chain
    /// and would otherwise miss lazily-reified builtin methods.
    pub(crate) fn ensure_chain_method(&mut self, oid: ObjectId, name_id: StringId) {
        let mut cur = Some(oid);
        let mut hops = 0;
        while let Some(c) = cur {
            self.ensure_builtin_proto_method(c, name_id);
            cur = self.heap.get(c).and_then(|o| o.prototype);
            hops += 1;
            if hops > 8 {
                break;
            }
        }
    }

    pub(crate) fn reify_builtin_proto_method(
        &mut self,
        oid: ObjectId,
        name_id: StringId,
    ) -> Option<Value> {
        const STRING_METHODS: &[(&str, i32)] = &[
            ("at", 1), ("charAt", 1), ("charCodeAt", 1), ("codePointAt", 1),
            ("concat", 1), ("endsWith", 1), ("includes", 1), ("indexOf", 1),
            ("lastIndexOf", 1), ("localeCompare", 1), ("toLocaleLowerCase", 0),
            ("toLocaleUpperCase", 0),
            ("match", 1), ("matchAll", 1), ("normalize", 0),
            ("padEnd", 1), ("padStart", 1), ("repeat", 1), ("replace", 2),
            ("slice", 2), ("split", 2), ("startsWith", 1), ("substr", 2),
            ("substring", 2), ("toLowerCase", 0), ("toString", 0),
            ("toUpperCase", 0), ("trim", 0), ("trimEnd", 0), ("trimStart", 0),
            ("isWellFormed", 0), ("toWellFormed", 0),
        ];
        const ARRAY_METHODS: &[(&str, i32)] = &[
            ("at", 1), ("concat", 1), ("copyWithin", 2), ("every", 1),
            ("fill", 1), ("filter", 1), ("find", 1), ("findIndex", 1),
            ("findLast", 1), ("findLastIndex", 1), ("flat", 0), ("flatMap", 1),
            ("forEach", 1), ("includes", 1), ("indexOf", 1), ("join", 1),
            ("keys", 0), ("lastIndexOf", 1), ("map", 1), ("pop", 0),
            ("push", 1), ("reduce", 1), ("reduceRight", 1), ("reverse", 0),
            ("shift", 0), ("slice", 2), ("some", 1), ("sort", 1),
            ("splice", 2), ("toLocaleString", 0), ("toReversed", 0),
            ("toSorted", 1), ("toSpliced", 2), ("toString", 0),
            ("unshift", 1), ("with", 2),
        ];
        // Object.prototype methods route through their exec_native_method
        // sentinels; Boolean.prototype through -630/-631.
        const OBJECT_METHODS: &[(&str, i32, i32)] = &[
            ("hasOwnProperty", 1, -590), ("propertyIsEnumerable", 1, -591),
            ("toString", 0, -592), ("valueOf", 0, -593),
            ("isPrototypeOf", 1, -594),
        ];
        const NUMBER_METHODS: &[(&str, i32)] = &[
            ("toString", 1), ("toLocaleString", 0), ("valueOf", 0),
            ("toFixed", 1), ("toExponential", 1), ("toPrecision", 1),
        ];
        const BOOLEAN_METHODS: &[(&str, i32, i32)] =
            &[("toString", 0, -630), ("valueOf", 0, -631)];
        const DATE_METHODS: &[(&str, i32)] = &[
            ("getTime", 0), ("valueOf", 0), ("getFullYear", 0), ("getUTCFullYear", 0),
            ("getMonth", 0), ("getUTCMonth", 0), ("getDate", 0), ("getUTCDate", 0),
            ("getDay", 0), ("getUTCDay", 0), ("getHours", 0), ("getUTCHours", 0),
            ("getMinutes", 0), ("getUTCMinutes", 0), ("getSeconds", 0), ("getUTCSeconds", 0),
            ("getMilliseconds", 0), ("getUTCMilliseconds", 0), ("getTimezoneOffset", 0),
            ("getYear", 0), ("setYear", 1),
            ("setTime", 1), ("setMilliseconds", 1), ("setUTCMilliseconds", 1),
            ("setSeconds", 2), ("setUTCSeconds", 2), ("setMinutes", 3), ("setUTCMinutes", 3),
            ("setHours", 4), ("setUTCHours", 4), ("setDate", 1), ("setUTCDate", 1),
            ("setMonth", 2), ("setUTCMonth", 2), ("setFullYear", 3), ("setUTCFullYear", 3),
            ("toString", 0), ("toDateString", 0), ("toTimeString", 0), ("toISOString", 0),
            ("toUTCString", 0), ("toGMTString", 0), ("toJSON", 1),
            ("toLocaleString", 0), ("toLocaleDateString", 0), ("toLocaleTimeString", 0),
        ];

        enum Route {
            String,
            Array,
            Sentinel(i32),
            /// Sentinel dispatch behind a RequireObjectCoercible check
            /// (Object.prototype.toLocaleString → this.toString()).
            CoercibleSentinel(i32),
            Number,
            Date,
        }
        let name = self.interner.resolve(name_id).to_owned();
        let entry: Option<(Route, i32)> = if oid == self.string_prototype {
            STRING_METHODS.iter().find(|(n, _)| *n == name).map(|(_, l)| (Route::String, *l))
        } else if oid == self.array_prototype {
            ARRAY_METHODS.iter().find(|(n, _)| *n == name).map(|(_, l)| (Route::Array, *l))
        } else if oid == self.object_prototype {
            if name == "toLocaleString" {
                Some((Route::CoercibleSentinel(-592), 0))
            } else {
                OBJECT_METHODS.iter().find(|(n, _, _)| *n == name).map(|(_, l, s)| (Route::Sentinel(*s), *l))
            }
        } else if oid == self.number_prototype {
            NUMBER_METHODS.iter().find(|(n, _)| *n == name).map(|(_, l)| (Route::Number, *l))
        } else if oid == self.boolean_prototype {
            BOOLEAN_METHODS.iter().find(|(n, _, _)| *n == name).map(|(_, l, s)| (Route::Sentinel(*s), *l))
        } else if oid == self.date_prototype {
            let lookup = if name == "toGMTString" { "toUTCString" } else { name.as_str() };
            DATE_METHODS.iter().find(|(n, _)| *n == lookup).map(|(_, l)| (Route::Date, *l))
        } else {
            None
        };
        let (route, fn_len) = entry?;
        let func: crate::runtime::object::NativeFn = match route {
            Route::String => std::sync::Arc::new(move |vm: &mut Vm, this: Value, args: &[Value]| {
                // RequireObjectCoercible: String.prototype methods reject
                // null/undefined receivers; ToString(Symbol) throws.
                if this.is_nullish() {
                    return Err(vm.make_native_error(
                        "TypeError",
                        "String.prototype method called on null or undefined",
                    ));
                }
                if this.is_symbol() {
                    return Err(vm.make_native_error(
                        "TypeError",
                        "Cannot convert a Symbol value to a string",
                    ));
                }
                // ToString(this): object receivers run their toString
                // (String.prototype.split.call(Math, …) sees "[object Math]").
                let prim = if this.is_object() && !this.is_symbol() {
                    match vm.try_coerce_to_primitive_hint(this, "string") {
                        Ok(p) => p,
                        Err(VmError::Throw(v)) => return Err(v),
                        Err(_) => this,
                    }
                } else {
                    this
                };
                let s = vm.value_to_string(prim);
                let ascii = s.is_ascii();
                match vm.exec_string_method(&s, name_id, args, ascii) {
                    Ok(v) => Ok(v),
                    Err(VmError::Throw(v)) => Err(v),
                    Err(e) => Err(vm.make_native_error("Error", &format!("{e:?}"))),
                }
            }),
            Route::Array => std::sync::Arc::new(move |vm: &mut Vm, this: Value, args: &[Value]| {
                // ToObject(this): primitives box (Boolean/Number/String
                // wrappers are legal array-like receivers).
                if this.is_nullish() {
                    return Err(vm.make_native_error(
                        "TypeError",
                        "Array.prototype method called on null or undefined",
                    ));
                }
                let this = if this.as_object_id().is_none() { vm.box_primitive(this) } else { this };
                let Some(this_oid) = this.as_object_id() else {
                    return Err(vm.make_native_error(
                        "TypeError",
                        "Array.prototype method called on incompatible receiver",
                    ));
                };
                match vm.exec_array_method(this_oid, name_id, args) {
                    Ok(v) => Ok(v),
                    Err(VmError::Throw(v)) => Err(v),
                    Err(e) => Err(vm.make_native_error("Error", &format!("{e:?}"))),
                }
            }),
            Route::Sentinel(sent) => std::sync::Arc::new(move |vm: &mut Vm, this: Value, args: &[Value]| {
                match vm.exec_native_method(sent, this, args) {
                    Ok(v) => Ok(v),
                    Err(VmError::Throw(v)) => Err(v),
                    Err(e) => Err(vm.make_native_error("Error", &format!("{e:?}"))),
                }
            }),
            Route::CoercibleSentinel(sent) => std::sync::Arc::new(move |vm: &mut Vm, this: Value, args: &[Value]| {
                if this.is_nullish() {
                    return Err(vm.make_native_error(
                        "TypeError",
                        "Cannot convert undefined or null to object",
                    ));
                }
                match vm.exec_native_method(sent, this, args) {
                    Ok(v) => Ok(v),
                    Err(VmError::Throw(v)) => Err(v),
                    Err(e) => Err(vm.make_native_error("Error", &format!("{e:?}"))),
                }
            }),
            Route::Date => {
                // thisTimeValue: only Date receivers; toGMTString aliases
                // toUTCString (same behavior, own name "toGMTString").
                let method = if name == "toGMTString" { "toUTCString".to_owned() } else { name.clone() };
                std::sync::Arc::new(move |vm: &mut Vm, this: Value, args: &[Value]| {
                    let Some(this_oid) = this.as_object_id() else {
                        return Err(vm.make_native_error(
                            "TypeError",
                            "Date.prototype method called on incompatible receiver",
                        ));
                    };
                    match vm.exec_date_method(this_oid, &method, args) {
                        Ok(v) => Ok(v),
                        Err(VmError::Throw(v)) => Err(v),
                        Err(e) => Err(vm.make_native_error("Error", &format!("{e:?}"))),
                    }
                })
            }
            Route::Number => std::sync::Arc::new(move |vm: &mut Vm, this: Value, args: &[Value]| {
                // thisNumberValue: only Number primitives / wrappers.
                let inner = vm.unwrap_wrapper_primitive(this, |v| v.is_int() || v.is_number());
                let Some(inner) = inner else {
                    return Err(vm.make_native_error(
                        "TypeError",
                        "Number.prototype method called on incompatible receiver",
                    ));
                };
                match vm.exec_number_method(inner, name_id, args) {
                    Ok(v) => Ok(v),
                    Err(VmError::Throw(v)) => Err(v),
                    Err(e) => Err(vm.make_native_error("Error", &format!("{e:?}"))),
                }
            }),
        };
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
        // Real own `name` / `length` with spec attributes so verifyProperty
        // and hasOwnProperty checks pass.
        let name_key = self.interner.intern("name");
        let len_key = self.interner.intern("length");
        fn_obj.define_property(
            name_key,
            Property::with_flags(Value::string(name_id), Property::CONFIGURABLE),
        );
        fn_obj.define_property(
            len_key,
            Property::with_flags(Value::int(fn_len), Property::CONFIGURABLE),
        );
        let f_oid = self.heap.allocate(fn_obj);
        let val = Value::object_id(f_oid);
        if let Some(proto) = self.heap.get_mut(oid) {
            proto.define_property(
                name_id,
                Property::with_flags(val, Property::WRITABLE | Property::CONFIGURABLE),
            );
        }
        Some(val)
    }
}

impl Vm {
    /// Index of the innermost non-arrow frame: `super()` / super-state flags
    /// inside arrow functions belong to the enclosing constructor's frame,
    /// not the arrow's own.
    pub(crate) fn super_frame_idx(&self) -> usize {
        let mut i = self.frames.len().saturating_sub(1);
        while i > 0
            && self.chunks[self.frames[i].chunk_idx]
                .flags
                .contains(crate::compiler::chunk::ChunkFlags::ARROW)
        {
            i -= 1;
        }
        i
    }
}

/// Double → string in an arbitrary radix (2..=36), fraction digits emitted
/// until the value's ulp is exhausted (V8's DoubleToRadixCString approach),
/// with final-digit rounding and carry.
pub(crate) fn f64_to_radix(n: f64, radix: u32) -> String {
    if n == 0.0 {
        return "0".to_string();
    }
    if n.is_nan() {
        return "NaN".to_string();
    }
    if n.is_infinite() {
        return if n > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let neg = n < 0.0;
    let x = n.abs();
    let int_part = x.trunc();
    let mut frac = x - int_part;
    // Integer digits (f64 division loop covers > u64 range too).
    let int_str = if int_part == 0.0 {
        "0".to_string()
    } else if int_part < 9.0e15 {
        radix_fmt(int_part as u64, radix)
    } else {
        let mut i = int_part;
        let mut v: Vec<u8> = Vec::new();
        while i >= 1.0 {
            let d = (i % radix as f64) as usize;
            v.push(digits[d]);
            i = (i / radix as f64).trunc();
        }
        v.reverse();
        String::from_utf8(v).unwrap()
    };
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(int_str.as_bytes());
    if frac > 0.0 {
        out.push(b'.');
        // Stop when we've resolved the double's precision.
        let mut delta = 0.5 * (x * f64::EPSILON).max(f64::MIN_POSITIVE);
        let mut budget = 1100usize;
        loop {
            frac *= radix as f64;
            delta *= radix as f64;
            let mut d = frac.trunc() as usize;
            frac -= d as f64;
            if frac < delta || budget == 0 {
                // Round the final digit and carry left if needed.
                if frac >= 0.5 && frac + delta > 1.0 - frac {
                    d += 1;
                }
                if d >= radix as usize {
                    // Carry through emitted digits.
                    out.push(digits[d - radix as usize]);
                    let mut i = out.len() - 2;
                    loop {
                        if out[i] == b'.' {
                            if i == 0 {
                                break;
                            }
                            i -= 1;
                            continue;
                        }
                        let pos = digits.iter().position(|&c| c == out[i]).unwrap_or(0);
                        if pos + 1 < radix as usize {
                            out[i] = digits[pos + 1];
                            break;
                        }
                        out[i] = b'0';
                        if i == 0 {
                            out.insert(0, b'1');
                            break;
                        }
                        i -= 1;
                    }
                } else {
                    out.push(digits[d]);
                }
                break;
            }
            out.push(digits[d]);
            budget -= 1;
        }
        // Trim trailing zeros and a bare point.
        while out.last() == Some(&b'0') {
            out.pop();
        }
        if out.last() == Some(&b'.') {
            out.pop();
        }
    }
    let body = String::from_utf8(out).unwrap();
    if neg { format!("-{body}") } else { body }
}
