/// Test262 conformance runner for Zinc.
/// Run with: cargo run --release --bin test262_runner
///
/// Memory strategy: orchestrator spawns one subprocess per category so that
/// each category gets a clean heap and RSS is fully reclaimed between categories.
#[cfg(feature = "fast-alloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::path::{Path, PathBuf};
use std::fs;
use std::time::Instant;
use std::collections::HashMap;
use zinc::engine::Engine;

/// Parsed test262 YAML frontmatter metadata.
struct TestMeta {
    flags: Vec<String>,
    includes: Vec<String>,
    features: Vec<String>,
    is_negative: bool,
    #[allow(dead_code)]
    negative_phase: String,
    is_async: bool,
}

fn parse_meta(source: &str) -> TestMeta {
    let mut flags = Vec::new();
    let mut includes = Vec::new();
    let mut features = Vec::new();
    let mut is_negative = false;
    let mut negative_phase = String::new();
    if let Some(start) = source.find("/*---")
        && let Some(end) = source[start..].find("---*/") {
            let yaml = &source[start + 5..start + end];
            let mut in_flags = false;
            let mut in_includes = false;
            let mut in_features = false;
            let mut in_negative = false;

            for line in yaml.lines() {
                let trimmed = line.trim();

                if trimmed.starts_with("flags:") {
                    in_flags = true; in_includes = false; in_features = false; in_negative = false;
                    if let Some(bracket_content) = extract_bracket_list(trimmed) {
                        flags = bracket_content;
                        in_flags = false;
                    }
                    continue;
                }
                if trimmed.starts_with("includes:") {
                    in_includes = true; in_flags = false; in_features = false; in_negative = false;
                    if let Some(bracket_content) = extract_bracket_list(trimmed) {
                        includes = bracket_content;
                        in_includes = false;
                    }
                    continue;
                }
                if trimmed.starts_with("features:") {
                    in_features = true; in_flags = false; in_includes = false; in_negative = false;
                    if let Some(bracket_content) = extract_bracket_list(trimmed) {
                        features = bracket_content;
                        in_features = false;
                    }
                    continue;
                }
                if trimmed.starts_with("negative:") {
                    is_negative = true;
                    in_negative = true; in_flags = false; in_includes = false; in_features = false;
                    continue;
                }
                if !trimmed.starts_with("- ") && !trimmed.starts_with("phase:") && !trimmed.starts_with("type:")
                    && !trimmed.is_empty() && !trimmed.starts_with('#')
                    && trimmed.contains(':')
                    && (!in_negative || (!trimmed.starts_with("phase:") && !trimmed.starts_with("type:")))
                {
                    in_flags = false; in_includes = false; in_features = false;
                    if !trimmed.starts_with("phase:") && !trimmed.starts_with("type:") {
                        in_negative = false;
                    }
                }

                if let Some(val) = trimmed.strip_prefix("- ") {
                    let val = val.trim().to_string();
                    if in_flags { flags.push(val); }
                    else if in_includes { includes.push(val); }
                    else if in_features { features.push(val); }
                }

                if in_negative && trimmed.starts_with("phase:") {
                    negative_phase = trimmed["phase:".len()..].trim().to_string();
                }
            }
        }

    let is_async = flags.contains(&"async".to_string());
    TestMeta { flags, includes, features, is_negative, negative_phase, is_async }
}

fn extract_bracket_list(line: &str) -> Option<Vec<String>> {
    if let Some(open) = line.find('[')
        && let Some(close) = line.find(']') {
            let inner = &line[open + 1..close];
            let items: Vec<String> = inner.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            return Some(items);
        }
    None
}

const UNSUPPORTED_FEATURES: &[&str] = &[
    "Proxy", "Reflect",
    "Symbol.asyncIterator", "Symbol.matchAll",
    "WeakRef", "FinalizationRegistry",
    "SharedArrayBuffer", "Atomics",
    "async-iteration", "for-await-of",
    "import-assertions", "import-attributes",
    "dynamic-import", "import.meta",
    "tail-call-optimization",
    "Intl", "Temporal",
    "resizable-arraybuffer", "arraybuffer-transfer",
    "Regexp.escape",
    "decorators",
    "explicit-resource-management",
    "iterator-helpers",
    "set-methods",
    "promise-with-resolvers",
    "regexp-v-flag", "regexp-unicode-property-escapes",
    "regexp-named-groups", "regexp-lookbehind", "regexp-dotall",
    "regexp-match-indices",
    "class-static-fields-public",
    "class-static-block",
    "logical-assignment-operators",
    "json-modules",
    "String.prototype.matchAll",
    "Array.fromAsync",
    "change-array-by-copy",
];

