use crate::compiler::compiler::Compiler;
use crate::lexer::lexer::Lexer;
use crate::parser::parser::Parser;
use crate::runtime::object::{FunctionKind, JsObject, NativeFn, ObjectKind};
use crate::runtime::value::Value;
use crate::util::interner::Interner;
use crate::vm::vm::{Vm, VmError};

/// Tag identifying a host class registered via `Engine::register_host_class`.
/// Tags are unique per `Engine`. The embedder uses the tag together with the
/// `payload` slot of an `ObjectKind::Host` to recover the backing data
/// (typically an index into a side table the embedder owns).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HostTag(pub u32);

/// The Zinc JavaScript Engine: orchestrates lexer -> parser -> compiler -> VM.
///
/// The VM, heap, globals and registered host functions persist across
/// `eval` calls, so multiple `<script>` tags can share a single global
/// scope and the embedder can install its DOM/host bindings once at
/// startup.
pub struct Engine {
    vm: Vm,
    max_steps: u64,
    silent_console: bool,
    next_host_tag: u32,
}

impl Engine {
    pub fn new() -> Self {
        // Compile an empty placeholder program so the VM has chunk[0] = an
        // executable script frame that quietly returns undefined. Subsequent
        // eval() calls append fresh chunks and run those instead.
        let interner = Interner::new();
        let mut tmp_interner = interner;
        let chunk = {
            let lexer_tokens = {
                let mut lexer = Lexer::new("", &mut tmp_interner);
                lexer.tokenize()
            };
            let prog = {
                let mut parser = Parser::new(lexer_tokens, "", &mut tmp_interner);
                parser.parse_program().expect("empty program parses")
            };
            let compiler = Compiler::new(&mut tmp_interner);
            compiler.compile_program(&prog).expect("empty program compiles")
        };
        let mut vm = Vm::new(chunk, tmp_interner);
        // Run the empty placeholder once so the script frame exits cleanly.
        let _ = vm.run();
        Self {
            vm,
            max_steps: 0,
            silent_console: false,
            next_host_tag: 1,
        }
    }

    /// Set a fuel limit (max VM instructions). 0 = unlimited (the default).
    pub fn set_max_steps(&mut self, n: u64) {
        self.max_steps = n;
        self.vm.max_steps = n;
    }

    /// Suppress stdout/stderr writes from `console.log/warn/error`.
    /// The output is still captured into the buffer returned by
    /// `eval_with_output`.
    pub fn set_silent_console(&mut self, silent: bool) {
        self.silent_console = silent;
        self.vm.silent_console = silent;
    }

    /// Register a host (Rust) function as a global on the engine. The function
    /// is callable from JS as a regular function value: `name(arg1, arg2, ...)`.
    /// The closure receives `&mut Vm` so it can intern strings, allocate
    /// objects, call back into JS, and throw via `Err(reason)`.
    pub fn register_host_fn<F>(&mut self, name: &str, f: F)
    where
        F: Fn(&mut Vm, Value, &[Value]) -> Result<Value, Value> + Send + Sync + 'static,
    {
        let name_id = self.vm.interner.intern(name);
        let func: NativeFn = std::sync::Arc::new(f);
        let obj = JsObject {
            properties: Vec::new(),
            prototype: None,
            kind: ObjectKind::Function(FunctionKind::Native { name: name_id, func }),
            marked: false,
            extensible: true,
        };
        let oid = self.vm.heap.allocate(obj);
        let val = Value::object_id(oid);
        self.vm.globals.insert(name_id, val);
        let idx = name_id.0 as usize;
        if idx >= self.vm.globals_vec.len() {
            self.vm.globals_vec.resize(idx + 1, Value::null());
        }
        self.vm.globals_vec[idx] = val;
        self.vm.global_version = self.vm.global_version.wrapping_add(1);
        // Mirror onto globalThis so `globalThis.name` and `Object.keys(globalThis)`
        // see it without a separate lookup path.
        let gt = self.vm.global_this_oid;
        if let Some(obj) = self.vm.heap.get_mut(gt) {
            obj.set_property(name_id, val);
        }
    }

