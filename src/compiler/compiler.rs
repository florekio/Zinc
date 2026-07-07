use crate::ast::node::*;
use crate::compiler::chunk::{Chunk, ChunkFlags};
use crate::compiler::opcode::OpCode;
use crate::runtime::value::Value;
use crate::util::interner::{Interner, StringId};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct CompileError {
    pub message: String,
    pub offset: u32,
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CompileError at {}: {}", self.offset, self.message)
    }
}

impl std::error::Error for CompileError {}

impl From<CompileError> for String {
    fn from(e: CompileError) -> Self {
        e.to_string()
    }
}

// ---------------------------------------------------------------------------
// Local variable tracking
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Local {
    name: StringId,
    depth: u32,
    initialized: bool,
    captured: bool,
    is_const: bool,
}

use crate::compiler::chunk::UpvalueDescriptor;

#[derive(Clone)]
struct CompilerUpvalue {
    /// Local slot in the enclosing function (is_local) or upvalue index
    /// in the enclosing function's list. u16: minified bundles routinely
    /// exceed 256 locals per function — truncating made closures capture
    /// the wrong variable (react-dom's `new Y(...)` saw a number).
    index: u16,
    is_local: bool,
}

/// One enclosing function scope on the capture chain. Holds that function's
/// locals and upvalues so a nested function can both mark an outer local
/// captured and thread an upvalue through every intermediate function.
struct EnclosingFrame {
    locals: Vec<Local>,
    upvalues: Vec<CompilerUpvalue>,
}

// ---------------------------------------------------------------------------
// Loop / break / continue bookkeeping
// ---------------------------------------------------------------------------

struct LoopCtx {
    /// Start of the condition (target for `continue` / `Loop`).
    continue_target: usize,
    /// Pending break-jump offsets that need patching after the loop.
    break_patches: Vec<usize>,
    /// Pending continue-jump offsets for `for` loops (patched to the update position).
    continue_patches: Vec<usize>,
    /// Scope depth when the loop was entered so we know how many locals to pop.
    scope_depth: u32,
    /// Optional label for labeled statements.
    label: Option<StringId>,
    /// True if this is a `for(;;)` loop with a deferred continue target.
    has_deferred_continue: bool,
    /// Number of active try/finally handlers when this loop was entered.
    try_depth: usize,
    /// Number of enclosing `with` blocks when this loop was entered, so a
    /// `break`/`continue` can emit WithExit for each `with` it jumps out of.
    with_depth: usize,
    /// True for a `switch` context, which accepts `break` but is NOT a valid
    /// `continue` target — `continue` must skip it to the enclosing loop.
    is_switch: bool,
}

// ---------------------------------------------------------------------------
// Compiler
// ---------------------------------------------------------------------------

pub struct Compiler<'a> {
    chunk: Chunk,
    locals: Vec<Local>,
    upvalues: Vec<CompilerUpvalue>,
    scope_depth: u32,
    interner: &'a mut Interner,
    loops: Vec<LoopCtx>,
    /// Stack of enclosing function scopes, outermost first and the immediate
    /// parent last. Each frame owns that function's locals + upvalues so that
    /// resolving a free variable can (a) mark the owning local captured and
    /// (b) thread an upvalue through every intermediate function — i.e. proper
    /// transitive capture from grandparent (and deeper) scopes, not just the
    /// immediate parent. Frames are pushed when descending into a nested
    /// function and popped (with their mutations intact) on the way out.
    enclosing_chain: Vec<EnclosingFrame>,
    /// Set of global-scope const variable names (to prevent reassignment).
    const_globals: std::collections::HashSet<StringId>,
    /// Label from an enclosing labeled statement, to be adopted by the next loop.
    pending_label: Option<StringId>,
    /// Name hint for anonymous function expressions (used for class methods).
    pending_function_name: Option<StringId>,
    /// Stack of active finally blocks (None if try has no finally). Used to
    /// inline finally code before break/continue that exits the try block.
    /// One entry per try statement currently being compiled (innermost
    /// last): the finally body to inline on early exits (None if the try
    /// has no finalizer), and whether the runtime exception handler is
    /// still active in that region (true inside the try block; false
    /// inside the catch block, where handle_throw already popped it).
    finally_stack: Vec<(Option<std::rc::Rc<Vec<Statement>>>, bool)>,
    /// Number of lexically-enclosing `with` blocks. A `break`/`continue` out of
    /// a `with` must emit WithExit for each scope it jumps past (the lexical
    /// WithExit at the body's end is skipped by the jump).
    with_depth: usize,
    /// Depth of nested class bodies. Class bodies (including method bodies)
    /// are implicitly strict per spec; tracking this lets
    /// compile_function_body_with_self set the STRICT flag for class methods.
    class_depth: u32,
    /// Top-level `let` / `const` names whose slots were reserved at function
    /// entry by the hoist pass (so hoisted function declarations can close
    /// over them). When the actual declaration statement compiles, it assigns
    /// into the reserved slot instead of pushing a new local, consuming its
    /// entry here. Saved/restored around each nested function body.
    predeclared_lex: Vec<StringId>,
    /// `locals.len()` at each enclosing `with` entry (innermost last). A local
    /// declared before the innermost `with` may be shadowed by the with-scope
    /// object at runtime and needs a WithGetCheck/WithSetCheck guard; one
    /// declared inside the body (e.g. `let x`) is inner to the with scope and
    /// never guarded.
    with_local_floor: Vec<usize>,
}