fn should_skip(source: &str, meta: &TestMeta) -> bool {
    if source.contains('\u{2028}') || source.contains('\u{2029}')
        || source.contains("\\u2028") || source.contains("\\u2029") {
        return true;
    }

    for feat in &meta.features {
        for unsupported in UNSUPPORTED_FEATURES {
            if feat == unsupported { return true; }
        }
    }

    if source.contains("Proxy(") || source.contains("new Proxy")
        || source.contains("Reflect.")
        || source.contains("WeakRef(")
        || source.contains("SharedArrayBuffer")
        || source.contains("Atomics.")
        || source.contains("import(") || source.contains("import.meta")
        || source.contains("Symbol.toPrimitive")
        || source.contains("Symbol.species")
        || source.contains("[Symbol.")
    {
        return true;
    }

    if meta.flags.contains(&"module".to_string()) { return true; }
    if meta.is_async { return true; }
    if source.contains("fnGlobalObject") { return true; }
    // Skip tests that use `with` statement (not implemented)
    if source.contains("with (") || source.contains("with(") { return true; }

    for inc in &meta.includes {
        match inc.as_str() {
            "compareArray.js" | "deepEqual.js" | "nans.js"
            | "decimalToHexString.js" | "isConstructor.js"
            | "propertyHelper.js" => {}
            _ => return true,
        }
    }

    if meta.flags.contains(&"raw".to_string()) { return true; }
    false
}

const CATEGORIES: &[&str] = &[
    "expressions/addition", "expressions/subtraction", "expressions/multiplication",
    "expressions/division", "expressions/remainder", "expressions/exponentiation",
    "expressions/unary-minus", "expressions/unary-plus", "expressions/typeof",
    "expressions/void", "expressions/delete", "expressions/logical-not",
    "expressions/logical-and", "expressions/logical-or", "expressions/bitwise-and",
    "expressions/bitwise-or", "expressions/bitwise-xor", "expressions/bitwise-not",
    "expressions/left-shift", "expressions/right-shift", "expressions/unsigned-right-shift",
    "expressions/equals", "expressions/does-not-equals", "expressions/strict-equals",
    "expressions/strict-does-not-equals", "expressions/less-than",
    "expressions/less-than-or-equal", "expressions/greater-than",
    "expressions/greater-than-or-equal", "expressions/conditional", "expressions/comma",
    "expressions/grouping", "expressions/postfix-increment", "expressions/postfix-decrement",
    "expressions/prefix-increment", "expressions/prefix-decrement", "expressions/assignment",
    "expressions/compound-assignment", "statements/if", "statements/while",
    "statements/do-while", "statements/for", "statements/switch", "statements/break",
    "statements/continue", "statements/return", "statements/throw", "statements/try",
    "statements/block", "statements/empty", "statements/variable", "statements/expression",
    "statements/labeled", "literals/numeric", "literals/string", "literals/boolean",
    "literals/null", "comments", "white-space", "punctuators", "types", "asi",
    "block-scope", "keywords", "line-terminators", "function-code", "global-code",
    "identifier-resolution", "rest-parameters", "computed-property-names", "statementList",
    "expressions/object", "expressions/function", "expressions/coalesce",
    "expressions/concatenation", "expressions/logical-assignment", "expressions/modulus",
    "expressions/relational", "expressions/arrow-function", "expressions/template-literal",
    "expressions/this", "expressions/optional-chaining", "expressions/async-function",
    "statements/for-of", "statements/const", "statements/let", "statements/class",
    "statements/function", "statements/for-in", "expressions/array", "expressions/in",
    "expressions/instanceof", "expressions/new", "expressions/call", "directive-prologue",
    "future-reserved-words", "reserved-words",
];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let verbose = args.contains(&"--verbose".to_string()) || args.contains(&"-v".to_string());
    let output_path = args.windows(2)
        .find(|w| w[0] == "--output" || w[0] == "-o")
        .map(|w| w[1].clone());

    // Single-category mode: used by the orchestrator subprocess.
    // Usage: test262_runner --run-category CATEGORY [--fail-log FILE]
    let run_category = args.windows(2)
        .find(|w| w[0] == "--run-category")
        .map(|w| w[1].clone());
    let fail_log = args.windows(2)
        .find(|w| w[0] == "--fail-log")
        .map(|w| w[1].clone());

    if let Some(ref category) = run_category {
        // Subprocess mode: run a single category, print machine line to stdout.
        run_single_category(category, fail_log.as_deref(), verbose);
        return;
    }

    // Also support --filter for quick manual testing (non-subprocess, single process)
    let filter = args.windows(2)
        .find(|w| w[0] == "--filter" || w[0] == "--category")
        .map(|w| w[1].clone());
    if filter.is_some() {
        run_filtered(filter.as_deref(), output_path.as_deref(), verbose);
        return;
    }

    // Orchestrator mode: spawn one subprocess per category to isolate memory.
    let exe = std::env::current_exe().expect("cannot find current exe");
    let mut output_file: Option<std::fs::File> = output_path.as_ref()
        .map(|p| std::fs::File::create(p).expect("cannot open output file"));

    let mut total = 0usize;
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    let mut category_results: Vec<(&str, usize, usize, usize)> = Vec::new();

    let start = Instant::now();

    for category in CATEGORIES {
        // Each category gets its own temp fail log, then we append to the main one.
        let fail_tmp = format!("/tmp/zinc_test262_{}.log", category.replace('/', "_"));

        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("--run-category").arg(category)
           .arg("--fail-log").arg(&fail_tmp);
        if verbose { cmd.arg("--verbose"); }

        let output = match cmd.output() {
            Ok(o) => o,
            Err(e) => { eprintln!("failed to spawn subprocess for {category}: {e}"); continue; }
        };

        // Parse machine line: "MACHINE passed failed skipped total"
        let stdout = String::from_utf8_lossy(&output.stdout);
        if let Some(line) = stdout.lines().find(|l| l.starts_with("MACHINE ")) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() == 5 {
                let p: usize = parts[1].parse().unwrap_or(0);
                let f: usize = parts[2].parse().unwrap_or(0);
                let s: usize = parts[3].parse().unwrap_or(0);
                let t: usize = parts[4].parse().unwrap_or(0);
                passed += p;
                failed += f;
                skipped += s;
                total += t;
                if t > 0 {
                    category_results.push((category, t, p, f));
                }
            }
        }

        // Append fail lines from temp file to main output file
        if let Some(ref mut out) = output_file {
            use std::io::Write;
            if let Ok(content) = fs::read_to_string(&fail_tmp) {
                let _ = out.write_all(content.as_bytes());
            }
        }
        let _ = fs::remove_file(&fail_tmp);
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
    println!("Skipped: {skipped}");
    println!("Time: {:.2}s", elapsed.as_secs_f64());
    println!();
    if let Some(ref mut f) = output_file {
        use std::io::Write;
        let _ = writeln!(f, "TOTAL {passed}/{total} ({pct:.1}%) skipped={skipped} time={:.2}s", elapsed.as_secs_f64());
    }
}

