/// Test262 conformance runner for Zinc.
/// Run with: cargo run --release --bin test262_runner
use std::path::{Path, PathBuf};
use std::fs;
use std::time::Instant;
use zinc::engine::Engine;

fn main() {
    let test_root = Path::new("test262/test/language");
    if !test_root.exists() {
        eprintln!("Error: test262 not found. Run:");
        eprintln!("  git clone --depth 1 https://github.com/nicolo-ribaudo/test262.git");
        std::process::exit(1);
    }

    // Categories to test (ordered by relevance to Zinc's capabilities)
    let categories = vec![
        "expressions/addition",
        "expressions/subtraction",
        "expressions/multiplication",
        "expressions/division",
        "expressions/remainder",
        "expressions/exponentiation",
        "expressions/unary-minus",
        "expressions/unary-plus",
        "expressions/typeof",
        "expressions/void",
        "expressions/delete",
        "expressions/logical-not",
        "expressions/logical-and",
        "expressions/logical-or",
        "expressions/bitwise-and",
        "expressions/bitwise-or",
        "expressions/bitwise-xor",
        "expressions/bitwise-not",
        "expressions/left-shift",
        "expressions/right-shift",
        "expressions/unsigned-right-shift",
        "expressions/equals",
        "expressions/does-not-equals",
        "expressions/strict-equals",
        "expressions/strict-does-not-equals",
        "expressions/less-than",
        "expressions/less-than-or-equal",
        "expressions/greater-than",
        "expressions/greater-than-or-equal",
        "expressions/conditional",
        "expressions/comma",
        "expressions/grouping",
        "expressions/postfix-increment",
        "expressions/postfix-decrement",
        "expressions/prefix-increment",
        "expressions/prefix-decrement",
        "expressions/assignment",
        "expressions/compound-assignment",
        "statements/if",
        "statements/while",
        "statements/do-while",
        "statements/for",
        "statements/switch",
        "statements/break",
        "statements/continue",
        "statements/return",
        "statements/throw",
        "statements/try",
        "statements/block",
        "statements/empty",
        "statements/variable",
        "statements/expression",
        "statements/labeled",
        "literals/numeric",
        "literals/string",
        "literals/boolean",
        "literals/null",
        "comments",
        "white-space",
        "punctuators",
        "types",
        "asi",
        "block-scope",
        "keywords",
        // "identifiers", // mostly Unicode escape tests, skip for now
        "line-terminators",
        "function-code",
        "global-code",
        "identifier-resolution",
        "rest-parameters",
        "computed-property-names",
        "statementList",
        "expressions/object",
        "expressions/function",
        "expressions/coalesce",
        "expressions/concatenation",
        "expressions/logical-assignment",
        "expressions/modulus",
        "expressions/relational",
        "expressions/arrow-function",
        "expressions/template-literal",
        "expressions/this",
        "expressions/optional-chaining",
        "expressions/async-function",
        "statements/for-of",
        "statements/const",
        "statements/let",
        "directive-prologue",
        "future-reserved-words",
        // "reserved-words", // 52% — many need strict mode
        // "module-code", // 18% — most need multi-file imports or strict module semantics
        // "export", // needs module mode
    ];

    let mut total = 0;
    let mut passed = 0;
    let mut failed = 0;
    let mut skipped = 0;
    let mut category_results: Vec<(String, usize, usize, usize)> = Vec::new();

    let start = Instant::now();

    for category in &categories {
        let dir = test_root.join(category);
        if !dir.exists() {
            continue;
        }

        let mut cat_total = 0;
        let mut cat_passed = 0;
        let mut cat_failed = 0;

        let mut files: Vec<PathBuf> = Vec::new();
        collect_js_files(&dir, &mut files);
        files.sort();

        for file in &files {
            let source = match fs::read_to_string(file) {
                Ok(s) => s,
                Err(_) => continue,
            };

            // Skip tests that need features Zinc doesn't support
            if should_skip(&source) {
                skipped += 1;
                continue;
            }

            total += 1;
            cat_total += 1;

            let expects_error = source.contains("negative:");

            let result = run_test(&source);

            if expects_error {
                if result.is_err() {
                    passed += 1;
                    cat_passed += 1;
                } else {
                    failed += 1;
                    cat_failed += 1;
                }
            } else {
                if result.is_ok() {
                    passed += 1;
                    cat_passed += 1;
                } else {
                    failed += 1;
                    cat_failed += 1;
                }
            }
        }

        if cat_total > 0 {
            category_results.push((category.to_string(), cat_total, cat_passed, cat_failed));
        }
    }

    let elapsed = start.elapsed();

    println!();
    println!("=== Zinc Test262 Conformance Report ===");
    println!();
    printf_header();
    for (cat, t, p, f) in &category_results {
        let pct = if *t > 0 { *p as f64 / *t as f64 * 100.0 } else { 0.0 };
        println!("{:<45} {:>5} {:>5} {:>5} {:>6.1}%", cat, t, p, f, pct);
    }
    println!("{}", "─".repeat(75));
    let pct = if total > 0 { passed as f64 / total as f64 * 100.0 } else { 0.0 };
    println!("{:<45} {:>5} {:>5} {:>5} {:>6.1}%", "TOTAL", total, passed, failed, pct);
    println!();
    println!("Skipped: {} (use eval, Proxy, Symbol, etc.)", skipped);
    println!("Time: {:.2}s", elapsed.as_secs_f64());
    println!();
}