impl<'a> Compiler<'a> {
    // ====================================================================
    // Construction & entry point
    // ====================================================================

    pub fn new(interner: &'a mut Interner) -> Self {
        let script_name = interner.intern("<script>");
        Self {
            chunk: Chunk::new(script_name, script_name),
            locals: Vec::new(),
            upvalues: Vec::new(),
            scope_depth: 0,
            interner,
            loops: Vec::new(),
            enclosing_chain: Vec::new(),
            const_globals: std::collections::HashSet::new(),
            pending_label: None,
            pending_function_name: None,
            finally_stack: Vec::new(),
            with_depth: 0,
            class_depth: 0,
            predeclared_lex: Vec::new(),
            with_local_floor: Vec::new(),
        }
    }

    pub fn compile_program(mut self, program: &Program) -> Result<Chunk, String> {
        // Top-level script/eval: value-producing statements update the VM
        // completion register so `eval(...)` returns the spec completion value.
        self.chunk.flags |= ChunkFlags::SCRIPT;
        if program.source_type == SourceType::Module {
            self.chunk.flags |= ChunkFlags::MODULE;
            self.chunk.flags |= ChunkFlags::STRICT; // modules are always strict
        }
        // Detect "use strict" directive prologue
        if self.has_use_strict_directive(&program.body) {
            self.chunk.flags |= ChunkFlags::STRICT;
        }
        // Hoist var declarations: scan for all `var` in the body and define them as undefined
        if self.scope_depth == 0 {
            let mut hoisted = Vec::new();
            for stmt in &program.body {
                collect_var_declarations(stmt, &mut hoisted);
            }
            let line = 0;
            for name in hoisted {
                // Only define if not already a function declaration (functions hoist with value)
                let idx = self.make_string_constant(name);
                self.chunk.emit_op(OpCode::Undefined, line);
                self.chunk.emit_op_u16(OpCode::DefineGlobal, idx, line);
            }
        }

        // Hoist top-level function declarations with their value (per spec, function
        // declarations are hoisted to the top of the script with a closure).
        let mut hoisted_top_fns: Vec<StringId> = Vec::new();
        if self.scope_depth == 0 {
            for stmt in &program.body {
                if let Statement::Function(f) = stmt
                    && let Some(name) = f.id
                {
                    let line = f.span.start;
                    let child_chunk = self.compile_function_body(name, &f.params, &f.body, f.is_async, f.is_generator)?;
                    let chunk_idx = self.chunk.child_chunks.len() as u16;
                    let upvalue_descs = child_chunk.upvalue_descriptors.clone();
                    self.chunk.child_chunks.push(child_chunk);
                    self.chunk.emit_op_u16(OpCode::Closure, chunk_idx, line);
                    for desc in &upvalue_descs {
                        self.chunk.emit_byte(if desc.is_local { 1 } else { 0 }, line);
                        self.chunk.emit_byte((desc.index >> 8) as u8, line);
            self.chunk.emit_byte((desc.index & 0xFF) as u8, line);
                    }
                    let idx = self.make_string_constant(name);
                    self.chunk.emit_op_u16(OpCode::DefineGlobal, idx, line);
                    hoisted_top_fns.push(name);
                }
            }
        }

        let len = program.body.len();
        for (i, stmt) in program.body.iter().enumerate() {
            // Skip already-hoisted top-level function declarations.
            if let Statement::Function(f) = stmt
                && let Some(name) = f.id
                && hoisted_top_fns.contains(&name)
            {
                continue;
            }
            let _ = i;
            let _ = len;
            // Every value-producing statement now routes its value into the
            // completion register via SetCompletion (see compile_statement),
            // and Halt returns that register — so no special last-statement case.
            self.compile_statement(stmt)?;
        }
        let line = self.current_line();
        self.chunk.emit_op(OpCode::Halt, line);
        self.chunk.local_count = self.locals.len() as u16;
        if self.chunk.jump_overflow {
            return Err("script too large: a jump offset exceeded the i16 encoding".to_string());
        }
        Ok(self.chunk)
    }

