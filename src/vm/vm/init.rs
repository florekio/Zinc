//! VM construction: interning the global environment, wiring native
//! function sentinels and prototypes, and flattening compiled chunks.

use super::*;

impl Vm {
    pub fn new(chunk: Chunk, mut interner: Interner) -> Self {
        let mut globals = HashMap::new();
        let undef_id = interner.intern("undefined");
        globals.insert(undef_id, Value::undefined());
        let nan_id = interner.intern("NaN");
        globals.insert(nan_id, Value::number(f64::NAN));
        let inf_id = interner.intern("Infinity");
        globals.insert(inf_id, Value::number(f64::INFINITY));

        // Flatten chunk tree: index 0 = script, children follow
        let mut chunks = Vec::new();
        Self::flatten_chunk(chunk, &mut chunks);

        let mut heap = ObjectHeap::new();
        let mut func_prototypes: HashMap<i32, ObjectId> = HashMap::new();

        // Create Object.prototype singleton (root of all prototype chains)
        let mut obj_proto = JsObject::ordinary();
        // prototype = null (Object.prototype is the root)
        let hop_key = interner.intern("hasOwnProperty");
        obj_proto.define_property(hop_key, Property::with_flags(Value::function(-590), Property::WRITABLE | Property::CONFIGURABLE));
        let pie_key = interner.intern("propertyIsEnumerable");
        obj_proto.define_property(pie_key, Property::with_flags(Value::function(-591), Property::WRITABLE | Property::CONFIGURABLE));
        let tostr_key = interner.intern("toString");
        obj_proto.define_property(tostr_key, Property::with_flags(Value::function(-592), Property::WRITABLE | Property::CONFIGURABLE));
        let valueof_key = interner.intern("valueOf");
        obj_proto.define_property(valueof_key, Property::with_flags(Value::function(-593), Property::WRITABLE | Property::CONFIGURABLE));
        let ipof_key = interner.intern("isPrototypeOf");
        obj_proto.define_property(ipof_key, Property::with_flags(Value::function(-594), Property::WRITABLE | Property::CONFIGURABLE));
        // Object.prototype.constructor = Object (sentinel -508), non-enumerable.
        let ctor_key = interner.intern("constructor");
        obj_proto.define_property(ctor_key, Property::with_flags(Value::function(-508), Property::WRITABLE | Property::CONFIGURABLE));
        let object_prototype = heap.allocate(obj_proto);

        // Create Function.prototype singleton (prototype = Object.prototype)
        let mut fn_proto = JsObject::ordinary();
        fn_proto.prototype = Some(object_prototype);
        let call_key = interner.intern("call");
        fn_proto.define_property(call_key, Property::with_flags(Value::function(-595), Property::WRITABLE | Property::CONFIGURABLE));
        let apply_key = interner.intern("apply");
        fn_proto.define_property(apply_key, Property::with_flags(Value::function(-596), Property::WRITABLE | Property::CONFIGURABLE));
        let bind_key = interner.intern("bind");
        fn_proto.define_property(bind_key, Property::with_flags(Value::function(-597), Property::WRITABLE | Property::CONFIGURABLE));
        let fn_length_key = interner.intern("length");
        fn_proto.define_property(fn_length_key, Property::with_flags(Value::int(0), Property::CONFIGURABLE));
        let fn_name_key = interner.intern("name");
        let empty_str = interner.intern("");
        fn_proto.define_property(fn_name_key, Property::with_flags(Value::string(empty_str), Property::CONFIGURABLE));
        let function_prototype = heap.allocate(fn_proto);

        // Create Array.prototype singleton (prototype = Object.prototype)
        // Array.prototype methods use sentinels -600 to -629
        let mut arr_proto = JsObject::ordinary();
        arr_proto.prototype = Some(object_prototype);
        for (name, sentinel) in [
            ("join", -600i32), ("push", -601), ("pop", -602), ("shift", -603),
            ("unshift", -604), ("indexOf", -605), ("includes", -606), ("forEach", -607),
            ("map", -608), ("filter", -609), ("reduce", -610), ("some", -611),
            ("every", -612), ("find", -613), ("findIndex", -614), ("slice", -615),
            ("concat", -616), ("reverse", -617), ("sort", -618), ("flat", -619),
            ("flatMap", -620), ("fill", -621), ("splice", -622), ("reduceRight", -623),
            ("at", -624), ("keys", -625), ("values", -626), ("entries", -627),
            ("lastIndexOf", -628), ("toString", -629),
        ] {
            let k = interner.intern(name);
            arr_proto.define_property(k, Property::with_flags(Value::function(sentinel), Property::WRITABLE | Property::CONFIGURABLE));
        }
        // Array.prototype.constructor = Array (-507)
        arr_proto.define_property(ctor_key, Property::with_flags(Value::function(-507), Property::WRITABLE | Property::CONFIGURABLE));
        // Array.prototype[Symbol.iterator] aliases Array.prototype.values per spec.
        // sym_iterator id is 0 (registered first in sym_descs below, see further down).
        let sym_iter_key = interner.intern("__sym_0__");
        arr_proto.define_property(sym_iter_key, Property::with_flags(Value::function(-626), Property::WRITABLE | Property::CONFIGURABLE));
        let array_prototype = heap.allocate(arr_proto);

        // Create Boolean.prototype (prototype = Object.prototype)
        let mut bool_proto = JsObject::ordinary();
        bool_proto.prototype = Some(object_prototype);
        bool_proto.define_property(ctor_key, Property::with_flags(Value::function(-506), Property::WRITABLE | Property::CONFIGURABLE));
        let bool_ts_key = interner.intern("toString");
        bool_proto.define_property(bool_ts_key, Property::with_flags(Value::function(-630), Property::WRITABLE | Property::CONFIGURABLE));
        let bool_vo_key = interner.intern("valueOf");
        bool_proto.define_property(bool_vo_key, Property::with_flags(Value::function(-631), Property::WRITABLE | Property::CONFIGURABLE));
        let boolean_prototype = heap.allocate(bool_proto);
        func_prototypes.insert(-506i32, boolean_prototype);

        // Create Number.prototype (prototype = Object.prototype)
        let mut num_proto = JsObject::ordinary();
        num_proto.prototype = Some(object_prototype);
        num_proto.define_property(ctor_key, Property::with_flags(Value::function(-505), Property::WRITABLE | Property::CONFIGURABLE));
        let num_ts_key = interner.intern("toString");
        num_proto.define_property(num_ts_key, Property::with_flags(Value::function(-632), Property::WRITABLE | Property::CONFIGURABLE));
        let num_vo_key = interner.intern("valueOf");
        num_proto.define_property(num_vo_key, Property::with_flags(Value::function(-633), Property::WRITABLE | Property::CONFIGURABLE));
        let number_prototype = heap.allocate(num_proto);
        func_prototypes.insert(-505i32, number_prototype);

        // Create String.prototype (prototype = Object.prototype)
        let mut str_proto = JsObject::ordinary();
        str_proto.prototype = Some(object_prototype);
        str_proto.define_property(ctor_key, Property::with_flags(Value::function(-504), Property::WRITABLE | Property::CONFIGURABLE));
        let str_ts_key = interner.intern("toString");
        str_proto.define_property(str_ts_key, Property::with_flags(Value::function(-634), Property::WRITABLE | Property::CONFIGURABLE));
        let str_vo_key = interner.intern("valueOf");
        str_proto.define_property(str_vo_key, Property::with_flags(Value::function(-635), Property::WRITABLE | Property::CONFIGURABLE));
        let string_prototype = heap.allocate(str_proto);
        func_prototypes.insert(-504i32, string_prototype);

        // Create Promise.prototype (prototype = Object.prototype). The actual
        // .then/.catch/.finally methods are dispatched specially in CallMethod
        // for Promise objects, so they don't need to live on this prototype —
        // but tests check `Object.getPrototypeOf(p) === Promise.prototype`.
        let mut promise_proto = JsObject::ordinary();
        promise_proto.prototype = Some(object_prototype);
        promise_proto.define_property(ctor_key, Property::with_flags(Value::function(-520), Property::WRITABLE | Property::CONFIGURABLE));
        // Real, extractable method values too: bundles lift them off the
        // prototype (`Promise.prototype.then.bind(Promise.resolve())` is
        // Preact's microtask scheduler). Each delegates to the same
        // dispatch CallMethod uses.
        for mname in ["then", "catch", "finally"] {
            let m_key = interner.intern(mname);
            let func: crate::runtime::object::NativeFn = std::sync::Arc::new(
                move |vm: &mut Vm, this: Value, args: &[Value]| -> Result<Value, Value> {
                    let Some(oid) = this.as_object_id() else {
                        return Ok(Value::undefined());
                    };
                    let m_id = vm.interner.intern(mname);
                    vm.exec_promise_method(oid, m_id, args).map_err(|e| match e {
                        VmError::Throw(v) => v,
                        other => {
                            let msg = format!("{other:?}");
                            vm.make_native_error("TypeError", &msg)
                        }
                    })
                },
            );
            let fobj = JsObject {
                properties: Vec::new(),
                prototype: None,
                kind: ObjectKind::Function(crate::runtime::object::FunctionKind::Native {
                    name: m_key,
                    func,
                }),
                marked: false,
                extensible: true,
            };
            let f_oid = heap.allocate(fobj);
            promise_proto.define_property(
                m_key,
                Property::with_flags(
                    Value::object_id(f_oid),
                    Property::WRITABLE | Property::CONFIGURABLE,
                ),
            );
        }
        let promise_prototype = heap.allocate(promise_proto);
        func_prototypes.insert(-520i32, promise_prototype);

        // Create Error.prototype objects so subclasses inherit `name`
        // First create Error.prototype itself
        let mut error_proto = JsObject::ordinary();
        error_proto.prototype = Some(object_prototype);
        let name_key_ep = interner.intern("name");
        let error_name_val = interner.intern("Error");
        error_proto.define_property(name_key_ep, Property::with_flags(Value::string(error_name_val), Property::WRITABLE | Property::CONFIGURABLE));
        let message_key_ep = interner.intern("message");
        let empty_str_ep = interner.intern("");
        error_proto.define_property(message_key_ep, Property::with_flags(Value::string(empty_str_ep), Property::WRITABLE | Property::CONFIGURABLE));
        let constructor_key_ep = interner.intern("constructor");
        error_proto.define_property(constructor_key_ep, Property::with_flags(Value::function(-510), Property::WRITABLE | Property::CONFIGURABLE));
        // Error.prototype.toString — `${name}: ${message}` (sentinel -598).
        let err_tostr_key = interner.intern("toString");
        error_proto.define_property(err_tostr_key, Property::with_flags(Value::function(-598), Property::WRITABLE | Property::CONFIGURABLE));
        let error_prototype_oid = heap.allocate(error_proto);
        func_prototypes.insert(-510i32, error_prototype_oid);

        // Create derived error prototypes with prototype = Error.prototype
        for (sentinel, type_name) in [
            (-511i32, "TypeError"), (-512, "RangeError"),
            (-513, "ReferenceError"), (-514, "SyntaxError"), (-515, "EvalError"), (-516, "URIError"),
        ] {
            let mut proto = JsObject::ordinary();
            proto.prototype = Some(error_prototype_oid);
            let name_key = interner.intern("name");
            let name_val = interner.intern(type_name);
            proto.define_property(name_key, Property::with_flags(Value::string(name_val), Property::WRITABLE | Property::CONFIGURABLE));
            let message_key = interner.intern("message");
            let empty_str = interner.intern("");
            proto.define_property(message_key, Property::with_flags(Value::string(empty_str), Property::WRITABLE | Property::CONFIGURABLE));
            let ctor_key = interner.intern("constructor");
            proto.define_property(ctor_key, Property::with_flags(Value::function(sentinel), Property::WRITABLE | Property::CONFIGURABLE));
            let ep = heap.allocate(proto);
            func_prototypes.insert(sentinel, ep);
        }

        // Create console object with log/warn/error methods
        let mut console_obj = JsObject::ordinary();
        let log_id = interner.intern("log");
        console_obj.set_property(log_id, Value::function(-100)); // sentinel for console.log
        let warn_id = interner.intern("warn");
        console_obj.set_property(warn_id, Value::function(-101)); // sentinel for console.warn
        let error_id = interner.intern("error");
        console_obj.set_property(error_id, Value::function(-102)); // sentinel for console.error
        let console_oid = heap.allocate(console_obj);
        let console_name = interner.intern("console");
        globals.insert(console_name, Value::object_id(console_oid));

        // Create Math object with constants and methods.
        // Per spec, Math.{PI,E,LN2,LN10,SQRT2,...} are non-writable, non-enumerable, non-configurable.
        let mut math_obj = JsObject::ordinary();
        let pi_name = interner.intern("PI");
        math_obj.define_property(pi_name, Property::with_flags(Value::number(std::f64::consts::PI), 0));
        let e_name = interner.intern("E");
        math_obj.define_property(e_name, Property::with_flags(Value::number(std::f64::consts::E), 0));
        let ln2_name = interner.intern("LN2");
        math_obj.define_property(ln2_name, Property::with_flags(Value::number(std::f64::consts::LN_2), 0));
        let ln10_name = interner.intern("LN10");
        math_obj.define_property(ln10_name, Property::with_flags(Value::number(std::f64::consts::LN_10), 0));
        let sqrt2_name = interner.intern("SQRT2");
        math_obj.define_property(sqrt2_name, Property::with_flags(Value::number(std::f64::consts::SQRT_2), 0));
        let log2e_name = interner.intern("LOG2E");
        math_obj.define_property(log2e_name, Property::with_flags(Value::number(std::f64::consts::LOG2_E), 0));
        let log10e_name = interner.intern("LOG10E");
        math_obj.define_property(log10e_name, Property::with_flags(Value::number(std::f64::consts::LOG10_E), 0));
        let sqrt1_2_name = interner.intern("SQRT1_2");
        math_obj.define_property(sqrt1_2_name, Property::with_flags(Value::number(std::f64::consts::FRAC_1_SQRT_2), 0));
        // Math methods as sentinel functions (-700 range)
        for (name, sentinel) in [
            ("sin", -700i32), ("cos", -701), ("abs", -702), ("floor", -703),
            ("ceil", -704), ("round", -705), ("sqrt", -706), ("pow", -707),
            ("max", -708), ("min", -709), ("exp", -710), ("log", -711),
            ("log2", -712), ("log10", -713), ("random", -714), ("trunc", -715),
            ("sign", -716), ("cbrt", -717), ("hypot", -718), ("atan2", -719),
            ("atan", -720), ("asin", -721), ("acos", -722), ("tan", -723),
            ("clz32", -724), ("imul", -725), ("fround", -726),
        ] {
            let k = interner.intern(name);
            math_obj.define_property(k, Property::with_flags(Value::function(sentinel), Property::WRITABLE | Property::CONFIGURABLE));
        }
        let math_oid = heap.allocate(math_obj);
        let math_name = interner.intern("Math");
        globals.insert(math_name, Value::object_id(math_oid));

        // Create JSON object (methods handled in exec_json_method)
        let json_obj = JsObject::ordinary();
        let json_oid = heap.allocate(json_obj);
        let json_name = interner.intern("JSON");
        globals.insert(json_name, Value::object_id(json_oid));

        // Global functions as sentinel values
        let parse_int_name = interner.intern("parseInt");
        globals.insert(parse_int_name, Value::function(-500));
        let parse_float_name = interner.intern("parseFloat");
        globals.insert(parse_float_name, Value::function(-501));
        let is_nan_name = interner.intern("isNaN");
        globals.insert(is_nan_name, Value::function(-502));
        let is_finite_name = interner.intern("isFinite");
        globals.insert(is_finite_name, Value::function(-503));
        let str_name = interner.intern("String");
        globals.insert(str_name, Value::function(-504));
        let num_name = interner.intern("Number");
        globals.insert(num_name, Value::function(-505));
        let bool_name = interner.intern("Boolean");
        globals.insert(bool_name, Value::function(-506));
        let arr_is_arr = interner.intern("Array");
        globals.insert(arr_is_arr, Value::function(-507));
        let object_name = interner.intern("Object");
        globals.insert(object_name, Value::function(-508));

        // URI handling. Without native versions, pages (and core-js) install
        // slow JS polyfills that percent-decode/encode char-by-char in a loop;
        // DuckDuckGo's SERP bundle hammered a decodeURIComponent polyfill hard
        // enough to blow the execution-limit fuel before it could define
        // DDG.Pages.SERP.
        let decode_uri_component_name = interner.intern("decodeURIComponent");
        globals.insert(decode_uri_component_name, Value::function(-517));
        let encode_uri_component_name = interner.intern("encodeURIComponent");
        globals.insert(encode_uri_component_name, Value::function(-518));
        let decode_uri_name = interner.intern("decodeURI");
        globals.insert(decode_uri_name, Value::function(-519));
        let encode_uri_name = interner.intern("encodeURI");
        globals.insert(encode_uri_name, Value::function(-509));

        // Promise constructor
        let promise_name = interner.intern("Promise");
        globals.insert(promise_name, Value::function(-520));

        // Error constructors
        let error_name = interner.intern("Error");
        globals.insert(error_name, Value::function(-510));
        let type_error_name = interner.intern("TypeError");
        globals.insert(type_error_name, Value::function(-511));
        let range_error_name = interner.intern("RangeError");
        globals.insert(range_error_name, Value::function(-512));
        let ref_error_name = interner.intern("ReferenceError");
        globals.insert(ref_error_name, Value::function(-513));
        let syntax_error_name = interner.intern("SyntaxError");
        globals.insert(syntax_error_name, Value::function(-514));
        let eval_error_name = interner.intern("EvalError");
        globals.insert(eval_error_name, Value::function(-515));
        let uri_error_name = interner.intern("URIError");
        globals.insert(uri_error_name, Value::function(-516));
        let eval_name = interner.intern("eval");
        globals.insert(eval_name, Value::function(-560));
        let symbol_name = interner.intern("Symbol");
        globals.insert(symbol_name, Value::function(-570));
        let bigint_name = interner.intern("BigInt");
        globals.insert(bigint_name, Value::function(-638));
        let map_name = interner.intern("Map");
        globals.insert(map_name, Value::function(-540));
        // Map.prototype / Set.prototype with extractable this-aware
        // methods. Instance calls (m.entries()) still dispatch on
        // ObjectKind in CallMethod first; these NativeFns serve property
        // reads and the uncurry-this pattern — core-js does
        // `var entries = uncurry(Map.prototype.entries); entries(map)`
        // while feature-testing collections (DuckDuckGo's polyfills
        // bundle), and reads `instance.set` as a value before .call-ing
        // it. Each NativeFn delegates to exec_map_method/exec_set_method
        // so semantics can't drift from the dispatch path.
        {
            // kind: 0 = Map, 1 = Set, 2 = WeakMap, 3 = WeakSet
            let make_delegate = |kind: u8, name: &str| -> crate::runtime::object::NativeFn {
                let name = name.to_owned();
                std::sync::Arc::new(move |vm: &mut Vm, this: Value, args: &[Value]| -> Result<Value, Value> {
                    let kind_ok = this.as_object_id().is_some_and(|o| {
                        vm.heap.get(o).is_some_and(|x| match kind {
                            0 => matches!(&x.kind, ObjectKind::Map { .. }),
                            1 => matches!(&x.kind, ObjectKind::Set { .. }),
                            2 => matches!(&x.kind, ObjectKind::WeakMap { .. }),
                            _ => matches!(&x.kind, ObjectKind::WeakSet { .. }),
                        })
                    });
                    let Some(oid) = this.as_object_id().filter(|_| kind_ok) else {
                        let which = ["Map", "Set", "WeakMap", "WeakSet"][kind as usize];
                        let msg = format!("{which} method called on incompatible receiver");
                        return Err(vm.make_native_error("TypeError", &msg));
                    };
                    let mid = vm.interner.intern(&name);
                    let res = match kind {
                        0 => vm.exec_map_method(oid, mid, args),
                        1 => vm.exec_set_method(oid, mid, args),
                        2 => vm.exec_weakmap_method(oid, mid, args),
                        _ => vm.exec_weakset_method(oid, mid, args),
                    };
                    match res {
                        Ok(v) => Ok(v),
                        Err(VmError::Throw(v)) => Err(v),
                        Err(e) => {
                            let msg = format!("{e:?}");
                            Err(vm.make_native_error("Error", &msg))
                        }
                    }
                })
            };
            let seed = |sentinel: i32, kind: u8, names: &[&str],
                            heap: &mut ObjectHeap, interner: &mut Interner,
                            func_prototypes: &mut HashMap<i32, ObjectId>| {
                let mut proto = JsObject::ordinary();
                proto.prototype = Some(object_prototype);
                proto.define_property(ctor_key, Property::with_flags(Value::function(sentinel), Property::WRITABLE | Property::CONFIGURABLE));
                for name in names {
                    let key = interner.intern(name);
                    let fn_obj = JsObject {
                        properties: Vec::new(),
                        prototype: None,
                        kind: ObjectKind::Function(crate::runtime::object::FunctionKind::Native { name: key, func: make_delegate(kind, name) }),
                        marked: false,
                        extensible: true,
                    };
                    let val = Value::object_id(heap.allocate(fn_obj));
                    proto.define_property(key, Property::with_flags(val, Property::WRITABLE | Property::CONFIGURABLE));
                }
                let proto_oid = heap.allocate(proto);
                func_prototypes.insert(sentinel, proto_oid);
            };
            seed(-540, 0, &["get", "set", "has", "delete", "clear", "forEach", "keys", "values", "entries"], &mut heap, &mut interner, &mut func_prototypes);
            seed(-541, 1, &["add", "has", "delete", "clear", "forEach", "keys", "values", "entries"], &mut heap, &mut interner, &mut func_prototypes);
            seed(-542, 2, &["get", "set", "has", "delete"], &mut heap, &mut interner, &mut func_prototypes);
            seed(-543, 3, &["add", "has", "delete"], &mut heap, &mut interner, &mut func_prototypes);
        }
        // RegExp.prototype (-580) with extractable test/exec/toString —
        // core-js uncurries `/./.exec` while building its JSON.stringify
        // surrogate-escaping wrapper.
        {
            let make_re_delegate = |name: &str| -> crate::runtime::object::NativeFn {
                let name = name.to_owned();
                std::sync::Arc::new(move |vm: &mut Vm, this: Value, args: &[Value]| -> Result<Value, Value> {
                    let Some(oid) = this.as_object_id()
                        .filter(|o| vm.heap.get(*o).is_some_and(|x| matches!(&x.kind, ObjectKind::RegExp { .. })))
                    else {
                        // RegExp.prototype.toString is GENERIC per spec:
                        // it reads .source / .flags off any receiver —
                        // core-js feature-tests exactly this with a plain
                        // {source, flags} object.
                        if name == "toString"
                            && let Some(roid) = this.as_object_id() {
                                let source_key = vm.interner.intern("source");
                                let flags_key = vm.interner.intern("flags");
                                let src = vm.heap.get_property_chain(roid, source_key)
                                    .map(|v| vm.value_to_string(v))
                                    .unwrap_or_else(|| "undefined".to_string());
                                let flg = vm.heap.get_property_chain(roid, flags_key)
                                    .map(|v| vm.value_to_string(v))
                                    .unwrap_or_else(|| "undefined".to_string());
                                let s = format!("/{src}/{flg}");
                                return Ok(Value::string(vm.interner.intern(&s)));
                            }
                        let repr = vm.value_to_string(this);
                        let msg = format!("RegExp.prototype.{name} called on incompatible receiver ({repr})");
                        return Err(vm.make_native_error("TypeError", &msg));
                    };
                    let mid = vm.interner.intern(&name);
                    match vm.exec_regexp_method(oid, mid, args) {
                        Ok(v) => Ok(v),
                        Err(VmError::Throw(v)) => Err(v),
                        Err(e) => {
                            let msg = format!("{e:?}");
                            Err(vm.make_native_error("Error", &msg))
                        }
                    }
                })
            };
            let mut re_proto = JsObject::ordinary();
            re_proto.prototype = Some(object_prototype);
            re_proto.define_property(ctor_key, Property::with_flags(Value::function(-580), Property::WRITABLE | Property::CONFIGURABLE));
            for name in ["test", "exec", "toString"] {
                let key = interner.intern(name);
                let fn_obj = JsObject {
                    properties: Vec::new(),
                    prototype: None,
                    kind: ObjectKind::Function(crate::runtime::object::FunctionKind::Native { name: key, func: make_re_delegate(name) }),
                    marked: false,
                    extensible: true,
                };
                let val = Value::object_id(heap.allocate(fn_obj));
                re_proto.define_property(key, Property::with_flags(val, Property::WRITABLE | Property::CONFIGURABLE));
            }
            let re_proto_oid = heap.allocate(re_proto);
            func_prototypes.insert(-580i32, re_proto_oid);
        }
        let set_name = interner.intern("Set");
        globals.insert(set_name, Value::function(-541));
        let weakmap_name = interner.intern("WeakMap");
        globals.insert(weakmap_name, Value::function(-542));
        let weakset_name = interner.intern("WeakSet");
        globals.insert(weakset_name, Value::function(-543));
        let date_name = interner.intern("Date");
        globals.insert(date_name, Value::function(-550));

        let regexp_name = interner.intern("RegExp");
        globals.insert(regexp_name, Value::function(-580));

        // Function constructor: `new Function("a", "b", "return a+b")` or `Function("...")`.
        // We stash a sentinel; Call/Construct handles it by concatenating source,
        // compiling, and creating a callable function.
        let function_name = interner.intern("Function");
        globals.insert(function_name, Value::function(-551));

        // Reflect: plain object (spec requires it to be an ordinary object)
        let reflect_obj = JsObject::ordinary();
        let reflect_oid = heap.allocate(reflect_obj);
        let reflect_name = interner.intern("Reflect");
        globals.insert(reflect_name, Value::object_id(reflect_oid));

        // globalThis: create a global object whose own property lookups
        // proxy to the globals map. For simplicity we expose a plain object
        // pre-populated with the common primitives; reads/writes go through
        // the object, not the globals map.
        let mut global_this_obj = JsObject::ordinary();
        // Populate non-configurable read-only globals (NaN, Infinity, undefined)
        // so `globalThis.Infinity = X` in strict mode correctly throws.
        let nan_key = interner.intern("NaN");
        global_this_obj.define_property(nan_key,
            Property::with_flags(Value::number(f64::NAN), 0));
        let inf_key = interner.intern("Infinity");
        global_this_obj.define_property(inf_key,
            Property::with_flags(Value::number(f64::INFINITY), 0));
        let undef_key = interner.intern("undefined");
        global_this_obj.define_property(undef_key,
            Property::with_flags(Value::undefined(), 0));
        let global_this_oid = heap.allocate(global_this_obj);
        let global_this_name = interner.intern("globalThis");
        globals.insert(global_this_name, Value::object_id(global_this_oid));

        // Pre-register well-known symbol descriptions
        let sym_descs = vec![
            Some(interner.intern("Symbol.iterator")),
            Some(interner.intern("Symbol.hasInstance")),
            Some(interner.intern("Symbol.toPrimitive")),
            Some(interner.intern("Symbol.toStringTag")),
            Some(interner.intern("Symbol.species")),
            Some(interner.intern("Symbol.unscopables")),
            Some(interner.intern("Symbol.asyncIterator")),
            Some(interner.intern("Symbol.matchAll")),
        ];

        // Pre-populate fast lookup Vec from all initial globals
        let globals_vec = {
            let max_id = globals.keys().map(|k| k.0 as usize).max().unwrap_or(0);
            let mut v = vec![Value::null(); max_id + 1];
            for (k, val) in &globals {
                v[k.0 as usize] = *val;
            }
            v
        };


        let mut vm = Self {
            chunks,
            frames: vec![CallFrame { chunk_idx: 0, ip: 0, base: 0, upvalues: Vec::new(), this_value: Value::object_id(global_this_oid), is_constructor: false, pending_super_call: false, generator_id: None, argc: 0, saved_args: Vec::new(), arguments_oid: None, is_derived_ctor: false, super_called: false, new_target: Value::undefined(), await_super_result: false, with_base: 0 }],
            stack: Vec::with_capacity(256),
            globals,
            interner,
            heap,
            globals_vec,
            global_ic: HashMap::new(),
            global_version: 0,
            global_ic_version: HashMap::new(),
            exc_handlers: Vec::new(),
            protect_throw_depth: 0,
            microtask_queue: Vec::new(),
            host_roots: Vec::new(),
            with_stack: Vec::new(),
            closure_withs: std::collections::HashMap::new(),
            lex_globals: std::collections::HashSet::new(),
            tdz_globals: std::collections::HashSet::new(),
            closure_arrow_ctx: std::collections::HashMap::new(),
            closure_arrow_args: std::collections::HashMap::new(),
            eval_inherit_with_base: None,
            direct_eval_pending: false,
            closure_private_env: std::collections::HashMap::new(),
            script_completion: Value::undefined(),
            param_scope_depth: 0,
            // Index 0 is a reserved dummy: plain chunk-index function values
            // (eval bodies, arguments.callee, …) decode to closure_id 0, so the
            // first real closure must not claim that id — otherwise its
            // upvalues/captured with-scopes would leak into those calls.
            closure_upvalues: vec![Vec::new()],
            open_upvalues: std::collections::HashMap::new(),
            #[cfg(any(all(target_arch = "aarch64", target_os = "macos"), target_arch = "x86_64"))]
            call_counts: HashMap::new(),
            #[cfg(any(all(target_arch = "aarch64", target_os = "macos"), target_arch = "x86_64"))]
            jit_functions: HashMap::new(),
            output: Vec::new(),
            silent_console: false,
            module_cache: HashMap::new(),
            module_dir: None,
            regex_cache: crate::vm::regexp::RegexCache::new(),
            func_prototypes,
            object_prototype,
            function_prototype,
            array_prototype,
            promise_prototype,
            iterator_prototype: None,
            boolean_prototype,
            number_prototype,
            string_prototype,
            global_this_oid,
            math_oid: Some(math_oid),
            json_oid: Some(json_oid),
            symbol_descriptions: sym_descs,
            symbol_registry: std::collections::HashMap::new(),
            next_symbol_id: 8, // 0-7 are well-known
            sym_iterator: 0,
            sym_has_instance: 1,
            sym_to_primitive: 2,
            sym_to_string_tag: 3,
            sym_species: 4,
            sym_unscopables: 5,
            sym_async_iterator: 6,
            sym_match_all: 7,
            fn_property_overrides: HashMap::new(),
            pending_private_brands: HashMap::new(),
            computed_exclusions: Vec::new(),
            steps: 0,
            max_steps: 0,
            deadline: None,
            fuel_samples: HashMap::new(),
            string_recv_kinds: [0; 4],
            fuel_call_counts: HashMap::new(),
        };
        vm.init_typed_arrays();
        vm
    }

    pub(crate) fn flatten_chunk(mut chunk: Chunk, out: &mut Vec<Chunk>) {
        let children = std::mem::take(&mut chunk.child_chunks);
        // Record absolute indices of each direct child before recursing.
        // The first child will be at out.len() + 1 (after we push self).
        // Subsequent children follow after their siblings' full subtrees.
        let self_idx = out.len();
        out.push(chunk);
        for child in children {
            let child_abs_idx = out.len();
            out[self_idx].children.push(child_abs_idx);
            Self::flatten_chunk(child, out);
        }
    }
}