    /// Allocate a fresh tag identifying a host class (e.g. `HTMLElement`,
    /// `EventTarget`). Tags are opaque integers — store the returned tag and
    /// pass it back into `alloc_host_object`.
    pub fn register_host_class(&mut self, _name: &str) -> HostTag {
        let tag = HostTag(self.next_host_tag);
        self.next_host_tag += 1;
        tag
    }

    /// Allocate a JS object whose internal kind is `Host { tag, payload }`,
    /// returning a `Value` the embedder can hand to JS. The payload is an
    /// opaque 64-bit handle interpreted by the embedder (typically an index
    /// into a side table).
    pub fn alloc_host_object(&mut self, tag: HostTag, payload: u64) -> Value {
        let obj = JsObject {
            properties: Vec::new(),
            prototype: None,
            kind: ObjectKind::Host { tag: tag.0, payload },
            marked: false,
            extensible: true,
        };
        let oid = self.vm.heap.allocate(obj);
        Value::object_id(oid)
    }

    /// Recover the (tag, payload) of a host-allocated object. Returns `None`
    /// for any value that isn't an `ObjectKind::Host`.
    pub fn host_payload(&self, value: Value) -> Option<(HostTag, u64)> {
        let oid = value.as_object_id()?;
        let obj = self.vm.heap.get(oid)?;
        if let ObjectKind::Host { tag, payload } = &obj.kind {
            Some((HostTag(*tag), *payload))
        } else {
            None
        }
    }

    /// Borrow the underlying VM. Useful for callbacks that need to call back
    /// into JS (`vm.call_function(...)`) outside the registered native-fn
    /// context.
    pub fn vm(&mut self) -> &mut Vm {
        &mut self.vm
    }

    /// Evaluate a JavaScript source string and return the result. State (the
    /// global scope, registered host functions, and the heap) persists from
    /// previous calls.
    pub fn eval(&mut self, source: &str) -> Result<Value, EngineError> {
        let tokens = {
            let mut lexer = Lexer::new(source, &mut self.vm.interner);
            lexer.tokenize()
        };
        let program = {
            let mut parser = Parser::new(tokens, source, &mut self.vm.interner);
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
        let chunk = {
            let compiler = Compiler::new(&mut self.vm.interner);
            compiler
                .compile_program(&program)
                .map_err(EngineError::CompileError)?
        };
        let result = self.vm.load_and_run(chunk).map_err(EngineError::RuntimeError);
        // Drain microtask queue (Promise .then callbacks)
        let _ = self.vm.drain_microtasks();
        result
    }

    /// Evaluate source and return (result_string, console_output_lines).
    pub fn eval_with_output(&mut self, source: &str) -> (String, Vec<String>) {
        let tokens = {
            let mut lexer = Lexer::new(source, &mut self.vm.interner);
            lexer.tokenize()
        };
        let program = {
            let mut parser = Parser::new(tokens, source, &mut self.vm.interner);
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
            let compiler = Compiler::new(&mut self.vm.interner);
            match compiler.compile_program(&program) {
                Ok(c) => c,
                Err(e) => return (format!("CompileError: {e}"), vec![]),
            }
        };
        let pre_output_len = self.vm.output.len();
        let result = self.vm.load_and_run(chunk);
        let _ = self.vm.drain_microtasks();
        let output: Vec<String> = self.vm.output[pre_output_len..].to_vec();
        let result_str = match result {
            Ok(val) => self.display_value(&val),
            Err(e) => format!("Error: {e}"),
        };
        (result_str, output)
    }

    /// Get a reference to the string interner (for resolving StringIds in results).
    pub fn interner(&self) -> &Interner {
        &self.vm.interner
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
            self.vm.interner.resolve(id).to_string()
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