    // ====================================================================
    // Tiny helpers
    // ====================================================================

    fn current_line(&self) -> u32 {
        self.chunk.lines.last().map(|l| l.1).unwrap_or(1)
    }

    fn make_string_constant(&mut self, name: StringId) -> u16 {
        self.chunk.add_constant(Value::string(name))
    }

    fn emit_constant(&mut self, value: Value, line: u32) {
        let idx = self.chunk.add_constant(value);
        self.chunk.emit_op_u16(OpCode::Const, idx, line);
    }

    /// Emit GetProperty with an embedded IC slot (5 bytes total).
    fn emit_get_property(&mut self, name_idx: u16, line: u32) {
        let ic_slot = self.chunk.alloc_ic_slot();
        self.chunk.emit_byte(OpCode::GetProperty as u8, line);
        self.chunk.code.push((name_idx >> 8) as u8);
        self.chunk.code.push((name_idx & 0xFF) as u8);
        self.chunk.code.push((ic_slot >> 8) as u8);
        self.chunk.code.push((ic_slot & 0xFF) as u8);
    }

    /// Emit SetProperty with an embedded IC slot (5 bytes total).
    fn emit_set_property(&mut self, name_idx: u16, line: u32) {
        let ic_slot = self.chunk.alloc_ic_slot();
        self.chunk.emit_byte(OpCode::SetProperty as u8, line);
        self.chunk.code.push((name_idx >> 8) as u8);
        self.chunk.code.push((name_idx & 0xFF) as u8);
        self.chunk.code.push((ic_slot >> 8) as u8);
        self.chunk.code.push((ic_slot & 0xFF) as u8);
    }


    // ---- scope ----










    // ---- variable get / set ----








    /// At script/eval level, reset the completion register to `undefined`.
    /// Used by statements whose runtime semantics are `UpdateEmpty(value, undefined)`
    /// (if / with): emitting this before the body means an empty body yields
    /// `undefined`, while a body containing value statements overwrites it.
    fn emit_completion_reset(&mut self, line: u32) {
        if self.chunk.flags.contains(ChunkFlags::SCRIPT) {
            self.chunk.emit_op(OpCode::Undefined, line);
            self.chunk.emit_op(OpCode::SetCompletion, line);
        }
    }

    /// Emit bytecode that pops the current stack value and throws a runtime TypeError.
    fn emit_throw_type_error(&mut self, msg: &str, line: u32) {
        self.chunk.emit_op(OpCode::Pop, line);
        let te_name = self.interner.intern("TypeError");
        let te_idx = self.make_string_constant(te_name);
        self.chunk.emit_op_u16(OpCode::GetGlobal, te_idx, line);
        let msg_id = self.interner.intern(msg);
        let msg_idx = self.make_string_constant(msg_id);
        self.chunk.emit_op_u16(OpCode::Const, msg_idx, line);
        self.chunk.emit_op_u8(OpCode::Construct, 1, line);
        self.chunk.emit_op(OpCode::Throw, line);
    }

    // ====================================================================
    // Statements
    // ====================================================================


    // ---- variable declaration ----


    // ---- if / else ----


    // ---- while ----


    // ---- do-while ----


    // ---- for ----


    // ---- for-in (simplified) ----


    // ---- for-of ----


    // ---- switch ----


    // ---- return ----


    // ---- finally inlining helper ----


    // ---- break ----


    // ---- continue ----


    // ---- destructuring binding helpers ----










    // ---- throw ----


    // ---- try / catch / finally ----


    // ---- function declaration ----


    // ---- class declaration ----




    // ---- labeled ----


    // ---- with ----


    // ---- import (stub) ----



