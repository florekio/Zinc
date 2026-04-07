use crate::runtime::object::{JsObject, NativeFn, ObjectHeap, ObjectId};
use crate::runtime::value::Value;
use crate::util::interner::{Interner, StringId};

/// Build the console object with console.log, console.warn, console.error
pub fn create_console(heap: &mut ObjectHeap, interner: &mut Interner) -> ObjectId {
    let mut console = JsObject::ordinary();

    // console.log
    let log_name = interner.intern("log");
    let log_fn = JsObject::function_native(log_name, native_console_log);
    let log_id = heap.allocate(log_fn);
    console.set_property(log_name, Value::object_id(log_id));

    // console.warn (same as log for now)
    let warn_name = interner.intern("warn");
    let warn_fn = JsObject::function_native(warn_name, native_console_warn);
    let warn_id = heap.allocate(warn_fn);
    console.set_property(warn_name, Value::object_id(warn_id));

    // console.error
    let error_name = interner.intern("error");
    let error_fn = JsObject::function_native(error_name, native_console_error);
    let error_id = heap.allocate(error_fn);
    console.set_property(error_name, Value::object_id(error_id));

    heap.allocate(console)
}

fn native_console_log(
    _heap: &mut ObjectHeap,
    _this: Value,
    _args: &[Value],
) -> Result<Value, Value> {
    // Actual printing happens in the VM which has the interner
    // Return undefined; the VM handles display
    Ok(Value::undefined())
}

fn native_console_warn(
    _heap: &mut ObjectHeap,
    _this: Value,
    _args: &[Value],
) -> Result<Value, Value> {
    Ok(Value::undefined())
}

fn native_console_error(
    _heap: &mut ObjectHeap,
    _this: Value,
    _args: &[Value],
) -> Result<Value, Value> {
    Ok(Value::undefined())
}

/// Create Math object with common methods
pub fn create_math(heap: &mut ObjectHeap, interner: &mut Interner) -> ObjectId {
    let mut math = JsObject::ordinary();

    // Math constants
    let pi_name = interner.intern("PI");
    math.set_property(pi_name, Value::number(std::f64::consts::PI));

    let e_name = interner.intern("E");
    math.set_property(e_name, Value::number(std::f64::consts::E));

    // Math methods as native functions
    register_math_fn(&mut math, heap, interner, "abs", native_math_abs);
    register_math_fn(&mut math, heap, interner, "floor", native_math_floor);
    register_math_fn(&mut math, heap, interner, "ceil", native_math_ceil);
    register_math_fn(&mut math, heap, interner, "round", native_math_round);
    register_math_fn(&mut math, heap, interner, "sqrt", native_math_sqrt);
    register_math_fn(&mut math, heap, interner, "max", native_math_max);
    register_math_fn(&mut math, heap, interner, "min", native_math_min);
    register_math_fn(&mut math, heap, interner, "pow", native_math_pow);
    register_math_fn(&mut math, heap, interner, "random", native_math_random);
    register_math_fn(&mut math, heap, interner, "trunc", native_math_trunc);
    register_math_fn(&mut math, heap, interner, "sign", native_math_sign);
    register_math_fn(&mut math, heap, interner, "log", native_math_log);

    heap.allocate(math)
}

fn register_math_fn(
    math: &mut JsObject,
    heap: &mut ObjectHeap,
    interner: &mut Interner,
    name: &str,
    func: NativeFn,
) {
    let name_id = interner.intern(name);
    let fn_obj = JsObject::function_native(name_id, func);
    let fn_id = heap.allocate(fn_obj);
    math.set_property(name_id, Value::object_id(fn_id));
}

// Math native functions - these extract f64 from Value

fn native_math_abs(
    _heap: &mut ObjectHeap,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let n = args.first().and_then(|v| v.as_number()).unwrap_or(f64::NAN);
    Ok(Value::number(n.abs()))
}

fn native_math_floor(
    _heap: &mut ObjectHeap,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let n = args.first().and_then(|v| v.as_number()).unwrap_or(f64::NAN);
    Ok(Value::number(n.floor()))
}

fn native_math_ceil(
    _heap: &mut ObjectHeap,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let n = args.first().and_then(|v| v.as_number()).unwrap_or(f64::NAN);
    Ok(Value::number(n.ceil()))
}

fn native_math_round(
    _heap: &mut ObjectHeap,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let n = args.first().and_then(|v| v.as_number()).unwrap_or(f64::NAN);
    Ok(Value::number(n.round()))
}

