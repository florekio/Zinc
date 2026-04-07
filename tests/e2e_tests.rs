use zinc::engine::Engine;

fn eval(source: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(source) {
        Ok(result) => engine.display_value(&result),
        Err(e) => format!("ERROR: {e}"),
    }
}

#[test]
fn test_arithmetic() {
    assert_eq!(eval("1 + 2"), "3");
    assert_eq!(eval("10 - 3"), "7");
    assert_eq!(eval("4 * 5"), "20");
    assert_eq!(eval("15 / 4"), "3.75");
    assert_eq!(eval("17 % 5"), "2");
    assert_eq!(eval("2 ** 10"), "1024");
}

#[test]
fn test_operator_precedence() {
    assert_eq!(eval("1 + 2 * 3"), "7");
    assert_eq!(eval("(1 + 2) * 3"), "9");
    assert_eq!(eval("2 ** 3 ** 2"), "512"); // right-associative
}

#[test]
fn test_comparison() {
    assert_eq!(eval("1 < 2"), "true");
    assert_eq!(eval("2 >= 2"), "true");
    assert_eq!(eval("3 === 3"), "true");
    assert_eq!(eval("3 !== 4"), "true");
    assert_eq!(eval("1 == 1"), "true");
}

#[test]
fn test_logical() {
    assert_eq!(eval("true && false"), "false");
    assert_eq!(eval("true || false"), "true");
    assert_eq!(eval("!true"), "false");
    assert_eq!(eval("!0"), "true");
    assert_eq!(eval("!''"), "true");
}