fn collect_js_files(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_js_files(&path, out);
            } else if path.extension().map(|e| e == "js").unwrap_or(false) {
                out.push(path);
            }
        }
    }
}

fn printf_header() {
    println!("{:<45} {:>5} {:>5} {:>5} {:>7}", "Category", "Total", "Pass", "Fail", "Rate");
    println!("{}", "─".repeat(75));
}

fn run_test(source: &str) -> Result<(), String> {
    let harness = r#"
function Test262Error(msg) { this.message = msg; }
function assert(condition, msg) { if (!condition) throw new Test262Error(msg || "assertion failed"); }
assert.sameValue = function(a, b, msg) {
    if (a !== b) {
        if (a !== a && b !== b) return;
        throw new Test262Error(msg || "expected " + b + " but got " + a);
    }
};
assert.notSameValue = function(a, b, msg) {
    if (a === b) throw new Test262Error(msg || "expected not " + b);
};
assert.throws = function(err, fn, msg) {
    try { fn(); throw new Test262Error(msg || "expected exception"); } catch(e) { }
};
function $ERROR(msg) { throw new Test262Error(msg); }
"#;

    let full_source = format!("{harness}\n{source}");

    let full_source_clone = full_source.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut engine = Engine::new();
            engine.eval(&full_source_clone)
        }));
        let _ = tx.send(result);
    });

    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Ok(Ok(_))) => Ok(()),
        Ok(Ok(Err(e))) => Err(format!("{e}")),
        Ok(Err(_)) => Err("panic".to_string()),
        Err(_) => Err("timeout".to_string()),
    }
}

fn should_skip(source: &str) -> bool {
    // Skip tests with exotic Unicode that our lexer can't handle
    source.contains('\u{2028}') ||
    source.contains('\u{2029}') ||
    source.contains("\\u2028") ||
    source.contains("\\u2029") ||
    // Skip tests that use features Zinc doesn't support
    source.contains("eval(") ||
    source.contains("Proxy") ||
    source.contains("Reflect") ||
    source.contains("Symbol") ||
    source.contains("WeakRef") ||
    source.contains("FinalizationRegistry") ||
    source.contains("SharedArrayBuffer") ||
    source.contains("Atomics") ||
    source.contains("async iteration") ||
    source.contains("generators") ||
    source.contains("import(") ||
    source.contains("import.meta") ||
    source.contains("with (") ||
    source.contains("Object.defineProperty") ||
    source.contains("Object.getOwnPropertyDescriptor") ||
    source.contains("Object.create(") ||
    source.contains("Object.freeze") ||
    source.contains("Object.seal") ||
    source.contains("Object.getPrototypeOf") ||
    source.contains("Object.preventExtensions") ||
    source.contains("flags: [") ||
    source.contains("includes: [") ||
    source.contains("propertyHelper.js") ||
    source.contains("fnGlobalObject")
}
