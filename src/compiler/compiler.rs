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
    index: u8,
    is_local: bool,
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
    /// Parent compiler's locals (for upvalue resolution across function boundaries).
    /// This is set when compiling a nested function.
    enclosing_locals: Option<Vec<Local>>,
    enclosing_upvalues: Option<Vec<CompilerUpvalue>>,
    /// Set of global-scope const variable names (to prevent reassignment).
    const_globals: std::collections::HashSet<StringId>,
    /// Label from an enclosing labeled statement, to be adopted by the next loop.
    pending_label: Option<StringId>,
    /// Name hint for anonymous function expressions (used for class methods).
    pending_function_name: Option<StringId>,
    /// Stack of active finally blocks (None if try has no finally). Used to
    /// inline finally code before break/continue that exits the try block.
    finally_stack: Vec<Option<std::rc::Rc<Vec<Statement>>>>,
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
            enclosing_locals: None,
            enclosing_upvalues: None,
            const_globals: std::collections::HashSet::new(),
            pending_label: None,
            pending_function_name: None,
            finally_stack: Vec::new(),
        }
    }

    pub fn compile_program(mut self, program: &Program) -> Result<Chunk, String> {
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
                        self.chunk.emit_byte(desc.index, line);
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
            let is_last = i == len - 1;
            if is_last {
                // For the last statement, if it's an expression, keep value on stack for Halt
                if let Statement::Expression(e) = stmt {
                    self.compile_expr(&e.expression)?;
                } else {
                    self.compile_statement(stmt)?;
                }
            } else {
                self.compile_statement(stmt)?;
            }
        }
        let line = self.current_line();
        self.chunk.emit_op(OpCode::Halt, line);
        self.chunk.local_count = self.locals.len() as u16;
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

    /// How many locals sit above the given scope depth?
    fn locals_above_depth(&self, depth: u32) -> usize {
        self.locals
            .iter()
            .rev()
            .take_while(|l| l.depth > depth)
            .count()
    }

    // ---- scope ----

    fn begin_scope(&mut self) {
        self.scope_depth += 1;
    }

    fn end_scope(&mut self) {
        self.scope_depth -= 1;
        let line = self.current_line();
        while let Some(local) = self.locals.last() {
            if local.depth <= self.scope_depth {
                break;
            }
            if local.captured {
                self.chunk.emit_op(OpCode::CloseUpvalue, line);
            } else {
                self.chunk.emit_op(OpCode::Pop, line);
            }
            self.locals.pop();
        }
    }

    fn add_local(&mut self, name: StringId) {
        self.locals.push(Local {
            name,
            depth: self.scope_depth,
            initialized: false,
            captured: false,
            is_const: false,
        });
    }

    fn mark_initialized(&mut self) {
        if let Some(local) = self.locals.last_mut() {
            local.initialized = true;
        }
    }

    fn resolve_local(&self, name: StringId) -> Option<usize> {
        for (i, local) in self.locals.iter().enumerate().rev() {
            if local.name == name {
                return Some(i);
            }
        }
        None
    }

    /// Try to resolve a variable as an upvalue (captured from enclosing scope).
    fn resolve_upvalue(&mut self, name: StringId) -> Option<u8> {
        // Check if the variable is in the enclosing function's locals
        if let Some(ref mut enc_locals) = self.enclosing_locals {
            for (i, local) in enc_locals.iter_mut().enumerate().rev() {
                if local.name == name {
                    local.captured = true;
                    return Some(self.add_upvalue(i as u8, true));
                }
            }
        }

        // TODO: transitive upvalue capture (capturing from grandparent scopes)
        // Currently only supports one level of capture (enclosing locals).

        None
    }

    fn add_upvalue(&mut self, index: u8, is_local: bool) -> u8 {
        // Check if we already have this upvalue
        for (i, uv) in self.upvalues.iter().enumerate() {
            if uv.index == index && uv.is_local == is_local {
                return i as u8;
            }
        }
        let idx = self.upvalues.len() as u8;
        self.upvalues.push(CompilerUpvalue { index, is_local });
        idx
    }

    // ---- variable get / set ----

    fn compile_get_variable(&mut self, name: StringId, line: u32) -> Result<(), String> {
        if let Some(slot) = self.resolve_local(name) {
            if slot <= u8::MAX as usize {
                self.chunk.emit_op_u8(OpCode::GetLocal, slot as u8, line);
            } else {
                self.chunk
                    .emit_op_u16(OpCode::GetLocalWide, slot as u16, line);
            }
        } else if let Some(uv_idx) = self.resolve_upvalue(name) {
            self.chunk.emit_op_u8(OpCode::GetUpvalue, uv_idx, line);
        } else {
            let idx = self.make_string_constant(name);
            self.chunk.emit_op_u16(OpCode::GetGlobal, idx, line);
        }
        Ok(())
    }

    fn compile_set_variable(&mut self, name: StringId, line: u32) -> Result<(), String> {
        if let Some(slot) = self.resolve_local(name) {
            if self.locals[slot].is_const {
                let var_name = self.interner.resolve(name).to_owned();
                self.emit_throw_type_error(
                    &format!("Assignment to constant variable '{var_name}'"), line);
                return Ok(());
            }
            if slot <= u8::MAX as usize {
                self.chunk.emit_op_u8(OpCode::SetLocal, slot as u8, line);
            } else {
                self.chunk
                    .emit_op_u16(OpCode::SetLocalWide, slot as u16, line);
            }
        } else if let Some(uv_idx) = self.resolve_upvalue(name) {
            self.chunk.emit_op_u8(OpCode::SetUpvalue, uv_idx, line);
        } else {
            if self.const_globals.contains(&name) {
                let var_name = self.interner.resolve(name).to_owned();
                self.emit_throw_type_error(
                    &format!("Assignment to constant variable '{var_name}'"), line);
                return Ok(());
            }
            let idx = self.make_string_constant(name);
            self.chunk.emit_op_u16(OpCode::SetGlobal, idx, line);
        }
        Ok(())
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

    fn compile_statement(&mut self, stmt: &Statement) -> Result<(), String> {
        match stmt {
            Statement::Expression(e) => {
                self.compile_expr(&e.expression)?;
                self.chunk.emit_op(OpCode::Pop, self.current_line());
                Ok(())
            }
            Statement::Variable(decl) => self.compile_var_declaration(decl),
            Statement::Block(block) => {
                self.begin_scope();
                for s in &block.body {
                    self.compile_statement(s)?;
                }
                self.end_scope();
                Ok(())
            }
            Statement::If(if_stmt) => self.compile_if(if_stmt),
            Statement::While(w) => self.compile_while(w),
            Statement::DoWhile(d) => self.compile_do_while(d),
            Statement::For(f) => self.compile_for(f),
            Statement::ForIn(f) => self.compile_for_in(f),
            Statement::ForOf(f) => self.compile_for_of(f),
            Statement::Switch(s) => self.compile_switch(s),
            Statement::Return(r) => self.compile_return(r),
            Statement::Break(b) => self.compile_break(b),
            Statement::Continue(c) => self.compile_continue(c),
            Statement::Throw(t) => self.compile_throw(t),
            Statement::Try(t) => self.compile_try(t),
            Statement::Function(f) => self.compile_function_decl(f),
            Statement::Class(c) => self.compile_class_decl(c),
            Statement::Labeled(l) => self.compile_labeled(l),
            Statement::With(w) => self.compile_with(w),
            Statement::Import(i) => self.compile_import(i),
            Statement::Export(e) => self.compile_export(e),
            Statement::Empty(_) => Ok(()),
            Statement::Debugger(span) => {
                self.chunk.emit_op(OpCode::Debugger, span.start);
                Ok(())
            }
        }
    }

    // ---- variable declaration ----

    fn compile_var_declaration(&mut self, decl: &VariableDeclaration) -> Result<(), String> {
        for declarator in &decl.declarations {
            match &declarator.id {
                Pattern::Identifier(id) => {
                    let name = id.name;
                    let line = declarator.span.start;

                    if let Some(init) = &declarator.init {
                        if Self::is_anonymous_fn_def(init) {
                            self.pending_function_name = Some(name);
                        }
                        self.compile_expr(init)?;

                        if self.scope_depth == 0 {
                            if decl.kind == VarKind::Const {
                                self.const_globals.insert(name);
                            }
                            let idx = self.make_string_constant(name);
                            // For `var x = expr`, the binding is hoisted (already defined as
                            // undefined). The `x = expr` is a normal assignment that must go
                            // through the scope chain — including any active `with` scope.
                            // For let/const, use DefineGlobal which creates the binding fresh.
                            if decl.kind == VarKind::Var {
                                self.chunk.emit_op_u16(OpCode::SetGlobal, idx, line);
                                self.chunk.emit_op(OpCode::Pop, line);
                            } else {
                                self.chunk.emit_op_u16(OpCode::DefineGlobal, idx, line);
                            }
                        } else if decl.kind == VarKind::Var {
                            if let Some(slot) = self.resolve_local(name) {
                                if slot <= u8::MAX as usize {
                                    self.chunk.emit_op_u8(OpCode::SetLocal, slot as u8, line);
                                } else {
                                    self.chunk.emit_op_u16(OpCode::SetLocalWide, slot as u16, line);
                                }
                                self.chunk.emit_op(OpCode::Pop, line);
                            } else {
                                let idx = self.make_string_constant(name);
                                self.chunk.emit_op_u16(OpCode::SetGlobal, idx, line);
                                self.chunk.emit_op(OpCode::Pop, line);
                            }
                        } else {
                            self.add_local(name);
                            self.mark_initialized();
                            if decl.kind == VarKind::Const {
                                self.locals.last_mut().unwrap().is_const = true;
                            }
                            if decl.kind == VarKind::Let || decl.kind == VarKind::Const {
                                let slot = (self.locals.len() - 1) as u8;
                                self.chunk.emit_op_u8(OpCode::InitLet, slot, line);
                            }
                        }
                    } else if decl.kind == VarKind::Var {
                        // var without init: hoisting already initialized to undefined; no-op
                    } else {
                        // let/const without init: create binding initialized to undefined
                        self.chunk.emit_op(OpCode::Undefined, line);
                        self.add_local(name);
                        self.mark_initialized();
                        if decl.kind == VarKind::Const {
                            self.locals.last_mut().unwrap().is_const = true;
                        }
                        if decl.kind == VarKind::Let || decl.kind == VarKind::Const {
                            let slot = (self.locals.len() - 1) as u8;
                            self.chunk.emit_op_u8(OpCode::InitLet, slot, line);
                        }
                    }
                }
                Pattern::Object(obj_pat) => {
                    let line = declarator.span.start;
                    if let Some(init) = &declarator.init {
                        self.compile_expr(init)?;
                    } else {
                        self.chunk.emit_op(OpCode::Undefined, line);
                    }
                    if self.scope_depth > 0 {
                        let anon = self.interner.intern("__destruct_src__");
                        self.add_local(anon);
                        self.mark_initialized();
                        let src_slot = (self.locals.len() - 1) as u8;
                        self.compile_bind_obj_props_local(&obj_pat.properties, src_slot, line)?;
                    } else {
                        self.compile_bind_obj_props_global(&obj_pat.properties, line)?;
                        self.chunk.emit_op(OpCode::Pop, line);
                    }
                }
                Pattern::Array(arr_pat) => {
                    let line = declarator.span.start;
                    if let Some(init) = &declarator.init {
                        self.compile_expr(init)?;
                    } else {
                        self.chunk.emit_op(OpCode::Undefined, line);
                    }
                    self.chunk.emit_op(OpCode::GetIterator, line);
                    if self.scope_depth > 0 {
                        let anon = self.interner.intern("__destruct_iter__");
                        self.add_local(anon);
                        self.mark_initialized();
                        let iter_slot = (self.locals.len() - 1) as u8;
                        self.compile_bind_arr_elems_local(&arr_pat.elements, iter_slot, line)?;
                    } else {
                        self.compile_bind_arr_elems_global(&arr_pat.elements, line)?;
                        self.chunk.emit_op(OpCode::Pop, line);
                    }
                }
                _ => {
                    let line = declarator.span.start;
                    if let Some(init) = &declarator.init {
                        self.compile_expr(init)?;
                    } else {
                        self.chunk.emit_op(OpCode::Undefined, line);
                    }
                    self.chunk.emit_op(OpCode::Pop, line);
                }
            }
        }
        Ok(())
    }

    // ---- if / else ----

    fn compile_if(&mut self, s: &IfStatement) -> Result<(), String> {
        let line = s.span.start;
        self.compile_expr(&s.test)?;
        let then_jump = self.chunk.emit_jump(OpCode::JumpIfFalse, line);
        self.compile_statement(&s.consequent)?;

        if let Some(alt) = &s.alternate {
            let else_jump = self.chunk.emit_jump(OpCode::Jump, line);
            self.chunk.patch_jump(then_jump);
            self.compile_statement(alt)?;
            self.chunk.patch_jump(else_jump);
        } else {
            self.chunk.patch_jump(then_jump);
        }
        Ok(())
    }

    // ---- while ----

    fn compile_while(&mut self, w: &WhileStatement) -> Result<(), String> {
        let line = w.span.start;
        let loop_start = self.chunk.len();

        self.loops.push(LoopCtx {
            continue_target: loop_start,
            break_patches: Vec::new(),
            continue_patches: Vec::new(),
            scope_depth: self.scope_depth,
            label: None,
            has_deferred_continue: false,
            try_depth: self.finally_stack.len(),
        });

        self.compile_expr(&w.test)?;
        let exit_jump = self.chunk.emit_jump(OpCode::JumpIfFalse, line);
        self.compile_statement(&w.body)?;
        self.chunk.emit_loop(loop_start, line);
        self.chunk.patch_jump(exit_jump);

        self.patch_loop_breaks();
        Ok(())
    }

    // ---- do-while ----

    fn compile_do_while(&mut self, d: &DoWhileStatement) -> Result<(), String> {
        let line = d.span.start;
        let loop_start = self.chunk.len();

        // Use deferred continue patching so `continue` jumps to the test, not
        // back to the body start (which would skip the test and infinite-loop).
        self.loops.push(LoopCtx {
            continue_target: loop_start,
            break_patches: Vec::new(),
            continue_patches: Vec::new(),
            scope_depth: self.scope_depth,
            label: None,
            has_deferred_continue: true,
            try_depth: self.finally_stack.len(),
        });

        self.compile_statement(&d.body)?;
        // Patch any `continue` jumps to land here, at the test evaluation.
        if let Some(ctx) = self.loops.last_mut() {
            for patch in std::mem::take(&mut ctx.continue_patches) {
                self.chunk.patch_jump(patch);
            }
        }
        self.compile_expr(&d.test)?;
        let exit_jump = self.chunk.emit_jump(OpCode::JumpIfFalse, line);
        self.chunk.emit_loop(loop_start, line);
        self.chunk.patch_jump(exit_jump);

        self.patch_loop_breaks();
        Ok(())
    }

    // ---- for ----

    fn compile_for(&mut self, f: &ForStatement) -> Result<(), String> {
        let line = f.span.start;
        // Only create a scope for let/const — var should hoist to enclosing scope
        let needs_scope = matches!(&f.init, Some(ForInit::Variable(decl)) if decl.kind != VarKind::Var);
        if needs_scope { self.begin_scope(); }

        // Init.
        if let Some(init) = &f.init {
            match init {
                ForInit::Variable(decl) => self.compile_var_declaration(decl)?,
                ForInit::Expression(expr) => {
                    self.compile_expr(expr)?;
                    self.chunk.emit_op(OpCode::Pop, line);
                }
            }
        }

        let loop_start = self.chunk.len();

        // Push a loop context. For `for` loops with an update expression,
        // continues need to jump to the update, not back to the condition.
        let has_update = f.update.is_some();
        let label = self.pending_label.take();
        self.loops.push(LoopCtx {
            continue_target: loop_start,
            break_patches: Vec::new(),
            continue_patches: Vec::new(),
            scope_depth: self.scope_depth,
            label,
            has_deferred_continue: has_update,
            try_depth: self.finally_stack.len(),
        });

        // Condition.
        let exit_jump = if let Some(test) = &f.test {
            self.compile_expr(test)?;
            Some(self.chunk.emit_jump(OpCode::JumpIfFalse, line))
        } else {
            None
        };

        // Body.
        self.compile_statement(&f.body)?;

        // Patch any deferred continue jumps to land right before the update.
        let continue_target = self.chunk.len();
        if let Some(ctx) = self.loops.last_mut() {
            ctx.continue_target = continue_target;
            for patch in std::mem::take(&mut ctx.continue_patches) {
                self.chunk.patch_jump(patch);
            }
        }

        // Update.
        if let Some(update) = &f.update {
            self.compile_expr(update)?;
            self.chunk.emit_op(OpCode::Pop, line);
        }

        self.chunk.emit_loop(loop_start, line);

        if let Some(exit) = exit_jump {
            self.chunk.patch_jump(exit);
        }

        self.patch_loop_breaks();
        if needs_scope { self.end_scope(); }
        Ok(())
    }

    // ---- for-in (simplified) ----

    fn compile_for_in(&mut self, f: &ForInStatement) -> Result<(), String> {
        let line = f.span.start;
        // Only scope for let/const
        let is_var = matches!(&f.left, ForInOfLeft::Variable(decl) if decl.kind == VarKind::Var);
        if !is_var { self.begin_scope(); }

        // Declare the loop variable
        let var_name = match &f.left {
            ForInOfLeft::Variable(decl) => {
                decl.declarations.first().and_then(|d| {
                    if let Pattern::Identifier(id) = &d.id { Some(id.name) } else { None }
                })
            }
            ForInOfLeft::Pattern(Pattern::Identifier(id)) => Some(id.name),
            _ => None,
        };
        if let Some(name) = var_name {
            self.chunk.emit_op(OpCode::Undefined, line);
            if self.scope_depth <= 1 {
                let idx = self.make_string_constant(name);
                self.chunk.emit_op_u16(OpCode::DefineGlobal, idx, line);
            } else {
                self.add_local(name);
                self.mark_initialized();
            }
        }

        // Compile the object expression, then emit GetForInIterator (key iterator)
        self.compile_expr(&f.right)?;
        self.chunk.emit_op(OpCode::GetForInIterator, line);

        let loop_start = self.chunk.len();

        self.chunk.emit_op(OpCode::Dup, line);
        self.chunk.emit_op(OpCode::IteratorNext, line);
        self.chunk.emit_op(OpCode::Dup, line);
        self.chunk.emit_op(OpCode::IteratorDone, line);
        let exit_jump = self.chunk.emit_jump(OpCode::JumpIfTrue, line);

        self.chunk.emit_op(OpCode::IteratorValue, line);
        if let Some(name) = var_name {
            self.compile_set_variable(name, line)?;
            self.chunk.emit_op(OpCode::Pop, line);
        } else {
            self.chunk.emit_op(OpCode::Pop, line);
        }

        self.loops.push(LoopCtx {
            continue_target: loop_start,
            break_patches: Vec::new(),
            continue_patches: Vec::new(),
            scope_depth: self.scope_depth,
            label: None,
            has_deferred_continue: false,
            try_depth: self.finally_stack.len(),
        });
        self.compile_statement(&f.body)?;
        self.chunk.emit_loop(loop_start, line);

        self.chunk.patch_jump(exit_jump);
        self.chunk.emit_op(OpCode::Pop, line); // pop result
        self.chunk.emit_op(OpCode::Pop, line); // pop iterator

        self.patch_loop_breaks();
        if !is_var { self.end_scope(); }
        Ok(())
    }

    // ---- for-of ----

    fn compile_for_of(&mut self, f: &ForOfStatement) -> Result<(), String> {
        let line = f.span.start;
        let is_var = matches!(&f.left, ForInOfLeft::Variable(decl) if decl.kind == VarKind::Var);
        let is_let_const = matches!(&f.left, ForInOfLeft::Variable(decl) if decl.kind != VarKind::Var);
        if !is_var { self.begin_scope(); }

        // Determine the loop variable pattern
        enum LoopVar {
            Simple(StringId),
            ArrayDestructure,
            ObjectDestructure,
            None,
        }
        let loop_var = match &f.left {
            ForInOfLeft::Variable(decl) => {
                if let Some(d) = decl.declarations.first() {
                    match &d.id {
                        Pattern::Identifier(id) => LoopVar::Simple(id.name),
                        Pattern::Array(_) => LoopVar::ArrayDestructure,
                        Pattern::Object(_) => LoopVar::ObjectDestructure,
                        _ => LoopVar::None,
                    }
                } else { LoopVar::None }
            }
            ForInOfLeft::Pattern(Pattern::Identifier(id)) => LoopVar::Simple(id.name),
            ForInOfLeft::Pattern(Pattern::Array(_)) => LoopVar::ArrayDestructure,
            ForInOfLeft::Pattern(Pattern::Object(_)) => LoopVar::ObjectDestructure,
            ForInOfLeft::Expression(Expression::Array(_)) => LoopVar::ArrayDestructure,
            ForInOfLeft::Expression(Expression::Object(_)) => LoopVar::ObjectDestructure,
            ForInOfLeft::Expression(Expression::Identifier(id)) => LoopVar::Simple(id.name),
            ForInOfLeft::Expression(Expression::Member(_)) => LoopVar::None,
            _ => LoopVar::None,
        };
        // Pre-declare loop variable(s)
        let declare_var = |this: &mut Self, name: StringId| {
            this.chunk.emit_op(OpCode::Undefined, line);
            if this.scope_depth <= 1 {
                let idx = this.make_string_constant(name);
                this.chunk.emit_op_u16(OpCode::DefineGlobal, idx, line);
            } else {
                this.add_local(name);
                this.mark_initialized();
            }
        };
        // For `let/const x of arr`: use per-iteration fresh binding instead of pre-declaring x.
        let fresh_binding_simple = is_let_const && matches!(&loop_var, LoopVar::Simple(_));
        match &loop_var {
            LoopVar::Simple(name) => {
                // Only pre-declare for Variable (new declaration), not Expression (existing var).
                if !fresh_binding_simple && matches!(&f.left, ForInOfLeft::Variable(_)) {
                    declare_var(self, *name);
                }
            }
            LoopVar::ArrayDestructure | LoopVar::ObjectDestructure => {
                if matches!(&f.left, ForInOfLeft::Variable(_)) {
                    let pat = match &f.left {
                        ForInOfLeft::Variable(decl) => decl.declarations.first().map(|d| &d.id),
                        _ => None,
                    };
                    if let Some(pat) = pat {
                        for name in collect_binding_names(pat) {
                            declare_var(self, name);
                        }
                    }
                }
            }
            LoopVar::None => {}
        }

        // Compile the iterable and get its iterator
        self.compile_expr(&f.right)?;
        self.chunk.emit_op(OpCode::GetIterator, line);

        // For fresh-binding-simple: track iterator as anonymous local so slot accounting is correct.
        if fresh_binding_simple {
            let anon = self.interner.intern("(for-of-iter)");
            self.add_local(anon);
            self.mark_initialized();
        }

        let loop_start = self.chunk.len();

        // Call iterator.next()
        self.chunk.emit_op(OpCode::Dup, line);
        self.chunk.emit_op(OpCode::IteratorNext, line);
        self.chunk.emit_op(OpCode::Dup, line);
        self.chunk.emit_op(OpCode::IteratorDone, line);
        let exit_jump = self.chunk.emit_jump(OpCode::JumpIfTrue, line);

        // Get the value and assign to loop variable(s)
        self.chunk.emit_op(OpCode::IteratorValue, line);
        match &loop_var {
            LoopVar::Simple(name) => {
                if fresh_binding_simple {
                    // Value is at TOS; open a per-iteration scope and bind x to it.
                    self.begin_scope();
                    self.add_local(*name);
                    self.mark_initialized();
                } else {
                    self.compile_set_variable(*name, line)?;
                    self.chunk.emit_op(OpCode::Pop, line);
                }
            }
            LoopVar::ArrayDestructure | LoopVar::ObjectDestructure => {
                let pat = match &f.left {
                    ForInOfLeft::Variable(decl) => decl.declarations.first().map(|d| &d.id),
                    ForInOfLeft::Pattern(p) => Some(p),
                    _ => None,
                };
                if let Some(pat) = pat {
                    self.compile_assign_pat(pat, line)?;
                } else {
                    // Expression-based LHS: route through compile_assign_pat
                    match &f.left {
                        ForInOfLeft::Expression(expr @ Expression::Array(_)) => {
                            if let Some(pat) = Self::expr_to_pattern(expr) {
                                self.compile_assign_pat(&pat, line)?;
                            } else {
                                self.chunk.emit_op(OpCode::Pop, line);
                            }
                        }
                        ForInOfLeft::Expression(Expression::Object(obj)) => {
                            // RequireObjectCoercible: throw if null/undefined
                            self.chunk.emit_op(OpCode::Dup, line);
                            let nullish_j = self.chunk.emit_jump(OpCode::JumpIfNullishPeek, line);
                            self.chunk.emit_op(OpCode::Pop, line);
                            let skip_t = self.chunk.emit_jump(OpCode::Jump, line);
                            self.chunk.patch_jump(nullish_j);
                            self.chunk.emit_op(OpCode::Pop, line);
                            self.chunk.emit_op(OpCode::Pop, line);
                            let mid = self.interner.intern("Cannot destructure property of null or undefined");
                            self.emit_constant(Value::string(mid), line);
                            self.chunk.emit_op(OpCode::Throw, line);
                            self.chunk.patch_jump(skip_t);

                            // Collect excluded keys for any rest element
                            let has_rest = obj.properties.iter().any(|p| matches!(p, ObjectProperty::SpreadElement(_)));
                            let excluded_keys: Vec<StringId> = if has_rest {
                                obj.properties.iter().filter_map(|p| {
                                    if let ObjectProperty::Property(prop) = p {
                                        match &prop.key {
                                            PropertyKey::Identifier(s) | PropertyKey::StringLiteral(s) => Some(*s),
                                            _ => None,
                                        }
                                    } else { None }
                                }).collect()
                            } else { vec![] };
                            for prop in &obj.properties {
                                match prop {
                                    ObjectProperty::Property(p) => {
                                        match &p.value {
                                            Expression::Identifier(id) => {
                                                self.chunk.emit_op(OpCode::Dup, line);
                                                match &p.key {
                                                    PropertyKey::Identifier(k) | PropertyKey::StringLiteral(k) => {
                                                        let key_idx = self.make_string_constant(*k);
                                                        self.emit_get_property(key_idx, line);
                                                    }
                                                    PropertyKey::Computed(expr) => {
                                                        self.compile_expr(expr)?;
                                                        if has_rest { self.chunk.emit_op(OpCode::Dup, line); self.chunk.emit_op(OpCode::PushComputedExclude, line); }
                                                        self.chunk.emit_op(OpCode::GetElement, line);
                                                    }
                                                    PropertyKey::NumberLiteral(n) => {
                                                        self.emit_constant(Value::number(*n), line);
                                                        if has_rest { self.chunk.emit_op(OpCode::Dup, line); self.chunk.emit_op(OpCode::PushComputedExclude, line); }
                                                        self.chunk.emit_op(OpCode::GetElement, line);
                                                    }
                                                    _ => { self.chunk.emit_op(OpCode::Pop, line); continue; }
                                                }
                                                self.compile_set_variable(id.name, line)?;
                                                self.chunk.emit_op(OpCode::Pop, line);
                                            }
                                            Expression::Assignment(a) if matches!(&a.left, crate::ast::node::AssignmentTarget::Identifier(_)) => {
                                                // { key = default } or { key: identifier = default }
                                                if let crate::ast::node::AssignmentTarget::Identifier(var_id) = &a.left {
                                                    self.chunk.emit_op(OpCode::Dup, line);
                                                    match &p.key {
                                                        PropertyKey::Identifier(k) | PropertyKey::StringLiteral(k) => {
                                                            let key_idx = self.make_string_constant(*k);
                                                            self.emit_get_property(key_idx, line);
                                                        }
                                                        PropertyKey::Computed(expr) => {
                                                            self.compile_expr(expr)?;
                                                            if has_rest { self.chunk.emit_op(OpCode::Dup, line); self.chunk.emit_op(OpCode::PushComputedExclude, line); }
                                                            self.chunk.emit_op(OpCode::GetElement, line);
                                                        }
                                                        PropertyKey::NumberLiteral(n) => {
                                                            self.emit_constant(Value::number(*n), line);
                                                            if has_rest { self.chunk.emit_op(OpCode::Dup, line); self.chunk.emit_op(OpCode::PushComputedExclude, line); }
                                                            self.chunk.emit_op(OpCode::GetElement, line);
                                                        }
                                                        _ => { self.chunk.emit_op(OpCode::Pop, line); continue; }
                                                    }
                                                    if Self::is_anonymous_fn_def(&a.right) {
                                                        self.pending_function_name = Some(var_id.name);
                                                    }
                                                    self.emit_default_check(&a.right, line)?;
                                                    self.compile_set_variable(var_id.name, line)?;
                                                    self.chunk.emit_op(OpCode::Pop, line);
                                                }
                                            }
                                            val_expr => {
                                                // Nested pattern (object/array/assignment with pattern left)
                                                if let Some(nested_pat) = Self::expr_to_pattern(val_expr) {
                                                    self.chunk.emit_op(OpCode::Dup, line);
                                                    match &p.key {
                                                        PropertyKey::Identifier(k) | PropertyKey::StringLiteral(k) => {
                                                            let key_idx = self.make_string_constant(*k);
                                                            self.emit_get_property(key_idx, line);
                                                        }
                                                        PropertyKey::Computed(expr) => {
                                                            self.compile_expr(expr)?;
                                                            if has_rest { self.chunk.emit_op(OpCode::Dup, line); self.chunk.emit_op(OpCode::PushComputedExclude, line); }
                                                            self.chunk.emit_op(OpCode::GetElement, line);
                                                        }
                                                        PropertyKey::NumberLiteral(n) => {
                                                            self.emit_constant(Value::number(*n), line);
                                                            if has_rest { self.chunk.emit_op(OpCode::Dup, line); self.chunk.emit_op(OpCode::PushComputedExclude, line); }
                                                            self.chunk.emit_op(OpCode::GetElement, line);
                                                        }
                                                        _ => { self.chunk.emit_op(OpCode::Pop, line); continue; }
                                                    }
                                                    self.compile_assign_pat(&nested_pat, line)?;
                                                }
                                            }
                                        }
                                    }
                                    ObjectProperty::SpreadElement(spread) => {
                                        // Emit ObjectRest first (Dup + ObjectRest = rest_obj on TOS)
                                        self.chunk.emit_op(OpCode::Dup, line);
                                        self.chunk.emit_byte(OpCode::ObjectRest as u8, line);
                                        self.chunk.code.push(excluded_keys.len().min(255) as u8);
                                        for k in &excluded_keys {
                                            let idx = self.make_string_constant(*k);
                                            self.chunk.code.push((idx >> 8) as u8);
                                            self.chunk.code.push((idx & 0xFF) as u8);
                                        }
                                        // Assign rest_obj to target
                                        match &spread.argument {
                                            Expression::Identifier(rest_id) => {
                                                self.compile_set_variable(rest_id.name, line)?;
                                                self.chunk.emit_op(OpCode::Pop, line);
                                            }
                                            Expression::Member(m) => {
                                                // rest_obj is at TOS; push obj, swap, SetProperty
                                                self.compile_expr(&m.object)?;
                                                self.chunk.emit_op(OpCode::Swap, line);
                                                match &m.property {
                                                    MemberProperty::Identifier(name) => {
                                                        let idx = self.make_string_constant(*name);
                                                        self.emit_set_property(idx, line);
                                                    }
                                                    MemberProperty::Expression(expr) => {
                                                        // For computed: need [obj, key, val] order?
                                                        // Actually SetElement expects [obj, key_already_evaluated, val]
                                                        // We have [obj, rest_obj] after swap — insert key
                                                        self.compile_expr(expr)?;
                                                        // Stack: [obj, rest_obj, key] — need [obj, key, rest_obj]
                                                        self.chunk.emit_op(OpCode::Swap, line); // [obj, key, rest_obj]
                                                        self.chunk.emit_op(OpCode::SetElement, line);
                                                    }
                                                    _ => { self.chunk.emit_op(OpCode::Pop, line); }
                                                }
                                                self.chunk.emit_op(OpCode::Pop, line);
                                            }
                                            _ => { self.chunk.emit_op(OpCode::Pop, line); }
                                        }
                                    }
                                }
                            }
                            self.chunk.emit_op(OpCode::Pop, line);
                        }
                        _ => { self.chunk.emit_op(OpCode::Pop, line); }
                    }
                }
            }
            LoopVar::None => {
                if let ForInOfLeft::Expression(Expression::Member(m)) = &f.left {
                    // Stack: [..., iter_value] — assign to member target
                    self.compile_expr(&m.object)?;
                    self.chunk.emit_op(OpCode::Swap, line);
                    match &m.property {
                        MemberProperty::Identifier(name) => {
                            let idx = self.make_string_constant(*name);
                            self.emit_set_property(idx, line);
                        }
                        MemberProperty::Expression(key) => {
                            self.compile_expr(key)?;
                            self.chunk.emit_op(OpCode::Swap, line);
                            self.chunk.emit_op(OpCode::SetElement, line);
                        }
                        _ => {}
                    }
                    self.chunk.emit_op(OpCode::Pop, line);
                } else {
                    self.chunk.emit_op(OpCode::Pop, line);
                }
            }
        }

        // Compile body with loop context for break/continue.
        // For fresh-binding-simple: scope_depth - 1 is the outer (iter_anon) depth, so that
        // break/continue correctly pop the per-iteration binding before jumping.
        let loop_scope_depth = if fresh_binding_simple { self.scope_depth - 1 } else { self.scope_depth };
        self.loops.push(LoopCtx {
            continue_target: loop_start,
            break_patches: Vec::new(),
            continue_patches: Vec::new(),
            scope_depth: loop_scope_depth,
            label: None,
            has_deferred_continue: false,
            try_depth: self.finally_stack.len(),
        });

        self.compile_statement(&f.body)?;

        // For fresh-binding-simple: close per-iteration scope before looping back.
        if fresh_binding_simple {
            self.end_scope();
        }

        // Loop back
        self.chunk.emit_loop(loop_start, line);

        // Exit: pop the result (and iterator for non-fresh-binding).
        // For fresh-binding-simple the outer end_scope() below handles the iterator.
        self.chunk.patch_jump(exit_jump);
        self.chunk.emit_op(OpCode::Pop, line); // pop result
        if !fresh_binding_simple {
            self.chunk.emit_op(OpCode::Pop, line); // pop iterator
        }

        // Patch break jumps
        self.patch_loop_breaks();

        if !is_var { self.end_scope(); }
        Ok(())
    }

    // ---- switch ----

    fn compile_switch(&mut self, s: &SwitchStatement) -> Result<(), String> {
        let line = s.span.start;
        self.compile_expr(&s.discriminant)?;

        // Phase 1: emit comparisons.
        // For each non-default case, dup the discriminant, compile the test,
        // strict-equal, and conditionally jump to the case body.
        let mut case_entry_jumps: Vec<(usize, usize)> = Vec::new(); // (case idx, jump_pos)
        let mut default_index: Option<usize> = None;

        for (i, case) in s.cases.iter().enumerate() {
            if let Some(test) = &case.test {
                self.chunk.emit_op(OpCode::Dup, line);
                self.compile_expr(test)?;
                self.chunk.emit_op(OpCode::StrictEq, line);
                let jump = self.chunk.emit_jump(OpCode::JumpIfTrue, line);
                case_entry_jumps.push((i, jump));
            } else {
                default_index = Some(i);
            }
        }

        // After all comparisons, jump to default body or past all bodies.
        let end_of_compare = self.chunk.emit_jump(OpCode::Jump, line);

        // Phase 2: emit case bodies. The discriminant is still on the stack;
        // each matched case jumps here. We pop it once at the very start of
        // the body section.
        let pop_pos = self.chunk.len();
        self.chunk.emit_op(OpCode::Pop, line); // pop discriminant

        // Use the loop-break infrastructure so `break` works inside switch.
        self.loops.push(LoopCtx {
            continue_target: 0, // unused for switch
            break_patches: Vec::new(),
            continue_patches: Vec::new(),
            scope_depth: self.scope_depth,
            label: None,
            has_deferred_continue: false,
            try_depth: self.finally_stack.len(),
        });

        let mut body_starts: Vec<usize> = Vec::new();
        for case in &s.cases {
            body_starts.push(self.chunk.len());
            for stmt in &case.consequent {
                self.compile_statement(stmt)?;
            }
        }

        // Phase 3: patch jumps.
        // Each comparison JumpIfTrue should land at the Pop + its body.
        // Since JS switch uses fall-through, once we hit a matching case we
        // must pop the discriminant and then execute from that case onward.
        // With a single Pop before all bodies, the simplest approach is:
        // patch each case jump to `pop_pos` (which pops the discriminant),
        // then jump from there to the correct body.
        //
        // Unfortunately all case jumps landing at the same pop_pos doesn't
        // let us distinguish which body to enter. Instead, we emit a Pop
        // before each body entry and patch each jump directly there. But
        // fall-through between bodies would hit duplicate Pops.
        //
        // Simplest correct scheme: change the single Pop at pop_pos to Nop,
        // and before each body that is the target of a comparison jump,
        // insert nothing (we can't insert after the fact). Instead, accept
        // the extra value on the stack: the discriminant is consumed by the
        // Pop at the end.

        // Turn the Pop at pop_pos into a Nop (we'll pop at the very end).
        self.chunk.code[pop_pos] = OpCode::Nop as u8;

        for &(case_idx, jump_pos) in &case_entry_jumps {
            let target = body_starts[case_idx];
            let offset = target as i32 - jump_pos as i32 - 2;
            self.chunk.code[jump_pos] = (offset >> 8) as u8;
            self.chunk.code[jump_pos + 1] = (offset & 0xFF) as u8;
        }

        // end_of_compare: jump to default body or past bodies.
        if let Some(di) = default_index {
            let target = body_starts[di];
            let offset = target as i32 - end_of_compare as i32 - 2;
            self.chunk.code[end_of_compare] = (offset >> 8) as u8;
            self.chunk.code[end_of_compare + 1] = (offset & 0xFF) as u8;
        } else {
            self.chunk.patch_jump(end_of_compare);
        }

        // Pop the discriminant after all case bodies.
        self.chunk.emit_op(OpCode::Pop, line);

        self.patch_loop_breaks();
        Ok(())
    }

    // ---- return ----

    fn compile_return(&mut self, r: &ReturnStatement) -> Result<(), String> {
        let line = r.span.start;
        if let Some(arg) = &r.argument {
            self.compile_expr(arg)?;
            self.chunk.emit_op(OpCode::Return, line);
        } else {
            self.chunk.emit_op(OpCode::ReturnUndefined, line);
        }
        Ok(())
    }

    // ---- finally inlining helper ----

    /// Inline finally blocks for all try handlers entered after `target_try_depth`.
    /// Called before break/continue jumps so that finally blocks always execute.
    fn compile_inline_finallys(&mut self, target_try_depth: usize, line: u32) -> Result<(), String> {
        let depth = self.finally_stack.len();
        if depth <= target_try_depth {
            return Ok(());
        }
        // Collect Rc clones first to release the borrow on self.finally_stack
        let finallys: Vec<Option<std::rc::Rc<Vec<Statement>>>> =
            self.finally_stack[target_try_depth..depth].iter().rev().cloned().collect();
        for finally_opt in finallys {
            self.chunk.emit_op(OpCode::PopExcHandler, line);
            if let Some(stmts_rc) = finally_opt {
                let stmts = (*stmts_rc).clone();
                for stmt in &stmts {
                    self.compile_statement(stmt)?;
                }
            }
        }
        Ok(())
    }

    // ---- break ----

    fn compile_break(&mut self, b: &BreakStatement) -> Result<(), String> {
        let line = b.span.start;
        if self.loops.is_empty() {
            return Err(format!("'break' outside of loop/switch at offset {line}"));
        }
        // Find the target loop context (by label if specified, otherwise innermost)
        let target_idx = if let Some(label) = b.label {
            self.loops.iter().rposition(|l| l.label == Some(label))
                .ok_or_else(|| format!("label not found at offset {line}"))?
        } else {
            self.loops.len() - 1
        };
        let loop_depth = self.loops[target_idx].scope_depth;
        let target_try_depth = self.loops[target_idx].try_depth;
        let pop_n = self.locals_above_depth(loop_depth);
        if pop_n > 0 && pop_n <= u8::MAX as usize {
            self.chunk.emit_op_u8(OpCode::PopN, pop_n as u8, line);
        } else {
            for _ in 0..pop_n {
                self.chunk.emit_op(OpCode::Pop, line);
            }
        }
        self.compile_inline_finallys(target_try_depth, line)?;
        let patch = self.chunk.emit_jump(OpCode::Jump, line);
        self.loops[target_idx].break_patches.push(patch);
        Ok(())
    }

    // ---- continue ----

    fn compile_continue(&mut self, c: &ContinueStatement) -> Result<(), String> {
        let line = c.span.start;
        if self.loops.is_empty() {
            return Err(format!("'continue' outside of loop at offset {line}"));
        }
        // Find the matching loop context (labeled or innermost)
        let ctx_idx = if let Some(label) = c.label {
            self.loops.iter().rposition(|ctx| ctx.label == Some(label))
                .ok_or_else(|| format!("label not found at offset {line}"))?
        } else {
            self.loops.len() - 1
        };
        let target = self.loops[ctx_idx].continue_target;
        let loop_depth = self.loops[ctx_idx].scope_depth;
        let target_try_depth = self.loops[ctx_idx].try_depth;
        let deferred = self.loops[ctx_idx].has_deferred_continue;

        let pop_n = self.locals_above_depth(loop_depth);
        if pop_n > 0 && pop_n <= u8::MAX as usize {
            self.chunk.emit_op_u8(OpCode::PopN, pop_n as u8, line);
        } else {
            for _ in 0..pop_n {
                self.chunk.emit_op(OpCode::Pop, line);
            }
        }
        self.compile_inline_finallys(target_try_depth, line)?;
        if deferred {
            // Emit a forward jump; it will be patched to the update position later
            let patch = self.chunk.emit_jump(OpCode::Jump, line);
            self.loops[ctx_idx].continue_patches.push(patch);
        } else {
            self.chunk.emit_loop(target, line);
        }
        Ok(())
    }

    // ---- destructuring binding helpers ----

    /// value is on stack; if it is `undefined`, replace with the default expression
    fn emit_default_check(&mut self, right: &Expression, line: u32) -> Result<(), String> {
        self.chunk.emit_op(OpCode::Dup, line);
        self.chunk.emit_op(OpCode::Undefined, line);
        self.chunk.emit_op(OpCode::StrictNe, line);
        let jump_idx = self.chunk.code.len();
        self.chunk.emit_op(OpCode::JumpIfTrue, line);
        self.chunk.code.push(0); self.chunk.code.push(0);
        self.chunk.emit_op(OpCode::Pop, line);
        self.compile_expr(right)?;
        let target = self.chunk.code.len();
        let offset = (target as i16) - (jump_idx as i16) - 3;
        self.chunk.code[jump_idx + 1] = (offset >> 8) as u8;
        self.chunk.code[jump_idx + 2] = (offset & 0xFF) as u8;
        Ok(())
    }

    /// value is on stack; bind it to locals according to `pat` (consumes the value)
    fn compile_bind_value_local(&mut self, pat: &Pattern, line: u32) -> Result<(), String> {
        match pat {
            Pattern::Identifier(id) => {
                self.add_local(id.name);
                self.mark_initialized();
            }
            Pattern::Array(inner) => {
                self.chunk.emit_op(OpCode::GetIterator, line);
                let anon = self.interner.intern("__d__");
                self.add_local(anon);
                self.mark_initialized();
                let slot = (self.locals.len() - 1) as u8;
                self.compile_bind_arr_elems_local(&inner.elements, slot, line)?;
            }
            Pattern::Object(inner) => {
                let anon = self.interner.intern("__d__");
                self.add_local(anon);
                self.mark_initialized();
                let slot = (self.locals.len() - 1) as u8;
                self.compile_bind_obj_props_local(&inner.properties, slot, line)?;
            }
            _ => { self.chunk.emit_op(OpCode::Pop, line); }
        }
        Ok(())
    }

    /// bind array elements to locals via iterator protocol; iterator is at local slot `iter_slot`
    fn compile_bind_arr_elems_local(&mut self, elements: &[Option<Pattern>], iter_slot: u8, line: u32) -> Result<(), String> {
        let mut had_rest = false;
        // Allocate a done-flag local so IteratorClose only fires when the last
        // step's done was false (per spec).
        self.chunk.emit_op(OpCode::False, line);
        let done_slot = self.locals.len() as u8;
        let done_anon = self.interner.intern("__iter_done__");
        self.add_local(done_anon);
        self.mark_initialized();
        for elem in elements.iter() {
            match elem {
                None => {
                    // Elision: advance iterator and discard
                    self.chunk.emit_op_u8(OpCode::GetLocal, iter_slot, line);
                    self.chunk.emit_op(OpCode::IteratorNext, line);
                    self.chunk.emit_op(OpCode::Pop, line);
                }
                Some(Pattern::Rest(rest)) => {
                    had_rest = true;
                    // Collect remaining iterator values into an array
                    self.chunk.emit_op_u16(OpCode::CreateArray, 0, line);
                    let loop_start = self.chunk.len();
                    self.chunk.emit_op_u8(OpCode::GetLocal, iter_slot, line);
                    self.chunk.emit_op(OpCode::IteratorNext, line);
                    self.chunk.emit_op(OpCode::Dup, line);
                    self.chunk.emit_op(OpCode::IteratorDone, line);
                    let exit_jump = self.chunk.emit_jump(OpCode::JumpIfTrue, line);
                    self.chunk.emit_op(OpCode::IteratorValue, line);
                    self.chunk.emit_op(OpCode::ArrayAppend, line);
                    self.chunk.emit_loop(loop_start, line);
                    self.chunk.patch_jump(exit_jump);
                    self.chunk.emit_op(OpCode::Pop, line); // pop done result
                    self.compile_bind_value_local(&rest.argument, line)?;
                    break;
                }
                Some(pat) => {
                    // Get next value or undefined if iterator is done
                    self.chunk.emit_op_u8(OpCode::GetLocal, iter_slot, line);
                    self.chunk.emit_op(OpCode::IteratorNext, line);
                    self.chunk.emit_op(OpCode::Dup, line);
                    self.chunk.emit_op(OpCode::IteratorDone, line);
                    // Track done in done_slot for the conditional close at the end.
                    self.chunk.emit_op(OpCode::Dup, line);
                    self.chunk.emit_op_u8(OpCode::SetLocal, done_slot, line);
                    self.chunk.emit_op(OpCode::Pop, line);
                    let not_done_jump = self.chunk.emit_jump(OpCode::JumpIfFalse, line);
                    // done=true: pop result, use undefined
                    self.chunk.emit_op(OpCode::Pop, line);
                    self.chunk.emit_op(OpCode::Undefined, line);
                    let skip_jump = self.chunk.emit_jump(OpCode::Jump, line);
                    // done=false: extract value
                    self.chunk.patch_jump(not_done_jump);
                    self.chunk.emit_op(OpCode::IteratorValue, line);
                    self.chunk.patch_jump(skip_jump);
                    if let Pattern::Assignment(a) = pat {
                        if let Pattern::Identifier(id) = &a.left
                            && Self::is_anonymous_fn_def(&a.right)
                        {
                            self.pending_function_name = Some(id.name);
                        }
                        self.emit_default_check(&a.right, line)?;
                        self.compile_bind_value_local(&a.left, line)?;
                    } else {
                        self.compile_bind_value_local(pat, line)?;
                    }
                }
            }
        }
        // Per spec, if destructuring did not exhaust the iterator (no rest pattern)
        // AND the last step's done flag is false, call IteratorClose so user
        // iterators can run their `return()` cleanup.
        if !had_rest {
            self.chunk.emit_op_u8(OpCode::GetLocal, done_slot, line);
            let skip_close = self.chunk.emit_jump(OpCode::JumpIfTrue, line);
            self.chunk.emit_op_u8(OpCode::GetLocal, iter_slot, line);
            self.chunk.emit_op(OpCode::IteratorClose, line);
            self.chunk.patch_jump(skip_close);
        }
        Ok(())
    }

    /// bind object properties to locals; source object is at local slot `src_slot`
    fn compile_bind_obj_props_local(&mut self, properties: &[ObjectPatternProperty], src_slot: u8, line: u32) -> Result<(), String> {
        // RequireObjectCoercible: `const {} = null` / `let {} = undefined` throw TypeError.
        self.chunk.emit_op_u8(OpCode::GetLocal, src_slot, line);
        self.chunk.emit_op(OpCode::Dup, line);
        self.chunk.emit_op(OpCode::Undefined, line);
        self.chunk.emit_op(OpCode::StrictEq, line);
        let undef_jump = self.chunk.emit_jump(OpCode::JumpIfTrue, line);
        self.chunk.emit_op(OpCode::Dup, line);
        self.chunk.emit_op(OpCode::Null, line);
        self.chunk.emit_op(OpCode::StrictEq, line);
        let null_jump = self.chunk.emit_jump(OpCode::JumpIfTrue, line);
        let skip_throw = self.chunk.emit_jump(OpCode::Jump, line);
        self.chunk.patch_jump(undef_jump);
        self.chunk.patch_jump(null_jump);
        self.emit_throw_type_error("Cannot destructure 'undefined' or 'null'", line);
        self.chunk.patch_jump(skip_throw);
        self.chunk.emit_op(OpCode::Pop, line);
        for prop in properties {
            match prop {
                ObjectPatternProperty::Property { key, value, .. } => {
                    self.chunk.emit_op_u8(OpCode::GetLocal, src_slot, line);
                    match key {
                        PropertyKey::Identifier(id) | PropertyKey::StringLiteral(id) => {
                            let idx = self.make_string_constant(*id);
                            self.emit_get_property(idx, line);
                        }
                        PropertyKey::Computed(expr) => {
                            self.compile_expr(expr)?;
                            self.chunk.emit_op(OpCode::GetElement, line);
                        }
                        PropertyKey::NumberLiteral(n) => {
                            self.emit_constant(Value::number(*n), line);
                            self.chunk.emit_op(OpCode::GetElement, line);
                        }
                        _ => { self.chunk.emit_op(OpCode::Pop, line); continue; }
                    }
                    if let Pattern::Assignment(a) = value {
                        // If the default is an anonymous function and the binding is a plain
                        // identifier, propagate the binding name so the function gets a `name`.
                        if let Pattern::Identifier(id) = &a.left
                            && Self::is_anonymous_fn_def(&a.right)
                        {
                            self.pending_function_name = Some(id.name);
                        }
                        self.emit_default_check(&a.right, line)?;
                        self.compile_bind_value_local(&a.left, line)?;
                    } else {
                        self.compile_bind_value_local(value, line)?;
                    }
                }
                ObjectPatternProperty::Rest(rest) => {
                    if let Pattern::Identifier(id) = &rest.argument {
                        let excluded: Vec<StringId> = properties.iter().filter_map(|p| {
                            if let ObjectPatternProperty::Property { key, .. } = p {
                                match key {
                                    PropertyKey::Identifier(s) | PropertyKey::StringLiteral(s) => Some(*s),
                                    _ => None,
                                }
                            } else { None }
                        }).collect();
                        self.chunk.emit_op_u8(OpCode::GetLocal, src_slot, line);
                        self.chunk.emit_byte(OpCode::ObjectRest as u8, line);
                        self.chunk.code.push(excluded.len() as u8);
                        for k in &excluded {
                            let idx = self.make_string_constant(*k);
                            self.chunk.code.push((idx >> 8) as u8);
                            self.chunk.code.push((idx & 0xFF) as u8);
                        }
                        self.add_local(id.name);
                        self.mark_initialized();
                    }
                }
            }
        }
        Ok(())
    }

    /// value is on stack; bind it to globals according to `pat` (consumes the value)
    fn compile_bind_value_global(&mut self, pat: &Pattern, line: u32) -> Result<(), String> {
        match pat {
            Pattern::Identifier(id) => {
                let vidx = self.make_string_constant(id.name);
                self.chunk.emit_op_u16(OpCode::DefineGlobal, vidx, line);
            }
            Pattern::Array(inner) => {
                self.chunk.emit_op(OpCode::GetIterator, line);
                self.compile_bind_arr_elems_global(&inner.elements, line)?;
                self.chunk.emit_op(OpCode::Pop, line);
            }
            Pattern::Object(inner) => {
                self.compile_bind_obj_props_global(&inner.properties, line)?;
                self.chunk.emit_op(OpCode::Pop, line);
            }
            _ => { self.chunk.emit_op(OpCode::Pop, line); }
        }
        Ok(())
    }

    /// bind array elements to globals via iterator protocol; iterator is at TOS (caller Pops)
    fn compile_bind_arr_elems_global(&mut self, elements: &[Option<Pattern>], line: u32) -> Result<(), String> {
        // Track the iterator (TOS) as a temp local so we can GetLocal for IteratorNext calls
        let iter_slot = self.locals.len() as u8;
        let anon = self.interner.intern("__iter_g__");
        self.add_local(anon);
        self.mark_initialized();

        let mut had_rest = false;
        for elem in elements.iter() {
            match elem {
                None => {
                    self.chunk.emit_op_u8(OpCode::GetLocal, iter_slot, line);
                    self.chunk.emit_op(OpCode::IteratorNext, line);
                    self.chunk.emit_op(OpCode::Pop, line);
                }
                Some(Pattern::Rest(rest)) => {
                    had_rest = true;
                    self.chunk.emit_op_u16(OpCode::CreateArray, 0, line);
                    let loop_start = self.chunk.len();
                    self.chunk.emit_op_u8(OpCode::GetLocal, iter_slot, line);
                    self.chunk.emit_op(OpCode::IteratorNext, line);
                    self.chunk.emit_op(OpCode::Dup, line);
                    self.chunk.emit_op(OpCode::IteratorDone, line);
                    let exit_jump = self.chunk.emit_jump(OpCode::JumpIfTrue, line);
                    self.chunk.emit_op(OpCode::IteratorValue, line);
                    self.chunk.emit_op(OpCode::ArrayAppend, line);
                    self.chunk.emit_loop(loop_start, line);
                    self.chunk.patch_jump(exit_jump);
                    self.chunk.emit_op(OpCode::Pop, line);
                    self.compile_bind_value_global(&rest.argument, line)?;
                    break;
                }
                Some(pat) => {
                    self.chunk.emit_op_u8(OpCode::GetLocal, iter_slot, line);
                    self.chunk.emit_op(OpCode::IteratorNext, line);
                    self.chunk.emit_op(OpCode::Dup, line);
                    self.chunk.emit_op(OpCode::IteratorDone, line);
                    let not_done_jump = self.chunk.emit_jump(OpCode::JumpIfFalse, line);
                    self.chunk.emit_op(OpCode::Pop, line);
                    self.chunk.emit_op(OpCode::Undefined, line);
                    let skip_jump = self.chunk.emit_jump(OpCode::Jump, line);
                    self.chunk.patch_jump(not_done_jump);
                    self.chunk.emit_op(OpCode::IteratorValue, line);
                    self.chunk.patch_jump(skip_jump);
                    if let Pattern::Assignment(a) = pat {
                        if let Pattern::Identifier(id) = &a.left
                            && Self::is_anonymous_fn_def(&a.right)
                        {
                            self.pending_function_name = Some(id.name);
                        }
                        self.emit_default_check(&a.right, line)?;
                        self.compile_bind_value_global(&a.left, line)?;
                    } else {
                        self.compile_bind_value_global(pat, line)?;
                    }
                }
            }
        }

        // Per spec, if pattern didn't exhaust the iterator (no rest), call IteratorClose.
        // The VM's IteratorClose checks the iter's __iter_done__ flag and skips when set,
        // so this is safe even when the iterator is already exhausted.
        if !had_rest {
            self.chunk.emit_op_u8(OpCode::GetLocal, iter_slot, line);
            self.chunk.emit_op(OpCode::IteratorClose, line);
        }

        self.locals.pop(); // remove temp iterator tracking (stack value stays; caller Pops)
        Ok(())
    }

    /// bind object properties to globals; source is on top of stack (Dup for each prop; caller Pops)
    fn compile_bind_obj_props_global(&mut self, properties: &[ObjectPatternProperty], line: u32) -> Result<(), String> {
        // RequireObjectCoercible: source is on top of stack, throw if null/undefined.
        self.chunk.emit_op(OpCode::Dup, line);
        self.chunk.emit_op(OpCode::Dup, line);
        self.chunk.emit_op(OpCode::Undefined, line);
        self.chunk.emit_op(OpCode::StrictEq, line);
        let undef_jump = self.chunk.emit_jump(OpCode::JumpIfTrue, line);
        self.chunk.emit_op(OpCode::Dup, line);
        self.chunk.emit_op(OpCode::Null, line);
        self.chunk.emit_op(OpCode::StrictEq, line);
        let null_jump = self.chunk.emit_jump(OpCode::JumpIfTrue, line);
        let skip_throw = self.chunk.emit_jump(OpCode::Jump, line);
        self.chunk.patch_jump(undef_jump);
        self.chunk.patch_jump(null_jump);
        self.emit_throw_type_error("Cannot destructure 'undefined' or 'null'", line);
        self.chunk.patch_jump(skip_throw);
        self.chunk.emit_op(OpCode::Pop, line);
        for prop in properties {
            match prop {
                ObjectPatternProperty::Property { key, value, .. } => {
                    self.chunk.emit_op(OpCode::Dup, line);
                    match key {
                        PropertyKey::Identifier(id) | PropertyKey::StringLiteral(id) => {
                            let idx = self.make_string_constant(*id);
                            self.emit_get_property(idx, line);
                        }
                        PropertyKey::Computed(expr) => {
                            self.compile_expr(expr)?;
                            self.chunk.emit_op(OpCode::GetElement, line);
                        }
                        PropertyKey::NumberLiteral(n) => {
                            self.emit_constant(Value::number(*n), line);
                            self.chunk.emit_op(OpCode::GetElement, line);
                        }
                        _ => { self.chunk.emit_op(OpCode::Pop, line); continue; }
                    }
                    if let Pattern::Assignment(a) = value {
                        if let Pattern::Identifier(id) = &a.left
                            && Self::is_anonymous_fn_def(&a.right)
                        {
                            self.pending_function_name = Some(id.name);
                        }
                        self.emit_default_check(&a.right, line)?;
                        self.compile_bind_value_global(&a.left, line)?;
                    } else {
                        self.compile_bind_value_global(value, line)?;
                    }
                }
                ObjectPatternProperty::Rest(rest) => {
                    if let Pattern::Identifier(id) = &rest.argument {
                        let excluded: Vec<StringId> = properties.iter().filter_map(|p| {
                            if let ObjectPatternProperty::Property { key, .. } = p {
                                match key {
                                    PropertyKey::Identifier(s) | PropertyKey::StringLiteral(s) => Some(*s),
                                    _ => None,
                                }
                            } else { None }
                        }).collect();
                        self.chunk.emit_op(OpCode::Dup, line);
                        self.chunk.emit_byte(OpCode::ObjectRest as u8, line);
                        self.chunk.code.push(excluded.len() as u8);
                        for k in &excluded {
                            let idx = self.make_string_constant(*k);
                            self.chunk.code.push((idx >> 8) as u8);
                            self.chunk.code.push((idx & 0xFF) as u8);
                        }
                        let vidx = self.make_string_constant(id.name);
                        self.chunk.emit_op_u16(OpCode::DefineGlobal, vidx, line);
                    }
                }
            }
        }
        Ok(())
    }

    /// Convert a destructuring expression (LHS of for-of) into the equivalent
    /// Pattern so it can be compiled via `compile_assign_pat`.
    /// Returns `None` for elisions/unsupported forms.
    fn expr_to_pattern(expr: &Expression) -> Option<Pattern> {
        match expr {
            Expression::Identifier(id) => Some(Pattern::Identifier(id.clone())),
            Expression::Array(arr) => {
                let mut elems: Vec<Option<Pattern>> = Vec::new();
                for elem in &arr.elements {
                    match elem {
                        None => elems.push(None),
                        Some(e) => elems.push(Self::expr_to_pattern(e)),
                    }
                }
                Some(Pattern::Array(crate::ast::node::ArrayPattern {
                    elements: elems,
                    span: arr.span,
                }))
            }
            Expression::Object(obj) => {
                let mut props = Vec::new();
                for p in &obj.properties {
                    match p {
                        ObjectProperty::Property(prop) => {
                            let key = prop.key.clone();
                            let val_pat = Self::expr_to_pattern(&prop.value)
                                .unwrap_or(Pattern::Identifier(
                                    crate::ast::node::Identifier { name: crate::util::interner::StringId(0), span: prop.span }
                                ));
                            props.push(ObjectPatternProperty::Property {
                                key,
                                value: val_pat,
                                computed: false,
                                shorthand: prop.shorthand,
                                span: prop.span,
                            });
                        }
                        ObjectProperty::SpreadElement(s) => {
                            if let Some(arg) = Self::expr_to_pattern(&s.argument) {
                                props.push(ObjectPatternProperty::Rest(crate::ast::node::RestElement {
                                    argument: arg,
                                    span: s.span,
                                }));
                            }
                        }
                    }
                }
                Some(Pattern::Object(crate::ast::node::ObjectPattern {
                    properties: props,
                    span: obj.span,
                }))
            }
            Expression::Member(m) => Some(Pattern::Member(m.clone())),
            Expression::Assignment(a) => {
                let left_pat = match &a.left {
                    crate::ast::node::AssignmentTarget::Identifier(id) => Some(Pattern::Identifier(id.clone())),
                    crate::ast::node::AssignmentTarget::Member(m) => Some(Pattern::Member(m.clone())),
                    crate::ast::node::AssignmentTarget::Pattern(p) => Some(p.clone()),
                };
                left_pat.map(|lp| {
                    Pattern::Assignment(Box::new(crate::ast::node::AssignmentPattern {
                        left: lp,
                        right: a.right.clone(),
                        span: a.span,
                    }))
                })
            }
            Expression::Spread(s) => {
                Self::expr_to_pattern(&s.argument).map(|arg| {
                    Pattern::Rest(Box::new(crate::ast::node::RestElement {
                        argument: arg,
                        span: s.span,
                    }))
                })
            }
            _ => None,
        }
    }

    /// Destructure the value on top of stack into `pat`, consuming the value.
    /// Leaf identifiers must already be declared; uses compile_set_variable to assign.
    fn compile_assign_pat(&mut self, pat: &Pattern, line: u32) -> Result<(), String> {
        match pat {
            Pattern::Identifier(id) => {
                self.compile_set_variable(id.name, line)?;
                self.chunk.emit_op(OpCode::Pop, line);
            }
            Pattern::Array(inner) => {
                // Convert source to iterator per spec; keep iterator at TOS via Dup
                self.chunk.emit_op(OpCode::GetIterator, line);
                // Stack: [..., iter]
                let mut had_rest = false;
                for elem in inner.elements.iter() {
                    match elem {
                        None => {
                            // Elision: advance iterator
                            self.chunk.emit_op(OpCode::Dup, line);
                            self.chunk.emit_op(OpCode::IteratorNext, line);
                            self.chunk.emit_op(OpCode::Pop, line);
                        }
                        Some(Pattern::Rest(rest)) => {
                            had_rest = true;
                            // Collect remaining into array: [..., iter] → create arr, loop
                            self.chunk.emit_op_u16(OpCode::CreateArray, 0, line);
                            self.chunk.emit_op(OpCode::Swap, line);  // [..., arr, iter]
                            let loop_start = self.chunk.len();
                            self.chunk.emit_op(OpCode::Dup, line);        // [..., arr, iter, iter_dup]
                            self.chunk.emit_op(OpCode::IteratorNext, line); // [..., arr, iter, result]
                            self.chunk.emit_op(OpCode::Dup, line);         // [..., arr, iter, result, result_dup]
                            self.chunk.emit_op(OpCode::IteratorDone, line); // [..., arr, iter, result, done]
                            let exit_jump = self.chunk.emit_jump(OpCode::JumpIfTrue, line);
                            self.chunk.emit_op(OpCode::IteratorValue, line); // [..., arr, iter, value]
                            self.chunk.emit_op(OpCode::Swap, line);          // [..., arr, value, iter]
                            self.chunk.emit_op(OpCode::Rot3, line);          // [..., iter, arr, value]
                            self.chunk.emit_op(OpCode::ArrayAppend, line);   // [..., iter, arr]
                            self.chunk.emit_op(OpCode::Swap, line);          // [..., arr, iter]
                            self.chunk.emit_loop(loop_start, line);
                            // exit: [..., arr, iter, result]
                            self.chunk.patch_jump(exit_jump);
                            self.chunk.emit_op(OpCode::Pop, line); // pop result
                            self.chunk.emit_op(OpCode::Pop, line); // pop iter
                            // Stack: [..., arr]
                            self.compile_assign_pat(&rest.argument, line)?;
                            break;
                        }
                        Some(pat) => {
                            // Get next value or undefined if done; iter stays at TOS-1
                            self.chunk.emit_op(OpCode::Dup, line);          // [..., iter, iter_dup]
                            self.chunk.emit_op(OpCode::IteratorNext, line); // [..., iter, result]
                            self.chunk.emit_op(OpCode::Dup, line);          // [..., iter, result, result_dup]
                            self.chunk.emit_op(OpCode::IteratorDone, line); // [..., iter, result, done]
                            let not_done_jump = self.chunk.emit_jump(OpCode::JumpIfFalse, line);
                            // done=true: [..., iter, result]
                            self.chunk.emit_op(OpCode::Pop, line);
                            self.chunk.emit_op(OpCode::Undefined, line);
                            let skip_jump = self.chunk.emit_jump(OpCode::Jump, line);
                            // done=false: [..., iter, result]
                            self.chunk.patch_jump(not_done_jump);
                            self.chunk.emit_op(OpCode::IteratorValue, line);
                            self.chunk.patch_jump(skip_jump);
                            // Stack: [..., iter, value_or_undefined]
                            if let Pattern::Assignment(a) = pat {
                                if let Pattern::Identifier(id) = &a.left
                                    && Self::is_anonymous_fn_def(&a.right) {
                                        self.pending_function_name = Some(id.name);
                                }
                                self.emit_default_check(&a.right, line)?;
                                self.compile_assign_pat(&a.left, line)?;
                            } else {
                                self.compile_assign_pat(pat, line)?;
                            }
                            // Stack: [..., iter]
                        }
                    }
                }
                if !had_rest {
                    self.chunk.emit_op(OpCode::IteratorClose, line);
                }
            }
            Pattern::Object(inner) => {
                // RequireObjectCoercible: throw TypeError if source is null or undefined
                // JumpIfNullishPeek jumps when value IS nullish
                self.chunk.emit_op(OpCode::Dup, line);
                let nullish_jump = self.chunk.emit_jump(OpCode::JumpIfNullishPeek, line);
                // Not nullish path: pop dup and skip error
                self.chunk.emit_op(OpCode::Pop, line);
                let skip_throw = self.chunk.emit_jump(OpCode::Jump, line);
                // Nullish path (jump target):
                self.chunk.patch_jump(nullish_jump);
                self.chunk.emit_op(OpCode::Pop, line); // pop dup
                self.chunk.emit_op(OpCode::Pop, line); // pop source
                let msg_id = self.interner.intern("Cannot destructure property of null or undefined");
                self.emit_constant(Value::string(msg_id), line);
                self.chunk.emit_op(OpCode::Throw, line);
                // End of check (not nullish path resumes here):
                self.chunk.patch_jump(skip_throw);
                // Stack: [..., source]

                let has_rest_obj = inner.properties.iter().any(|p| matches!(p, ObjectPatternProperty::Rest(_)));
                let excluded: Vec<StringId> = inner.properties.iter().filter_map(|p| {
                    if let ObjectPatternProperty::Property { key, .. } = p {
                        match key {
                            PropertyKey::Identifier(s) | PropertyKey::StringLiteral(s) => Some(*s),
                            _ => None,
                        }
                    } else { None }
                }).collect();
                for prop in &inner.properties {
                    match prop {
                        ObjectPatternProperty::Property { key, value, .. } => {
                            self.chunk.emit_op(OpCode::Dup, line);
                            match key {
                                PropertyKey::Identifier(id) | PropertyKey::StringLiteral(id) => {
                                    let idx = self.make_string_constant(*id);
                                    self.emit_get_property(idx, line);
                                }
                                PropertyKey::Computed(expr) => {
                                    self.compile_expr(expr)?;
                                    if has_rest_obj { self.chunk.emit_op(OpCode::Dup, line); self.chunk.emit_op(OpCode::PushComputedExclude, line); }
                                    self.chunk.emit_op(OpCode::GetElement, line);
                                }
                                PropertyKey::NumberLiteral(n) => {
                                    self.emit_constant(Value::number(*n), line);
                                    if has_rest_obj { self.chunk.emit_op(OpCode::Dup, line); self.chunk.emit_op(OpCode::PushComputedExclude, line); }
                                    self.chunk.emit_op(OpCode::GetElement, line);
                                }
                                _ => { self.chunk.emit_op(OpCode::Pop, line); continue; }
                            }
                            if let Pattern::Assignment(a) = value {
                                if let Pattern::Identifier(target_id) = &a.left
                                    && Self::is_anonymous_fn_def(&a.right) {
                                        self.pending_function_name = Some(target_id.name);
                                }
                                self.emit_default_check(&a.right, line)?;
                                self.compile_assign_pat(&a.left, line)?;
                            } else {
                                self.compile_assign_pat(value, line)?;
                            }
                        }
                        ObjectPatternProperty::Rest(rest) => {
                            self.chunk.emit_op(OpCode::Dup, line);
                            self.chunk.emit_byte(OpCode::ObjectRest as u8, line);
                            self.chunk.code.push(excluded.len().min(255) as u8);
                            for k in &excluded {
                                let idx = self.make_string_constant(*k);
                                self.chunk.code.push((idx >> 8) as u8);
                                self.chunk.code.push((idx & 0xFF) as u8);
                            }
                            self.compile_assign_pat(&rest.argument, line)?;
                        }
                    }
                }
                self.chunk.emit_op(OpCode::Pop, line);
            }
            Pattern::Assignment(a) => {
                self.emit_default_check(&a.right, line)?;
                self.compile_assign_pat(&a.left, line)?;
            }
            Pattern::Member(m) => {
                // Stack: [..., val]
                self.compile_expr(&m.object)?;
                // Stack: [..., val, obj]
                self.chunk.emit_op(OpCode::Swap, line);
                // Stack: [..., obj, val]
                match &m.property {
                    crate::ast::node::MemberProperty::Identifier(name) => {
                        let idx = self.make_string_constant(*name);
                        self.emit_set_property(idx, line);
                    }
                    crate::ast::node::MemberProperty::Expression(key) => {
                        self.compile_expr(key)?;
                        // Stack: [..., obj, val, key]
                        self.chunk.emit_op(OpCode::Swap, line);
                        // Stack: [..., obj, key, val]
                        self.chunk.emit_op(OpCode::SetElement, line);
                    }
                    crate::ast::node::MemberProperty::PrivateIdentifier(name) => {
                        let idx = self.make_string_constant(*name);
                        self.chunk.emit_op_u16(OpCode::SetPrivate, idx, line);
                    }
                }
                self.chunk.emit_op(OpCode::Pop, line);
            }
            _ => { self.chunk.emit_op(OpCode::Pop, line); }
        }
        Ok(())
    }

    // ---- throw ----

    fn compile_throw(&mut self, t: &ThrowStatement) -> Result<(), String> {
        self.compile_expr(&t.argument)?;
        self.chunk.emit_op(OpCode::Throw, t.span.start);
        Ok(())
    }

    // ---- try / catch / finally ----

    fn compile_try(&mut self, t: &TryStatement) -> Result<(), String> {
        let line = t.span.start;

        // Emit PushExcHandler with placeholder offsets for catch and finally.
        // Layout: [PushExcHandler, catch_hi, catch_lo, finally_hi, finally_lo]
        let _handler_pos = self.chunk.len();
        self.chunk
            .emit_byte(OpCode::PushExcHandler as u8, line);
        let catch_placeholder = self.chunk.code.len();
        self.chunk
            .code
            .extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);

        // Track the finally block so break/continue can inline it.
        let finally_rc = t.finalizer.as_ref().map(|f| std::rc::Rc::new(f.body.clone()));
        self.finally_stack.push(finally_rc);

        // Compile try block (no scope — var declarations should be global/function-scoped).
        for stmt in &t.block.body {
            self.compile_statement(stmt)?;
        }

        self.finally_stack.pop();

        self.chunk.emit_op(OpCode::PopExcHandler, line);
        let skip_catch = self.chunk.emit_jump(OpCode::Jump, line);

        // Patch the catch target.
        let catch_target = self.chunk.len() as u16;
        if t.handler.is_some() {
            self.chunk.code[catch_placeholder] = (catch_target >> 8) as u8;
            self.chunk.code[catch_placeholder + 1] = (catch_target & 0xFF) as u8;
        }

        // Compile catch block.
        if let Some(handler) = &t.handler {
            self.begin_scope();
            match &handler.param {
                Some(Pattern::Identifier(id)) => {
                    if self.scope_depth > 0 {
                        self.add_local(id.name);
                        self.mark_initialized();
                        // Exception value is already on the stack as the local.
                    } else {
                        let idx = self.make_string_constant(id.name);
                        self.chunk.emit_op_u16(OpCode::DefineGlobal, idx, line);
                    }
                }
                Some(Pattern::Object(obj_pat)) => {
                    let anon = self.interner.intern("__catch_val__");
                    self.add_local(anon);
                    self.mark_initialized();
                    let src_slot = (self.locals.len() - 1) as u8;
                    self.compile_bind_obj_props_local(&obj_pat.properties, src_slot, line)?;
                }
                Some(Pattern::Array(arr_pat)) => {
                    self.chunk.emit_op(OpCode::GetIterator, line);
                    let anon = self.interner.intern("__catch_iter__");
                    self.add_local(anon);
                    self.mark_initialized();
                    let iter_slot = (self.locals.len() - 1) as u8;
                    self.compile_bind_arr_elems_local(&arr_pat.elements, iter_slot, line)?;
                }
                Some(_) => self.chunk.emit_op(OpCode::Pop, line),
                None => self.chunk.emit_op(OpCode::Pop, line),
            }
            for stmt in &handler.body.body {
                self.compile_statement(stmt)?;
            }
            self.end_scope();
        }

        self.chunk.patch_jump(skip_catch);

        // Compile finally block.
        if let Some(finalizer) = &t.finalizer {
            let finally_target = self.chunk.len() as u16;
            self.chunk.code[catch_placeholder + 2] = (finally_target >> 8) as u8;
            self.chunk.code[catch_placeholder + 3] = (finally_target & 0xFF) as u8;

            self.begin_scope();
            for stmt in &finalizer.body {
                self.compile_statement(stmt)?;
            }
            self.end_scope();
        }

        Ok(())
    }

    // ---- function declaration ----

    fn compile_function_decl(&mut self, f: &FunctionDeclaration) -> Result<(), String> {
        let name = f.id.unwrap_or_else(|| self.interner.intern("<anonymous>"));
        let line = f.span.start;

        let child_chunk =
            self.compile_function_body(name, &f.params, &f.body, f.is_async, f.is_generator)?;
        let chunk_idx = self.chunk.child_chunks.len() as u16;
        let upvalue_descs = child_chunk.upvalue_descriptors.clone();
        self.chunk.child_chunks.push(child_chunk);
        self.chunk.emit_op_u16(OpCode::Closure, chunk_idx, line);
        // Emit upvalue descriptors inline after the Closure opcode
        for desc in &upvalue_descs {
            self.chunk.emit_byte(if desc.is_local { 1 } else { 0 }, line);
            self.chunk.emit_byte(desc.index, line);
        }

        if self.scope_depth == 0 {
            let idx = self.make_string_constant(name);
            self.chunk.emit_op_u16(OpCode::DefineGlobal, idx, line);
        } else {
            self.add_local(name);
            self.mark_initialized();
        }
        Ok(())
    }

    // ---- class declaration ----

    fn compile_class_decl(&mut self, c: &ClassDeclaration) -> Result<(), String> {
        let line = c.span.start;
        let name = c.id.unwrap_or_else(|| self.interner.intern("<anonymous>"));
        let name_idx = self.make_string_constant(name);
        self.chunk.emit_op_u16(OpCode::Class, name_idx, line);

        if let Some(super_class) = &c.super_class {
            self.compile_expr(super_class)?;
            self.chunk.emit_op(OpCode::Inherit, line);
        }

        self.compile_class_body(&c.body, line)?;

        if self.scope_depth == 0 {
            let idx2 = self.make_string_constant(name);
            self.chunk.emit_op_u16(OpCode::DefineGlobal, idx2, line);
        } else {
            self.add_local(name);
            self.mark_initialized();
        }
        Ok(())
    }

    fn compile_class_body(&mut self, body: &ClassBody, line: u32) -> Result<(), String> {
        for member in &body.body {
            match member {
                ClassMember::Method(m) => {
                    let key_id = self.property_key_name(&m.key);
                    let key_unknown = key_id == StringId(0)
                        && matches!(&m.key, PropertyKey::Computed(_));
                    self.pending_function_name = Some(key_id);
                    // Truly computed keys whose value isn't statically known: emit
                    // `proto[expr] = fn` (or accessor variant) at runtime instead of
                    // baking the empty key into ClassMethod's u16 operand.
                    if key_unknown {
                        let proto_key = self.interner.intern("prototype");
                        let proto_idx = self.make_string_constant(proto_key);
                        // Stack on entry: [class]
                        if m.is_static {
                            // For static, target is the class itself.
                            self.chunk.emit_op(OpCode::Dup, line); // [class, class]
                        } else {
                            self.chunk.emit_op(OpCode::Dup, line); // [class, class]
                            self.emit_get_property(proto_idx, line); // [class, proto]
                        }
                        if let PropertyKey::Computed(expr) = &m.key {
                            self.compile_expr(expr)?;
                        }
                        self.compile_expr(&m.value)?;
                        match m.kind {
                            MethodKind::Get => self.chunk.emit_op(OpCode::DefineGetter, line),
                            MethodKind::Set => self.chunk.emit_op(OpCode::DefineSetter, line),
                            _ => self.chunk.emit_op(OpCode::DefineDataProp, line),
                        }
                        // After accessor-define, the target object is still on stack — pop it.
                        self.chunk.emit_op(OpCode::Pop, line);
                    } else {
                        self.compile_expr(&m.value)?;
                        // For getters/setters, use __get_name__ / __set_name__ convention.
                        let actual_key = match m.kind {
                            MethodKind::Get => {
                                let name = self.interner.resolve(key_id).to_owned();
                                self.interner.intern(&format!("__get_{name}__"))
                            }
                            MethodKind::Set => {
                                let name = self.interner.resolve(key_id).to_owned();
                                self.interner.intern(&format!("__set_{name}__"))
                            }
                            MethodKind::Constructor => self.interner.intern("\u{0}ctor"),
                            _ => key_id,
                        };
                        let idx = self.make_string_constant(actual_key);
                        let is_private = matches!(&m.key, PropertyKey::Private(_));
                        let op = match (is_private, m.is_static) {
                            (true,  false) => OpCode::ClassPrivateMethod,
                            (true,  true)  => OpCode::ClassStaticMethod,
                            (false, false) => OpCode::ClassMethod,
                            (false, true)  => OpCode::ClassStaticMethod,
                        };
                        self.chunk.emit_op_u16(op, idx, line);
                    }
                }
                ClassMember::Property(p) => {
                    if matches!(&p.key, PropertyKey::Computed(_) | PropertyKey::NumberLiteral(_)) {
                        // Computed key: emit key expr, then value, then computed opcode
                        self.compile_property_key(&p.key, line)?;
                        if let Some(val) = &p.value {
                            self.compile_expr(val)?;
                        } else {
                            self.chunk.emit_op(OpCode::Undefined, line);
                        }
                        let op = if p.is_static {
                            OpCode::ClassStaticFieldComputed
                        } else {
                            OpCode::ClassFieldComputed
                        };
                        self.chunk.emit_op(op, line);
                    } else {
                        if let Some(val) = &p.value {
                            self.compile_expr(val)?;
                        } else {
                            self.chunk.emit_op(OpCode::Undefined, line);
                        }
                        let key_id = self.property_key_name(&p.key);
                        let idx = self.make_string_constant(key_id);
                        let op = if p.is_static {
                            OpCode::ClassStaticField
                        } else {
                            OpCode::ClassField
                        };
                        self.chunk.emit_op_u16(op, idx, line);
                    }
                }
                ClassMember::StaticBlock(block) => {
                    // `static { ... }` runs once at class definition time with `this`
                    // bound to the class. Compile the block as a 0-param function and
                    // immediately invoke it with the class as the receiver.
                    let name = self.interner.intern("<static_block>");
                    let child_chunk = self.compile_function_body(
                        name,
                        &[],
                        block,
                        false, // is_async
                        false, // is_generator
                    )?;
                    let chunk_idx = self.chunk.child_chunks.len() as u16;
                    let uv_descs = child_chunk.upvalue_descriptors.clone();
                    self.chunk.child_chunks.push(child_chunk);
                    // Stack: [class]
                    // Dup the class so we can keep it on the stack while calling.
                    self.chunk.emit_op(OpCode::Dup, line);
                    // Build the closure for the block body.
                    self.chunk.emit_op_u16(OpCode::Closure, chunk_idx, line);
                    for desc in &uv_descs {
                        self.chunk.emit_byte(if desc.is_local { 1 } else { 0 }, line);
                        self.chunk.emit_byte(desc.index, line);
                    }
                    // Stack: [class, class, fn]
                    // Use Function.prototype.call to invoke with this=class:
                    //   fn.call(class)  →  emit CallMethod "call" with 1 arg (the class).
                    // Simpler path: swap so stack becomes [class, fn, class], then Call argc=0 won't bind `this`.
                    // Instead, model as: push fn, push receiver as arg, use a runtime helper.
                    // Easiest correct path: invoke via Function.prototype.call.
                    let call_name = self.interner.intern("call");
                    let call_id = self.make_string_constant(call_name);
                    // Swap: [class, fn, class]  (we already have [class, class, fn]; swap top two? no — Swap swaps top two)
                    self.chunk.emit_op(OpCode::Swap, line);
                    // Now stack: [class, fn, class] — but we wanted receiver-first for CallMethod.
                    // CallMethod expects [obj, arg0, arg1, ...] then pops obj+args, calls obj.method(args).
                    // We want to call fn.call(class) which means obj=fn, arg0=class.
                    // Stack: [class, fn, class] — top is class (arg0), below fn (obj). Perfect for CallMethod.
                    self.chunk.emit_byte(OpCode::CallMethod as u8, line);
                    self.chunk.emit_byte(1u8, line); // argc
                    self.chunk.code.push((call_id >> 8) as u8);
                    self.chunk.code.push((call_id & 0xFF) as u8);
                    // Stack: [class, return_value]
                    self.chunk.emit_op(OpCode::Pop, line);
                    // Stack: [class]
                }
            }
        }
        Ok(())
    }

    // ---- labeled ----

    fn compile_labeled(&mut self, l: &LabeledStatement) -> Result<(), String> {
        // If the body is a loop, pass the label to the loop's own LoopCtx
        // so that `continue label` targets the loop, not this wrapper.
        let is_loop = matches!(
            &l.body,
            Statement::For(_) | Statement::While(_) | Statement::DoWhile(_)
            | Statement::ForIn(_) | Statement::ForOf(_) | Statement::Labeled(_)
        );
        if is_loop {
            // Store the label so the child loop can pick it up
            let saved_label = self.pending_label.take();
            self.pending_label = Some(l.label);
            // Push a label context for `break label` only
            self.loops.push(LoopCtx {
                continue_target: self.chunk.len(),
                break_patches: Vec::new(),
                continue_patches: Vec::new(),
                scope_depth: self.scope_depth,
                label: Some(l.label),
                has_deferred_continue: false,
                try_depth: self.finally_stack.len(),
            });
            self.compile_statement(&l.body)?;
            self.patch_loop_breaks();
            self.pending_label = saved_label;
        } else {
            self.loops.push(LoopCtx {
                continue_target: self.chunk.len(),
                break_patches: Vec::new(),
                continue_patches: Vec::new(),
                scope_depth: self.scope_depth,
                label: Some(l.label),
                has_deferred_continue: false,
                try_depth: self.finally_stack.len(),
            });
            self.compile_statement(&l.body)?;
            self.patch_loop_breaks();
        }
        Ok(())
    }

    // ---- with ----

    fn compile_with(&mut self, w: &WithStatement) -> Result<(), String> {
        let line = w.span.start;
        self.compile_expr(&w.object)?;
        self.chunk.emit_op(OpCode::WithEnter, line);
        self.compile_statement(&w.body)?;
        self.chunk.emit_op(OpCode::WithExit, line);
        Ok(())
    }

    // ---- import (stub) ----

    fn compile_import(&mut self, i: &ImportDeclaration) -> Result<(), String> {
        match i {
            ImportDeclaration::Standard { specifiers, source, span } => {
                let line = span.start;
                // Emit ImportModule which pushes the module exports object
                let src_idx = self.make_string_constant(*source);
                self.chunk.emit_op_u16(OpCode::ImportModule, src_idx, line);

                if specifiers.is_empty() {
                    // Side-effect only import
                    self.chunk.emit_op(OpCode::Pop, line);
                } else {
                    // Bind each specifier from the module exports object
                    for spec in specifiers {
                        match spec {
                            ImportSpecifier::Default { local, .. } => {
                                self.chunk.emit_op(OpCode::Dup, line);
                                let key = self.interner.intern("default");
                                let idx = self.make_string_constant(key);
                                self.emit_get_property(idx, line);
                                let name_idx = self.make_string_constant(*local);
                                self.chunk.emit_op_u16(OpCode::DefineGlobal, name_idx, line);
                            }
                            ImportSpecifier::Named { imported, local, .. } => {
                                self.chunk.emit_op(OpCode::Dup, line);
                                let idx = self.make_string_constant(*imported);
                                self.emit_get_property(idx, line);
                                let name_idx = self.make_string_constant(*local);
                                self.chunk.emit_op_u16(OpCode::DefineGlobal, name_idx, line);
                            }
                            ImportSpecifier::Namespace { local, .. } => {
                                // The whole module object becomes the namespace
                                self.chunk.emit_op(OpCode::Dup, line);
                                let name_idx = self.make_string_constant(*local);
                                self.chunk.emit_op_u16(OpCode::DefineGlobal, name_idx, line);
                            }
                        }
                    }
                    self.chunk.emit_op(OpCode::Pop, line); // pop module object
                }
            }
        }
        Ok(())
    }

    fn compile_export(&mut self, e: &ExportDeclaration) -> Result<(), String> {
        match e {
            ExportDeclaration::Declaration { declaration, span } => {
                // export var/let/const/function/class — compile normally
                // The declaration becomes a global, which the module system can access
                self.compile_statement(declaration)?;
                // Mark as exported by setting on __exports__ object
                let export_names = self.extract_declaration_names(declaration);
                let line = span.start;
                for name in export_names {
                    let exports_key = self.interner.intern("__exports__");
                    let exports_idx = self.make_string_constant(exports_key);
                    self.chunk.emit_op_u16(OpCode::GetGlobal, exports_idx, line);
                    let name_idx = self.make_string_constant(name);
                    self.chunk.emit_op_u16(OpCode::GetGlobal, name_idx, line);
                    self.chunk.emit_op(OpCode::Swap, line);
                    // Stack: [value, exports_obj]
                    // Need: SetProperty on exports_obj
                    self.chunk.emit_op(OpCode::Swap, line);
                    self.emit_set_property(name_idx, line);
                    self.chunk.emit_op(OpCode::Pop, line);
                }
            }
            ExportDeclaration::Default { declaration, span } => {
                let line = span.start;
                self.compile_expr(declaration)?;
                // Store the value on __exports__.default
                let exports_key = self.interner.intern("__exports__");
                let exports_idx = self.make_string_constant(exports_key);
                self.chunk.emit_op_u16(OpCode::GetGlobal, exports_idx, line);
                self.chunk.emit_op(OpCode::Swap, line);
                // Stack: [exports_obj, value]
                self.chunk.emit_op(OpCode::Swap, line);
                let default_key = self.interner.intern("default");
                let default_idx = self.make_string_constant(default_key);
                self.emit_set_property(default_idx, line);
                self.chunk.emit_op(OpCode::Pop, line);
            }
            ExportDeclaration::Named { specifiers, span, .. } => {
                let line = span.start;
                for spec in specifiers {
                    let exports_key = self.interner.intern("__exports__");
                    let exports_idx = self.make_string_constant(exports_key);
                    self.chunk.emit_op_u16(OpCode::GetGlobal, exports_idx, line);
                    let local_idx = self.make_string_constant(spec.local);
                    self.chunk.emit_op_u16(OpCode::GetGlobal, local_idx, line);
                    let exported_idx = self.make_string_constant(spec.exported);
                    self.chunk.emit_op(OpCode::Swap, line);
                    self.emit_set_property(exported_idx, line);
                    self.chunk.emit_op(OpCode::Pop, line);
                }
            }
            ExportDeclaration::All { source, span, .. } => {
                let line = span.start;
                let src_idx = self.make_string_constant(*source);
                self.chunk.emit_op_u16(OpCode::ExportAllFrom, src_idx, line);
            }
        }
        Ok(())
    }

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

    /// Destructure a pattern, using the value stored in local slot `src_slot`.
    /// New identifier bindings are added as locals.
    fn destructure_pattern_from_slot(&mut self, pat: &Pattern, src_slot: u8, line: u32) -> Result<(), String> {
        match pat {
            Pattern::Array(arr) => {
                // Get source from slot and convert to iterator
                self.chunk.emit_op_u8(OpCode::GetLocal, src_slot, line);
                self.chunk.emit_op(OpCode::GetIterator, line);
                let iter_slot = self.locals.len() as u8;
                let iter_anon = self.interner.intern("__arr_iter__");
                self.add_local(iter_anon);
                self.mark_initialized();
                // Track whether the iterator has been exhausted, so we only call
                // IteratorClose (->return()) if the LAST step produced done=false.
                self.chunk.emit_op(OpCode::False, line);
                let done_slot = self.locals.len() as u8;
                let done_anon = self.interner.intern("__iter_done__");
                self.add_local(done_anon);
                self.mark_initialized();

                let has_rest = arr.elements.iter()
                    .any(|e| matches!(e, Some(Pattern::Rest(_))));
                for elem_opt in arr.elements.iter() {
                    if let Some(elem) = elem_opt {
                        if let Pattern::Rest(r) = elem {
                            // Collect remaining iterator values into an array
                            self.chunk.emit_op_u16(OpCode::CreateArray, 0, line);
                            let loop_start = self.chunk.len();
                            self.chunk.emit_op_u8(OpCode::GetLocal, iter_slot, line);
                            self.chunk.emit_op(OpCode::IteratorNext, line);
                            self.chunk.emit_op(OpCode::Dup, line);
                            self.chunk.emit_op(OpCode::IteratorDone, line);
                            let exit_jump = self.chunk.emit_jump(OpCode::JumpIfTrue, line);
                            self.chunk.emit_op(OpCode::IteratorValue, line);
                            self.chunk.emit_op(OpCode::ArrayAppend, line);
                            self.chunk.emit_loop(loop_start, line);
                            self.chunk.patch_jump(exit_jump);
                            self.chunk.emit_op(OpCode::Pop, line); // pop done result
                            match &r.argument {
                                Pattern::Identifier(id) => {
                                    self.add_local(id.name);
                                    self.mark_initialized();
                                }
                                Pattern::Array(_) | Pattern::Object(_) => {
                                    let anon = self.interner.intern("__destruct_rest__");
                                    self.add_local(anon);
                                    self.mark_initialized();
                                    let inner_slot = (self.locals.len() - 1) as u8;
                                    self.destructure_pattern_from_slot(&r.argument, inner_slot, line)?;
                                }
                                _ => { self.chunk.emit_op(OpCode::Pop, line); }
                            }
                            break;
                        }
                        // Regular element: get next value or undefined if done
                        self.chunk.emit_op_u8(OpCode::GetLocal, iter_slot, line);
                        self.chunk.emit_op(OpCode::IteratorNext, line);
                        self.chunk.emit_op(OpCode::Dup, line);
                        self.chunk.emit_op(OpCode::IteratorDone, line);
                        // Track done in done_slot for the conditional close at the end.
                        self.chunk.emit_op(OpCode::Dup, line);
                        self.chunk.emit_op_u8(OpCode::SetLocal, done_slot, line);
                        self.chunk.emit_op(OpCode::Pop, line);
                        let not_done_jump = self.chunk.emit_jump(OpCode::JumpIfFalse, line);
                        self.chunk.emit_op(OpCode::Pop, line);
                        self.chunk.emit_op(OpCode::Undefined, line);
                        let skip_jump = self.chunk.emit_jump(OpCode::Jump, line);
                        self.chunk.patch_jump(not_done_jump);
                        self.chunk.emit_op(OpCode::IteratorValue, line);
                        self.chunk.patch_jump(skip_jump);
                        match elem {
                            Pattern::Identifier(id) => {
                                self.add_local(id.name);
                                self.mark_initialized();
                            }
                            Pattern::Assignment(a) => {
                                // Apply default if value is undefined
                                self.chunk.emit_op(OpCode::Dup, line);
                                self.chunk.emit_op(OpCode::Undefined, line);
                                self.chunk.emit_op(OpCode::StrictNe, line);
                                let jump_idx = self.chunk.code.len();
                                self.chunk.emit_op(OpCode::JumpIfTrue, line);
                                self.chunk.code.push(0); self.chunk.code.push(0);
                                self.chunk.emit_op(OpCode::Pop, line);
                                if let Pattern::Identifier(id) = &a.left
                                    && Self::is_anonymous_fn_def(&a.right) {
                                        self.pending_function_name = Some(id.name);
                                }
                                self.compile_expr(&a.right)?;
                                let target = self.chunk.code.len();
                                let offset = (target as i16) - (jump_idx as i16) - 3;
                                self.chunk.code[jump_idx + 1] = (offset >> 8) as u8;
                                self.chunk.code[jump_idx + 2] = (offset & 0xFF) as u8;
                                match &a.left {
                                    Pattern::Identifier(id) => {
                                        self.add_local(id.name);
                                        self.mark_initialized();
                                    }
                                    Pattern::Array(_) | Pattern::Object(_) => {
                                        let anon = self.interner.intern("__destruct_inner__");
                                        self.add_local(anon);
                                        self.mark_initialized();
                                        let inner_slot = (self.locals.len() - 1) as u8;
                                        self.destructure_pattern_from_slot(&a.left, inner_slot, line)?;
                                    }
                                    _ => { self.chunk.emit_op(OpCode::Pop, line); }
                                }
                            }
                            Pattern::Array(_) | Pattern::Object(_) => {
                                let anon = self.interner.intern("__destruct_inner__");
                                self.add_local(anon);
                                self.mark_initialized();
                                let inner_slot = (self.locals.len() - 1) as u8;
                                self.destructure_pattern_from_slot(elem, inner_slot, line)?;
                            }
                            _ => { self.chunk.emit_op(OpCode::Pop, line); }
                        }
                    } else {
                        // Elision: advance iterator
                        self.chunk.emit_op_u8(OpCode::GetLocal, iter_slot, line);
                        self.chunk.emit_op(OpCode::IteratorNext, line);
                        self.chunk.emit_op(OpCode::Pop, line);
                    }
                }
                // If we didn't exhaust the iterator (no rest), close it — but only
                // when the last step's `done` flag was false (i.e., we know the
                // iterator still has at least one pending element when destructuring
                // exits early).
                if !has_rest {
                    self.chunk.emit_op_u8(OpCode::GetLocal, done_slot, line);
                    let skip_close = self.chunk.emit_jump(OpCode::JumpIfTrue, line);
                    self.chunk.emit_op_u8(OpCode::GetLocal, iter_slot, line);
                    self.chunk.emit_op(OpCode::IteratorClose, line);
                    self.chunk.patch_jump(skip_close);
                }
            }
            Pattern::Object(obj) => {
                // Per spec (RequireObjectCoercible), destructuring null/undefined into
                // an object pattern must throw TypeError — even for the empty pattern `{}`.
                self.chunk.emit_op_u8(OpCode::GetLocal, src_slot, line);
                let skip = self.chunk.emit_jump(OpCode::JumpIfFalsePeek, line);
                // Truthy: pop and continue (no throw needed).
                self.chunk.emit_op(OpCode::Pop, line);
                let after_throw = self.chunk.emit_jump(OpCode::Jump, line);
                self.chunk.patch_jump(skip);
                // Falsy: only null/undefined should throw — not 0, "", false.
                // Stack: [src]. Re-check via StrictEq with undefined/null.
                self.chunk.emit_op(OpCode::Dup, line);
                self.chunk.emit_op(OpCode::Undefined, line);
                self.chunk.emit_op(OpCode::StrictEq, line);
                let is_undef_jump = self.chunk.emit_jump(OpCode::JumpIfTrue, line);
                self.chunk.emit_op(OpCode::Dup, line);
                self.chunk.emit_op(OpCode::Null, line);
                self.chunk.emit_op(OpCode::StrictEq, line);
                let is_null_jump = self.chunk.emit_jump(OpCode::JumpIfTrue, line);
                // Other falsy (0, "", false) — fine, just pop and continue.
                self.chunk.emit_op(OpCode::Pop, line);
                let after_throw2 = self.chunk.emit_jump(OpCode::Jump, line);
                self.chunk.patch_jump(is_undef_jump);
                self.chunk.patch_jump(is_null_jump);
                self.emit_throw_type_error(
                    "Cannot destructure 'undefined' or 'null'",
                    line,
                );
                self.chunk.patch_jump(after_throw);
                self.chunk.patch_jump(after_throw2);

                // Collect excluded keys for any rest element
                let excluded_keys: Vec<StringId> = obj.properties.iter()
                    .filter_map(|p| {
                        if let ObjectPatternProperty::Property { key, .. } = p {
                            match key {
                                PropertyKey::Identifier(id) | PropertyKey::StringLiteral(id) => Some(*id),
                                _ => None,
                            }
                        } else { None }
                    })
                    .collect();

                for prop in &obj.properties {
                    match prop {
                        ObjectPatternProperty::Property { key, value, .. } => {
                            self.chunk.emit_op_u8(OpCode::GetLocal, src_slot, line);
                            match key {
                                PropertyKey::Identifier(id) | PropertyKey::StringLiteral(id) => {
                                    let idx = self.make_string_constant(*id);
                                    self.emit_get_property(idx, line);
                                }
                                PropertyKey::Computed(expr) => {
                                    self.compile_expr(expr)?;
                                    self.chunk.emit_op(OpCode::GetElement, line);
                                }
                                PropertyKey::NumberLiteral(n) => {
                                    self.emit_constant(Value::number(*n), line);
                                    self.chunk.emit_op(OpCode::GetElement, line);
                                }
                                _ => { self.chunk.emit_op(OpCode::Pop, line); continue; }
                            }
                            match value {
                                Pattern::Identifier(id) => {
                                    self.add_local(id.name);
                                    self.mark_initialized();
                                }
                                Pattern::Assignment(a) => {
                                    self.chunk.emit_op(OpCode::Dup, line);
                                    self.chunk.emit_op(OpCode::Undefined, line);
                                    self.chunk.emit_op(OpCode::StrictNe, line);
                                    let jump_idx = self.chunk.code.len();
                                    self.chunk.emit_op(OpCode::JumpIfTrue, line);
                                    self.chunk.code.push(0); self.chunk.code.push(0);
                                    self.chunk.emit_op(OpCode::Pop, line);
                                    if let Pattern::Identifier(id) = &a.left
                                        && Self::is_anonymous_fn_def(&a.right)
                                    {
                                        self.pending_function_name = Some(id.name);
                                    }
                                    self.compile_expr(&a.right)?;
                                    let target = self.chunk.code.len();
                                    let offset = (target as i16) - (jump_idx as i16) - 3;
                                    self.chunk.code[jump_idx + 1] = (offset >> 8) as u8;
                                    self.chunk.code[jump_idx + 2] = (offset & 0xFF) as u8;
                                    match &a.left {
                                        Pattern::Identifier(id) => {
                                            self.add_local(id.name);
                                            self.mark_initialized();
                                        }
                                        Pattern::Array(_) | Pattern::Object(_) => {
                                            let anon = self.interner.intern("__destruct_inner__");
                                            self.add_local(anon);
                                            self.mark_initialized();
                                            let inner_slot = (self.locals.len() - 1) as u8;
                                            self.destructure_pattern_from_slot(&a.left, inner_slot, line)?;
                                        }
                                        _ => { self.chunk.emit_op(OpCode::Pop, line); }
                                    }
                                }
                                Pattern::Array(_) | Pattern::Object(_) => {
                                    let anon = self.interner.intern("__destruct_inner__");
                                    self.add_local(anon);
                                    self.mark_initialized();
                                    let inner_slot = (self.locals.len() - 1) as u8;
                                    self.destructure_pattern_from_slot(value, inner_slot, line)?;
                                }
                                _ => { self.chunk.emit_op(OpCode::Pop, line); }
                            }
                        }
                        ObjectPatternProperty::Rest(rest) => {
                            // ObjectRest: collect all props except excluded ones
                            self.chunk.emit_op_u8(OpCode::GetLocal, src_slot, line);
                            let n = excluded_keys.len().min(255) as u8;
                            self.chunk.emit_byte(OpCode::ObjectRest as u8, line);
                            self.chunk.code.push(n);
                            for &key_id in &excluded_keys {
                                let idx = self.make_string_constant(key_id);
                                self.chunk.code.push((idx >> 8) as u8);
                                self.chunk.code.push((idx & 0xFF) as u8);
                            }
                            if let Pattern::Identifier(id) = &rest.argument {
                                self.add_local(id.name);
                                self.mark_initialized();
                            } else {
                                self.chunk.emit_op(OpCode::Pop, line);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn compile_function_body(
        &mut self,
        name: StringId,
        params: &[Pattern],
        body: &BlockStatement,
        is_async: bool,
        is_generator: bool,
    ) -> Result<Chunk, String> {
        self.compile_function_body_with_self(name, params, body, is_async, is_generator, None)
    }

    fn compile_function_body_with_self(
        &mut self,
        name: StringId,
        params: &[Pattern],
        body: &BlockStatement,
        is_async: bool,
        is_generator: bool,
        self_binding: Option<StringId>,
    ) -> Result<Chunk, String> {
        let source_name = self.chunk.source_name;

        let mut child_chunk = Chunk::new(name, source_name);
        child_chunk.param_count = params.len() as u16;
        // Function.length: count params before first default or rest
        child_chunk.formal_length = params.iter()
            .take_while(|p| !matches!(p, Pattern::Assignment(_) | Pattern::Rest(_)))
            .count() as u16;

        let mut flags = ChunkFlags::empty();
        if is_async {
            flags |= ChunkFlags::ASYNC;
        }
        if is_generator {
            flags |= ChunkFlags::GENERATOR;
        }
        // Inherit strict mode from parent, or detect "use strict" directive
        if self.chunk.flags.contains(ChunkFlags::STRICT) || self.has_use_strict_directive(&body.body) {
            flags |= ChunkFlags::STRICT;
        }
        child_chunk.flags = flags;

        // Swap compiler state -- save parent's locals so inner functions can capture them.
        let parent_chunk = std::mem::replace(&mut self.chunk, child_chunk);
        let parent_locals = std::mem::take(&mut self.locals);
        let parent_upvalues = std::mem::take(&mut self.upvalues);
        let parent_depth = self.scope_depth;
        let parent_loops = std::mem::take(&mut self.loops);
        let parent_enclosing_locals = self.enclosing_locals.take();
        let parent_enclosing_upvalues = self.enclosing_upvalues.take();

        // Make parent's locals available for upvalue resolution
        self.enclosing_locals = Some(parent_locals.clone());
        self.enclosing_upvalues = Some(parent_upvalues.clone());

        self.scope_depth = 1; // function body is its own scope

        // Declare parameters as locals (use anonymous slot for destructuring patterns).
        for (param_idx, param) in params.iter().enumerate() {
            match param {
                Pattern::Identifier(id) => {
                    self.add_local(id.name);
                    self.mark_initialized();
                }
                Pattern::Assignment(a) => {
                    match &a.left {
                        Pattern::Identifier(id) => {
                            self.add_local(id.name);
                            self.mark_initialized();
                        }
                        _ => {
                            let anon = self.interner.intern(&format!("__param{param_idx}__"));
                            self.add_local(anon);
                            self.mark_initialized();
                        }
                    }
                }
                Pattern::Rest(r) => {
                    match &r.argument {
                        Pattern::Identifier(id) => {
                            self.add_local(id.name);
                            self.mark_initialized();
                        }
                        _ => {
                            let anon = self.interner.intern(&format!("__param{param_idx}__"));
                            self.add_local(anon);
                            self.mark_initialized();
                        }
                    }
                }
                Pattern::Array(_) | Pattern::Object(_) | Pattern::Member(_) => {
                    let anon = self.interner.intern(&format!("__param{param_idx}__"));
                    self.add_local(anon);
                    self.mark_initialized();
                }
            }
        }

        // Emit default parameter initialization code.
        // For each parameter with a default value, check if undefined and assign default.
        for (i, param) in params.iter().enumerate() {
            if let Pattern::Assignment(a) = param {
                let line = 0;
                self.chunk.emit_op(OpCode::GetLocal, line);
                self.chunk.code.push(i as u8);
                self.chunk.emit_op(OpCode::Undefined, line);
                self.chunk.emit_op(OpCode::StrictNe, line);
                let jump_idx = self.chunk.code.len();
                self.chunk.emit_op(OpCode::JumpIfTrue, line);
                self.chunk.code.push(0);
                self.chunk.code.push(0);
                // Default value expression
                self.compile_expr(&a.right)?;
                // Set the local
                self.chunk.emit_op(OpCode::SetLocal, line);
                self.chunk.code.push(i as u8);
                self.chunk.emit_op(OpCode::Pop, line);
                // Patch the jump
                let target = self.chunk.code.len();
                let offset = (target as i16) - (jump_idx as i16) - 3;
                self.chunk.code[jump_idx + 1] = (offset >> 8) as u8;
                self.chunk.code[jump_idx + 2] = (offset & 0xFF) as u8;
            }
        }

        // Emit CollectRest for rest parameters
        for (i, param) in params.iter().enumerate() {
            if let Pattern::Rest(r) = param
                && let Pattern::Identifier(_) = &r.argument {
                    self.chunk.emit_byte(OpCode::CollectRest as u8, 0);
                    self.chunk.code.push(i as u8);
                    self.chunk.code.push(i as u8);
                }
        }

        // Destructure array/object pattern parameters (after defaults applied).
        for (i, param) in params.iter().enumerate() {
            let pattern_to_destructure = match param {
                Pattern::Array(_) | Pattern::Object(_) => Some(param),
                Pattern::Assignment(a) => match &a.left {
                    Pattern::Array(_) | Pattern::Object(_) => Some(&a.left),
                    _ => None,
                },
                _ => None,
            };
            if let Some(pat) = pattern_to_destructure {
                self.destructure_pattern_from_slot(pat, i as u8, 0)?;
            }
        }

        // Hoist var declarations inside the function body.
        {
            let mut hoisted_names = Vec::new();
            for stmt in &body.body {
                collect_var_declarations(stmt, &mut hoisted_names);
            }
            let param_names: Vec<StringId> = self.locals.iter().map(|l| l.name).collect();
            for name in hoisted_names {
                // Don't re-declare parameters
                if !param_names.contains(&name) && self.resolve_local(name).is_none() {
                    self.chunk.emit_op(OpCode::Undefined, 0);
                    self.add_local(name);
                    self.mark_initialized();
                }
            }
        }

        // Named function expression self-binding: `function f() { ...f()... }` exposes
        // `f` inside the body as an immutable reference to the function itself.
        // Skip if the name collides with a parameter (the parameter wins).
        if let Some(self_name) = self_binding {
            let already_a_param = self.locals.iter().any(|l| l.name == self_name);
            if !already_a_param {
                self.chunk.emit_op(OpCode::LoadCallee, 0);
                self.add_local(self_name);
                self.mark_initialized();
            }
        }

        // Hoist function declarations: each top-level `function f() {...}` in the body
        // is initialized at the top with its closure, shadowing any same-named parameter
        // or var binding (per spec, function declarations have higher precedence than
        // params/vars in function code).
        //
        // Skip the hoist if the body has any top-level `let` / `const` — pre-compiling
        // inner functions would resolve those bindings as missing upvalues since their
        // slots are not yet allocated. The functions still work in execution order; only
        // the rare param-shadowing pattern (`function f(x){ return x; function x(){} }`)
        // depends on hoisting and that pattern doesn't typically include lexical decls.
        let has_top_level_lex = body.body.iter().any(|s| matches!(
            s,
            Statement::Variable(d) if matches!(d.kind, VarKind::Let | VarKind::Const)
        ));
        let mut hoisted_fns: Vec<StringId> = Vec::new();
        // Function declaration named `arguments` shouldn't shadow the arguments
        // object when params have expressions (defaults/destructuring/rest), per
        // spec — the arguments object is required and the user-visible binding.
        let has_param_exprs = params.iter().any(|p| matches!(
            p,
            Pattern::Assignment(_) | Pattern::Array(_) | Pattern::Object(_) | Pattern::Rest(_)
        ));
        let arguments_id = self.interner.intern("arguments");
        if !has_top_level_lex {
            for stmt in &body.body {
                if let Statement::Function(f) = stmt
                    && let Some(name) = f.id
                {
                    // Skip hoisting `function arguments(){}` if the arguments object
                    // is needed (params have expressions or `arguments` isn't a param).
                    if name == arguments_id
                        && (has_param_exprs || !params.iter().any(|p| matches!(p, Pattern::Identifier(id) if id.name == arguments_id)))
                    {
                        continue;
                    }
                    let line = f.span.start;
                    let child_chunk = self.compile_function_body(name, &f.params, &f.body, f.is_async, f.is_generator)?;
                    let chunk_idx = self.chunk.child_chunks.len() as u16;
                    let upvalue_descs = child_chunk.upvalue_descriptors.clone();
                    self.chunk.child_chunks.push(child_chunk);
                    self.chunk.emit_op_u16(OpCode::Closure, chunk_idx, line);
                    for desc in &upvalue_descs {
                        self.chunk.emit_byte(if desc.is_local { 1 } else { 0 }, line);
                        self.chunk.emit_byte(desc.index, line);
                    }
                    if let Some(slot) = self.resolve_local(name) {
                        if slot <= u8::MAX as usize {
                            self.chunk.emit_op_u8(OpCode::SetLocal, slot as u8, line);
                        } else {
                            self.chunk.emit_op_u16(OpCode::SetLocalWide, slot as u16, line);
                        }
                        self.chunk.emit_op(OpCode::Pop, line);
                    } else {
                        self.add_local(name);
                        self.mark_initialized();
                    }
                    hoisted_fns.push(name);
                }
            }
        }

        // For generator functions, emit a CreateGenerator opcode here so the
        // parameter destructuring and other prologue work runs eagerly when the
        // function is called. The opcode captures the current frame and returns
        // a generator object; the body proper runs lazily on the first .next().
        if is_generator {
            self.chunk.emit_op(OpCode::CreateGenerator, 0);
        }

        // Compile body. Skip top-level function declarations that were hoisted above.
        for stmt in &body.body {
            if let Statement::Function(f) = stmt
                && let Some(name) = f.id
                && hoisted_fns.contains(&name)
            {
                continue;
            }
            self.compile_statement(stmt)?;
        }

        // Implicit return.
        let line = self.current_line();
        self.chunk.emit_op(OpCode::ReturnUndefined, line);
        self.chunk.local_count = self.locals.len() as u16;

        // Store upvalue descriptors in the compiled chunk.
        let upvalue_descs: Vec<UpvalueDescriptor> = self.upvalues.iter().map(|uv| {
            UpvalueDescriptor { index: uv.index, is_local: uv.is_local }
        }).collect();
        self.chunk.upvalue_count = upvalue_descs.len() as u16;
        self.chunk.upvalue_descriptors = upvalue_descs;

        // Swap back.
        let compiled = std::mem::replace(&mut self.chunk, parent_chunk);

        // Propagate captured flags back to parent locals
        let mut restored_locals = parent_locals;
        if let Some(enc_locals) = self.enclosing_locals.take() {
            for (i, enc) in enc_locals.iter().enumerate() {
                if enc.captured && i < restored_locals.len() {
                    restored_locals[i].captured = true;
                }
            }
        }
        self.locals = restored_locals;
        self.upvalues = parent_upvalues;
        self.scope_depth = parent_depth;
        self.loops = parent_loops;
        self.enclosing_locals = parent_enclosing_locals;
        self.enclosing_upvalues = parent_enclosing_upvalues;

        Ok(compiled)
    }

    fn compile_arrow_body(
        &mut self,
        params: &[Pattern],
        body: &ArrowBody,
        is_async: bool,
    ) -> Result<Chunk, String> {
        let source_name = self.chunk.source_name;
        let arrow_name = self.interner.intern("<arrow>");

        let mut child_chunk = Chunk::new(arrow_name, source_name);
        child_chunk.param_count = params.len() as u16;
        child_chunk.formal_length = params.iter()
            .take_while(|p| !matches!(p, Pattern::Assignment(_) | Pattern::Rest(_)))
            .count() as u16;
        child_chunk.flags = ChunkFlags::ARROW;
        if is_async {
            child_chunk.flags |= ChunkFlags::ASYNC;
        }

        let parent_chunk = std::mem::replace(&mut self.chunk, child_chunk);
        let parent_locals = std::mem::take(&mut self.locals);
        let parent_upvalues = std::mem::take(&mut self.upvalues);
        let parent_depth = self.scope_depth;
        let parent_loops = std::mem::take(&mut self.loops);
        let parent_enclosing_locals = self.enclosing_locals.take();
        let parent_enclosing_upvalues = self.enclosing_upvalues.take();

        // Make parent's locals available for upvalue resolution (enables nested closures)
        self.enclosing_locals = Some(parent_locals.clone());
        self.enclosing_upvalues = Some(parent_upvalues.clone());

        self.scope_depth = 1;

        for (param_idx, param) in params.iter().enumerate() {
            match param {
                Pattern::Identifier(id) => {
                    self.add_local(id.name);
                    self.mark_initialized();
                }
                Pattern::Assignment(a) => {
                    match &a.left {
                        Pattern::Identifier(id) => {
                            self.add_local(id.name);
                            self.mark_initialized();
                        }
                        _ => {
                            let anon = self.interner.intern(&format!("__param{param_idx}__"));
                            self.add_local(anon);
                            self.mark_initialized();
                        }
                    }
                }
                Pattern::Rest(r) => {
                    match &r.argument {
                        Pattern::Identifier(id) => {
                            self.add_local(id.name);
                            self.mark_initialized();
                        }
                        _ => {
                            let anon = self.interner.intern(&format!("__param{param_idx}__"));
                            self.add_local(anon);
                            self.mark_initialized();
                        }
                    }
                }
                Pattern::Array(_) | Pattern::Object(_) | Pattern::Member(_) => {
                    let anon = self.interner.intern(&format!("__param{param_idx}__"));
                    self.add_local(anon);
                    self.mark_initialized();
                }
            }
        }

        // Emit default parameter initialization for arrow functions
        for (i, param) in params.iter().enumerate() {
            if let Pattern::Assignment(a) = param {
                let line = 0;
                self.chunk.emit_op(OpCode::GetLocal, line);
                self.chunk.code.push(i as u8);
                self.chunk.emit_op(OpCode::Undefined, line);
                self.chunk.emit_op(OpCode::StrictNe, line);
                let jump_idx = self.chunk.code.len();
                self.chunk.emit_op(OpCode::JumpIfTrue, line);
                self.chunk.code.push(0);
                self.chunk.code.push(0);
                self.compile_expr(&a.right)?;
                self.chunk.emit_op(OpCode::SetLocal, line);
                self.chunk.code.push(i as u8);
                self.chunk.emit_op(OpCode::Pop, line);
                let target = self.chunk.code.len();
                let offset = (target as i16) - (jump_idx as i16) - 3;
                self.chunk.code[jump_idx + 1] = (offset >> 8) as u8;
                self.chunk.code[jump_idx + 2] = (offset & 0xFF) as u8;
            }
        }

        // Emit CollectRest for rest parameters
        for (i, param) in params.iter().enumerate() {
            if let Pattern::Rest(r) = param
                && let Pattern::Identifier(_) = &r.argument {
                    self.chunk.emit_byte(OpCode::CollectRest as u8, 0);
                    self.chunk.code.push(i as u8);
                    self.chunk.code.push(i as u8);
                }
        }

        // Destructure array/object pattern parameters
        for (i, param) in params.iter().enumerate() {
            let pattern_to_destructure = match param {
                Pattern::Array(_) | Pattern::Object(_) => Some(param),
                Pattern::Assignment(a) => match &a.left {
                    Pattern::Array(_) | Pattern::Object(_) => Some(&a.left),
                    _ => None,
                },
                _ => None,
            };
            if let Some(pat) = pattern_to_destructure {
                self.destructure_pattern_from_slot(pat, i as u8, 0)?;
            }
        }

        match body {
            ArrowBody::Expression(expr) => {
                self.compile_expr(expr)?;
                let line = self.current_line();
                self.chunk.emit_op(OpCode::Return, line);
            }
            ArrowBody::Block(block) => {
                for stmt in &block.body {
                    self.compile_statement(stmt)?;
                }
                let line = self.current_line();
                self.chunk.emit_op(OpCode::ReturnUndefined, line);
            }
        }

        self.chunk.local_count = self.locals.len() as u16;

        // Store upvalue descriptors
        let upvalue_descs: Vec<UpvalueDescriptor> = self.upvalues.iter().map(|uv| {
            UpvalueDescriptor { index: uv.index, is_local: uv.is_local }
        }).collect();
        self.chunk.upvalue_count = upvalue_descs.len() as u16;
        self.chunk.upvalue_descriptors = upvalue_descs;

        let compiled = std::mem::replace(&mut self.chunk, parent_chunk);

        // Propagate captured flags back to parent locals
        let mut restored_locals = parent_locals;
        for uv in &self.upvalues {
            if uv.is_local && (uv.index as usize) < restored_locals.len() {
                restored_locals[uv.index as usize].captured = true;
            }
        }
        self.locals = restored_locals;
        self.upvalues = parent_upvalues;
        self.scope_depth = parent_depth;
        self.loops = parent_loops;
        self.enclosing_locals = parent_enclosing_locals;
        self.enclosing_upvalues = parent_enclosing_upvalues;

        Ok(compiled)
    }

    // ====================================================================
    // Expressions
    // ====================================================================

    fn compile_expr(&mut self, expr: &Expression) -> Result<(), String> {
        match expr {
            Expression::NumberLiteral(n) => self.compile_number(n),
            Expression::StringLiteral(s) => self.compile_string_lit(s),
            Expression::BooleanLiteral(b) => {
                let op = if b.value { OpCode::True } else { OpCode::False };
                self.chunk.emit_op(op, b.span.start);
                Ok(())
            }
            Expression::NullLiteral(span) => {
                self.chunk.emit_op(OpCode::Null, span.start);
                Ok(())
            }
            Expression::Identifier(id) => self.compile_identifier(id),
            Expression::This(span) => {
                // Look up __this__ (set by Construct opcode)
                let this_name = self.interner.intern("__this__");
                let idx = self.make_string_constant(this_name);
                self.chunk.emit_op_u16(OpCode::GetGlobal, idx, span.start);
                Ok(())
            }
            Expression::Binary(b) => self.compile_binary(b),
            Expression::Unary(u) => self.compile_unary(u),
            Expression::Update(u) => self.compile_update(u),
            Expression::Logical(l) => self.compile_logical(l),
            Expression::Conditional(c) => self.compile_conditional(c),
            Expression::Assignment(a) => self.compile_assignment(a),
            Expression::Sequence(s) => self.compile_sequence(s),
            Expression::Member(m) => self.compile_member(m),
            Expression::Call(c) => self.compile_call(c),
            Expression::New(n) => self.compile_new(n),
            Expression::Array(a) => self.compile_array(a),
            Expression::Object(o) => self.compile_object(o),
            Expression::Function(f) => self.compile_function_expr(f),
            Expression::ArrowFunction(a) => self.compile_arrow_expr(a),
            Expression::Class(c) => self.compile_class_expr(c),
            Expression::TemplateLiteral(t) => self.compile_template_literal(t),
            Expression::TaggedTemplate(t) => self.compile_tagged_template(t),
            Expression::Spread(s) => self.compile_expr(&s.argument),
            Expression::Yield(y) => self.compile_yield(y),
            Expression::Await(a) => self.compile_await(a),
            Expression::OptionalChain(o) => self.compile_optional_chain(o),
            Expression::RegExpLiteral(r) => self.compile_regexp(r),
            Expression::MetaProperty(m) => self.compile_meta_property(m),
            Expression::Import(i) => {
                self.compile_expr(&i.source)?;
                self.chunk.emit_op(OpCode::ImportDynamic, i.span.start);
                Ok(())
            }
            Expression::Super(_) => {
                // super outside of a call is handled by compile_call
                self.chunk.emit_op(OpCode::GetSuperConstructor, 0);
                Ok(())
            }
        }
    }

    // ---- number literal ----

    fn compile_number(&mut self, n: &NumberLiteral) -> Result<(), String> {
        let line = n.span.start;
        let v = n.value;
        if v == 0.0 && !v.is_sign_negative() {
            self.chunk.emit_op(OpCode::Zero, line);
        } else if v == 1.0 {
            self.chunk.emit_op(OpCode::One, line);
        } else if v.fract() == 0.0
            && !v.is_nan()
            && v >= i32::MIN as f64
            && v <= i32::MAX as f64
        {
            let idx = self.chunk.add_constant(Value::int(v as i32));
            self.chunk.emit_op_u16(OpCode::Const, idx, line);
        } else {
            let idx = self.chunk.add_constant(Value::number(v));
            self.chunk.emit_op_u16(OpCode::Const, idx, line);
        }
        Ok(())
    }

    // ---- string literal ----

    fn compile_string_lit(&mut self, s: &StringLiteral) -> Result<(), String> {
        let idx = self.chunk.add_constant(Value::string(s.value));
        self.chunk.emit_op_u16(OpCode::Const, idx, s.span.start);
        Ok(())
    }

    // ---- identifier ----

    fn compile_identifier(&mut self, id: &Identifier) -> Result<(), String> {
        let line = id.span.start;
        let name_str = self.interner.resolve(id.name);
        if name_str == "undefined" {
            self.chunk.emit_op(OpCode::Undefined, line);
            return Ok(());
        }
        self.compile_get_variable(id.name, line)
    }

    // ---- binary ----

    fn compile_binary(&mut self, b: &BinaryExpression) -> Result<(), String> {
        let line = b.span.start;
        self.compile_expr(&b.left)?;
        self.compile_expr(&b.right)?;
        let op = match b.operator {
            BinaryOperator::Add => OpCode::Add,
            BinaryOperator::Sub => OpCode::Sub,
            BinaryOperator::Mul => OpCode::Mul,
            BinaryOperator::Div => OpCode::Div,
            BinaryOperator::Rem => OpCode::Rem,
            BinaryOperator::Exp => OpCode::Exp,
            BinaryOperator::EqEq => OpCode::Eq,
            BinaryOperator::NotEq => OpCode::Ne,
            BinaryOperator::StrictEq => OpCode::StrictEq,
            BinaryOperator::StrictNotEq => OpCode::StrictNe,
            BinaryOperator::Lt => OpCode::Lt,
            BinaryOperator::LtEq => OpCode::Le,
            BinaryOperator::Gt => OpCode::Gt,
            BinaryOperator::GtEq => OpCode::Ge,
            BinaryOperator::BitAnd => OpCode::BitAnd,
            BinaryOperator::BitOr => OpCode::BitOr,
            BinaryOperator::BitXor => OpCode::BitXor,
            BinaryOperator::Shl => OpCode::Shl,
            BinaryOperator::Shr => OpCode::Shr,
            BinaryOperator::UShr => OpCode::UShr,
            BinaryOperator::In => OpCode::In,
            BinaryOperator::InstanceOf => OpCode::InstanceOf,
        };
        self.chunk.emit_op(op, line);
        Ok(())
    }

    // ---- unary ----

    fn compile_unary(&mut self, u: &UnaryExpression) -> Result<(), String> {
        let line = u.span.start;

        // typeof <identifier> must not throw ReferenceError on undeclared globals.
        if u.operator == UnaryOperator::TypeOf
            && let Expression::Identifier(id) = &u.argument
                && self.resolve_local(id.name).is_none() {
                    let idx = self.make_string_constant(id.name);
                    self.chunk.emit_op_u16(OpCode::TypeOfGlobal, idx, line);
                    return Ok(());
                }

        // delete needs special handling per target type.
        if u.operator == UnaryOperator::Delete {
            return self.compile_delete(&u.argument, line);
        }

        self.compile_expr(&u.argument)?;
        match u.operator {
            UnaryOperator::Minus => self.chunk.emit_op(OpCode::Neg, line),
            UnaryOperator::Plus => self.chunk.emit_op(OpCode::Pos, line),
            UnaryOperator::Not => self.chunk.emit_op(OpCode::Not, line),
            UnaryOperator::BitNot => self.chunk.emit_op(OpCode::BitNot, line),
            UnaryOperator::TypeOf => self.chunk.emit_op(OpCode::TypeOf, line),
            UnaryOperator::Void => self.chunk.emit_op(OpCode::Void, line),
            UnaryOperator::Delete => unreachable!(),
        }
        Ok(())
    }

    fn compile_delete(&mut self, argument: &Expression, line: u32) -> Result<(), String> {
        match argument {
            Expression::Member(m) => {
                self.compile_expr(&m.object)?;
                match &m.property {
                    MemberProperty::Identifier(id) => {
                        self.emit_constant(Value::string(*id), line);
                    }
                    MemberProperty::Expression(expr) => {
                        self.compile_expr(expr)?;
                    }
                    MemberProperty::PrivateIdentifier(_) => {
                        self.chunk.emit_op(OpCode::False, line);
                        return Ok(());
                    }
                }
                self.chunk.emit_op(OpCode::DeleteProp, line);
            }
            Expression::Identifier(id) => {
                let idx = self.make_string_constant(id.name);
                self.chunk.emit_op_u16(OpCode::DeleteGlobal, idx, line);
            }
            _ => {
                // `delete <non-reference>` evaluates the expression, pops it, returns true.
                self.compile_expr(argument)?;
                self.chunk.emit_op(OpCode::Pop, line);
                self.chunk.emit_op(OpCode::True, line);
            }
        }
        Ok(())
    }

    // ---- update (++ / --) ----

    fn compile_update(&mut self, u: &UpdateExpression) -> Result<(), String> {
        let line = u.span.start;
        let inc_op = match u.operator {
            UpdateOperator::Increment => OpCode::Inc,
            UpdateOperator::Decrement => OpCode::Dec,
        };

        match &u.argument {
            Expression::Identifier(id) => {
                self.compile_get_variable(id.name, line)?;
                if u.prefix {
                    self.chunk.emit_op(inc_op, line);
                    self.compile_set_variable(id.name, line)?;
                } else {
                    // postfix: apply ToNumber to old value, then inc/dec the copy
                    self.chunk.emit_op(OpCode::Pos, line); // ToNumber on original
                    self.chunk.emit_op(OpCode::Dup, line);
                    self.chunk.emit_op(inc_op, line);
                    self.compile_set_variable(id.name, line)?;
                    self.chunk.emit_op(OpCode::Pop, line);
                }
            }
            Expression::Member(m) => {
                self.compile_expr(&m.object)?;
                match &m.property {
                    MemberProperty::Identifier(name) => {
                        let idx = self.make_string_constant(*name);
                        self.chunk.emit_op(OpCode::Dup, line);
                        self.emit_get_property(idx, line);
                        if u.prefix {
                            self.chunk.emit_op(inc_op, line);
                            self.emit_set_property(idx, line);
                        } else {
                            self.chunk.emit_op(OpCode::Dup, line);
                            self.chunk.emit_op(OpCode::Rot3, line);
                            self.chunk.emit_op(inc_op, line);
                            self.emit_set_property(idx, line);
                            self.chunk.emit_op(OpCode::Pop, line);
                        }
                    }
                    MemberProperty::Expression(key) => {
                        self.compile_expr(key)?;
                        self.chunk.emit_op(OpCode::Dup2, line);
                        self.chunk.emit_op(OpCode::GetElement, line);
                        self.chunk.emit_op(inc_op, line);
                        self.chunk.emit_op(OpCode::SetElement, line);
                    }
                    _ => {
                        self.chunk.emit_op(OpCode::Undefined, line);
                    }
                }
            }
            _ => {
                return Err(format!("invalid update expression target at {line}"));
            }
        }
        Ok(())
    }

    // ---- logical (short-circuit) ----

    fn compile_logical(&mut self, l: &LogicalExpression) -> Result<(), String> {
        let line = l.span.start;
        self.compile_expr(&l.left)?;
        match l.operator {
            LogicalOperator::And => {
                let jump = self.chunk.emit_jump(OpCode::JumpIfFalsePeek, line);
                self.chunk.emit_op(OpCode::Pop, line);
                self.compile_expr(&l.right)?;
                self.chunk.patch_jump(jump);
            }
            LogicalOperator::Or => {
                let jump = self.chunk.emit_jump(OpCode::JumpIfTruePeek, line);
                self.chunk.emit_op(OpCode::Pop, line);
                self.compile_expr(&l.right)?;
                self.chunk.patch_jump(jump);
            }
            LogicalOperator::NullishCoalescing => {
                let jump = self.chunk.emit_jump(OpCode::JumpIfNullishPeek, line);
                let end = self.chunk.emit_jump(OpCode::Jump, line);
                self.chunk.patch_jump(jump);
                self.chunk.emit_op(OpCode::Pop, line);
                self.compile_expr(&l.right)?;
                self.chunk.patch_jump(end);
            }
        }
        Ok(())
    }

    // ---- conditional (ternary) ----

    fn compile_conditional(&mut self, c: &ConditionalExpression) -> Result<(), String> {
        let line = c.span.start;
        self.compile_expr(&c.test)?;
        let then_jump = self.chunk.emit_jump(OpCode::JumpIfFalse, line);
        self.compile_expr(&c.consequent)?;
        let else_jump = self.chunk.emit_jump(OpCode::Jump, line);
        self.chunk.patch_jump(then_jump);
        self.compile_expr(&c.alternate)?;
        self.chunk.patch_jump(else_jump);
        Ok(())
    }

    // ---- assignment ----

    /// Destructure the value currently on top of the stack into `pat`, assigning
    /// to existing variables (compile_set_variable). Consumes the stack top.
    /// Used by nested destructuring assignment like `({x: {y}} = obj)`.
    fn compile_assign_to_pattern(&mut self, pat: &Pattern, line: u32) -> Result<(), String> {
        match pat {
            Pattern::Identifier(id) => {
                self.compile_set_variable(id.name, line)?;
                self.chunk.emit_op(OpCode::Pop, line);
            }
            Pattern::Member(m) => {
                // Stack: [..., value]. Need: assign value to obj.prop, leaving stack
                // unchanged on entry but consuming the value.
                self.compile_expr(&m.object)?;
                match &m.property {
                    MemberProperty::Identifier(id) => {
                        let idx = self.make_string_constant(*id);
                        // Stack: [..., value, obj]. Need [obj, value] for SetProperty.
                        self.chunk.emit_op(OpCode::Swap, line);
                        self.emit_set_property(idx, line);
                        self.chunk.emit_op(OpCode::Pop, line);
                    }
                    MemberProperty::Expression(expr) => {
                        // Stack: [..., value, obj]; eval key.
                        self.compile_expr(expr)?;
                        // Stack: [..., value, obj, key]. Need [obj, key, value] for SetElement.
                        self.chunk.emit_op(OpCode::Rot3, line);
                        self.chunk.emit_op(OpCode::Rot3, line);
                        self.chunk.emit_op(OpCode::SetElement, line);
                        self.chunk.emit_op(OpCode::Pop, line);
                    }
                    _ => {
                        self.chunk.emit_op(OpCode::Pop, line);
                        self.chunk.emit_op(OpCode::Pop, line);
                    }
                }
            }
            Pattern::Object(obj) => {
                for prop in &obj.properties {
                    if let ObjectPatternProperty::Property { key, value, .. } = prop {
                        self.chunk.emit_op(OpCode::Dup, line);
                        match key {
                            PropertyKey::Identifier(s) | PropertyKey::StringLiteral(s) => {
                                let key_idx = self.make_string_constant(*s);
                                self.emit_get_property(key_idx, line);
                            }
                            PropertyKey::Computed(expr) => {
                                self.compile_expr(expr)?;
                                self.chunk.emit_op(OpCode::GetElement, line);
                            }
                            PropertyKey::NumberLiteral(n) => {
                                self.emit_constant(Value::number(*n), line);
                                self.chunk.emit_op(OpCode::GetElement, line);
                            }
                            _ => { self.chunk.emit_op(OpCode::Pop, line); continue; }
                        }
                        match value {
                            Pattern::Identifier(id) => {
                                self.compile_set_variable(id.name, line)?;
                                self.chunk.emit_op(OpCode::Pop, line);
                            }
                            Pattern::Assignment(a) => {
                                self.chunk.emit_op(OpCode::Dup, line);
                                self.chunk.emit_op(OpCode::Undefined, line);
                                self.chunk.emit_op(OpCode::StrictEq, line);
                                let jump_idx = self.chunk.code.len();
                                self.chunk.emit_op(OpCode::JumpIfFalse, line);
                                self.chunk.code.push(0); self.chunk.code.push(0);
                                self.chunk.emit_op(OpCode::Pop, line);
                                self.compile_expr(&a.right)?;
                                let target = self.chunk.code.len();
                                let offset = (target as i16) - (jump_idx as i16) - 3;
                                self.chunk.code[jump_idx + 1] = (offset >> 8) as u8;
                                self.chunk.code[jump_idx + 2] = (offset & 0xFF) as u8;
                                if let Pattern::Identifier(id) = &a.left {
                                    self.compile_set_variable(id.name, line)?;
                                }
                                self.chunk.emit_op(OpCode::Pop, line);
                            }
                            Pattern::Object(_) | Pattern::Array(_) => {
                                self.compile_assign_to_pattern(value, line)?;
                            }
                            _ => { self.chunk.emit_op(OpCode::Pop, line); }
                        }
                    }
                }
                self.chunk.emit_op(OpCode::Pop, line);
            }
            Pattern::Array(arr) => {
                // Per spec, destructuring null/undefined as an array pattern throws
                // TypeError (GetIterator step). Inline the check here since we use
                // GetElement (not GetIterator) for nested array destructuring.
                self.chunk.emit_op(OpCode::Dup, line);
                self.chunk.emit_op(OpCode::Undefined, line);
                self.chunk.emit_op(OpCode::StrictEq, line);
                let undef_jump = self.chunk.emit_jump(OpCode::JumpIfTrue, line);
                self.chunk.emit_op(OpCode::Dup, line);
                self.chunk.emit_op(OpCode::Null, line);
                self.chunk.emit_op(OpCode::StrictEq, line);
                let null_jump = self.chunk.emit_jump(OpCode::JumpIfTrue, line);
                let skip_throw = self.chunk.emit_jump(OpCode::Jump, line);
                self.chunk.patch_jump(undef_jump);
                self.chunk.patch_jump(null_jump);
                self.emit_throw_type_error("object is not iterable", line);
                self.chunk.patch_jump(skip_throw);

                for (i, elem) in arr.elements.iter().enumerate() {
                    if let Some(elem_pat) = elem {
                        self.chunk.emit_op(OpCode::Dup, line);
                        let idx = self.chunk.add_constant(Value::int(i as i32));
                        self.chunk.emit_op_u16(OpCode::Const, idx, line);
                        self.chunk.emit_op(OpCode::GetElement, line);
                        match elem_pat {
                            Pattern::Identifier(id) => {
                                self.compile_set_variable(id.name, line)?;
                                self.chunk.emit_op(OpCode::Pop, line);
                            }
                            Pattern::Object(_) | Pattern::Array(_) => {
                                self.compile_assign_to_pattern(elem_pat, line)?;
                            }
                            _ => { self.chunk.emit_op(OpCode::Pop, line); }
                        }
                    }
                }
                self.chunk.emit_op(OpCode::Pop, line);
            }
            _ => { self.chunk.emit_op(OpCode::Pop, line); }
        }
        Ok(())
    }

    fn compile_assignment(&mut self, a: &AssignmentExpression) -> Result<(), String> {
        let line = a.span.start;
        match &a.left {
            AssignmentTarget::Identifier(id) => {
                if a.operator == AssignmentOperator::Assign {
                    if Self::is_anonymous_fn_def(&a.right) {
                        self.pending_function_name = Some(id.name);
                    }
                    self.compile_expr(&a.right)?;
                } else {
                    self.compile_get_variable(id.name, line)?;

                    // Logical assignment operators need short-circuit.
                    match a.operator {
                        AssignmentOperator::AndAssign => {
                            let jump = self.chunk.emit_jump(OpCode::JumpIfFalsePeek, line);
                            self.chunk.emit_op(OpCode::Pop, line);
                            self.compile_expr(&a.right)?;
                            self.chunk.patch_jump(jump);
                            self.compile_set_variable(id.name, line)?;
                            return Ok(());
                        }
                        AssignmentOperator::OrAssign => {
                            let jump = self.chunk.emit_jump(OpCode::JumpIfTruePeek, line);
                            self.chunk.emit_op(OpCode::Pop, line);
                            self.compile_expr(&a.right)?;
                            self.chunk.patch_jump(jump);
                            self.compile_set_variable(id.name, line)?;
                            return Ok(());
                        }
                        AssignmentOperator::NullishAssign => {
                            let jump = self.chunk.emit_jump(OpCode::JumpIfNullishPeek, line);
                            let end = self.chunk.emit_jump(OpCode::Jump, line);
                            self.chunk.patch_jump(jump);
                            self.chunk.emit_op(OpCode::Pop, line);
                            self.compile_expr(&a.right)?;
                            self.chunk.patch_jump(end);
                            self.compile_set_variable(id.name, line)?;
                            return Ok(());
                        }
                        _ => {}
                    }

                    self.compile_expr(&a.right)?;
                    self.emit_compound_arith(a.operator, line)?;
                }
                self.compile_set_variable(id.name, line)?;
            }
            AssignmentTarget::Member(m) => {
                self.compile_member_assignment(m, a.operator, &a.right, line)?;
            }
            AssignmentTarget::Pattern(pat) => {
                // Destructuring assignment: compile RHS, then assign to pattern
                self.compile_expr(&a.right)?;
                match pat {
                    Pattern::Array(arr_pat) => {
                        let has_rest = arr_pat.elements.iter()
                            .any(|e| matches!(e, Some(Pattern::Rest(_))));
                        if !has_rest {
                            // Iterator-protocol path: keeps `iter` on stack one slot
                            // below `rhs`. Conditionally calls IteratorClose at the
                            // end based on the last step's done flag.
                            // Stack invariant during loop: [..., rhs, iter] (iter at top).
                            //
                            // The done flag is stored at the BOTTOM (below rhs) as a local
                            // slot so it doesn't interfere with the Dup-of-iter pattern.
                            //
                            // At entry: stack has [rhs] at the top (from compile_expr above).
                            // Move it down to make room for the done flag.
                            // Pattern: [rhs] -> Pop, push False (done), push rhs back.
                            // We need a temp ferry — emit Swap with False:
                            //   [rhs] -> push False [rhs, false]
                            //         -> Swap [false, rhs]
                            // Now done flag is at locals slot N, and rhs is on top.
                            self.chunk.emit_op(OpCode::False, line);
                            self.chunk.emit_op(OpCode::Swap, line);
                            // Stack: [false, rhs]; done is at locals slot N (below rhs).
                            let done_slot = self.locals.len() as u8;
                            let done_anon = self.interner.intern("__assign_iter_done__");
                            self.add_local(done_anon);
                            self.mark_initialized();
                            self.chunk.emit_op(OpCode::Dup, line);          // [done, rhs, rhs]
                            self.chunk.emit_op(OpCode::GetIterator, line);  // [done, rhs, iter]
                            for elem in arr_pat.elements.iter() {
                                match elem {
                                    None => {
                                        self.chunk.emit_op(OpCode::Dup, line);
                                        self.chunk.emit_op(OpCode::IteratorNext, line);
                                        self.chunk.emit_op(OpCode::Pop, line);
                                    }
                                    Some(pat) => {
                                        self.chunk.emit_op(OpCode::Dup, line);
                                        self.chunk.emit_op(OpCode::IteratorNext, line);
                                        self.chunk.emit_op(OpCode::Dup, line);
                                        self.chunk.emit_op(OpCode::IteratorDone, line);
                                        self.chunk.emit_op(OpCode::Dup, line);
                                        self.chunk.emit_op_u8(OpCode::SetLocal, done_slot, line);
                                        self.chunk.emit_op(OpCode::Pop, line);
                                        let not_done = self.chunk.emit_jump(OpCode::JumpIfFalse, line);
                                        self.chunk.emit_op(OpCode::Pop, line);
                                        self.chunk.emit_op(OpCode::Undefined, line);
                                        let after = self.chunk.emit_jump(OpCode::Jump, line);
                                        self.chunk.patch_jump(not_done);
                                        self.chunk.emit_op(OpCode::IteratorValue, line);
                                        self.chunk.patch_jump(after);
                                        if let Pattern::Assignment(a) = pat {
                                            self.chunk.emit_op(OpCode::Dup, line);
                                            self.chunk.emit_op(OpCode::Undefined, line);
                                            self.chunk.emit_op(OpCode::StrictNe, line);
                                            let skip = self.chunk.emit_jump(OpCode::JumpIfTrue, line);
                                            self.chunk.emit_op(OpCode::Pop, line);
                                            if let Pattern::Identifier(id) = &a.left
                                                && Self::is_anonymous_fn_def(&a.right)
                                            {
                                                self.pending_function_name = Some(id.name);
                                            }
                                            self.compile_expr(&a.right)?;
                                            self.chunk.patch_jump(skip);
                                            self.compile_assign_to_pattern(&a.left, line)?;
                                        } else {
                                            self.compile_assign_to_pattern(pat, line)?;
                                        }
                                    }
                                }
                            }
                            // Stack: [done, rhs, iter]. Conditionally close.
                            self.chunk.emit_op_u8(OpCode::GetLocal, done_slot, line);
                            let skip_close = self.chunk.emit_jump(OpCode::JumpIfTrue, line);
                            self.chunk.emit_op(OpCode::IteratorClose, line);
                            let after_close = self.chunk.emit_jump(OpCode::Jump, line);
                            self.chunk.patch_jump(skip_close);
                            self.chunk.emit_op(OpCode::Pop, line);
                            self.chunk.patch_jump(after_close);
                            // Stack: [done, rhs]. We want [rhs] as the assignment result,
                            // so swap and pop done.
                            self.chunk.emit_op(OpCode::Swap, line);
                            self.chunk.emit_op(OpCode::Pop, line);
                            self.locals.pop();
                        } else {
                            // Rest path: fall back to GetElement-based destructuring.
                            // (Rest consumes the iterator anyway, and Array.slice for
                            // arrays gives the same observable result without the
                            // protocol gymnastics.)
                            for (i, elem) in arr_pat.elements.iter().enumerate() {
                                if let Some(elem_pat) = elem {
                                    self.chunk.emit_op(OpCode::Dup, line);
                                    let idx_val = Value::int(i as i32);
                                    let idx = self.chunk.add_constant(idx_val);
                                    self.chunk.emit_op_u16(OpCode::Const, idx, line);
                                    self.chunk.emit_op(OpCode::GetElement, line);
                                    match elem_pat {
                                        Pattern::Identifier(id) => {
                                            self.compile_set_variable(id.name, line)?;
                                            self.chunk.emit_op(OpCode::Pop, line);
                                        }
                                        Pattern::Rest(r) => {
                                            // Discard the index lookup; collect from i onwards.
                                            self.chunk.emit_op(OpCode::Pop, line);
                                            // Stack: [..., rhs]. We need to slice rhs from i onwards.
                                            // Build a sub-array via Array.from(rhs).slice(i).
                                            // Simpler: emit a runtime call: collect via iterator from rhs.
                                            self.chunk.emit_op(OpCode::Dup, line); // [..., rhs, rhs]
                                            self.chunk.emit_op(OpCode::GetIterator, line);
                                            // Skip first i items
                                            for _ in 0..i {
                                                self.chunk.emit_op(OpCode::Dup, line);
                                                self.chunk.emit_op(OpCode::IteratorNext, line);
                                                self.chunk.emit_op(OpCode::Pop, line);
                                            }
                                            // Now collect remaining
                                            self.chunk.emit_op_u16(OpCode::CreateArray, 0, line);
                                            self.chunk.emit_op(OpCode::Swap, line); // [..., rhs, arr, iter]
                                            let loop_start = self.chunk.len();
                                            self.chunk.emit_op(OpCode::Dup, line);
                                            self.chunk.emit_op(OpCode::IteratorNext, line);
                                            self.chunk.emit_op(OpCode::Dup, line);
                                            self.chunk.emit_op(OpCode::IteratorDone, line);
                                            let exit_jump = self.chunk.emit_jump(OpCode::JumpIfTrue, line);
                                            self.chunk.emit_op(OpCode::IteratorValue, line);
                                            self.chunk.emit_op(OpCode::Swap, line); // [..., rhs, arr, value, iter]
                                            self.chunk.emit_op(OpCode::Rot3, line); // [..., rhs, iter, arr, value]
                                            self.chunk.emit_op(OpCode::ArrayAppend, line); // [..., rhs, iter, arr]
                                            self.chunk.emit_op(OpCode::Swap, line); // [..., rhs, arr, iter]
                                            self.chunk.emit_loop(loop_start, line);
                                            self.chunk.patch_jump(exit_jump);
                                            self.chunk.emit_op(OpCode::Pop, line); // pop result
                                            self.chunk.emit_op(OpCode::Pop, line); // pop iter
                                            // Stack: [..., rhs, arr]
                                            // Bind arr to r.argument pattern.
                                            match &r.argument {
                                                Pattern::Identifier(id) => {
                                                    self.compile_set_variable(id.name, line)?;
                                                    self.chunk.emit_op(OpCode::Pop, line);
                                                }
                                                Pattern::Member(m) => {
                                                    // Stack: [..., arr]. Compile obj.prop = arr.
                                                    self.compile_expr(&m.object)?;
                                                    match &m.property {
                                                        MemberProperty::Identifier(id) => {
                                                            let idx = self.make_string_constant(*id);
                                                            // Stack: [arr, obj]. Need [obj, arr] for SetProperty.
                                                            self.chunk.emit_op(OpCode::Swap, line);
                                                            self.emit_set_property(idx, line);
                                                        }
                                                        MemberProperty::Expression(expr) => {
                                                            // Stack: [arr, obj]; eval key.
                                                            self.compile_expr(expr)?;
                                                            // Stack: [arr, obj, key]. Need [obj, key, arr] for SetElement.
                                                            self.chunk.emit_op(OpCode::Rot3, line); // [key, arr, obj] - rot3 brings top to bottom-3
                                                            // Actually rot3([a,b,c]) -> [c,a,b]. So [arr,obj,key] -> [key,arr,obj].
                                                            self.chunk.emit_op(OpCode::Rot3, line); // Apply twice: [obj,key,arr]
                                                            self.chunk.emit_op(OpCode::SetElement, line);
                                                        }
                                                        _ => { self.chunk.emit_op(OpCode::Pop, line); self.chunk.emit_op(OpCode::Pop, line); }
                                                    }
                                                    self.chunk.emit_op(OpCode::Pop, line);
                                                }
                                                Pattern::Object(_)
                                                | Pattern::Array(_) => {
                                                    self.compile_assign_to_pattern(&r.argument, line)?;
                                                }
                                                _ => { self.chunk.emit_op(OpCode::Pop, line); }
                                            }
                                        }
                                        Pattern::Object(_) | Pattern::Array(_) => {
                                            self.compile_assign_to_pattern(elem_pat, line)?;
                                        }
                                        _ => { self.chunk.emit_op(OpCode::Pop, line); }
                                    }
                                }
                            }
                        }
                    }
                    Pattern::Object(obj_pat) => {
                        // RequireObjectCoercible: throw TypeError if source is null/undefined.
                        // Even an empty pattern {} = null must throw per spec.
                        self.chunk.emit_op(OpCode::Dup, line);
                        let nullish_jump = self.chunk.emit_jump(OpCode::JumpIfNullishPeek, line);
                        self.chunk.emit_op(OpCode::Pop, line); // not nullish: drop the dup
                        let skip_throw = self.chunk.emit_jump(OpCode::Jump, line);
                        self.chunk.patch_jump(nullish_jump);
                        self.chunk.emit_op(OpCode::Pop, line); // pop dup
                        self.emit_throw_type_error("Cannot destructure 'undefined' or 'null'", line);
                        self.chunk.patch_jump(skip_throw);
                        // Check whether there is a rest element — if so, computed keys must be saved
                        // in the VM's computed_exclusions buffer via PushComputedExclude.
                        let has_rest = obj_pat.properties.iter().any(|p| matches!(p, ObjectPatternProperty::Rest(_)));
                        for prop in &obj_pat.properties {
                            match prop {
                            ObjectPatternProperty::Property { key, value, .. } => {
                                self.chunk.emit_op(OpCode::Dup, line);
                                match key {
                                    PropertyKey::Identifier(s) | PropertyKey::StringLiteral(s) => {
                                        let key_idx = self.make_string_constant(*s);
                                        self.emit_get_property(key_idx, line);
                                    }
                                    PropertyKey::Computed(expr) => {
                                        self.compile_expr(expr)?;
                                        if has_rest {
                                            // Save a copy of the computed key for ObjectRest exclusion
                                            self.chunk.emit_op(OpCode::Dup, line);
                                            self.chunk.emit_op(OpCode::PushComputedExclude, line);
                                        }
                                        self.chunk.emit_op(OpCode::GetElement, line);
                                    }
                                    PropertyKey::NumberLiteral(n) => {
                                        self.emit_constant(Value::number(*n), line);
                                        if has_rest {
                                            self.chunk.emit_op(OpCode::Dup, line);
                                            self.chunk.emit_op(OpCode::PushComputedExclude, line);
                                        }
                                        self.chunk.emit_op(OpCode::GetElement, line);
                                    }
                                    _ => { self.chunk.emit_op(OpCode::Pop, line); continue; }
                                }
                                match value {
                                    Pattern::Identifier(id) => {
                                        self.compile_set_variable(id.name, line)?;
                                        self.chunk.emit_op(OpCode::Pop, line);
                                    }
                                    Pattern::Assignment(a) => {
                                        self.chunk.emit_op(OpCode::Dup, line);
                                        self.chunk.emit_op(OpCode::Undefined, line);
                                        self.chunk.emit_op(OpCode::StrictEq, line);
                                        let jump_idx = self.chunk.code.len();
                                        self.chunk.emit_op(OpCode::JumpIfFalse, line);
                                        self.chunk.code.push(0); self.chunk.code.push(0);
                                        self.chunk.emit_op(OpCode::Pop, line);
                                        if let Pattern::Identifier(id) = &a.left
                                            && Self::is_anonymous_fn_def(&a.right) {
                                                self.pending_function_name = Some(id.name);
                                        }
                                        self.compile_expr(&a.right)?;
                                        let target = self.chunk.code.len();
                                        let offset = (target as i16) - (jump_idx as i16) - 3;
                                        self.chunk.code[jump_idx + 1] = (offset >> 8) as u8;
                                        self.chunk.code[jump_idx + 2] = (offset & 0xFF) as u8;
                                        if let Pattern::Identifier(id) = &a.left {
                                            self.compile_set_variable(id.name, line)?;
                                        }
                                        self.chunk.emit_op(OpCode::Pop, line);
                                    }
                                    Pattern::Object(_) | Pattern::Array(_) => {
                                        // Nested destructuring assignment: recurse via the
                                        // assign-to-pattern helper.
                                        self.compile_assign_to_pattern(value, line)?;
                                    }
                                    _ => { self.chunk.emit_op(OpCode::Pop, line); }
                                }
                            }
                            ObjectPatternProperty::Rest(rest) => {
                                if let Pattern::Identifier(id) = &rest.argument {
                                    // Collect excluded static key names
                                    let excluded: Vec<StringId> = obj_pat.properties.iter().filter_map(|p| {
                                        if let ObjectPatternProperty::Property { key, .. } = p {
                                            match key {
                                                PropertyKey::Identifier(s) | PropertyKey::StringLiteral(s) => Some(*s),
                                                _ => None,
                                            }
                                        } else { None }
                                    }).collect();
                                    self.chunk.emit_op(OpCode::Dup, line);
                                    self.chunk.emit_byte(OpCode::ObjectRest as u8, line);
                                    self.chunk.code.push(excluded.len() as u8);
                                    for k in &excluded {
                                        let idx = self.make_string_constant(*k);
                                        self.chunk.code.push((idx >> 8) as u8);
                                        self.chunk.code.push((idx & 0xFF) as u8);
                                    }
                                    self.compile_set_variable(id.name, line)?;
                                    self.chunk.emit_op(OpCode::Pop, line);
                                }
                            }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn compile_member_assignment(
        &mut self,
        m: &MemberExpression,
        op: AssignmentOperator,
        rhs: &Expression,
        line: u32,
    ) -> Result<(), String> {
        // Logical compound semantics:
        //   &&= : assign rhs if oldval is truthy   (skip when falsy)
        //   ||= : assign rhs if oldval is falsy    (skip when truthy)
        //   ??= : assign rhs if oldval is nullish  (skip when NOT nullish)
        // Returns Some((skip_op, sense)) where sense=true means the jump skips
        // assignment, sense=false means the jump targets the assignment branch.
        let logical = match op {
            AssignmentOperator::AndAssign => Some((OpCode::JumpIfFalsePeek, true)),
            AssignmentOperator::OrAssign => Some((OpCode::JumpIfTruePeek, true)),
            AssignmentOperator::NullishAssign => Some((OpCode::JumpIfNullishPeek, false)),
            _ => None,
        };

        self.compile_expr(&m.object)?;

        match &m.property {
            MemberProperty::Identifier(name) => {
                let idx = self.make_string_constant(*name);
                if let Some((jump_op, jump_skips)) = logical {
                    self.chunk.emit_op(OpCode::Dup, line);            // [obj, obj]
                    self.emit_get_property(idx, line);                // [obj, oldval]
                    self.emit_logical_member_assign_inline(jump_op, jump_skips, idx, rhs, line)?;
                } else if op != AssignmentOperator::Assign {
                    self.chunk.emit_op(OpCode::Dup, line);
                    self.emit_get_property(idx, line);
                    self.compile_expr(rhs)?;
                    self.emit_compound_arith(op, line)?;
                    self.emit_set_property(idx, line);
                } else {
                    self.compile_expr(rhs)?;
                    self.emit_set_property(idx, line);
                }
            }
            MemberProperty::Expression(key) => {
                self.compile_expr(key)?;
                if let Some((jump_op, jump_skips)) = logical {
                    // Stack: [obj, key]
                    self.chunk.emit_op(OpCode::Dup2, line);           // [obj, key, obj, key]
                    self.chunk.emit_op(OpCode::GetElement, line);     // [obj, key, oldval]
                    self.emit_logical_elem_assign(jump_op, jump_skips, rhs, line)?;
                } else if op != AssignmentOperator::Assign {
                    self.chunk.emit_op(OpCode::Dup2, line);
                    self.chunk.emit_op(OpCode::GetElement, line);
                    self.compile_expr(rhs)?;
                    self.emit_compound_arith(op, line)?;
                    self.chunk.emit_op(OpCode::SetElement, line);
                } else {
                    self.compile_expr(rhs)?;
                    self.chunk.emit_op(OpCode::SetElement, line);
                }
            }
            MemberProperty::PrivateIdentifier(name) => {
                let idx = self.make_string_constant(*name);
                if let Some((jump_op, jump_skips)) = logical {
                    self.chunk.emit_op(OpCode::Dup, line);                   // [obj, obj]
                    self.chunk.emit_op_u16(OpCode::GetPrivate, idx, line);   // [obj, oldval]
                    self.emit_logical_priv_assign(jump_op, jump_skips, idx, rhs, line)?;
                } else if op != AssignmentOperator::Assign {
                    self.chunk.emit_op(OpCode::Dup, line);
                    self.chunk.emit_op_u16(OpCode::GetPrivate, idx, line);
                    self.compile_expr(rhs)?;
                    self.emit_compound_arith(op, line)?;
                    self.chunk.emit_op_u16(OpCode::SetPrivate, idx, line);
                } else {
                    self.compile_expr(rhs)?;
                    self.chunk.emit_op_u16(OpCode::SetPrivate, idx, line);
                }
            }
        }
        Ok(())
    }

    /// Logical-compound assignment for `obj.name` (entry point shared by all 3 ops).
    /// On entry stack: [obj, oldval]. On exit: [result].
    /// `jump_skips` chooses the conditional-jump direction:
    ///   true  → jump to the SKIP branch (&&=/||=)
    ///   false → jump to the ASSIGN branch (??=)
    fn emit_logical_member_assign_inline(
        &mut self,
        jump_op: OpCode,
        jump_skips: bool,
        idx: u16,
        rhs: &Expression,
        line: u32,
    ) -> Result<(), String> {
        if jump_skips {
            // Jump skips assignment: AND/OR pattern.
            let skip = self.chunk.emit_jump(jump_op, line);
            self.chunk.emit_op(OpCode::Pop, line);             // [obj]
            self.compile_expr(rhs)?;                           // [obj, rhs]
            self.emit_set_property(idx, line);                 // [rhs]
            let end = self.chunk.emit_jump(OpCode::Jump, line);
            self.chunk.patch_jump(skip);
            // Short-circuit: [obj, oldval] → [oldval]
            self.chunk.emit_op(OpCode::Swap, line);
            self.chunk.emit_op(OpCode::Pop, line);
            self.chunk.patch_jump(end);
        } else {
            // Jump goes to ASSIGN branch: ??= pattern (jump when nullish → assign).
            let to_assign = self.chunk.emit_jump(jump_op, line);
            // Fall-through (not-nullish): keep oldval, drop obj.
            self.chunk.emit_op(OpCode::Swap, line);            // [oldval, obj]
            self.chunk.emit_op(OpCode::Pop, line);             // [oldval]
            let end = self.chunk.emit_jump(OpCode::Jump, line);
            self.chunk.patch_jump(to_assign);
            self.chunk.emit_op(OpCode::Pop, line);             // [obj]
            self.compile_expr(rhs)?;                           // [obj, rhs]
            self.emit_set_property(idx, line);                 // [rhs]
            self.chunk.patch_jump(end);
        }
        Ok(())
    }

    /// Same shape as `emit_logical_member_assign_inline`, for `obj[key]`.
    /// On entry stack: [obj, key, oldval]. On exit: [result].
    fn emit_logical_elem_assign(
        &mut self,
        jump_op: OpCode,
        jump_skips: bool,
        rhs: &Expression,
        line: u32,
    ) -> Result<(), String> {
        if jump_skips {
            let skip = self.chunk.emit_jump(jump_op, line);
            self.chunk.emit_op(OpCode::Pop, line);             // [obj, key]
            self.compile_expr(rhs)?;                           // [obj, key, rhs]
            self.chunk.emit_op(OpCode::SetElement, line);      // [rhs]
            let end = self.chunk.emit_jump(OpCode::Jump, line);
            self.chunk.patch_jump(skip);
            // Short-circuit: [obj, key, oldval] → [oldval]
            self.chunk.emit_op(OpCode::Swap, line);            // [obj, oldval, key]
            self.chunk.emit_op(OpCode::Pop, line);             // [obj, oldval]
            self.chunk.emit_op(OpCode::Swap, line);            // [oldval, obj]
            self.chunk.emit_op(OpCode::Pop, line);             // [oldval]
            self.chunk.patch_jump(end);
        } else {
            let to_assign = self.chunk.emit_jump(jump_op, line);
            // Fall-through (not-nullish): cleanup to oldval.
            self.chunk.emit_op(OpCode::Swap, line);
            self.chunk.emit_op(OpCode::Pop, line);
            self.chunk.emit_op(OpCode::Swap, line);
            self.chunk.emit_op(OpCode::Pop, line);
            let end = self.chunk.emit_jump(OpCode::Jump, line);
            self.chunk.patch_jump(to_assign);
            self.chunk.emit_op(OpCode::Pop, line);             // [obj, key]
            self.compile_expr(rhs)?;                           // [obj, key, rhs]
            self.chunk.emit_op(OpCode::SetElement, line);      // [rhs]
            self.chunk.patch_jump(end);
        }
        Ok(())
    }

    fn emit_logical_priv_assign(
        &mut self,
        jump_op: OpCode,
        jump_skips: bool,
        idx: u16,
        rhs: &Expression,
        line: u32,
    ) -> Result<(), String> {
        if jump_skips {
            let skip = self.chunk.emit_jump(jump_op, line);
            self.chunk.emit_op(OpCode::Pop, line);
            self.compile_expr(rhs)?;
            self.chunk.emit_op_u16(OpCode::SetPrivate, idx, line);
            let end = self.chunk.emit_jump(OpCode::Jump, line);
            self.chunk.patch_jump(skip);
            self.chunk.emit_op(OpCode::Swap, line);
            self.chunk.emit_op(OpCode::Pop, line);
            self.chunk.patch_jump(end);
        } else {
            let to_assign = self.chunk.emit_jump(jump_op, line);
            self.chunk.emit_op(OpCode::Swap, line);
            self.chunk.emit_op(OpCode::Pop, line);
            let end = self.chunk.emit_jump(OpCode::Jump, line);
            self.chunk.patch_jump(to_assign);
            self.chunk.emit_op(OpCode::Pop, line);
            self.compile_expr(rhs)?;
            self.chunk.emit_op_u16(OpCode::SetPrivate, idx, line);
            self.chunk.patch_jump(end);
        }
        Ok(())
    }

    fn emit_compound_arith(&mut self, op: AssignmentOperator, line: u32) -> Result<(), String> {
        let bytecode_op = match op {
            AssignmentOperator::AddAssign => OpCode::Add,
            AssignmentOperator::SubAssign => OpCode::Sub,
            AssignmentOperator::MulAssign => OpCode::Mul,
            AssignmentOperator::DivAssign => OpCode::Div,
            AssignmentOperator::RemAssign => OpCode::Rem,
            AssignmentOperator::ExpAssign => OpCode::Exp,
            AssignmentOperator::BitAndAssign => OpCode::BitAnd,
            AssignmentOperator::BitOrAssign => OpCode::BitOr,
            AssignmentOperator::BitXorAssign => OpCode::BitXor,
            AssignmentOperator::ShlAssign => OpCode::Shl,
            AssignmentOperator::ShrAssign => OpCode::Shr,
            AssignmentOperator::UShrAssign => OpCode::UShr,
            _ => return Err(format!("unexpected compound assignment operator at {line}")),
        };
        self.chunk.emit_op(bytecode_op, line);
        Ok(())
    }

    // ---- sequence ----

    fn compile_sequence(&mut self, s: &SequenceExpression) -> Result<(), String> {
        for (i, expr) in s.expressions.iter().enumerate() {
            self.compile_expr(expr)?;
            if i < s.expressions.len() - 1 {
                self.chunk.emit_op(OpCode::Pop, self.current_line());
            }
        }
        Ok(())
    }

    // ---- member access ----

    fn compile_member(&mut self, m: &MemberExpression) -> Result<(), String> {
        let line = m.span.start;
        // Special case: super.method — push the parent class object
        if matches!(&m.object, Expression::Super(_)) {
            // Emit __super_class__ access (this.__class__.__super__)
            self.chunk.emit_op(OpCode::GetSuperClass, line);
            match &m.property {
                MemberProperty::Identifier(name) => {
                    let idx = self.make_string_constant(*name);
                    self.emit_get_property(idx, line);
                }
                MemberProperty::Expression(key) => {
                    self.compile_expr(key)?;
                    self.chunk.emit_op(OpCode::GetElement, line);
                }
                _ => {}
            }
            // Mark next call to propagate this
            return Ok(());
        }
        self.compile_expr(&m.object)?;
        match &m.property {
            MemberProperty::Identifier(name) => {
                let idx = self.make_string_constant(*name);
                self.emit_get_property(idx, line);
            }
            MemberProperty::Expression(key) => {
                self.compile_expr(key)?;
                self.chunk.emit_op(OpCode::GetElement, line);
            }
            MemberProperty::PrivateIdentifier(name) => {
                let idx = self.make_string_constant(*name);
                self.chunk.emit_op_u16(OpCode::GetPrivate, idx, line);
            }
        }
        Ok(())
    }

    // ---- call ----

    fn compile_call(&mut self, c: &CallExpression) -> Result<(), String> {
        let line = c.span.start;
        let argc = c.arguments.len() as u8;

        // Method call: obj.method(args) -> CallMethod
        // Stack layout for CallMethod: [obj, arg0, arg1, ..., argN]
        if let Expression::Member(m) = &c.callee {
            // Special case: super.method() — get method from super class but call with original this
            if matches!(&m.object, Expression::Super(_))
                && let MemberProperty::Identifier(name) = &m.property {
                    // Get the method from super class
                    self.chunk.emit_op(OpCode::GetSuperClass, line);
                    let idx = self.make_string_constant(*name);
                    self.emit_get_property(idx, line);
                    // Push args
                    for arg in &c.arguments {
                        self.compile_expr(arg)?;
                    }
                    // Use Call (not CallMethod) — pending_super_call will use original this
                    self.chunk.emit_op_u8(OpCode::Call, argc, line);
                    return Ok(());
                }
            // Computed member call obj[expr](args). Fast path for string-literal key
            // (`obj["method"](args)`): emit identical bytecode to `obj.method(args)` so
            // the receiver is bound and primitive method dispatch works (false["toString"]() etc).
            if let MemberProperty::Expression(key_expr) = &m.property {
                if let Expression::StringLiteral(s) = key_expr {
                    self.compile_expr(&m.object)?;
                    for arg in &c.arguments {
                        self.compile_expr(arg)?;
                    }
                    let idx = self.make_string_constant(s.value);
                    self.chunk.emit_byte(OpCode::CallMethod as u8, line);
                    self.chunk.emit_byte(argc, line);
                    self.chunk.code.push((idx >> 8) as u8);
                    self.chunk.code.push((idx & 0xFF) as u8);
                    return Ok(());
                }
                // Dynamic key: emit `obj[key].call(obj, ...args)` to preserve `this`.
                // Layout: [obj, obj, key] → GetElement → [obj, fn] → Swap → [fn, obj]
                //         → push args → CallMethod "call" with argc+1.
                if (argc as usize) < 255 {
                    self.compile_expr(&m.object)?;
                    self.chunk.emit_op(OpCode::Dup, line);
                    self.compile_expr(key_expr)?;
                    self.chunk.emit_op(OpCode::GetElement, line);
                    self.chunk.emit_op(OpCode::Swap, line);
                    for arg in &c.arguments {
                        self.compile_expr(arg)?;
                    }
                    let call_name = self.interner.intern("call");
                    let idx = self.make_string_constant(call_name);
                    self.chunk.emit_byte(OpCode::CallMethod as u8, line);
                    self.chunk.emit_byte(argc + 1, line);
                    self.chunk.code.push((idx >> 8) as u8);
                    self.chunk.code.push((idx & 0xFF) as u8);
                    return Ok(());
                }
                // Fallback for argc==255.
                self.compile_expr(&m.object)?;
                self.compile_expr(key_expr)?;
                self.chunk.emit_op(OpCode::GetElement, line);
                for arg in &c.arguments {
                    self.compile_expr(arg)?;
                }
                self.chunk.emit_op_u8(OpCode::Call, argc, line);
                return Ok(());
            }

            // Method call with spread args: emit `obj.method.apply(obj, [args...])`
            // since CallMethod has a fixed argc and can't directly handle spreads.
            let method_has_spread = c.arguments.iter().any(|a| matches!(a, Expression::Spread(_)));
            if method_has_spread
                && let MemberProperty::Identifier(name) = &m.property
            {
                self.compile_expr(&m.object)?;        // [obj]
                self.chunk.emit_op(OpCode::Dup, line); // [obj, obj]
                let name_idx = self.make_string_constant(*name);
                self.emit_get_property(name_idx, line); // [obj, fn]
                self.chunk.emit_op(OpCode::Swap, line);  // [fn, obj]
                // Build args array with spreads expanded.
                self.chunk.emit_op_u16(OpCode::CreateArray, 0, line);
                let mut idx: u32 = 0;
                for arg in &c.arguments {
                    if let Expression::Spread(sp) = arg {
                        self.compile_expr(&sp.argument)?;
                        self.chunk.emit_op(OpCode::ArraySpread, line);
                        idx = u32::MAX;
                    } else {
                        self.compile_expr(arg)?;
                        self.chunk.emit_op_u32(OpCode::SetArrayItem, idx, line);
                        if idx != u32::MAX { idx = idx.saturating_add(1); }
                    }
                }
                // Stack: [fn, obj, args_array] — invoke fn.apply(obj, args_array).
                let apply_name = self.interner.intern("apply");
                let apply_idx = self.make_string_constant(apply_name);
                self.chunk.emit_byte(OpCode::CallMethod as u8, line);
                self.chunk.emit_byte(2, line);
                self.chunk.code.push((apply_idx >> 8) as u8);
                self.chunk.code.push((apply_idx & 0xFF) as u8);
                return Ok(());
            }

            self.compile_expr(&m.object)?; // push obj
            for arg in &c.arguments {
                self.compile_expr(arg)?;
            }
            // Encode the method name in the constant pool
            match &m.property {
                MemberProperty::Identifier(name) => {
                    let idx = self.make_string_constant(*name);
                    // Emit method name index as a u16 right after CallMethod
                    self.chunk.emit_byte(OpCode::CallMethod as u8, line);
                    self.chunk.emit_byte(argc, line);
                    self.chunk.code.push((idx >> 8) as u8);
                    self.chunk.code.push((idx & 0xFF) as u8);
                }
                MemberProperty::PrivateIdentifier(name) => {
                    let idx = self.make_string_constant(*name);
                    self.chunk.emit_byte(OpCode::CallMethod as u8, line);
                    self.chunk.emit_byte(argc, line);
                    self.chunk.code.push((idx >> 8) as u8);
                    self.chunk.code.push((idx & 0xFF) as u8);
                }
                _ => {
                    // Other computed forms (shouldn't normally reach here)
                    self.chunk.emit_byte(OpCode::CallMethod as u8, line);
                    self.chunk.emit_byte(argc, line);
                    self.chunk.code.push(0);
                    self.chunk.code.push(0);
                }
            }
            return Ok(());
        }

        // super(args) — call parent constructor
        if matches!(&c.callee, Expression::Super(_)) {
            self.chunk.emit_op(OpCode::GetSuperConstructor, line);
            for arg in &c.arguments {
                self.compile_expr(arg)?;
            }
            self.chunk.emit_op_u8(OpCode::Call, argc, line);
            return Ok(());
        }

        // Regular call.
        self.compile_expr(&c.callee)?;
        // Check if any argument is a spread
        let has_spread = c.arguments.iter().any(|a| matches!(a, Expression::Spread(_)));
        if has_spread {
            // Build a flat args array (with spreads expanded), then SpreadCall
            self.chunk.emit_op_u16(OpCode::CreateArray, 0, line);
            let mut idx: u32 = 0;
            for arg in &c.arguments {
                if let Expression::Spread(sp) = arg {
                    self.compile_expr(&sp.argument)?;
                    self.chunk.emit_op(OpCode::ArraySpread, line);
                    idx = u32::MAX; // length unknown after spread
                } else {
                    self.compile_expr(arg)?;
                    self.chunk.emit_op_u32(OpCode::SetArrayItem, idx, line);
                    if idx != u32::MAX { idx = idx.saturating_add(1); }
                }
            }
            // Stack: [func, args_array]; SpreadCall pops both, pushes result
            self.chunk.emit_op_u8(OpCode::SpreadCall, 0, line);
            return Ok(());
        }

        for arg in &c.arguments {
            self.compile_expr(arg)?;
        }
        self.chunk.emit_op_u8(OpCode::Call, argc, line);
        Ok(())
    }

    // ---- new ----

    fn compile_new(&mut self, n: &NewExpression) -> Result<(), String> {
        let line = n.span.start;
        let has_spread = n.arguments.iter().any(|a| matches!(a, Expression::Spread(_)));
        if has_spread {
            // Build args array, then SpreadConstruct
            self.compile_expr(&n.callee)?;
            self.chunk.emit_op_u16(OpCode::CreateArray, 0, line);
            let mut idx: u32 = 0;
            for arg in &n.arguments {
                if let Expression::Spread(sp) = arg {
                    self.compile_expr(&sp.argument)?;
                    self.chunk.emit_op(OpCode::ArraySpread, line);
                    idx = u32::MAX;
                } else {
                    self.compile_expr(arg)?;
                    self.chunk.emit_op_u32(OpCode::SetArrayItem, idx, line);
                    if idx != u32::MAX { idx = idx.saturating_add(1); }
                }
            }
            self.chunk.emit_op_u8(OpCode::SpreadConstruct, 0, line);
            return Ok(());
        }
        self.compile_expr(&n.callee)?;
        for arg in &n.arguments {
            self.compile_expr(arg)?;
        }
        self.chunk
            .emit_op_u8(OpCode::Construct, n.arguments.len() as u8, line);
        Ok(())
    }

    // ---- array ----

    fn compile_array(&mut self, a: &ArrayExpression) -> Result<(), String> {
        let line = a.span.start;
        self.chunk
            .emit_op_u16(OpCode::CreateArray, a.elements.len() as u16, line);
        for (i, elem) in a.elements.iter().enumerate() {
            if let Some(e) = elem {
                if let Expression::Spread(sp) = e {
                    self.compile_expr(&sp.argument)?;
                    self.chunk.emit_op(OpCode::ArraySpread, line);
                } else {
                    self.compile_expr(e)?;
                    self.chunk
                        .emit_op_u32(OpCode::SetArrayItem, i as u32, line);
                }
            } else {
                // Hole element (e.g. [,]): emit undefined to preserve array length
                self.chunk.emit_op(OpCode::Undefined, line);
                self.chunk.emit_op_u32(OpCode::SetArrayItem, i as u32, line);
            }
        }
        Ok(())
    }

    // ---- object ----

    fn compile_object(&mut self, o: &ObjectExpression) -> Result<(), String> {
        let line = o.span.start;
        self.chunk.emit_op(OpCode::CreateObject, line);
        for prop in &o.properties {
            match prop {
                ObjectProperty::Property(p) => self.compile_object_property(p, line)?,
                ObjectProperty::SpreadElement(s) => {
                    self.compile_expr(&s.argument)?;
                    self.chunk.emit_op(OpCode::ObjectSpread, line);
                }
            }
        }
        Ok(())
    }

    fn compile_object_property(&mut self, p: &Property, line: u32) -> Result<(), String> {
        // `{__proto__: val}` in an object literal sets the prototype rather than
        // defining a property — but only for the explicit form
        // (PropertyName : AssignmentExpression). Shorthand, computed, and
        // method forms are treated as ordinary properties per Annex B.3.1.
        if matches!(p.kind, PropertyKindVal::Init) && !p.method && !p.shorthand && !p.computed {
            let is_proto = match &p.key {
                PropertyKey::Identifier(id) | PropertyKey::StringLiteral(id) => {
                    self.interner.resolve(*id) == "__proto__"
                }
                _ => false,
            };
            if is_proto {
                self.compile_expr(&p.value)?;
                self.chunk.emit_op(OpCode::SetObjectProto, line);
                return Ok(());
            }
        }

        self.compile_property_key(&p.key, line)?;

        match p.kind {
            PropertyKindVal::Init => {
                if Self::is_anonymous_fn_def(&p.value) {
                    let key_name = self.property_key_name(&p.key);
                    if key_name != StringId(0) {
                        self.pending_function_name = Some(key_name);
                    }
                }
                self.compile_expr(&p.value)?;
                self.chunk.emit_op(OpCode::DefineDataProp, line);
            }
            PropertyKindVal::Get => {
                self.compile_expr(&p.value)?;
                self.chunk.emit_op(OpCode::DefineGetter, line);
            }
            PropertyKindVal::Set => {
                self.compile_expr(&p.value)?;
                self.chunk.emit_op(OpCode::DefineSetter, line);
            }
        }
        Ok(())
    }

    fn compile_property_key(&mut self, key: &PropertyKey, line: u32) -> Result<(), String> {
        match key {
            PropertyKey::Identifier(id) | PropertyKey::StringLiteral(id) | PropertyKey::Private(id) => {
                self.emit_constant(Value::string(*id), line);
            }
            PropertyKey::NumberLiteral(n) => {
                self.emit_constant(Value::number(*n), line);
            }
            PropertyKey::Computed(expr) => {
                self.compile_expr(expr)?;
            }
        }
        Ok(())
    }

    fn property_key_name(&mut self, key: &PropertyKey) -> StringId {
        match key {
            PropertyKey::Identifier(id)
            | PropertyKey::StringLiteral(id)
            | PropertyKey::Private(id) => *id,
            PropertyKey::NumberLiteral(n) => {
                // Convert numeric key to its canonical (JS-spec) string form so
                // e.g. 0.0000001 maps to "1e-7", not "0.0000001".
                let s = js_canonical_number_string(*n);
                self.interner.intern(&s)
            }
            PropertyKey::Computed(expr) => {
                // Detect well-known symbol access: Symbol.xxx
                if let Expression::Member(mem) = expr.as_ref()
                    && let Expression::Identifier(obj_id) = &mem.object
                    && self.interner.resolve(obj_id.name) == "Symbol"
                    && let MemberProperty::Identifier(prop_id) = &mem.property
                {
                    let prop_name = self.interner.resolve(*prop_id);
                    let sym_idx: u32 = match prop_name {
                        "iterator" => 0,
                        "hasInstance" => 1,
                        "toPrimitive" => 2,
                        "toStringTag" => 3,
                        "species" => 4,
                        "unscopables" => 5,
                        "asyncIterator" => 6,
                        "matchAll" => 7,
                        _ => return StringId(0),
                    };
                    return self.interner.intern(&format!("__sym_{sym_idx}__"));
                }
                // Constant string literal in brackets: ['name'] is equivalent to .name
                if let Expression::StringLiteral(lit) = expr.as_ref() {
                    return lit.value;
                }
                // Constant number literal in brackets: [1] becomes the canonical "1" key
                if let Expression::NumberLiteral(lit) = expr.as_ref() {
                    let n = lit.value;
                    let s = if n.fract() == 0.0 && n.is_finite() && n.abs() < 1e21 {
                        format!("{}", n as i64)
                    } else {
                        format!("{n}")
                    };
                    return self.interner.intern(&s);
                }
                StringId(0)
            }
        }
    }

    // ---- function expression ----

    fn compile_function_expr(&mut self, f: &FunctionExpression) -> Result<(), String> {
        let name = f.id
            .or_else(|| self.pending_function_name.take())
            .unwrap_or_else(|| self.interner.intern("<anonymous>"));
        // For *named* function expressions, the name binds to the function itself
        // inside the body (so `function f() { ... f() ... }` self-recurses).
        let self_binding = f.id;
        let child_chunk =
            self.compile_function_body_with_self(name, &f.params, &f.body, f.is_async, f.is_generator, self_binding)?;
        let chunk_idx = self.chunk.child_chunks.len() as u16;
        let uv_descs = child_chunk.upvalue_descriptors.clone();
        self.chunk.child_chunks.push(child_chunk);
        self.chunk
            .emit_op_u16(OpCode::Closure, chunk_idx, f.span.start);
        for desc in &uv_descs {
            let line = f.span.start;
            self.chunk.emit_byte(if desc.is_local { 1 } else { 0 }, line);
            self.chunk.emit_byte(desc.index, line);
        }
        Ok(())
    }

    fn is_anonymous_fn_def(expr: &Expression) -> bool {
        match expr {
            Expression::Function(f) => f.id.is_none(),
            Expression::ArrowFunction(_) => true,
            Expression::Class(c) => c.id.is_none(),
            _ => false,
        }
    }

    // ---- arrow function expression ----

    fn compile_arrow_expr(&mut self, a: &ArrowFunctionExpression) -> Result<(), String> {
        let name_hint = self.pending_function_name.take();
        let mut child_chunk = self.compile_arrow_body(&a.params, &a.body, a.is_async)?;
        if let Some(name) = name_hint {
            child_chunk.name = name;
        }
        let chunk_idx = self.chunk.child_chunks.len() as u16;
        let uv_descs = child_chunk.upvalue_descriptors.clone();
        self.chunk.child_chunks.push(child_chunk);
        self.chunk
            .emit_op_u16(OpCode::Closure, chunk_idx, a.span.start);
        for desc in &uv_descs {
            let line = a.span.start;
            self.chunk.emit_byte(if desc.is_local { 1 } else { 0 }, line);
            self.chunk.emit_byte(desc.index, line);
        }
        Ok(())
    }

    // ---- class expression ----

    fn compile_class_expr(&mut self, c: &ClassExpression) -> Result<(), String> {
        let line = c.span.start;
        let name = c.id.or_else(|| self.pending_function_name.take()).unwrap_or(StringId(0));
        let name_idx = self.make_string_constant(name);
        self.chunk.emit_op_u16(OpCode::Class, name_idx, line);

        if let Some(super_class) = &c.super_class {
            self.compile_expr(super_class)?;
            self.chunk.emit_op(OpCode::Inherit, line);
        }

        self.compile_class_body(&c.body, line)?;
        Ok(())
    }

    // ---- template literal ----

    fn compile_template_literal(&mut self, t: &TemplateLiteral) -> Result<(), String> {
        let line = t.span.start;
        let mut parts = 0u32;

        for (i, quasi) in t.quasis.iter().enumerate() {
            let str_id = quasi.cooked.unwrap_or(quasi.raw);
            let text = self.interner.resolve(str_id);
            let is_empty = text.is_empty();

            if !is_empty {
                self.emit_constant(Value::string(str_id), line);
                if parts > 0 {
                    self.chunk.emit_op(OpCode::Add, line);
                }
                parts += 1;
            }

            if i < t.expressions.len() {
                self.compile_expr(&t.expressions[i])?;
                if parts > 0 {
                    self.chunk.emit_op(OpCode::Add, line);
                }
                parts += 1;
            }
        }

        if parts == 0 {
            let empty = self.interner.intern("");
            self.emit_constant(Value::string(empty), line);
        }

        Ok(())
    }

    // ---- tagged template ----

    fn compile_tagged_template(&mut self, t: &TaggedTemplateExpression) -> Result<(), String> {
        let line = t.span.start;
        self.compile_expr(&t.tag)?;
        let total = (t.quasi.quasis.len() + t.quasi.expressions.len()) as u8;
        for q in &t.quasi.quasis {
            let str_id = q.cooked.unwrap_or(q.raw);
            self.emit_constant(Value::string(str_id), line);
        }
        for e in &t.quasi.expressions {
            self.compile_expr(e)?;
        }
        self.chunk.emit_op_u8(OpCode::TemplateTag, total, line);
        Ok(())
    }

    // ---- optional chaining ----

    fn compile_optional_chain(&mut self, o: &OptionalChainExpression) -> Result<(), String> {
        let line = o.span.start;
        self.compile_expr(&o.base)?;

        // Collect skip jumps from any `?.` along the chain; they all must jump past the
        // entire remaining chain so a short-circuit on `a?.b.c.d` skips `.c.d` too.
        let mut skips: Vec<usize> = Vec::new();

        let mut i = 0;
        while i < o.chain.len() {
            let element = &o.chain[i];
            match element {
                OptionalChainElement::Member {
                    property, optional, ..
                } => {
                    if *optional {
                        skips.push(self.chunk.emit_jump(OpCode::OptionalChain, line));
                    }
                    // Look ahead: if the next element is a non-optional Call, combine into
                    // CallMethod so that `this` is bound to the receiver (e.g. a?.b().c).
                    let next_is_call = matches!(
                        o.chain.get(i + 1),
                        Some(OptionalChainElement::Call { optional: false, .. })
                    );
                    if next_is_call
                        && let MemberProperty::Identifier(id) = property
                        && let Some(OptionalChainElement::Call { arguments, .. }) = o.chain.get(i + 1)
                    {
                        // Stack already has the receiver; emit args then CallMethod.
                        for arg in arguments {
                            self.compile_expr(arg)?;
                        }
                        let idx = self.make_string_constant(*id);
                        self.chunk.emit_byte(OpCode::CallMethod as u8, line);
                        self.chunk.emit_byte(arguments.len() as u8, line);
                        self.chunk.code.push((idx >> 8) as u8);
                        self.chunk.code.push((idx & 0xFF) as u8);
                        i += 2;
                        continue;
                    }
                    match property {
                        MemberProperty::Identifier(id) => {
                            let idx = self.make_string_constant(*id);
                            self.emit_get_property(idx, line);
                        }
                        MemberProperty::Expression(e) => {
                            self.compile_expr(e)?;
                            self.chunk.emit_op(OpCode::GetElement, line);
                        }
                        MemberProperty::PrivateIdentifier(id) => {
                            let idx = self.make_string_constant(*id);
                            self.chunk.emit_op_u16(OpCode::GetPrivate, idx, line);
                        }
                    }
                }
                OptionalChainElement::Call {
                    arguments, optional,
                } => {
                    if *optional {
                        skips.push(self.chunk.emit_jump(OpCode::OptionalChain, line));
                    }
                    for arg in arguments {
                        self.compile_expr(arg)?;
                    }
                    self.chunk
                        .emit_op_u8(OpCode::Call, arguments.len() as u8, line);
                }
            }
            i += 1;
        }
        for s in skips {
            self.chunk.patch_jump(s);
        }
        Ok(())
    }

    // ---- yield ----

    fn compile_yield(&mut self, y: &YieldExpression) -> Result<(), String> {
        let line = y.span.start;
        if y.delegate {
            // yield * <expr>:
            //   let it = GetIterator(<expr>);
            //   loop:
            //     let r = it.next();
            //     if r.done: break with r.value as the result of the yield* expression
            //     else: yield r.value; continue
            let arg = y.argument.as_ref().ok_or_else(|| "yield* requires an argument".to_string())?;
            self.compile_expr(arg)?;
            self.chunk.emit_op(OpCode::GetIterator, line);
            // Stack: [iter]
            let loop_start = self.chunk.len();
            // Dup iter, IteratorNext leaves [iter, result]
            self.chunk.emit_op(OpCode::Dup, line);
            self.chunk.emit_op(OpCode::IteratorNext, line);
            // Dup result, IteratorDone leaves [iter, result, done]
            self.chunk.emit_op(OpCode::Dup, line);
            self.chunk.emit_op(OpCode::IteratorDone, line);
            let exit_jump = self.chunk.emit_jump(OpCode::JumpIfTrue, line);
            // Not done: result is on top; extract value, yield it, then pop yielded value
            self.chunk.emit_op(OpCode::IteratorValue, line);
            // Stack: [iter, value]
            self.chunk.emit_op(OpCode::Yield, line);
            // After yield, the yielded value's "sent" replacement is on top; we discard it
            // (we don't pass values back into delegated iterators in this minimal impl).
            self.chunk.emit_op(OpCode::Pop, line);
            self.chunk.emit_loop(loop_start, line);
            // Done branch: stack is [iter, result, true]; we need [value-from-result] only
            self.chunk.patch_jump(exit_jump);
            self.chunk.emit_op(OpCode::Pop, line);            // pop done flag
            self.chunk.emit_op(OpCode::IteratorValue, line);   // result -> value
            // Stack: [iter, value]; remove iter from below
            self.chunk.emit_op(OpCode::Swap, line);
            self.chunk.emit_op(OpCode::Pop, line);
            return Ok(());
        }
        if let Some(arg) = &y.argument {
            self.compile_expr(arg)?;
        } else {
            self.chunk.emit_op(OpCode::Undefined, line);
        }
        self.chunk.emit_op(OpCode::Yield, line);
        Ok(())
    }

    // ---- await ----

    fn compile_await(&mut self, a: &AwaitExpression) -> Result<(), String> {
        self.compile_expr(&a.argument)?;
        self.chunk.emit_op(OpCode::Await, a.span.start);
        Ok(())
    }

    // ---- regexp ----

    fn compile_regexp(&mut self, r: &RegExpLiteral) -> Result<(), String> {
        let line = r.span.start;
        let pat_idx = self.make_string_constant(r.pattern);
        let flags_idx = self.make_string_constant(r.flags);
        self.chunk.emit_byte(OpCode::CreateRegExp as u8, line);
        self.chunk.code.push((pat_idx >> 8) as u8);
        self.chunk.code.push((pat_idx & 0xFF) as u8);
        self.chunk.code.push((flags_idx >> 8) as u8);
        self.chunk.code.push((flags_idx & 0xFF) as u8);
        Ok(())
    }

    // ---- meta property ----

    fn compile_meta_property(&mut self, m: &MetaProperty) -> Result<(), String> {
        let line = m.span.start;
        let meta = self.interner.resolve(m.meta);
        let prop = self.interner.resolve(m.property);
        if meta == "new" && prop == "target" {
            self.chunk.emit_op(OpCode::NewTarget, line);
        } else if meta == "import" && prop == "meta" {
            self.chunk.emit_op(OpCode::ImportMeta, line);
        } else {
            self.chunk.emit_op(OpCode::Undefined, line);
        }
        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::disassemble::disassemble;
    use crate::lexer::lexer::Lexer;
    use crate::parser::parser::Parser;

    fn compile(source: &str) -> (Chunk, Interner) {
        let mut interner = Interner::new();
        let tokens = {
            let mut lexer = Lexer::new(source, &mut interner);
            lexer.tokenize()
        };
        let program = {
            let mut parser = Parser::new(tokens, source, &mut interner);
            parser.parse_program().expect("parse error")
        };
        let chunk = {
            let compiler = Compiler::new(&mut interner);
            compiler.compile_program(&program).expect("compile error")
        };
        (chunk, interner)
    }

    #[test]
    fn test_compile_number() {
        let (chunk, interner) = compile("42;");
        let dis = disassemble(&chunk, &interner);
        assert!(dis.contains("Const"));
        assert!(dis.contains("Halt"));
    }

    #[test]
    fn test_compile_addition() {
        let (chunk, interner) = compile("1 + 2;");
        let dis = disassemble(&chunk, &interner);
        assert!(dis.contains("One"));
        assert!(dis.contains("Add"));
    }

    #[test]
    fn test_compile_variable() {
        let (chunk, interner) = compile("var x = 10;");
        let dis = disassemble(&chunk, &interner);
        assert!(dis.contains("DefineGlobal"));
    }

    #[test]
    fn test_compile_if() {
        let (chunk, interner) = compile("if (true) { 1; } else { 2; }");
        let dis = disassemble(&chunk, &interner);
        assert!(dis.contains("JumpIfFalse"));
        assert!(dis.contains("Jump"));
    }

    #[test]
    fn test_compile_while() {
        let (chunk, interner) = compile("while (true) { 1; }");
        let dis = disassemble(&chunk, &interner);
        assert!(dis.contains("Loop"));
    }

    #[test]
    fn test_compile_function() {
        let (chunk, interner) = compile("function foo(x) { return x + 1; }");
        let dis = disassemble(&chunk, &interner);
        assert!(dis.contains("Closure"));
        assert!(dis.contains("DefineGlobal"));
        assert!(dis.contains("Return"));
    }

    #[test]
    fn test_compile_boolean_true() {
        let (chunk, _) = compile("true;");
        assert_eq!(chunk.code[0], OpCode::True as u8);
    }

    #[test]
    fn test_compile_boolean_false() {
        let (chunk, _) = compile("false;");
        assert_eq!(chunk.code[0], OpCode::False as u8);
    }

    #[test]
    fn test_compile_null() {
        let (chunk, _) = compile("null;");
        assert_eq!(chunk.code[0], OpCode::Null as u8);
    }

    #[test]
    fn test_compile_zero_one() {
        let (chunk, _) = compile("0;");
        assert_eq!(chunk.code[0], OpCode::Zero as u8);
        let (chunk, _) = compile("1;");
        assert_eq!(chunk.code[0], OpCode::One as u8);
    }

    #[test]
    fn test_compile_for_loop() {
        let (chunk, interner) = compile("for (var i = 0; i < 10; i++) { i; }");
        let dis = disassemble(&chunk, &interner);
        assert!(dis.contains("Loop"));
        assert!(dis.contains("JumpIfFalse"));
    }

    #[test]
    fn test_compile_logical_and() {
        let (chunk, interner) = compile("true && false;");
        let dis = disassemble(&chunk, &interner);
        assert!(dis.contains("JumpIfFalsePeek"));
    }

    #[test]
    fn test_compile_logical_or() {
        let (chunk, interner) = compile("false || true;");
        let dis = disassemble(&chunk, &interner);
        assert!(dis.contains("JumpIfTruePeek"));
    }

    #[test]
    fn test_compile_ternary() {
        let (chunk, interner) = compile("true ? 1 : 2;");
        let dis = disassemble(&chunk, &interner);
        assert!(dis.contains("JumpIfFalse"));
        assert!(dis.contains("Jump"));
    }

    #[test]
    fn test_compile_unary_neg() {
        let (chunk, interner) = compile("-1;");
        let dis = disassemble(&chunk, &interner);
        assert!(dis.contains("Neg"));
    }

    #[test]
    fn test_compile_typeof_global() {
        let (chunk, interner) = compile("typeof x;");
        let dis = disassemble(&chunk, &interner);
        assert!(dis.contains("TypeOfGlobal"));
    }

    #[test]
    fn test_compile_throw() {
        let (chunk, interner) = compile("throw 42;");
        let dis = disassemble(&chunk, &interner);
        assert!(dis.contains("Throw"));
    }

    #[test]
    fn test_compile_return() {
        let (chunk, interner) = compile("function f() { return 1; }");
        let dis = disassemble(&chunk, &interner);
        assert!(dis.contains("Return"));
    }

    #[test]
    fn test_compile_new() {
        let (chunk, interner) = compile("new Foo(1, 2);");
        let dis = disassemble(&chunk, &interner);
        assert!(dis.contains("Construct"));
    }

    #[test]
    fn test_compile_array() {
        let (chunk, interner) = compile("[1, 2, 3];");
        let dis = disassemble(&chunk, &interner);
        assert!(dis.contains("CreateArray"));
        assert!(dis.contains("SetArrayItem"));
    }

    #[test]
    fn test_compile_object() {
        let (chunk, interner) = compile("({a: 1});");
        let dis = disassemble(&chunk, &interner);
        assert!(dis.contains("CreateObject"));
        assert!(dis.contains("DefineDataProp"));
    }

    #[test]
    fn test_compile_arrow() {
        let (chunk, interner) = compile("var f = (x) => x + 1;");
        let dis = disassemble(&chunk, &interner);
        assert!(dis.contains("Closure"));
    }

    #[test]
    fn test_compile_arrow_block() {
        let (chunk, interner) = compile("var f = (x) => { return x; };");
        let dis = disassemble(&chunk, &interner);
        assert!(dis.contains("Closure"));
        assert!(dis.contains("Return"));
    }

    #[test]
    fn test_compile_string() {
        let (chunk, interner) = compile("\"hello\";");
        let dis = disassemble(&chunk, &interner);
        assert!(dis.contains("Const"));
    }

    #[test]
    fn test_compile_break_continue() {
        let (chunk, interner) = compile("while (true) { break; }");
        let dis = disassemble(&chunk, &interner);
        assert!(dis.contains("Jump"));
        assert!(dis.contains("Loop"));
    }

    #[test]
    fn test_compile_do_while() {
        let (chunk, interner) = compile("do { 1; } while (true);");
        let dis = disassemble(&chunk, &interner);
        assert!(dis.contains("Loop"));
    }

    #[test]
    fn test_compile_member_access() {
        let (chunk, interner) = compile("a.b;");
        let dis = disassemble(&chunk, &interner);
        assert!(dis.contains("GetProperty"));
    }

    #[test]
    fn test_compile_computed_access() {
        let (chunk, interner) = compile("a[0];");
        let dis = disassemble(&chunk, &interner);
        assert!(dis.contains("GetElement"));
    }

    #[test]
    fn test_compile_assignment_add() {
        let (chunk, interner) = compile("var x = 0; x += 1;");
        let dis = disassemble(&chunk, &interner);
        assert!(dis.contains("Add"));
        assert!(dis.contains("SetGlobal"));
    }

    // with statement not yet implemented

    #[test]
    fn test_compile_debugger() {
        let (chunk, _) = compile("debugger;");
        assert!(chunk.code.contains(&(OpCode::Debugger as u8)));
    }

    #[test]
    fn test_compile_undefined_ident() {
        let (chunk, _) = compile("undefined;");
        assert_eq!(chunk.code[0], OpCode::Undefined as u8);
    }

    #[test]
    fn test_compile_delete_global() {
        let (chunk, interner) = compile("delete x;");
        let dis = disassemble(&chunk, &interner);
        assert!(dis.contains("DeleteGlobal"));
    }

    #[test]
    fn test_compile_method_call() {
        let (chunk, interner) = compile("a.b(1);");
        let dis = disassemble(&chunk, &interner);
        assert!(dis.contains("CallMethod"));
    }
}