/// Run a single category (called in subprocess mode).
/// Prints "MACHINE <passed> <failed> <skipped> <total>" to stdout.
fn run_single_category(category: &str, fail_log: Option<&str>, verbose: bool) {
    let test_root = Path::new("test262/test/language");
    let harness_root = Path::new("test262/harness");

    if !test_root.exists() {
        eprintln!("Error: test262 not found.");
        std::process::exit(1);
    }

    let mut harness_cache: HashMap<String, String> = HashMap::new();
    if harness_root.exists()
        && let Ok(entries) = fs::read_dir(harness_root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "js").unwrap_or(false)
                    && let Ok(content) = fs::read_to_string(&path) {
                        let name = path.file_name().unwrap().to_string_lossy().to_string();
                        harness_cache.insert(name, content);
                    }
            }
        }

    let dir = test_root.join(category);
    let mut files: Vec<PathBuf> = Vec::new();
    if dir.exists() {
        collect_js_files(&dir, &mut files);
        files.sort();
    }

    let mut fail_file: Option<std::fs::File> = fail_log
        .map(|p| std::fs::File::create(p).expect("cannot open fail log"));

    let mut total = 0usize;
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;

    for file in &files {
        let source = match fs::read_to_string(file) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let meta = parse_meta(&source);
        if should_skip(&source, &meta) {
            skipped += 1;
            continue;
        }

        let result = run_test(&source, &meta, &harness_cache);
        let pass = if meta.is_negative { result.is_err() } else { result.is_ok() };

        total += 1;
        if pass {
            passed += 1;
        } else {
            failed += 1;
            if verbose || fail_file.is_some() {
                let fname = file.file_name().unwrap_or_default().to_string_lossy();
                let err_msg = match result {
                    Ok(_) => "expected failure but passed".to_string(),
                    Err(e) => e,
                };
                let fail_line = format!("FAIL {category}/{fname}: {err_msg}\n");
                if verbose { eprint!("{}", fail_line); }
                if let Some(ref mut f) = fail_file {
                    use std::io::Write;
                    let _ = f.write_all(fail_line.as_bytes());
                }
            }
        }
    }

    println!("MACHINE {passed} {failed} {skipped} {total}");
}

