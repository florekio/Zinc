use zinc::engine::Engine;
use zinc::runtime::object::Property;
use zinc::runtime::value::Value;

#[test]
fn value_from_str_creates_js_string() {
    let mut engine = Engine::new();
    engine.register_host_fn("greet", |vm, _this, _args| {
        Ok(vm.value_from_str("hello"))
    });
    let (out, _) = engine.eval_with_output("greet()");
    assert_eq!(out, "hello");
}

#[test]
fn alloc_array_returns_js_array() {
    let mut engine = Engine::new();
    engine.register_host_fn("triple", |vm, _this, _args| {
        let items = vec![Value::int(1), Value::int(2), Value::int(3)];
        Ok(vm.alloc_array(items))
    });
    let (out, _) = engine.eval_with_output("var a = triple(); a.length + ',' + a[0] + ',' + a[2]");
    assert_eq!(out, "3,1,3");
}

#[test]
fn alloc_object_supports_set_and_get_property() {
    let mut engine = Engine::new();
    engine.register_host_fn("makePoint", |vm, _this, _args| {
        let obj = vm.alloc_object();
        vm.set_property(obj, "x", Value::int(7));
        vm.set_property(obj, "y", Value::int(11));
        Ok(obj)
    });
    let (out, _) = engine.eval_with_output("var p = makePoint(); p.x + ',' + p.y");
    assert_eq!(out, "7,11");
}

#[test]
fn type_error_helper_throws_typed_error() {
    let mut engine = Engine::new();
    engine.register_host_fn("boom", |vm, _this, _args| {
        Err(vm.type_error("nope"))
    });
    let (out, _) = engine.eval_with_output(
        "(function(){try { boom() } catch(e) { return e.name + ':' + e.message; }})()",
    );
    assert_eq!(out, "TypeError:nope");
}

#[test]
fn range_error_helper_throws_typed_error() {
    let mut engine = Engine::new();
    engine.register_host_fn("oob", |vm, _this, _args| {
        Err(vm.range_error("out of range"))
    });
    let (out, _) = engine.eval_with_output(
        "(function(){try { oob() } catch(e) { return e.name + ':' + e.message; }})()",
    );
    assert_eq!(out, "RangeError:out of range");
}

#[test]
fn call_function_invokes_js_callback_from_host() {
    let mut engine = Engine::new();
    engine.register_host_fn("twice", |vm, _this, args| {
        let cb = args.first().copied().unwrap_or(Value::undefined());
        let a = vm.host_call(cb, &[Value::int(10)])?;
        let b = vm.host_call(cb, &[Value::int(32)])?;
        // Sum the two return values: 11 + 33 = 44 if cb is x => x+1.
        let sum = a.as_int().unwrap_or(0) + b.as_int().unwrap_or(0);
        Ok(Value::int(sum))
    });
    let (out, _) = engine.eval_with_output("twice(x => x + 1)");
    assert_eq!(out, "44");
}

#[test]
fn define_property_with_readonly_flags() {
    let mut engine = Engine::new();
    engine.register_host_fn("makeFrozen", |vm, _this, _args| {
        let obj = vm.alloc_object();
        let v = vm.value_from_str("1.0");
        // read-only, non-enumerable, non-configurable
        vm.define_property(obj, "VERSION", v, 0);
        Ok(obj)
    });
    let (out, _) = engine.eval_with_output(
        "var c = makeFrozen(); var d = Object.getOwnPropertyDescriptor(c, 'VERSION'); \
         d.value + ',' + d.writable + ',' + d.enumerable",
    );
    assert_eq!(out, "1.0,false,false");
    let _ = Property::WRITABLE; // ensure flag constants are reachable from this crate
}
