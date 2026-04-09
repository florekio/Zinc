pub mod ast;
pub mod compiler;
pub mod engine;
pub mod gc;
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
pub mod jit;
pub mod lexer;
pub mod parser;
pub mod runtime;
pub mod util;
pub mod vm;

// ---- WebAssembly entry point ----

use wasm_bindgen::prelude::*;

/// Evaluate JavaScript source code and return the result as a string.
/// This is the main entry point for the WASM build.
#[wasm_bindgen]
pub fn zinc_eval(source: &str) -> String {
    let mut engine = engine::Engine::new();
    match engine.eval(source) {
        Ok(result) => engine.display_value(&result),
        Err(e) => format!("Error: {e}"),
    }
}

/// Evaluate JavaScript and return both the result and any console output.
/// Returns a JSON string: {"result": "...", "output": ["line1", "line2", ...]}
#[wasm_bindgen]
pub fn zinc_eval_with_output(source: &str) -> String {
    let mut engine = engine::Engine::new();
    let (result_str, output) = engine.eval_with_output(source);
    // Manual JSON construction (no serde dependency)
    let output_json: Vec<String> = output.iter()
        .map(|s| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")))
        .collect();
    format!("{{\"result\":\"{}\",\"output\":[{}]}}",
        result_str.replace('\\', "\\\\").replace('"', "\\\""),
        output_json.join(","))
}