    /// Check if the body starts with a "use strict" directive prologue.
    fn has_use_strict_directive(&self, body: &[Statement]) -> bool {
        for stmt in body {
            match stmt {
                Statement::Expression(expr_stmt) => {
                    if let Expression::StringLiteral(s) = &expr_stmt.expression {
                        let text = self.interner.resolve(s.value);
                        if text == "use strict" {
                            return true;
                        }
                        // Continue checking — directives can be multiple strings
                        continue;
                    }
                    break; // Non-string expression ends directive prologue
                }
                _ => break, // Any non-expression statement ends prologue
            }
        }
        false
    }

    fn extract_declaration_names(&self, stmt: &Statement) -> Vec<StringId> {
        match stmt {
            Statement::Variable(decl) => {
                decl.declarations.iter().filter_map(|d| {
                    if let Pattern::Identifier(id) = &d.id { Some(id.name) } else { None }
                }).collect()
            }
            Statement::Function(f) => {
                if let Some(name) = f.id { vec![name] } else { Vec::new() }
            }
            Statement::Class(c) => {
                if let Some(name) = c.id { vec![name] } else { Vec::new() }
            }
            _ => Vec::new(),
        }
    }

    // ---- loop-break helper ----

    fn patch_loop_breaks(&mut self) {
        let ctx = self.loops.pop().expect("no loop context to pop");
        for patch in ctx.break_patches {
            self.chunk.patch_jump(patch);
        }
    }

    // ====================================================================
    // Function / arrow compilation (child chunk via state swap)
    // ====================================================================







    // ====================================================================
    // Expressions
    // ====================================================================


    // ---- number literal ----


    // ---- string literal ----


    // ---- identifier ----


    // ---- binary ----


    // ---- unary ----



    // ---- update (++ / --) ----


    // ---- logical (short-circuit) ----


    // ---- conditional (ternary) ----


    // ---- assignment ----








    // ---- sequence ----


    // ---- member access ----


    // ---- call ----


    // ---- new ----



    // ---- array ----


    // ---- object ----





    // ---- function expression ----



    // ---- arrow function expression ----


    // ---- class expression ----


    // ---- template literal ----


    // ---- tagged template ----


    // ---- optional chaining ----


    // ---- yield ----


    // ---- await ----


    // ---- regexp ----


    // ---- meta property ----

}

/// Format a finite f64 the way `Number.prototype.toString` does (shortest
/// round-trip decimal, with exponential notation for |x| < 1e-6 or |x| >= 1e21).
fn js_canonical_number_string(f: f64) -> String {
    if f.is_nan() { return "NaN".into(); }
    if f.is_infinite() { return if f > 0.0 { "Infinity".into() } else { "-Infinity".into() }; }
    if f == 0.0 { return "0".into(); }
    let abs = f.abs();
    if !(1e-6..1e21).contains(&abs) {
        let raw = format!("{f:e}");
        if let Some(epos) = raw.find('e') {
            let exp = &raw[epos + 1..];
            if !exp.starts_with('-') && !exp.starts_with('+') {
                return format!("{}e+{}", &raw[..epos], exp);
            }
        }
        raw
    } else {
        format!("{f}")
    }
}

/// Recursively collect all leaf identifier names from a binding pattern.
fn collect_binding_names(pat: &Pattern) -> Vec<StringId> {
    let mut names = Vec::new();
    collect_binding_names_into(pat, &mut names);
    names
}
fn collect_binding_names_into(pat: &Pattern, out: &mut Vec<StringId>) {
    match pat {
        Pattern::Identifier(id) => out.push(id.name),
        Pattern::Array(inner) => {
            for elem in inner.elements.iter().flatten() {
                match elem {
                    Pattern::Rest(r) => collect_binding_names_into(&r.argument, out),
                    Pattern::Assignment(a) => collect_binding_names_into(&a.left, out),
                    p => collect_binding_names_into(p, out),
                }
            }
        }
        Pattern::Object(inner) => {
            for prop in &inner.properties {
                match prop {
                    ObjectPatternProperty::Property { value, .. } => {
                        if let Pattern::Assignment(a) = value {
                            collect_binding_names_into(&a.left, out);
                        } else {
                            collect_binding_names_into(value, out);
                        }
                    }
                    ObjectPatternProperty::Rest(r) => collect_binding_names_into(&r.argument, out),
                }
            }
        }
        Pattern::Rest(r) => collect_binding_names_into(&r.argument, out),
        Pattern::Assignment(a) => collect_binding_names_into(&a.left, out),
        Pattern::Member(_) => {}
    }
}