fn native_math_sqrt(
    _heap: &mut ObjectHeap,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let n = args.first().and_then(|v| v.as_number()).unwrap_or(f64::NAN);
    Ok(Value::number(n.sqrt()))
}

fn native_math_max(
    _heap: &mut ObjectHeap,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    if args.is_empty() {
        return Ok(Value::number(f64::NEG_INFINITY));
    }
    let mut result = f64::NEG_INFINITY;
    for arg in args {
        let n = arg.as_number().unwrap_or(f64::NAN);
        if n.is_nan() {
            return Ok(Value::number(f64::NAN));
        }
        if n > result {
            result = n;
        }
    }
    Ok(Value::number(result))
}

fn native_math_min(
    _heap: &mut ObjectHeap,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    if args.is_empty() {
        return Ok(Value::number(f64::INFINITY));
    }
    let mut result = f64::INFINITY;
    for arg in args {
        let n = arg.as_number().unwrap_or(f64::NAN);
        if n.is_nan() {
            return Ok(Value::number(f64::NAN));
        }
        if n < result {
            result = n;
        }
    }
    Ok(Value::number(result))
}

fn native_math_pow(
    _heap: &mut ObjectHeap,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let base = args.first().and_then(|v| v.as_number()).unwrap_or(f64::NAN);
    let exp = args.get(1).and_then(|v| v.as_number()).unwrap_or(f64::NAN);
    Ok(Value::number(base.powf(exp)))
}

fn native_math_random(
    _heap: &mut ObjectHeap,
    _this: Value,
    _args: &[Value],
) -> Result<Value, Value> {
    // Simple pseudo-random using system time
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let r = (t as f64 / u32::MAX as f64).fract();
    Ok(Value::number(r))
}

fn native_math_trunc(
    _heap: &mut ObjectHeap,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let n = args.first().and_then(|v| v.as_number()).unwrap_or(f64::NAN);
    Ok(Value::number(n.trunc()))
}

fn native_math_sign(
    _heap: &mut ObjectHeap,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let n = args.first().and_then(|v| v.as_number()).unwrap_or(f64::NAN);
    Ok(Value::number(n.signum()))
}

fn native_math_log(
    _heap: &mut ObjectHeap,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let n = args.first().and_then(|v| v.as_number()).unwrap_or(f64::NAN);
    Ok(Value::number(n.ln()))
}

/// Register global functions: parseInt, parseFloat, isNaN, isFinite
pub fn create_global_functions(
    heap: &mut ObjectHeap,
    interner: &mut Interner,
) -> Vec<(StringId, ObjectId)> {
    let mut result = Vec::new();

    register_global_fn(heap, interner, "parseInt", native_parse_int, &mut result);
    register_global_fn(heap, interner, "parseFloat", native_parse_float, &mut result);
    register_global_fn(heap, interner, "isNaN", native_is_nan, &mut result);
    register_global_fn(heap, interner, "isFinite", native_is_finite, &mut result);

    result
}

fn register_global_fn(
    heap: &mut ObjectHeap,
    interner: &mut Interner,
    name: &str,
    func: NativeFn,
    result: &mut Vec<(StringId, ObjectId)>,
) {
    let name_id = interner.intern(name);
    let fn_obj = JsObject::function_native(name_id, func);
    let fn_id = heap.allocate(fn_obj);
    result.push((name_id, fn_id));
}

fn native_parse_int(
    _heap: &mut ObjectHeap,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    // Simplified: just parse the number part
    let n = args.first().and_then(|v| v.as_number()).unwrap_or(f64::NAN);
    if n.is_nan() {
        return Ok(Value::number(f64::NAN));
    }
    Ok(Value::number(n.trunc()))
}

fn native_parse_float(
    _heap: &mut ObjectHeap,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let n = args.first().and_then(|v| v.as_number()).unwrap_or(f64::NAN);
    Ok(Value::number(n))
}

fn native_is_nan(
    _heap: &mut ObjectHeap,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let n = args.first().and_then(|v| v.as_number()).unwrap_or(f64::NAN);
    Ok(Value::boolean(n.is_nan()))
}

fn native_is_finite(
    _heap: &mut ObjectHeap,
    _this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let n = args.first().and_then(|v| v.as_number()).unwrap_or(f64::NAN);
    Ok(Value::boolean(n.is_finite()))
}
