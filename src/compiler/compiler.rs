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
    /// Scope depth when the loop was entered so we know how many locals to pop.
    scope_depth: u32,
    /// Optional label for labeled statements.
    label: Option<StringId>,
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
        let len = program.body.len();
        for (i, stmt) in program.body.iter().enumerate() {
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
            // Check for const reassignment
            if self.locals[slot].is_const {
                let var_name = self.interner.resolve(name).to_owned();
                return Err(format!("TypeError: Assignment to constant variable '{var_name}'"));
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
            // Check for global const reassignment
            if self.const_globals.contains(&name) {
                let var_name = self.interner.resolve(name).to_owned();
                return Err(format!("TypeError: Assignment to constant variable '{var_name}'"));
            }
            let idx = self.make_string_constant(name);
            self.chunk.emit_op_u16(OpCode::SetGlobal, idx, line);
        }
        Ok(())
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
                        self.compile_expr(init)?;
                    } else {
                        self.chunk.emit_op(OpCode::Undefined, line);
                    }

                    if self.scope_depth == 0 {
                        if decl.kind == VarKind::Const {
                            self.const_globals.insert(name);
                        }
                        let idx = self.make_string_constant(name);
                        self.chunk.emit_op_u16(OpCode::DefineGlobal, idx, line);
                    } else if decl.kind == VarKind::Var {
                        // `var` hoists to function/global scope — look for existing binding
                        if let Some(slot) = self.resolve_local(name) {
                            if slot <= u8::MAX as usize {
                                self.chunk.emit_op_u8(OpCode::SetLocal, slot as u8, line);
                            } else {
                                self.chunk.emit_op_u16(OpCode::SetLocalWide, slot as u16, line);
                            }
                            self.chunk.emit_op(OpCode::Pop, line);
                        } else {
                            // Not found as local — must be a global (hoisted at program start)
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
                        // TDZ bookkeeping for let/const.
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
                        // In function scope: source object occupies an anonymous local slot
                        let anon = self.interner.intern("__destruct_src__");
                        self.add_local(anon);
                        self.mark_initialized();
                        let src_slot = (self.locals.len() - 1) as u8;
                        for prop in &obj_pat.properties {
                            if let ObjectPatternProperty::Property { key, value, .. } = prop {
                                let prop_name = match key {
                                    PropertyKey::Identifier(id) | PropertyKey::StringLiteral(id) => *id,
                                    _ => continue,
                                };
                                // Get the property from source object
                                self.chunk.emit_op_u8(OpCode::GetLocal, src_slot, line);
                                let idx = self.make_string_constant(prop_name);
                                self.chunk.emit_op_u16(OpCode::GetProperty, idx, line);
                                // Handle different value patterns
                                match value {
                                    Pattern::Identifier(id) => {
                                        self.add_local(id.name);
                                        self.mark_initialized();
                                    }
                                    Pattern::Assignment(a) => {
                                        if let Pattern::Identifier(id) = &a.left {
                                            // Check undefined → use default
                                            self.chunk.emit_op(OpCode::Dup, line);
                                            self.chunk.emit_op(OpCode::Undefined, line);
                                            self.chunk.emit_op(OpCode::StrictNe, line);
                                            let jump_idx = self.chunk.code.len();
                                            self.chunk.emit_op(OpCode::JumpIfTrue, line);
                                            self.chunk.code.push(0); self.chunk.code.push(0);
                                            self.chunk.emit_op(OpCode::Pop, line);
                                            self.compile_expr(&a.right)?;
                                            let target = self.chunk.code.len();
                                            let offset = (target as i16) - (jump_idx as i16) - 3;
                                            self.chunk.code[jump_idx + 1] = (offset >> 8) as u8;
                                            self.chunk.code[jump_idx + 2] = (offset & 0xFF) as u8;
                                            self.add_local(id.name);
                                            self.mark_initialized();
                                        }
                                    }
                                    Pattern::Object(inner_obj) => {
                                        // Nested {a: {b, c}} — the property value is on stack
                                        // Save to anon local, then destructure
                                        let anon_inner = self.interner.intern("__destruct_inner__");
                                        self.add_local(anon_inner);
                                        self.mark_initialized();
                                        let inner_slot = (self.locals.len() - 1) as u8;
                                        for inner_prop in &inner_obj.properties {
                                            if let ObjectPatternProperty::Property { key: ikey, value: ival, .. } = inner_prop {
                                                let iprop_name = match ikey {
                                                    PropertyKey::Identifier(id) | PropertyKey::StringLiteral(id) => *id,
                                                    _ => continue,
                                                };
                                                self.chunk.emit_op_u8(OpCode::GetLocal, inner_slot, line);
                                                let iidx = self.make_string_constant(iprop_name);
                                                self.chunk.emit_op_u16(OpCode::GetProperty, iidx, line);
                                                if let Pattern::Identifier(iid) = ival {
                                                    self.add_local(iid.name);
                                                    self.mark_initialized();
                                                } else {
                                                    self.chunk.emit_op(OpCode::Pop, line);
                                                }
                                            }
                                        }
                                    }
                                    Pattern::Array(inner_arr) => {
                                        // Nested {a: [x, y]} — array destructure the value
                                        let anon_inner = self.interner.intern("__destruct_inner__");
                                        self.add_local(anon_inner);
                                        self.mark_initialized();
                                        let inner_slot = (self.locals.len() - 1) as u8;
                                        for (i, elem) in inner_arr.elements.iter().enumerate() {
                                            if let Some(Pattern::Identifier(id)) = elem {
                                                self.chunk.emit_op_u8(OpCode::GetLocal, inner_slot, line);
                                                let idx_val = Value::int(i as i32);
                                                let cidx = self.chunk.add_constant(idx_val);
                                                self.chunk.emit_op_u16(OpCode::Const, cidx, line);
                                                self.chunk.emit_op(OpCode::GetElement, line);
                                                self.add_local(id.name);
                                                self.mark_initialized();
                                            }
                                        }
                                    }
                                    _ => { self.chunk.emit_op(OpCode::Pop, line); }
                                }
                            }
                        }
                    } else {
                        // Global scope
                        for prop in &obj_pat.properties {
                            if let ObjectPatternProperty::Property { key, value, .. } = prop {
                                let prop_name = match key {
                                    PropertyKey::Identifier(id) | PropertyKey::StringLiteral(id) => *id,
                                    _ => continue,
                                };
                                self.chunk.emit_op(OpCode::Dup, line);
                                let idx = self.make_string_constant(prop_name);
                                self.chunk.emit_op_u16(OpCode::GetProperty, idx, line);
                                match value {
                                    Pattern::Identifier(id) => {
                                        let vidx = self.make_string_constant(id.name);
                                        self.chunk.emit_op_u16(OpCode::DefineGlobal, vidx, line);
                                    }
                                    Pattern::Assignment(a) => {
                                        if let Pattern::Identifier(id) = &a.left {
                                            self.chunk.emit_op(OpCode::Dup, line);
                                            self.chunk.emit_op(OpCode::Undefined, line);
                                            self.chunk.emit_op(OpCode::StrictNe, line);
                                            let jump_idx = self.chunk.code.len();
                                            self.chunk.emit_op(OpCode::JumpIfTrue, line);
                                            self.chunk.code.push(0); self.chunk.code.push(0);
                                            self.chunk.emit_op(OpCode::Pop, line);
                                            self.compile_expr(&a.right)?;
                                            let target = self.chunk.code.len();
                                            let offset = (target as i16) - (jump_idx as i16) - 3;
                                            self.chunk.code[jump_idx + 1] = (offset >> 8) as u8;
                                            self.chunk.code[jump_idx + 2] = (offset & 0xFF) as u8;
                                            let vidx = self.make_string_constant(id.name);
                                            self.chunk.emit_op_u16(OpCode::DefineGlobal, vidx, line);
                                        }
                                    }
                                    Pattern::Object(inner_obj) => {
                                        // Global nested {a: {b}} — value on stack
                                        for inner_prop in &inner_obj.properties {
                                            if let ObjectPatternProperty::Property { key: ikey, value: ival, .. } = inner_prop {
                                                let iprop_name = match ikey {
                                                    PropertyKey::Identifier(id) | PropertyKey::StringLiteral(id) => *id,
                                                    _ => continue,
                                                };
                                                self.chunk.emit_op(OpCode::Dup, line);
                                                let iidx = self.make_string_constant(iprop_name);
                                                self.chunk.emit_op_u16(OpCode::GetProperty, iidx, line);
                                                if let Pattern::Identifier(iid) = ival {
                                                    let vidx = self.make_string_constant(iid.name);
                                                    self.chunk.emit_op_u16(OpCode::DefineGlobal, vidx, line);
                                                } else {
                                                    self.chunk.emit_op(OpCode::Pop, line);
                                                }
                                            }
                                        }
                                        self.chunk.emit_op(OpCode::Pop, line);
                                    }
                                    Pattern::Array(inner_arr) => {
                                        for (i, elem) in inner_arr.elements.iter().enumerate() {
                                            if let Some(Pattern::Identifier(id)) = elem {
                                                self.chunk.emit_op(OpCode::Dup, line);
                                                let idx_val = Value::int(i as i32);
                                                let cidx = self.chunk.add_constant(idx_val);
                                                self.chunk.emit_op_u16(OpCode::Const, cidx, line);
                                                self.chunk.emit_op(OpCode::GetElement, line);
                                                let vidx = self.make_string_constant(id.name);
                                                self.chunk.emit_op_u16(OpCode::DefineGlobal, vidx, line);
                                            }
                                        }
                                        self.chunk.emit_op(OpCode::Pop, line);
                                    }
                                    _ => { self.chunk.emit_op(OpCode::Pop, line); }
                                }
                            }
                        }
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
                    if self.scope_depth > 0 {
                        let anon = self.interner.intern("__destruct_src__");
                        self.add_local(anon);
                        self.mark_initialized();
                        let src_slot = (self.locals.len() - 1) as u8;
                        for (i, elem) in arr_pat.elements.iter().enumerate() {
                            match elem {
                                Some(Pattern::Identifier(id)) => {
                                    self.chunk.emit_op_u8(OpCode::GetLocal, src_slot, line);
                                    let idx_val = Value::int(i as i32);
                                    let cidx = self.chunk.add_constant(idx_val);
                                    self.chunk.emit_op_u16(OpCode::Const, cidx, line);
                                    self.chunk.emit_op(OpCode::GetElement, line);
                                    self.add_local(id.name);
                                    self.mark_initialized();
                                }
                                Some(Pattern::Rest(rest)) => {
                                    if let Pattern::Identifier(id) = &rest.argument {
                                        // Emit: source.slice(i)
                                        self.chunk.emit_op_u8(OpCode::GetLocal, src_slot, line);
                                        // Call slice method with start index
                                        let slice_name = self.interner.intern("slice");
                                        let slice_idx = self.make_string_constant(slice_name);
                                        let start_val = Value::int(i as i32);
                                        let start_idx = self.chunk.add_constant(start_val);
                                        self.chunk.emit_op_u16(OpCode::Const, start_idx, line);
                                        // Use CallMethod for array.slice(i)
                                        self.chunk.emit_byte(OpCode::CallMethod as u8, line);
                                        self.chunk.code.push(1); // 1 arg
                                        self.chunk.code.push((slice_idx >> 8) as u8);
                                        self.chunk.code.push((slice_idx & 0xFF) as u8);
                                        self.add_local(id.name);
                                        self.mark_initialized();
                                    }
                                }
                                Some(Pattern::Assignment(a)) => {
                                    if let Pattern::Identifier(id) = &a.left {
                                        // element with default: get element, check undefined, use default
                                        self.chunk.emit_op_u8(OpCode::GetLocal, src_slot, line);
                                        let idx_val = Value::int(i as i32);
                                        let cidx = self.chunk.add_constant(idx_val);
                                        self.chunk.emit_op_u16(OpCode::Const, cidx, line);
                                        self.chunk.emit_op(OpCode::GetElement, line);
                                        // Check if undefined → use default
                                        self.chunk.emit_op(OpCode::Dup, line);
                                        self.chunk.emit_op(OpCode::Undefined, line);
                                        self.chunk.emit_op(OpCode::StrictNe, line);
                                        let jump_idx = self.chunk.code.len();
                                        self.chunk.emit_op(OpCode::JumpIfTrue, line);
                                        self.chunk.code.push(0);
                                        self.chunk.code.push(0);
                                        self.chunk.emit_op(OpCode::Pop, line);
                                        self.compile_expr(&a.right)?;
                                        let target = self.chunk.code.len();
                                        let offset = (target as i16) - (jump_idx as i16) - 3;
                                        self.chunk.code[jump_idx + 1] = (offset >> 8) as u8;
                                        self.chunk.code[jump_idx + 2] = (offset & 0xFF) as u8;
                                        self.add_local(id.name);
                                        self.mark_initialized();
                                    }
                                }
                                _ => {}
                            }
                        }
                    } else {
                        for (i, elem) in arr_pat.elements.iter().enumerate() {
                            match elem {
                                Some(Pattern::Identifier(id)) => {
                                    self.chunk.emit_op(OpCode::Dup, line);
                                    let idx_val = Value::int(i as i32);
                                    let cidx = self.chunk.add_constant(idx_val);
                                    self.chunk.emit_op_u16(OpCode::Const, cidx, line);
                                    self.chunk.emit_op(OpCode::GetElement, line);
                                    let vidx = self.make_string_constant(id.name);
                                    self.chunk.emit_op_u16(OpCode::DefineGlobal, vidx, line);
                                }
                                Some(Pattern::Rest(rest)) => {
                                    if let Pattern::Identifier(id) = &rest.argument {
                                        self.chunk.emit_op(OpCode::Dup, line);
                                        let slice_name = self.interner.intern("slice");
                                        let slice_idx = self.make_string_constant(slice_name);
                                        let start_val = Value::int(i as i32);
                                        let start_idx = self.chunk.add_constant(start_val);
                                        self.chunk.emit_op_u16(OpCode::Const, start_idx, line);
                                        self.chunk.emit_byte(OpCode::CallMethod as u8, line);
                                        self.chunk.code.push(1);
                                        self.chunk.code.push((slice_idx >> 8) as u8);
                                        self.chunk.code.push((slice_idx & 0xFF) as u8);
                                        let vidx = self.make_string_constant(id.name);
                                        self.chunk.emit_op_u16(OpCode::DefineGlobal, vidx, line);
                                    }
                                }
                                Some(Pattern::Assignment(a)) => {
                                    if let Pattern::Identifier(id) = &a.left {
                                        self.chunk.emit_op(OpCode::Dup, line);
                                        let idx_val = Value::int(i as i32);
                                        let cidx = self.chunk.add_constant(idx_val);
                                        self.chunk.emit_op_u16(OpCode::Const, cidx, line);
                                        self.chunk.emit_op(OpCode::GetElement, line);
                                        // Check if undefined → use default
                                        self.chunk.emit_op(OpCode::Dup, line);
                                        self.chunk.emit_op(OpCode::Undefined, line);
                                        self.chunk.emit_op(OpCode::StrictNe, line);
                                        let jump_idx = self.chunk.code.len();
                                        self.chunk.emit_op(OpCode::JumpIfTrue, line);
                                        self.chunk.code.push(0);
                                        self.chunk.code.push(0);
                                        self.chunk.emit_op(OpCode::Pop, line);
                                        self.compile_expr(&a.right)?;
                                        let target = self.chunk.code.len();
                                        let offset = (target as i16) - (jump_idx as i16) - 3;
                                        self.chunk.code[jump_idx + 1] = (offset >> 8) as u8;
                                        self.chunk.code[jump_idx + 2] = (offset & 0xFF) as u8;
                                        let vidx = self.make_string_constant(id.name);
                                        self.chunk.emit_op_u16(OpCode::DefineGlobal, vidx, line);
                                    }
                                }
                                _ => {}
                            }
                        }
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
            scope_depth: self.scope_depth,
            label: None,
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

        self.loops.push(LoopCtx {
            continue_target: loop_start,
            break_patches: Vec::new(),
            scope_depth: self.scope_depth,
            label: None,
        });

        self.compile_statement(&d.body)?;
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

        // Push a loop context. We'll update continue_target after the body.
        self.loops.push(LoopCtx {
            continue_target: loop_start,
            break_patches: Vec::new(),
            scope_depth: self.scope_depth,
            label: None,
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

        // `continue` should land right before the update expression.
        let continue_target = self.chunk.len();
        if let Some(ctx) = self.loops.last_mut() {
            ctx.continue_target = continue_target;
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
            scope_depth: self.scope_depth,
            label: None,
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
        match &loop_var {
            LoopVar::Simple(name) => declare_var(self, *name),
            LoopVar::ArrayDestructure => {
                let arr_pat = match &f.left {
                    ForInOfLeft::Variable(decl) => if let Some(d) = decl.declarations.first() { if let Pattern::Array(a) = &d.id { Some(a) } else { None } } else { None },
                    _ => None,
                };
                if let Some(ap) = arr_pat {
                    for elem in ap.elements.iter().flatten() {
                        let name = match elem {
                                Pattern::Identifier(id) => Some(id.name),
                                Pattern::Rest(r) => if let Pattern::Identifier(id) = &r.argument { Some(id.name) } else { None },
                                Pattern::Assignment(a) => if let Pattern::Identifier(id) = &a.left { Some(id.name) } else { None },
                                _ => None,
                            };
                            if let Some(n) = name { declare_var(self, n); }
                    }
                }
            }
            LoopVar::ObjectDestructure => {
                let obj_pat = match &f.left {
                    ForInOfLeft::Variable(decl) => if let Some(d) = decl.declarations.first() { if let Pattern::Object(o) = &d.id { Some(o) } else { None } } else { None },
                    _ => None,
                };
                if let Some(op) = obj_pat {
                    for prop in &op.properties {
                        if let ObjectPatternProperty::Property { value: Pattern::Identifier(id), .. } = prop {
                            declare_var(self, id.name);
                        }
                    }
                }
            }
            LoopVar::None => {}
        }

        // Compile the iterable and get its iterator
        self.compile_expr(&f.right)?;
        self.chunk.emit_op(OpCode::GetIterator, line);

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
                self.compile_set_variable(*name, line)?;
                self.chunk.emit_op(OpCode::Pop, line);
            }
            LoopVar::ArrayDestructure => {
                let arr_pat = match &f.left {
                    ForInOfLeft::Variable(decl) => if let Some(d) = decl.declarations.first() { if let Pattern::Array(a) = &d.id { Some(a) } else { None } } else { None },
                    _ => None,
                };
                if let Some(ap) = arr_pat {
                    for (i, elem) in ap.elements.iter().enumerate() {
                        if let Some(Pattern::Identifier(id)) = elem {
                            self.chunk.emit_op(OpCode::Dup, line);
                            let idx_val = Value::int(i as i32);
                            let idx = self.chunk.add_constant(idx_val);
                            self.chunk.emit_op_u16(OpCode::Const, idx, line);
                            self.chunk.emit_op(OpCode::GetElement, line);
                            self.compile_set_variable(id.name, line)?;
                            self.chunk.emit_op(OpCode::Pop, line);
                        }
                    }
                }
                self.chunk.emit_op(OpCode::Pop, line);
            }
            LoopVar::ObjectDestructure => {
                let obj_pat = match &f.left {
                    ForInOfLeft::Variable(decl) => if let Some(d) = decl.declarations.first() { if let Pattern::Object(o) = &d.id { Some(o) } else { None } } else { None },
                    _ => None,
                };
                if let Some(op) = obj_pat {
                    for prop in &op.properties {
                        if let ObjectPatternProperty::Property { key, value: Pattern::Identifier(id), .. } = prop {
                            let key_sid = match key {
                                PropertyKey::Identifier(s) | PropertyKey::StringLiteral(s) => *s,
                                _ => continue,
                            };
                            self.chunk.emit_op(OpCode::Dup, line);
                            let key_idx = self.make_string_constant(key_sid);
                            self.chunk.emit_byte(OpCode::GetProperty as u8, line);
                            self.chunk.code.push((key_idx >> 8) as u8);
                            self.chunk.code.push((key_idx & 0xFF) as u8);
                            self.compile_set_variable(id.name, line)?;
                            self.chunk.emit_op(OpCode::Pop, line);
                        }
                    }
                }
                self.chunk.emit_op(OpCode::Pop, line);
            }
            LoopVar::None => {
                self.chunk.emit_op(OpCode::Pop, line);
            }
        }

        // Compile body with loop context for break/continue
        self.loops.push(LoopCtx {
            continue_target: loop_start,
            break_patches: Vec::new(),
            scope_depth: self.scope_depth,
            label: None,
        });

        self.compile_statement(&f.body)?;

        // Loop back
        self.chunk.emit_loop(loop_start, line);

        // Exit: pop the result and iterator
        self.chunk.patch_jump(exit_jump);
        self.chunk.emit_op(OpCode::Pop, line); // pop result
        self.chunk.emit_op(OpCode::Pop, line); // pop iterator

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
            scope_depth: self.scope_depth,
            label: None,
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
        let pop_n = self.locals_above_depth(loop_depth);
        if pop_n > 0 && pop_n <= u8::MAX as usize {
            self.chunk.emit_op_u8(OpCode::PopN, pop_n as u8, line);
        } else {
            for _ in 0..pop_n {
                self.chunk.emit_op(OpCode::Pop, line);
            }
        }
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
        let ctx = self.loops.last().unwrap();
        let target = ctx.continue_target;
        let loop_depth = ctx.scope_depth;

        let pop_n = self.locals_above_depth(loop_depth);
        if pop_n > 0 && pop_n <= u8::MAX as usize {
            self.chunk.emit_op_u8(OpCode::PopN, pop_n as u8, line);
        } else {
            for _ in 0..pop_n {
                self.chunk.emit_op(OpCode::Pop, line);
            }
        }
        self.chunk.emit_loop(target, line);
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

        // Compile try block (no scope — var declarations should be global/function-scoped).
        for stmt in &t.block.body {
            self.compile_statement(stmt)?;
        }

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
                    // Destructure the caught exception: catch({ message })
                    // Exception is on the stack — destructure it
                    let anon = self.interner.intern("__catch_val__");
                    self.add_local(anon);
                    self.mark_initialized();
                    let src_slot = (self.locals.len() - 1) as u8;
                    for prop in &obj_pat.properties {
                        if let ObjectPatternProperty::Property { key, value: Pattern::Identifier(id), .. } = prop {
                            let key_sid = match key {
                                PropertyKey::Identifier(s) | PropertyKey::StringLiteral(s) => *s,
                                _ => continue,
                            };
                            self.chunk.emit_op_u8(OpCode::GetLocal, src_slot, line);
                            let key_idx = self.make_string_constant(key_sid);
                            self.chunk.emit_byte(OpCode::GetProperty as u8, line);
                            self.chunk.code.push((key_idx >> 8) as u8);
                            self.chunk.code.push((key_idx & 0xFF) as u8);
                            self.add_local(id.name);
                            self.mark_initialized();
                        }
                    }
                }
                Some(Pattern::Array(arr_pat)) => {
                    let anon = self.interner.intern("__catch_val__");
                    self.add_local(anon);
                    self.mark_initialized();
                    let src_slot = (self.locals.len() - 1) as u8;
                    for (i, elem) in arr_pat.elements.iter().enumerate() {
                        if let Some(Pattern::Identifier(id)) = elem {
                            self.chunk.emit_op_u8(OpCode::GetLocal, src_slot, line);
                            let idx_val = Value::int(i as i32);
                            let cidx = self.chunk.add_constant(idx_val);
                            self.chunk.emit_op_u16(OpCode::Const, cidx, line);
                            self.chunk.emit_op(OpCode::GetElement, line);
                            self.add_local(id.name);
                            self.mark_initialized();
                        }
                    }
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
                    self.compile_expr(&m.value)?;
                    let key_id = self.property_key_name(&m.key);
                    // For getters/setters, use __get_name__ / __set_name__ convention
                    let actual_key = match m.kind {
                        MethodKind::Get => {
                            let name = self.interner.resolve(key_id).to_owned();
                            self.interner.intern(&format!("__get_{name}__"))
                        }
                        MethodKind::Set => {
                            let name = self.interner.resolve(key_id).to_owned();
                            self.interner.intern(&format!("__set_{name}__"))
                        }
                        _ => key_id,
                    };
                    let idx = self.make_string_constant(actual_key);
                    let op = if m.is_static {
                        OpCode::ClassStaticMethod
                    } else {
                        OpCode::ClassMethod
                    };
                    self.chunk.emit_op_u16(op, idx, line);
                }
                ClassMember::Property(p) => {
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
                ClassMember::StaticBlock(_) => { /* skip */ }
            }
        }
        Ok(())
    }

    // ---- labeled ----

    fn compile_labeled(&mut self, l: &LabeledStatement) -> Result<(), String> {
        // Push a label context so `break label` can find it
        self.loops.push(LoopCtx {
            continue_target: self.chunk.len(), // not meaningful for non-loop labels
            break_patches: Vec::new(),
            scope_depth: self.scope_depth,
            label: Some(l.label),
        });
        self.compile_statement(&l.body)?;
        // Patch any break jumps targeting this label
        self.patch_loop_breaks();
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
                                self.chunk.emit_op_u16(OpCode::GetProperty, idx, line);
                                let name_idx = self.make_string_constant(*local);
                                self.chunk.emit_op_u16(OpCode::DefineGlobal, name_idx, line);
                            }
                            ImportSpecifier::Named { imported, local, .. } => {
                                self.chunk.emit_op(OpCode::Dup, line);
                                let idx = self.make_string_constant(*imported);
                                self.chunk.emit_op_u16(OpCode::GetProperty, idx, line);
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
                    self.chunk.emit_byte(OpCode::SetProperty as u8, line);
                    self.chunk.code.push((name_idx >> 8) as u8);
                    self.chunk.code.push((name_idx & 0xFF) as u8);
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
                self.chunk.emit_byte(OpCode::SetProperty as u8, line);
                self.chunk.code.push((default_idx >> 8) as u8);
                self.chunk.code.push((default_idx & 0xFF) as u8);
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
                    self.chunk.emit_byte(OpCode::SetProperty as u8, line);
                    self.chunk.code.push((exported_idx >> 8) as u8);
                    self.chunk.code.push((exported_idx & 0xFF) as u8);
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

    fn compile_function_body(
        &mut self,
        name: StringId,
        params: &[Pattern],
        body: &BlockStatement,
        is_async: bool,
        is_generator: bool,
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

        // Declare parameters as locals.
        for param in params {
            match param {
                Pattern::Identifier(id) => {
                    self.add_local(id.name);
                    self.mark_initialized();
                }
                Pattern::Assignment(a) => {
                    if let Pattern::Identifier(id) = &a.left {
                        self.add_local(id.name);
                        self.mark_initialized();
                    }
                }
                Pattern::Rest(r) => {
                    if let Pattern::Identifier(id) = &r.argument {
                        self.add_local(id.name);
                        self.mark_initialized();
                    }
                }
                _ => {}
            }
        }

        // Emit default parameter initialization code.
        // For each parameter with a default value, check if undefined and assign default.
        for (i, param) in params.iter().enumerate() {
            if let Pattern::Assignment(a) = param
                && let Pattern::Identifier(_) = &a.left {
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

        // Compile body.
        for stmt in &body.body {
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

        for param in params {
            match param {
                Pattern::Identifier(id) => {
                    self.add_local(id.name);
                    self.mark_initialized();
                }
                Pattern::Assignment(a) => {
                    if let Pattern::Identifier(id) = &a.left {
                        self.add_local(id.name);
                        self.mark_initialized();
                    }
                }
                Pattern::Rest(r) => {
                    if let Pattern::Identifier(id) = &r.argument {
                        self.add_local(id.name);
                        self.mark_initialized();
                    }
                }
                _ => {}
            }
        }

        // Emit default parameter initialization for arrow functions
        for (i, param) in params.iter().enumerate() {
            if let Pattern::Assignment(a) = param
                && let Pattern::Identifier(_) = &a.left {
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
                        self.chunk.emit_op_u16(OpCode::GetProperty, idx, line);
                        if u.prefix {
                            self.chunk.emit_op(inc_op, line);
                            self.chunk.emit_op_u16(OpCode::SetProperty, idx, line);
                        } else {
                            self.chunk.emit_op(OpCode::Dup, line);
                            self.chunk.emit_op(OpCode::Rot3, line);
                            self.chunk.emit_op(inc_op, line);
                            self.chunk.emit_op_u16(OpCode::SetProperty, idx, line);
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

    fn compile_assignment(&mut self, a: &AssignmentExpression) -> Result<(), String> {
        let line = a.span.start;
        match &a.left {
            AssignmentTarget::Identifier(id) => {
                if a.operator == AssignmentOperator::Assign {
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
                                        // ...rest — collect remaining elements
                                        // Pop the single element, re-dup array, slice from i
                                        self.chunk.emit_op(OpCode::Pop, line); // pop single elem
                                        // For rest, we'd need Array.slice — just skip for now
                                        if let Pattern::Identifier(id) = &r.argument {
                                            // Build rest array: emit code to slice
                                            self.chunk.emit_op(OpCode::Dup, line);
                                            self.compile_set_variable(id.name, line)?;
                                            self.chunk.emit_op(OpCode::Pop, line);
                                        }
                                    }
                                    _ => { self.chunk.emit_op(OpCode::Pop, line); }
                                }
                            }
                        }
                    }
                    Pattern::Object(obj_pat) => {
                        for prop in &obj_pat.properties {
                            if let ObjectPatternProperty::Property { key, value: Pattern::Identifier(id), .. } = prop {
                                let key_sid = match key {
                                    PropertyKey::Identifier(s) | PropertyKey::StringLiteral(s) => *s,
                                    _ => continue,
                                };
                                self.chunk.emit_op(OpCode::Dup, line);
                                let key_idx = self.make_string_constant(key_sid);
                                self.chunk.emit_byte(OpCode::GetProperty as u8, line);
                                self.chunk.code.push((key_idx >> 8) as u8);
                                self.chunk.code.push((key_idx & 0xFF) as u8);
                                self.compile_set_variable(id.name, line)?;
                                self.chunk.emit_op(OpCode::Pop, line);
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
        self.compile_expr(&m.object)?;

        match &m.property {
            MemberProperty::Identifier(name) => {
                let idx = self.make_string_constant(*name);
                if op != AssignmentOperator::Assign {
                    self.chunk.emit_op(OpCode::Dup, line);
                    self.chunk.emit_op_u16(OpCode::GetProperty, idx, line);
                    self.compile_expr(rhs)?;
                    self.emit_compound_arith(op, line)?;
                } else {
                    self.compile_expr(rhs)?;
                }
                self.chunk.emit_op_u16(OpCode::SetProperty, idx, line);
            }
            MemberProperty::Expression(key) => {
                self.compile_expr(key)?;
                if op != AssignmentOperator::Assign {
                    self.chunk.emit_op(OpCode::Dup2, line);
                    self.chunk.emit_op(OpCode::GetElement, line);
                    self.compile_expr(rhs)?;
                    self.emit_compound_arith(op, line)?;
                } else {
                    self.compile_expr(rhs)?;
                }
                self.chunk.emit_op(OpCode::SetElement, line);
            }
            MemberProperty::PrivateIdentifier(name) => {
                let idx = self.make_string_constant(*name);
                if op != AssignmentOperator::Assign {
                    self.chunk.emit_op(OpCode::Dup, line);
                    self.chunk.emit_op_u16(OpCode::GetPrivate, idx, line);
                    self.compile_expr(rhs)?;
                    self.emit_compound_arith(op, line)?;
                } else {
                    self.compile_expr(rhs)?;
                }
                self.chunk.emit_op_u16(OpCode::SetPrivate, idx, line);
            }
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
        self.compile_expr(&m.object)?;
        match &m.property {
            MemberProperty::Identifier(name) => {
                let idx = self.make_string_constant(*name);
                self.chunk.emit_op_u16(OpCode::GetProperty, idx, line);
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
                _ => {
                    // Computed property method call - simplified
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
        for arg in &c.arguments {
            self.compile_expr(arg)?;
        }
        self.chunk.emit_op_u8(OpCode::Call, argc, line);
        Ok(())
    }

    // ---- new ----

    fn compile_new(&mut self, n: &NewExpression) -> Result<(), String> {
        let line = n.span.start;
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
        self.compile_property_key(&p.key, line)?;

        match p.kind {
            PropertyKindVal::Init => {
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

    fn property_key_name(&self, key: &PropertyKey) -> StringId {
        match key {
            PropertyKey::Identifier(id)
            | PropertyKey::StringLiteral(id)
            | PropertyKey::Private(id) => *id,
            _ => StringId(0),
        }
    }

    // ---- function expression ----

    fn compile_function_expr(&mut self, f: &FunctionExpression) -> Result<(), String> {
        let name = f
            .id
            .unwrap_or_else(|| self.interner.intern("<anonymous>"));
        let child_chunk =
            self.compile_function_body(name, &f.params, &f.body, f.is_async, f.is_generator)?;
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
        // For named function expressions, store function as global so it can self-reference
        // (the name should only be visible inside the function body per spec,
        // but global binding makes self-recursion work)
        if f.id.is_some() {
            self.chunk.emit_op(OpCode::Dup, f.span.start);
            let idx = self.make_string_constant(name);
            self.chunk.emit_op_u16(OpCode::DefineGlobal, idx, f.span.start);
        }
        Ok(())
    }

    // ---- arrow function expression ----

    fn compile_arrow_expr(&mut self, a: &ArrowFunctionExpression) -> Result<(), String> {
        let child_chunk = self.compile_arrow_body(&a.params, &a.body, a.is_async)?;
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
        let name = c.id.unwrap_or(StringId(0));
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

        for element in &o.chain {
            match element {
                OptionalChainElement::Member {
                    property, optional, ..
                } => {
                    let skip = if *optional {
                        Some(self.chunk.emit_jump(OpCode::OptionalChain, line))
                    } else {
                        None
                    };
                    match property {
                        MemberProperty::Identifier(id) => {
                            let idx = self.make_string_constant(*id);
                            self.chunk.emit_op_u16(OpCode::GetProperty, idx, line);
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
                    if let Some(s) = skip {
                        self.chunk.patch_jump(s);
                    }
                }
                OptionalChainElement::Call {
                    arguments, optional,
                } => {
                    let skip = if *optional {
                        Some(self.chunk.emit_jump(OpCode::OptionalChain, line))
                    } else {
                        None
                    };
                    for arg in arguments {
                        self.compile_expr(arg)?;
                    }
                    self.chunk
                        .emit_op_u8(OpCode::Call, arguments.len() as u8, line);
                    if let Some(s) = skip {
                        self.chunk.patch_jump(s);
                    }
                }
            }
        }
        Ok(())
    }

    // ---- yield ----

    fn compile_yield(&mut self, y: &YieldExpression) -> Result<(), String> {
        let line = y.span.start;
        if let Some(arg) = &y.argument {
            self.compile_expr(arg)?;
        } else {
            self.chunk.emit_op(OpCode::Undefined, line);
        }
        if y.delegate {
            self.chunk.emit_op(OpCode::YieldStar, line);
        } else {
            self.chunk.emit_op(OpCode::Yield, line);
        }
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