/// Collect all `var` declaration names from a statement tree (for hoisting).
/// Collect the identifier names bound by a parameter pattern (including
/// destructuring and defaults), for direct-eval-in-parameter collision checks.
fn collect_pattern_names(pat: &Pattern, out: &mut Vec<StringId>) {
    match pat {
        Pattern::Identifier(id) => out.push(id.name),
        Pattern::Assignment(a) => collect_pattern_names(&a.left, out),
        Pattern::Rest(r) => collect_pattern_names(&r.argument, out),
        Pattern::Array(arr) => {
            for el in arr.elements.iter().flatten() { collect_pattern_names(el, out); }
        }
        Pattern::Object(obj) => {
            for prop in &obj.properties {
                match prop {
                    ObjectPatternProperty::Property { value, .. } => collect_pattern_names(value, out),
                    ObjectPatternProperty::Rest(r) => collect_pattern_names(&r.argument, out),
                }
            }
        }
        Pattern::Member(_) => {}
    }
}

/// Collect the top-level lexical (let/const/class) binding names of a function
/// body — names a direct eval's var/function declaration may not redeclare.
fn collect_body_lexical_names(body: &[Statement]) -> Vec<StringId> {
    let mut out = Vec::new();
    for stmt in body {
        match stmt {
            Statement::Variable(d) if matches!(d.kind, VarKind::Let | VarKind::Const) => {
                for dec in &d.declarations { collect_pattern_names(&dec.id, &mut out); }
            }
            Statement::Class(c) => { if let Some(name) = c.id { out.push(name); } }
            _ => {}
        }
    }
    out
}

/// Collect all `var`-declared names in a statement list (recursing into nested
/// statements but not into nested functions). Used for eval var-hoisting.
pub(crate) fn collect_program_var_names(body: &[Statement], out: &mut Vec<StringId>) {
    for stmt in body { collect_var_declarations(stmt, out); }
}

fn collect_var_declarations(stmt: &Statement, out: &mut Vec<StringId>) {
    match stmt {
        Statement::Variable(decl) if decl.kind == VarKind::Var => {
            for d in &decl.declarations {
                if let Pattern::Identifier(id) = &d.id {
                    out.push(id.name);
                }
            }
        }
        Statement::Block(b) => {
            for s in &b.body { collect_var_declarations(s, out); }
        }
        Statement::If(i) => {
            collect_var_declarations(&i.consequent, out);
            if let Some(alt) = &i.alternate { collect_var_declarations(alt, out); }
        }
        Statement::While(w) => collect_var_declarations(&w.body, out),
        Statement::DoWhile(d) => collect_var_declarations(&d.body, out),
        Statement::For(f) => {
            if let Some(ForInit::Variable(decl)) = &f.init
                && decl.kind == VarKind::Var {
                    for d in &decl.declarations {
                        if let Pattern::Identifier(id) = &d.id { out.push(id.name); }
                    }
                }
            collect_var_declarations(&f.body, out);
        }
        Statement::ForIn(fi) => {
            if let ForInOfLeft::Variable(decl) = &fi.left
                && decl.kind == VarKind::Var {
                    for d in &decl.declarations {
                        if let Pattern::Identifier(id) = &d.id { out.push(id.name); }
                    }
                }
            collect_var_declarations(&fi.body, out);
        }
        Statement::ForOf(fo) => {
            if let ForInOfLeft::Variable(decl) = &fo.left
                && decl.kind == VarKind::Var {
                    for d in &decl.declarations {
                        if let Pattern::Identifier(id) = &d.id { out.push(id.name); }
                    }
                }
            collect_var_declarations(&fo.body, out);
        }
        Statement::Switch(s) => {
            for case in &s.cases {
                for cs in &case.consequent { collect_var_declarations(cs, out); }
            }
        }
        Statement::Try(t) => {
            for s in &t.block.body { collect_var_declarations(s, out); }
            if let Some(h) = &t.handler {
                for s in &h.body.body { collect_var_declarations(s, out); }
            }
            if let Some(f) = &t.finalizer {
                for s in &f.body { collect_var_declarations(s, out); }
            }
        }
        Statement::Labeled(l) => collect_var_declarations(&l.body, out),
        Statement::With(w) => collect_var_declarations(&w.body, out),
        _ => {}
    }
}

// ============================================================================
// Tests
// ============================================================================

mod expressions;
mod functions;
mod patterns;
mod scopes;
mod statements;

#[cfg(test)]
mod tests;