#[test]
fn test_string_concatenation() {
    assert_eq!(eval(r#""hello" + " " + "world""#), "hello world");
    assert_eq!(eval(r#""count: " + 42"#), "count: 42");
    assert_eq!(eval(r#"1 + "2""#), "12");
}

#[test]
fn test_variables() {
    assert_eq!(eval("var x = 42; x"), "42");
    assert_eq!(eval("var a = 10; var b = 20; a + b"), "30");
    assert_eq!(eval("var x = 1; x = x + 1; x"), "2");
}

#[test]
fn test_if_else() {
    assert_eq!(eval("var x; if (true) { x = 1; } else { x = 2; } x"), "1");
    assert_eq!(eval("var x; if (false) { x = 1; } else { x = 2; } x"), "2");
}

#[test]
fn test_while_loop() {
    assert_eq!(eval("var i = 0; var sum = 0; while (i < 10) { sum = sum + i; i = i + 1; } sum"), "45");
}

#[test]
fn test_for_loop() {
    assert_eq!(eval("var sum = 0; for (var i = 1; i <= 10; i = i + 1) { sum = sum + i; } sum"), "55");
}

#[test]
fn test_typeof() {
    assert_eq!(eval("typeof 42"), "number");
    assert_eq!(eval("typeof true"), "boolean");
    assert_eq!(eval(r#"typeof "hello""#), "string");
    assert_eq!(eval("typeof undefined"), "undefined");
    assert_eq!(eval("typeof null"), "object");
}

#[test]
fn test_nullish_coalescing() {
    assert_eq!(eval("null ?? 42"), "42");
    assert_eq!(eval("undefined ?? 42"), "42");
    assert_eq!(eval("0 ?? 42"), "0");
    assert_eq!(eval(r#""" ?? 42"#), "");
}

#[test]
fn test_ternary() {
    assert_eq!(eval("true ? 1 : 2"), "1");
    assert_eq!(eval("false ? 1 : 2"), "2");
    assert_eq!(eval("1 > 0 ? 'yes' : 'no'"), "yes");
}

#[test]
fn test_unary() {
    assert_eq!(eval("-5"), "-5");
    assert_eq!(eval("void 0"), "undefined");
    assert_eq!(eval("~0"), "-1");
}

#[test]
fn test_bitwise() {
    assert_eq!(eval("5 & 3"), "1");
    assert_eq!(eval("5 | 3"), "7");
    assert_eq!(eval("5 ^ 3"), "6");
    assert_eq!(eval("1 << 3"), "8");
    assert_eq!(eval("16 >> 2"), "4");
}

#[test]
fn test_basic_math_file() {
    // var x = 10; var y = 20; var z = x + y * 2; z;
    assert_eq!(eval("var x = 10; var y = 20; var z = x + y * 2; z"), "50");
}

#[test]
fn test_closure_capture() {
    assert_eq!(eval("function outer() { var x = 10; function inner() { return x; } return inner; } outer()()"), "10");
}

#[test]
fn test_closure_mutation() {
    assert_eq!(eval("function makeCounter() { var c = 0; function inc() { c = c + 1; return c; } return inc; } var f = makeCounter(); f(); f(); f()"), "3");
}

#[test]
fn test_closure_adder() {
    assert_eq!(eval("function makeAdder(x) { return function(y) { return x + y; }; } makeAdder(10)(20)"), "30");
}

#[test]
fn test_function_call() {
    assert_eq!(eval("function add(a, b) { return a + b; } add(3, 4)"), "7");
}

#[test]
fn test_object_literal() {
    assert_eq!(eval("var o = { x: 1, y: 2 }; o.x + o.y"), "3");
}

#[test]
fn test_object_property_set() {
    assert_eq!(eval("var o = { x: 1 }; o.x = 42; o.x"), "42");
}

#[test]
fn test_array_literal() {
    assert_eq!(eval("var a = [10, 20, 30]; a[1]"), "20");
}

#[test]
fn test_array_length() {
    assert_eq!(eval("var a = [1, 2, 3, 4, 5]; a.length"), "5");
}

#[test]
fn test_string_length() {
    assert_eq!(eval(r#""hello".length"#), "5");
}

#[test]
fn test_function_returns_object() {
    assert_eq!(eval("function f() { return { v: 99 }; } f().v"), "99");
}

// ---- String methods ----

#[test]
fn test_string_to_upper() {
    assert_eq!(eval(r#""hello".toUpperCase()"#), "HELLO");
}

#[test]
fn test_string_to_lower() {
    assert_eq!(eval(r#""HELLO".toLowerCase()"#), "hello");
}

#[test]
fn test_string_trim() {
    assert_eq!(eval(r#""  hello  ".trim()"#), "hello");
}

#[test]
fn test_string_index_of() {
    assert_eq!(eval(r#""hello world".indexOf("world")"#), "6");
    assert_eq!(eval(r#""hello".indexOf("xyz")"#), "-1");
}

#[test]
fn test_string_includes() {
    assert_eq!(eval(r#""hello world".includes("world")"#), "true");
    assert_eq!(eval(r#""hello".includes("xyz")"#), "false");
}

#[test]
fn test_string_slice() {
    assert_eq!(eval(r#""hello world".slice(0, 5)"#), "hello");
    assert_eq!(eval(r#""hello".slice(-3)"#), "llo");
}

#[test]
fn test_string_split() {
    assert_eq!(eval(r#""a,b,c".split(",").length"#), "3");
}

#[test]
fn test_string_replace() {
    assert_eq!(eval(r#""hello world".replace("world", "zinc")"#), "hello zinc");
}

#[test]
fn test_string_repeat() {
    assert_eq!(eval(r#""abc".repeat(3)"#), "abcabcabc");
}

#[test]
fn test_string_starts_ends_with() {
    assert_eq!(eval(r#""hello".startsWith("hel")"#), "true");
    assert_eq!(eval(r#""hello".endsWith("llo")"#), "true");
}

#[test]
fn test_string_char_at() {
    assert_eq!(eval(r#""hello".charAt(1)"#), "e");
}

#[test]
fn test_string_pad_start() {
    assert_eq!(eval(r#""5".padStart(3, "0")"#), "005");
}

// ---- Math ----

#[test]
fn test_math_floor() {
    assert_eq!(eval("Math.floor(3.7)"), "3");
}

#[test]
fn test_math_ceil() {
    assert_eq!(eval("Math.ceil(3.2)"), "4");
}

#[test]
fn test_math_abs() {
    assert_eq!(eval("Math.abs(-42)"), "42");
}

#[test]
fn test_math_sqrt() {
    assert_eq!(eval("Math.sqrt(144)"), "12");
}

#[test]
fn test_math_max_min() {
    assert_eq!(eval("Math.max(1, 5, 3)"), "5");
    assert_eq!(eval("Math.min(1, 5, 3)"), "1");
}

#[test]
fn test_math_pow() {
    assert_eq!(eval("Math.pow(2, 10)"), "1024");
}

#[test]
fn test_math_pi() {
    assert_eq!(eval("Math.floor(Math.PI * 100)"), "314");
}

// ---- Array methods ----

#[test]
fn test_array_push_pop() {
    assert_eq!(eval("var a = [1,2]; a.push(3); a.length"), "3");
    assert_eq!(eval("var a = [1,2,3]; a.pop()"), "3");
}

#[test]
fn test_array_index_of() {
    assert_eq!(eval("var a = [10,20,30]; a.indexOf(20)"), "1");
    assert_eq!(eval("var a = [10,20,30]; a.indexOf(99)"), "-1");
}

#[test]
fn test_array_includes() {
    assert_eq!(eval("var a = [1,2,3]; a.includes(2)"), "true");
    assert_eq!(eval("var a = [1,2,3]; a.includes(9)"), "false");
}

#[test]
fn test_array_join() {
    assert_eq!(eval(r#"var a = [1,2,3]; a.join("-")"#), "1-2-3");
}

#[test]
fn test_recursive_fibonacci() {
    assert_eq!(eval("function fib(n) { if (n <= 1) return n; return fib(n-1) + fib(n-2); } fib(10)"), "55");
}

#[test]
fn test_factorial() {
    assert_eq!(eval("function f(n) { if (n <= 1) return 1; return n * f(n-1); } f(10)"), "3628800");
}

#[test]
fn test_console_log_debug() {
    use zinc::compiler::compiler::Compiler;
    use zinc::compiler::disassemble::disassemble;
    use zinc::lexer::lexer::Lexer;
    use zinc::parser::parser::Parser;
    use zinc::util::interner::Interner;

    let source = r#"console.log(1 + 2); console.log("after");"#;
    let mut interner = Interner::new();
    let tokens = {
        let mut lexer = Lexer::new(source, &mut interner);
        lexer.tokenize()
    };
    let program = {
        let mut parser = Parser::new(tokens, source, &mut interner);
        parser.parse_program().unwrap()
    };
    let chunk = {
        let compiler = Compiler::new(&mut interner);
        compiler.compile_program(&program).unwrap()
    };
    let dis = disassemble(&chunk, &interner);
    println!("{dis}");
    // Just check it compiles and disassembles
    assert!(dis.contains("CallMethod"));
}

// ---- try/catch ----

#[test]
fn test_try_catch_basic() {
    assert_eq!(eval(r#"var r; try { throw "oops"; } catch (e) { r = e; } r"#), "oops");
}

#[test]
fn test_try_catch_number() {
    assert_eq!(eval("var r; try { throw 42; } catch (e) { r = e; } r"), "42");
}

#[test]
fn test_try_catch_nested() {
    assert_eq!(eval(r#"
        var result;
        try {
            try { throw "inner"; }
            catch (e) { throw "re:" + e; }
        } catch (e) { result = e; }
        result
    "#), "re:inner");
}

#[test]
fn test_try_catch_no_throw() {
    assert_eq!(eval("var x = 1; try { x = 2; } catch(e) { x = 3; } x"), "2");
}

// ---- for...of ----

#[test]
fn test_for_of_sum() {
    assert_eq!(eval("var s = 0; for (var x of [1,2,3,4,5]) { s = s + x; } s"), "15");
}

#[test]
fn test_for_of_strings() {
    assert_eq!(eval(r#"var r = ""; for (var x of ["a","b","c"]) { r = r + x; } r"#), "abc");
}

// ---- new / constructors ----

#[test]
fn test_constructor_basic() {
    assert_eq!(eval("function Foo(x) { this.x = x; } var f = new Foo(42); f.x"), "42");
}

#[test]
fn test_constructor_multiple_props() {
    assert_eq!(eval("function P(x,y) { this.x = x; this.y = y; } var p = new P(3,4); p.x + p.y"), "7");
}

#[test]
fn test_constructor_typeof() {
    assert_eq!(eval("function F() {} typeof new F()"), "object");
}

// ---- JSON ----

#[test]
fn test_json_parse_object() {
    assert_eq!(eval(r#"JSON.parse('{"x":1}').x"#), "1");
}

#[test]
fn test_json_parse_array() {
    assert_eq!(eval(r#"JSON.parse('[1,2,3]').length"#), "3");
}

#[test]
fn test_json_parse_nested() {
    assert_eq!(eval(r#"JSON.parse('{"a":{"b":42}}').a.b"#), "42");
}

#[test]
fn test_json_parse_string() {
    assert_eq!(eval(r#"JSON.parse('"hello"')"#), "hello");
}

#[test]
fn test_json_parse_bool_null() {
    assert_eq!(eval(r#"JSON.parse('true')"#), "true");
    assert_eq!(eval(r#"JSON.parse('null')"#), "null");
}

#[test]
fn test_json_stringify() {
    assert_eq!(eval(r#"JSON.stringify(42)"#), "42");
    assert_eq!(eval(r#"JSON.stringify("hi")"#), r#""hi""#);
    assert_eq!(eval(r#"JSON.stringify(true)"#), "true");
    assert_eq!(eval(r#"JSON.stringify(null)"#), "null");
}

// ---- Global functions ----

#[test]
fn test_parse_int() {
    assert_eq!(eval(r#"parseInt("42")"#), "42");
    assert_eq!(eval(r#"parseInt("0xFF", 16)"#), "255");
    assert_eq!(eval(r#"parseInt("111", 2)"#), "7");
}

#[test]
fn test_parse_float() {
    assert_eq!(eval(r#"parseFloat("3.14")"#), "3.14");
}

#[test]
fn test_is_nan() {
    assert_eq!(eval("isNaN(NaN)"), "true");
    assert_eq!(eval("isNaN(42)"), "false");
}

#[test]
fn test_is_finite() {
    assert_eq!(eval("isFinite(42)"), "true");
    assert_eq!(eval("isFinite(Infinity)"), "false");
}

#[test]
fn test_string_constructor() {
    assert_eq!(eval("String(42)"), "42");
    assert_eq!(eval("String(true)"), "true");
}

#[test]
fn test_number_constructor() {
    assert_eq!(eval(r#"Number("3.14")"#), "3.14");
    assert_eq!(eval("Number(true)"), "1");
}

#[test]
fn test_boolean_constructor() {
    assert_eq!(eval("Boolean(0)"), "false");
    assert_eq!(eval("Boolean(1)"), "true");
    assert_eq!(eval(r#"Boolean("")"#), "false");
}

// ---- Template literals ----

#[test]
fn test_template_literal_interpolation() {
    assert_eq!(eval(r#"var x = "World"; `Hello, ${x}!`"#), "Hello, World!");
}

#[test]
fn test_template_literal_expression() {
    assert_eq!(eval("`2 + 3 = ${2 + 3}`"), "2 + 3 = 5");
}

// ---- Destructuring ----

#[test]
fn test_object_destructuring() {
    assert_eq!(eval("var {x, y} = {x: 1, y: 2}; x + y"), "3");
}

#[test]
fn test_array_destructuring() {
    assert_eq!(eval("var [a, b, c] = [10, 20, 30]; a + b + c"), "60");
}

#[test]
fn test_destructuring_in_function() {
    assert_eq!(eval("function f() { var {a, b} = {a: 100, b: 200}; return a + b; } f()"), "300");
}

// ---- Default parameters ----

#[test]
fn test_missing_args_are_undefined() {
    assert_eq!(eval("function f(a, b) { if (b === undefined) b = 10; return a + b; } f(5)"), "15");
}

// ---- Proper this binding ----

#[test]
fn test_this_in_constructor() {
    assert_eq!(eval("function F(v) { this.v = v; } var f = new F(42); f.v"), "42");
}

// ---- ES6 Classes ----

#[test]
fn test_class_constructor() {
    assert_eq!(eval("class Foo { constructor(x) { this.x = x; } } var f = new Foo(42); f.x"), "42");
}

#[test]
fn test_class_method() {
    assert_eq!(eval(r#"class Greeter { constructor(n) { this.n = n; } greet() { return "Hi " + this.n; } } new Greeter("World").greet()"#), "Hi World");
}

#[test]
fn test_class_multiple_methods() {
    assert_eq!(eval("class C { constructor(v) { this.v = v; } double() { return this.v * 2; } triple() { return this.v * 3; } } var c = new C(10); c.double() + c.triple()"), "50");
}

// ---- for...in ----

#[test]
fn test_for_in_object_keys() {
    // Order may vary since HashMap, so just check count
    assert_eq!(eval("var count = 0; for (var k in {a:1, b:2, c:3}) { count++; } count"), "3");
}

// ---- i++ in for loops ----

#[test]
fn test_for_loop_increment() {
    assert_eq!(eval("var s = 0; for (var i = 1; i <= 10; i++) { s += i; } s"), "55");
}

#[test]
fn test_postfix_increment() {
    assert_eq!(eval("var x = 5; var y = x++; x + y"), "11"); // x=6, y=5
}

#[test]
fn test_prefix_decrement() {
    assert_eq!(eval("var x = 5; var y = --x; x + y"), "8"); // x=4, y=4
}

#[test]
fn test_compound_assignment() {
    assert_eq!(eval("var x = 10; x += 5; x -= 3; x *= 2; x"), "24");
}

// ---- typeof for functions ----

#[test]
fn test_typeof_function() {
    assert_eq!(eval("typeof function(){}"), "function");
}

#[test]
fn test_typeof_named_function() {
    assert_eq!(eval("function foo(){} typeof foo"), "function");
}

#[test]
fn test_typeof_builtin() {
    assert_eq!(eval("typeof parseInt"), "function");
}

// ---- Array higher-order methods ----

#[test]
fn test_array_map() {
    assert_eq!(eval("[1,2,3].map(function(x) { return x * 2; }).join(',')"), "2,4,6");
}

#[test]
fn test_array_filter() {
    assert_eq!(eval("[1,2,3,4,5].filter(function(x) { return x > 3; }).length"), "2");
}

#[test]
fn test_array_reduce() {
    assert_eq!(eval("[1,2,3,4,5].reduce(function(a, b) { return a + b; }, 0)"), "15");
}

#[test]
fn test_array_find() {
    assert_eq!(eval("[10,20,30].find(function(x) { return x > 15; })"), "20");
}

#[test]
fn test_array_some() {
    assert_eq!(eval("[1,2,3].some(function(x) { return x > 2; })"), "true");
    assert_eq!(eval("[1,2,3].some(function(x) { return x > 5; })"), "false");
}

#[test]
fn test_array_every() {
    assert_eq!(eval("[1,2,3].every(function(x) { return x > 0; })"), "true");
    assert_eq!(eval("[1,2,3].every(function(x) { return x > 1; })"), "false");
}

// ---- Error objects ----

#[test]
fn test_error_constructor() {
    assert_eq!(eval(r#"var e = new Error("boom"); e.message"#), "boom");
}

#[test]
fn test_error_name() {
    assert_eq!(eval(r#"new Error("x").name"#), "Error");
    assert_eq!(eval(r#"new TypeError("x").name"#), "TypeError");
}

#[test]
fn test_throw_catch_error() {
    assert_eq!(eval(r#"var r; try { throw new Error("fail"); } catch(e) { r = e.message; } r"#), "fail");
}

// ---- Promises ----

#[test]
fn test_promise_resolve_then() {
    let mut engine = Engine::new();
    let (result, output) = engine.eval_with_output(
        r#"new Promise(function(resolve) { resolve(42); }).then(function(v) { console.log(v); });"#
    );
    assert_eq!(output, vec!["42"]);
}

#[test]
fn test_promise_static_resolve() {
    let mut engine = Engine::new();
    let (_, output) = engine.eval_with_output(
        "Promise.resolve(99).then(function(v) { console.log(v); });"
    );
    assert_eq!(output, vec!["99"]);
}

#[test]
fn test_promise_chain() {
    let mut engine = Engine::new();
    let (_, output) = engine.eval_with_output(
        "Promise.resolve(1).then(function(v) { return v + 1; }).then(function(v) { return v * 10; }).then(function(v) { console.log(v); });"
    );
    assert_eq!(output, vec!["20"]);
}

// ---- Async/Await ----

#[test]
fn test_async_function_return() {
    let mut engine = Engine::new();
    let (_, output) = engine.eval_with_output(
        "async function f() { return 42; } f().then(function(v) { console.log(v); });"
    );
    assert_eq!(output, vec!["42"]);
}

#[test]
fn test_await_non_promise() {
    let mut engine = Engine::new();
    let (_, output) = engine.eval_with_output(
        "async function f() { var x = await 10; return x; } f().then(function(v) { console.log(v); });"
    );
    assert_eq!(output, vec!["10"]);
}

#[test]
fn test_await_promise_resolve() {
    let mut engine = Engine::new();
    let (_, output) = engine.eval_with_output(
        "async function f() { var x = await Promise.resolve(42); return x; } f().then(function(v) { console.log(v); });"
    );
    assert_eq!(output, vec!["42"]);
}

#[test]
fn test_await_multiple() {
    let mut engine = Engine::new();
    let (_, output) = engine.eval_with_output(
        "async function f() { var a = await Promise.resolve(10); var b = await Promise.resolve(20); return a + b; } f().then(function(v) { console.log(v); });"
    );
    assert_eq!(output, vec!["30"]);
}
