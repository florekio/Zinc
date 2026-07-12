//! Scope tracking and variable access: locals, upvalues, and the
//! with-scope guards emitted around static binding reads/writes.

use super::*;

impl<'a> Compiler<'a> {
    /// How many locals sit above the given scope depth?
    pub(super) fn locals_above_depth(&self, depth: u32) -> usize {
        self.locals
            .iter()
            .rev()
            .take_while(|l| l.depth > depth)
            .count()
    }

    pub(super) fn begin_scope(&mut self) {
        self.scope_depth += 1;
    }

    pub(super) fn end_scope(&mut self) {
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

    pub(super) fn add_local(&mut self, name: StringId) {
        self.locals.push(Local {
            name,
            depth: self.scope_depth,
            initialized: false,
            captured: false,
            is_const: false,
            is_fn_self_name: false,
        });
    }

    pub(super) fn mark_initialized(&mut self) {
        if let Some(local) = self.locals.last_mut() {
            local.initialized = true;
        }
    }

    pub(super) fn resolve_local(&self, name: StringId) -> Option<usize> {
        for (i, local) in self.locals.iter().enumerate().rev() {
            if local.name == name {
                return Some(i);
            }
        }
        None
    }

    /// If `name` is a top-level lexical whose slot was reserved at function
    /// entry by the hoist pass, consume the reservation and return the slot.
    /// Only applies at the function body's own scope (depth 1) — `let` in a
    /// nested block is a distinct binding and must get a fresh local.
    pub(super) fn take_predeclared_lex(&mut self, name: StringId) -> Option<usize> {
        if self.scope_depth != 1 {
            return None;
        }
        let pos = self.predeclared_lex.iter().position(|&n| n == name)?;
        self.predeclared_lex.swap_remove(pos);
        self.resolve_local(name)
    }

    /// Try to resolve a variable as an upvalue captured from an enclosing
    /// scope. Walks the enclosing chain from the immediate parent outward;
    /// when it finds the owning local at some ancestor level it threads an
    /// upvalue through every function in between (transitive capture), so a
    /// grandparent (or deeper) binding is reachable even when the intermediate
    /// functions never reference it themselves.
    pub(super) fn resolve_upvalue(&mut self, name: StringId) -> Option<u16> {
        let n = self.enclosing_chain.len();
        if n == 0 {
            return None;
        }
        // level indexes enclosing_chain; start at the immediate parent (last).
        self.resolve_upvalue_at(name, n - 1)
    }

    /// Resolve `name` against the function at chain index `level`, returning
    /// the upvalue index added to that function's *child* (the function at
    /// `level + 1`, or the current function when `level + 1 == chain.len()`).
    pub(super) fn resolve_upvalue_at(&mut self, name: StringId, level: usize) -> Option<u16> {
        // 1. Is `name` an own local of frame[level]? Mark it captured.
        let local_idx = {
            let frame = &mut self.enclosing_chain[level];
            let mut found = None;
            for (i, local) in frame.locals.iter_mut().enumerate().rev() {
                if local.name == name {
                    local.captured = true;
                    found = Some(i);
                    break;
                }
            }
            found
        };
        if let Some(i) = local_idx {
            return Some(self.add_upvalue_at(level + 1, i as u16, true));
        }
        // 2. Otherwise recurse into the grandparent; if found, the upvalue it
        //    added to frame[level] becomes a (non-local) upvalue of frame[level+1].
        if level == 0 {
            return None;
        }
        if let Some(parent_uv) = self.resolve_upvalue_at(name, level - 1) {
            return Some(self.add_upvalue_at(level + 1, parent_uv, false));
        }
        None
    }

    /// Add (deduplicated) an upvalue to the function at chain index `level`,
    /// or to the current function when `level == chain.len()`. Returns its index.
    pub(super) fn add_upvalue_at(&mut self, level: usize, index: u16, is_local: bool) -> u16 {
        let upvalues = if level == self.enclosing_chain.len() {
            &mut self.upvalues
        } else {
            &mut self.enclosing_chain[level].upvalues
        };
        for (i, uv) in upvalues.iter().enumerate() {
            if uv.index == index && uv.is_local == is_local {
                return i as u16;
            }
        }
        let idx = upvalues.len() as u16;
        upvalues.push(CompilerUpvalue { index, is_local });
        idx
    }

    /// Inside a `with` body, a name that statically resolves to a local or
    /// upvalue may still be shadowed by the with-scope object at runtime.
    /// Emit a WithGetCheck/WithSetCheck guard before the static access; the
    /// guard handles the name via the with-object (and jumps over the
    /// fallback) when the object owns the property. Returns the patch
    /// position for the embedded jump offset.
    pub(super) fn emit_with_guard(&mut self, op: OpCode, name: StringId, line: u32) -> usize {
        let idx = self.make_string_constant(name);
        self.chunk.emit_op_u16(op, idx, line);
        self.chunk.emit_offset_placeholder(line)
    }

    /// Whether a local in `slot` needs a with-scope guard: we must be inside a
    /// `with` body, and the local must be declared *outside* the innermost
    /// `with` (bindings created inside the body — e.g. `let x` — are inner to
    /// the with scope and can never be shadowed by it).
    pub(super) fn local_needs_with_guard(&self, slot: usize) -> bool {
        self.with_depth > 0
            && self.with_local_floor.last().is_none_or(|&floor| slot < floor)
    }

    /// True when an identifier assignment target inside a `with` body must be
    /// resolved as a reference before the RHS runs (spec: references resolve
    /// before the right-hand side, and a compound assignment writes back
    /// through the same reference). Applies to guarded locals, upvalues and
    /// globals. Const bindings are excluded — compile_set_variable's static
    /// TypeError path handles them.
    pub(super) fn ident_needs_with_ref(&mut self, name: StringId) -> bool {
        if self.with_depth == 0 {
            return false;
        }
        if let Some(slot) = self.resolve_local(name) {
            !self.locals[slot].is_const && self.local_needs_with_guard(slot)
        } else if self.resolve_upvalue(name).is_some() {
            true
        } else {
            !self.const_globals.contains(&name)
        }
    }

    /// Emit the read for a with-scope reference already resolved by
    /// WithRefResolve (stack: [ref] → [ref, value]). WithRefGet peeks the
    /// ref: it either reads through it and jumps, or falls through to the
    /// plain (unguarded) local/upvalue read.
    pub(super) fn compile_get_variable_resolved(&mut self, name: StringId, line: u32) -> Result<(), String> {
        let guard = self.emit_with_guard(OpCode::WithRefGet, name, line);
        if let Some(slot) = self.resolve_local(name) {
            if slot <= u8::MAX as usize {
                self.chunk.emit_op_u8(OpCode::GetLocal, slot as u8, line);
            } else {
                self.chunk.emit_op_u16(OpCode::GetLocalWide, slot as u16, line);
            }
        } else if let Some(uv_idx) = self.resolve_upvalue(name) {
            if uv_idx <= u8::MAX as u16 {
                self.chunk.emit_op_u8(OpCode::GetUpvalue, uv_idx as u8, line);
            } else {
                self.chunk.emit_op_u16(OpCode::GetUpvalueWide, uv_idx, line);
            }
        } else {
            let idx = self.make_string_constant(name);
            self.chunk.emit_op_u16(OpCode::GetGlobal, idx, line);
        }
        self.chunk.patch_jump(guard);
        Ok(())
    }

    /// Emit the store for an assignment whose with-scope reference was already
    /// resolved by WithRefResolve (stack: [ref, value]). WithRefSet consumes
    /// the ref: it either stores through it and jumps, or falls through to the
    /// plain (unguarded) local/upvalue set.
    pub(super) fn compile_set_variable_resolved(&mut self, name: StringId, line: u32) -> Result<(), String> {
        let guard = self.emit_with_guard(OpCode::WithRefSet, name, line);
        if let Some(slot) = self.resolve_local(name) {
            if slot <= u8::MAX as usize {
                self.chunk.emit_op_u8(OpCode::SetLocal, slot as u8, line);
            } else {
                self.chunk.emit_op_u16(OpCode::SetLocalWide, slot as u16, line);
            }
        } else if let Some(uv_idx) = self.resolve_upvalue(name) {
            if uv_idx <= u8::MAX as u16 {
                self.chunk.emit_op_u8(OpCode::SetUpvalue, uv_idx as u8, line);
            } else {
                self.chunk.emit_op_u16(OpCode::SetUpvalueWide, uv_idx, line);
            }
        } else {
            let idx = self.make_string_constant(name);
            self.chunk.emit_op_u16(OpCode::SetGlobal, idx, line);
        }
        self.chunk.patch_jump(guard);
        Ok(())
    }

    /// While compiling parameter k's default expression, a direct reference
    /// to parameter slot >= k reads an uninitialized binding — emit a
    /// ReferenceError throw in its place (parameters initialize left to
    /// right, so this is statically known).
    fn param_tdz_violation(&mut self, name: StringId) -> bool {
        if let Some((k, param_count)) = self.param_init_active
            && let Some(slot) = self.resolve_local(name)
        {
            slot >= k && slot < param_count
        } else {
            false
        }
    }

    pub(super) fn compile_get_variable(&mut self, name: StringId, line: u32) -> Result<(), String> {
        if self.param_tdz_violation(name) {
            let var_name = self.interner.resolve(name).to_owned();
            self.emit_throw_reference_error(
                &format!("Cannot access '{var_name}' before initialization"), line);
            return Ok(());
        }
        if let Some(slot) = self.resolve_local(name) {
            let guard = self.local_needs_with_guard(slot)
                .then(|| self.emit_with_guard(OpCode::WithGetCheck, name, line));
            if slot <= u8::MAX as usize {
                self.chunk.emit_op_u8(OpCode::GetLocal, slot as u8, line);
            } else {
                self.chunk
                    .emit_op_u16(OpCode::GetLocalWide, slot as u16, line);
            }
            if let Some(pos) = guard { self.chunk.patch_jump(pos); }
        } else if self.interner.resolve(name) == "arguments"
            && !self.chunk.flags.contains(crate::compiler::chunk::ChunkFlags::ARROW)
        {
            // Every non-arrow function has its OWN `arguments` binding: an
            // outer `var arguments` must not be captured as an upvalue.
            self.chunk.uses_arguments = true;
            let idx = self.make_string_constant(name);
            self.chunk.emit_op_u16(OpCode::GetGlobal, idx, line);
        } else if let Some(uv_idx) = self.resolve_upvalue(name) {
            let guard = (self.with_depth > 0)
                .then(|| self.emit_with_guard(OpCode::WithGetCheck, name, line));
            if uv_idx <= u8::MAX as u16 {
                self.chunk.emit_op_u8(OpCode::GetUpvalue, uv_idx as u8, line);
            } else {
                self.chunk.emit_op_u16(OpCode::GetUpvalueWide, uv_idx, line);
            }
            if let Some(pos) = guard { self.chunk.patch_jump(pos); }
        } else {
            if self.interner.resolve(name) == "arguments" {
                self.chunk.uses_arguments = true;
            }
            let idx = self.make_string_constant(name);
            self.chunk.emit_op_u16(OpCode::GetGlobal, idx, line);
        }
        Ok(())
    }

    pub(super) fn compile_set_variable(&mut self, name: StringId, line: u32) -> Result<(), String> {
        if self.param_tdz_violation(name) {
            let var_name = self.interner.resolve(name).to_owned();
            self.emit_throw_reference_error(
                &format!("Cannot access '{var_name}' before initialization"), line);
            return Ok(());
        }
        if let Some(slot) = self.resolve_local(name) {
            // Named function expression self-binding is immutable: sloppy
            // assignments are silently ignored (the RHS remains the
            // expression's value); strict ones throw TypeError.
            if self.locals[slot].is_fn_self_name {
                if self.chunk.flags.contains(ChunkFlags::STRICT) {
                    let var_name = self.interner.resolve(name).to_owned();
                    self.emit_throw_type_error(
                        &format!("Assignment to constant variable '{var_name}'"), line);
                }
                return Ok(());
            }
            if self.locals[slot].is_const {
                let var_name = self.interner.resolve(name).to_owned();
                self.emit_throw_type_error(
                    &format!("Assignment to constant variable '{var_name}'"), line);
                return Ok(());
            }
            let guard = self.local_needs_with_guard(slot)
                .then(|| self.emit_with_guard(OpCode::WithSetCheck, name, line));
            if slot <= u8::MAX as usize {
                self.chunk.emit_op_u8(OpCode::SetLocal, slot as u8, line);
            } else {
                self.chunk
                    .emit_op_u16(OpCode::SetLocalWide, slot as u16, line);
            }
            if let Some(pos) = guard { self.chunk.patch_jump(pos); }
        } else if let Some(uv_idx) = self.resolve_upvalue(name) {
            // The captured binding may be a named-function-expression
            // self-binding in an enclosing function — same immutability
            // rules as the local case above.
            let captured_self_name = self.enclosing_chain.iter().rev().find_map(|frame| {
                frame.locals.iter().rev().find(|l| l.name == name).map(|l| l.is_fn_self_name)
            });
            if captured_self_name == Some(true) {
                if self.chunk.flags.contains(ChunkFlags::STRICT) {
                    let var_name = self.interner.resolve(name).to_owned();
                    self.emit_throw_type_error(
                        &format!("Assignment to constant variable '{var_name}'"), line);
                }
                return Ok(());
            }
            let guard = (self.with_depth > 0)
                .then(|| self.emit_with_guard(OpCode::WithSetCheck, name, line));
            if uv_idx <= u8::MAX as u16 {
                self.chunk.emit_op_u8(OpCode::SetUpvalue, uv_idx as u8, line);
            } else {
                self.chunk.emit_op_u16(OpCode::SetUpvalueWide, uv_idx, line);
            }
            if let Some(pos) = guard { self.chunk.patch_jump(pos); }
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
}
