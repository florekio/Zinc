use crate::compiler::compiler::Compiler;
use crate::lexer::lexer::Lexer;
use crate::parser::parser::Parser;
use crate::runtime::value::Value;
use crate::util::interner::Interner;
use crate::vm::vm::{Vm, VmError};

/// The Zinc JavaScript Engine: orchestrates lexer -> parser -> compiler -> VM.
pub struct Engine {
    interner: Interner,
}

impl Engine {
    pub fn new() -> Self {
        Self {
            interner: Interner::new(),
        }
    }

    /// Evaluate a JavaScript source string and return the result.
    pub fn eval(&mut self, source: &str) -> Result<Value, EngineError> {
        // 1. Lex
        let tokens = {
            let mut lexer = Lexer::new(source, &mut self.interner);
            lexer.tokenize()
        };

        // 2. Parse
        let program = {
            let mut parser = Parser::new(tokens, source, &mut self.interner);
            let prog = parser
                .parse_program()
                .map_err(|e| EngineError::ParseError(e.to_string()))?;
            if !parser.errors.is_empty() {
                return Err(EngineError::ParseError(
                    parser.errors.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("\n"),
                ));
            }
            prog
        };

        // 3. Compile
        let chunk = {
            let compiler = Compiler::new(&mut self.interner);
            compiler
                .compile_program(&program)
                .map_err(EngineError::CompileError)?
        };

        // 4. Execute
        // The VM takes ownership of the interner during execution.
        // We swap it out and swap it back after.
        let interner = std::mem::take(&mut self.interner);
        let mut vm = Vm::new(chunk, interner);
        let result = vm.run().map_err(EngineError::RuntimeError);
        // Drain microtask queue (Promise .then callbacks)
        let _ = vm.drain_microtasks();
        self.interner = vm.take_interner();
        result
    }

    /// Evaluate source and return (result_string, console_output_lines).
    pub fn eval_with_output(&mut self, source: &str) -> (String, Vec<String>) {
        let tokens = {
            let mut lexer = Lexer::new(source, &mut self.interner);
            lexer.tokenize()
        };
        let program = {
            let mut parser = Parser::new(tokens, source, &mut self.interner);
            match parser.parse_program() {
                Ok(prog) => {
                    if !parser.errors.is_empty() {
                        let err = parser.errors.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("\n");
                        return (format!("SyntaxError: {err}"), vec![]);
                    }
                    prog
                }
                Err(e) => return (format!("SyntaxError: {e}"), vec![]),
            }
        };
        let chunk = {
            let compiler = Compiler::new(&mut self.interner);
            match compiler.compile_program(&program) {
                Ok(c) => c,
                Err(e) => return (format!("CompileError: {e}"), vec![]),
            }
        };
        let interner = std::mem::take(&mut self.interner);
        let mut vm = Vm::new(chunk, interner);
        let result = vm.run();
        let _ = vm.drain_microtasks();
        let output = vm.output.clone();
        self.interner = vm.take_interner();
        let result_str = match result {
            Ok(val) => self.display_value(&val),
            Err(e) => format!("Error: {e}"),
        };
        (result_str, output)
    }

    /// Get a reference to the string interner (for resolving StringIds in results).
    pub fn interner(&self) -> &Interner {
        &self.interner
    }

    /// Format a Value as a display string.
    pub fn display_value(&self, value: &Value) -> String {
        if value.is_undefined() {
            "undefined".to_string()
        } else if value.is_null() {
            "null".to_string()
        } else if value.is_boolean() {
            format!("{}", value.as_bool().unwrap())
        } else if value.is_int() {
            format!("{}", value.as_int().unwrap())
        } else if value.is_float() {
            let n = value.as_float().unwrap();
            if n.is_nan() {
                "NaN".to_string()
            } else if n.is_infinite() {
                if n > 0.0 {
                    "Infinity".to_string()
                } else {
                    "-Infinity".to_string()
                }
            } else if n == 0.0 && n.is_sign_negative() {
                "0".to_string()
            } else if n.fract() == 0.0 && n.abs() < 1e15 {
                format!("{}", n as i64)
            } else {
                format!("{n}")
            }
        } else if value.is_string() {
            let id = value.as_string_id().unwrap();
            self.interner.resolve(id).to_string()
        } else if value.is_object() {
            "[object Object]".to_string()
        } else {
            format!("{value:?}")
        }
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub enum EngineError {
    ParseError(String),
    CompileError(String),
    RuntimeError(VmError),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::ParseError(e) => write!(f, "SyntaxError: {e}"),
            EngineError::CompileError(e) => write!(f, "CompileError: {e}"),
            EngineError::RuntimeError(e) => write!(f, "RuntimeError: {e}"),
        }
    }
}