/// Run with --filter for interactive use (single process, no subprocess isolation).
fn run_filtered(filter: Option<&str>, output_path: Option<&str>, verbose: bool) {
    let test_root = Path::new("test262/test/language");
    let harness_root = Path::new("test262/harness");

    if !test_root.exists() {
        eprintln!("Error: test262 not found.");
        std::process::exit(1);
    }

    let mut harness_cache: HashMap<String, String> = HashMap::new();
    if harness_root.exists()
        && let Ok(entries) = fs::read_dir(harness_root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "js").unwrap_or(false)
                    && let Ok(content) = fs::read_to_string(&path) {
                        let name = path.file_name().unwrap().to_string_lossy().to_string();
                        harness_cache.insert(name, content);
                    }
            }
        }

    let mut output_file: Option<std::fs::File> = output_path
        .map(|p| std::fs::File::create(p).expect("cannot open output file"));

    let mut total = 0usize;
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    let mut category_results: Vec<(&str, usize, usize, usize)> = Vec::new();

    let start = Instant::now();

    for category in CATEGORIES {
        if filter.is_some_and(|f| !category.contains(f)) {
            continue;
        }

        let dir = test_root.join(category);
        if !dir.exists() { continue; }

        let mut files: Vec<PathBuf> = Vec::new();
        collect_js_files(&dir, &mut files);
        files.sort();

        let mut cat_total = 0usize;
        let mut cat_passed = 0usize;
        let mut cat_failed = 0usize;

        for file in &files {
            let source = match fs::read_to_string(file) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let meta = parse_meta(&source);
            if should_skip(&source, &meta) {
                skipped += 1;
                continue;
            }

            let result = run_test(&source, &meta, &harness_cache);
            let pass = if meta.is_negative { result.is_err() } else { result.is_ok() };

            total += 1;
            cat_total += 1;
            if pass {
                passed += 1;
                cat_passed += 1;
            } else {
                failed += 1;
                cat_failed += 1;
                if verbose || output_file.is_some() {
                    let fname = file.file_name().unwrap_or_default().to_string_lossy();
                    let err_msg = match result {
                        Ok(_) => "expected failure but passed".to_string(),
                        Err(e) => e,
                    };
                    let fail_line = format!("FAIL {category}/{fname}: {err_msg}\n");
                    if verbose { eprint!("{}", fail_line); }
                    if let Some(ref mut f) = output_file {
                        use std::io::Write;
                        let _ = f.write_all(fail_line.as_bytes());
                    }
                }
            }
        }

        if cat_total > 0 {
            category_results.push((category, cat_total, cat_passed, cat_failed));
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
    println!("Skipped: {skipped}");
    println!("Time: {:.2}s", elapsed.as_secs_f64());
    println!();
    if let Some(ref mut f) = output_file {
        use std::io::Write;
        let _ = writeln!(f, "TOTAL {passed}/{total} ({pct:.1}%) skipped={skipped} time={:.2}s", elapsed.as_secs_f64());
    }
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

const BASE_HARNESS: &str = r#"
function Test262Error(msg) { this.message = msg; this.name = "Test262Error"; }
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
    try { fn(); } catch(e) { return; }
    throw new Test262Error(msg || "expected exception");
};
function $ERROR(msg) { throw new Test262Error(msg); }
var $262 = {};
"#;

fn run_test(source: &str, meta: &TestMeta, harness_cache: &HashMap<String, String>) -> Result<(), String> {
    let mut parts = Vec::new();

    if !meta.flags.contains(&"raw".to_string()) {
        parts.push(BASE_HARNESS.to_string());
        for inc in &meta.includes {
            if let Some(content) = harness_cache.get(inc) {
                parts.push(content.clone());
            }
        }
    }

    if meta.flags.contains(&"onlyStrict".to_string()) {
        parts.push("\"use strict\";\n".to_string());
    }

    parts.push(source.to_string());
    let full_source = parts.join("\n");

    // 2M instructions is plenty for any real test; cuts off infinite loops quickly
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut engine = Engine::new();
        engine.set_max_steps(2_000_000);
        engine.eval(&full_source)
    }));

    match result {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(format!("{e}")),
        Err(_) => Err("panic".to_string()),
    }
}
