//! Statement compilation: declarations, control flow, loops, try/catch,
//! with, and module import/export.

use super::*;

impl<'a> Compiler<'a> {
    pub(super) fn compile_statement(&mut self, stmt: &Statement) -> Result<(), String> {
        match stmt {
            Statement::Expression(e) => {
                self.compile_expr(&e.expression)?;
                // At script/eval level the statement value becomes the program's
                // completion value (consumed by Halt); inside functions it's discarded.
                if self.chunk.flags.contains(ChunkFlags::SCRIPT) {
                    self.chunk.emit_op(OpCode::SetCompletion, self.current_line());
                } else {
                    self.chunk.emit_op(OpCode::Pop, self.current_line());
                }
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

    pub(super) fn compile_var_declaration(&mut self, decl: &VariableDeclaration) -> Result<(), String> {
        for declarator in &decl.declarations {
            match &declarator.id {
                Pattern::Identifier(id) => {
                    let name = id.name;
                    self.check_strict_restricted(name, "binding")?;
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
                                // Top-level let/const are lexical bindings in the
                                // global environment — NOT properties of globalThis.
                                self.chunk.emit_op_u16(OpCode::DefineGlobalLex, idx, line);
                            }
                        } else if decl.kind == VarKind::Var {
                            // `var x = expr` initializes via an ordinary assignment,
                            // which goes through the scope chain — including any
                            // active `with` scope (compile_set_variable guards it).
                            self.compile_set_variable(name, line)?;
                            self.chunk.emit_op(OpCode::Pop, line);
                        } else if let Some(slot) = self.take_predeclared_lex(name) {
                            // Slot was reserved at function entry by the
                            // hoist pass — assign into it instead of pushing
                            // a fresh local. InitLet ends the TDZ so the
                            // SetLocal below doesn't reject the write.
                            if decl.kind == VarKind::Const {
                                self.locals[slot].is_const = true;
                            }
                            if slot <= u8::MAX as usize {
                                self.chunk.emit_op_u8(OpCode::InitLet, slot as u8, line);
                                self.chunk.emit_op_u8(OpCode::SetLocal, slot as u8, line);
                            } else {
                                self.chunk.emit_op_u16(OpCode::SetLocalWide, slot as u16, line);
                            }
                            self.chunk.emit_op(OpCode::Pop, line);
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
                    } else if let Some(slot) = self.take_predeclared_lex(name) {
                        // Pre-reserved slot holds the TDZ marker — the
                        // declaration ends the TDZ (binding becomes undefined).
                        if decl.kind == VarKind::Const {
                            self.locals[slot].is_const = true;
                        }
                        if slot <= u8::MAX as usize {
                            self.chunk.emit_op_u8(OpCode::InitLet, slot as u8, line);
                        }
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

    pub(super) fn compile_if(&mut self, s: &IfStatement) -> Result<(), String> {
        // IfStatement completion is UpdateEmpty(branch, undefined) for the taken
        // branch, or undefined when the test is false and there is no else clause.
        // Resetting the completion register to undefined before each branch body
        // captures that: an empty body leaves undefined, a body with value
        // statements overwrites it.
        let line = s.span.start;
        let is_script = self.chunk.flags.contains(ChunkFlags::SCRIPT);
        self.compile_expr(&s.test)?;
        let then_jump = self.chunk.emit_jump(OpCode::JumpIfFalse, line);
        self.emit_completion_reset(line);
        self.compile_statement(&s.consequent)?;

        if let Some(alt) = &s.alternate {
            let else_jump = self.chunk.emit_jump(OpCode::Jump, line);
            self.chunk.patch_jump(then_jump);
            self.emit_completion_reset(line);
            self.compile_statement(alt)?;
            self.chunk.patch_jump(else_jump);
        } else if is_script {
            // No else: the false path must still yield undefined.
            let else_jump = self.chunk.emit_jump(OpCode::Jump, line);
            self.chunk.patch_jump(then_jump);
            self.emit_completion_reset(line);
            self.chunk.patch_jump(else_jump);
        } else {
            self.chunk.patch_jump(then_jump);
        }
        Ok(())
    }

    pub(super) fn compile_while(&mut self, w: &WhileStatement) -> Result<(), String> {
        let line = w.span.start;
        // Loop completion starts at undefined (spec: V = undefined before the loop);
        // body iterations overwrite it, so zero iterations yield undefined.
        self.emit_completion_reset(line);
        // Consume any enclosing label so `continue label` / `break label` target
        // this loop's own continue point rather than the labeled-statement wrapper.
        let label = self.pending_label.take();
        let loop_start = self.chunk.len();

        self.loops.push(LoopCtx {
            continue_target: loop_start,
            break_patches: Vec::new(),
            continue_patches: Vec::new(),
            scope_depth: self.scope_depth,
            label,
            has_deferred_continue: false,
            try_depth: self.finally_stack.len(),
            with_depth: self.with_depth,
            is_switch: false,
            for_of_iter_slot: None,
        });

        self.compile_expr(&w.test)?;
        let exit_jump = self.chunk.emit_jump(OpCode::JumpIfFalse, line);
        self.compile_statement(&w.body)?;
        self.chunk.emit_loop(loop_start, line);
        self.chunk.patch_jump(exit_jump);

        self.patch_loop_breaks();
        Ok(())
    }

    pub(super) fn compile_do_while(&mut self, d: &DoWhileStatement) -> Result<(), String> {
        let line = d.span.start;
        self.emit_completion_reset(line);
        // Consume any enclosing label so `continue label` targets this loop's
        // deferred continue point (the test), not the labeled-statement wrapper.
        let label = self.pending_label.take();
        let loop_start = self.chunk.len();

        // Use deferred continue patching so `continue` jumps to the test, not
        // back to the body start (which would skip the test and infinite-loop).
        self.loops.push(LoopCtx {
            continue_target: loop_start,
            break_patches: Vec::new(),
            continue_patches: Vec::new(),
            scope_depth: self.scope_depth,
            label,
            has_deferred_continue: true,
            try_depth: self.finally_stack.len(),
            with_depth: self.with_depth,
            is_switch: false,
            for_of_iter_slot: None,
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

    pub(super) fn compile_for(&mut self, f: &ForStatement) -> Result<(), String> {
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

        self.emit_completion_reset(line);
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
            with_depth: self.with_depth,
            is_switch: false,
            for_of_iter_slot: None,
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

    pub(super) fn compile_for_in(&mut self, f: &ForInStatement) -> Result<(), String> {
        let line = f.span.start;
        // Only scope for let/const
        let is_var = matches!(&f.left, ForInOfLeft::Variable(decl) if decl.kind == VarKind::Var);
        let is_const = matches!(&f.left, ForInOfLeft::Variable(decl) if decl.kind == VarKind::Const);
        // A bare-identifier loop head (`for (e in o)`, no declaration) assigns to
        // an existing binding, like `var` — it must not get a fresh local either.
        let is_bare = matches!(&f.left, ForInOfLeft::Pattern(Pattern::Identifier(_)));
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
        // Bare-identifier loop heads are assignments: strict early error for
        // eval/arguments targets.
        if is_bare && let Some(n) = var_name {
            self.check_strict_restricted(n, "assignment target")?;
        }
        let mut local_slot: Option<u8> = None;
        let mut is_global = false;
        if let Some(name) = var_name {
            if (is_var || is_bare) && self.scope_depth > 1 {
                // `var` (hoisted) and bare-identifier loop heads assign to an
                // existing binding, so no fresh local is needed. Adding a
                // duplicate local here gave it a slot index equal to the stack
                // position of the on-stack for-in iterator — a nested for-in's
                // SetLocal then clobbered the outer iterator (→ "not an
                // iterator"). Store into the existing binding via
                // compile_set_variable instead (below). Only let/const need a
                // fresh scoped local.
            } else {
                // A let/const loop var is born in its TDZ: per spec it is in
                // scope (uninitialized) while the object expression evaluates,
                // so `for (let k in {a: k})` throws ReferenceError. The
                // per-iteration store clears the marker (InitLet below).
                if is_var || is_bare {
                    self.chunk.emit_op(OpCode::Undefined, line);
                } else {
                    self.chunk.emit_op(OpCode::PushEmpty, line);
                }
                if self.scope_depth <= 1 {
                    // Globals hold undefined (no TDZ tracking there).
                    if !(is_var || is_bare) {
                        self.chunk.emit_op(OpCode::Pop, line);
                        self.chunk.emit_op(OpCode::Undefined, line);
                    }
                    let idx = self.make_string_constant(name);
                    self.chunk.emit_op_u16(OpCode::DefineGlobal, idx, line);
                    is_global = true;
                } else {
                    self.add_local(name);
                    self.mark_initialized();
                    local_slot = Some((self.locals.len() - 1) as u8);
                }
            }
        }

        // Compile the object expression, then emit GetForInIterator (key iterator)
        self.compile_expr(&f.right)?;
        self.chunk.emit_op(OpCode::GetForInIterator, line);

        // When the loop head declares a fresh block-scoped local (let/const),
        // reserve a scoped local for the iterator and address it via GetLocal
        // rather than assuming it stays at the top of the stack (Dup). A nested
        // let/const for-in adds its own loop-var local whose Undefined lands on
        // the stack above the outer iterator, which would corrupt the Dup scheme
        // (→ "not an iterator"). A fixed slot is immune; end_scope reclaims it.
        let iter_slot = if local_slot.is_some() {
            let s = self.locals.len() as u8;
            let anon = self.interner.intern("(for-in-iter)");
            self.add_local(anon);
            self.mark_initialized();
            Some(s)
        } else {
            None
        };

        self.emit_completion_reset(line);
        let loop_start = self.chunk.len();

        if let Some(s) = iter_slot {
            self.chunk.emit_op_u8(OpCode::GetLocal, s, line);
        } else {
            self.chunk.emit_op(OpCode::Dup, line);
        }
        self.chunk.emit_op(OpCode::IteratorNext, line);
        self.chunk.emit_op(OpCode::Dup, line);
        self.chunk.emit_op(OpCode::IteratorDone, line);
        let exit_jump = self.chunk.emit_jump(OpCode::JumpIfTrue, line);

        self.chunk.emit_op(OpCode::IteratorValue, line);
        if let Some(name) = var_name {
            // For `const x in ...`, the per-iteration BindingInitialization
            // stores into the loop var directly (bypasses the const check).
            // After this raw store, mark the local as const so the body's
            // assignments throw.
            // The per-iteration BindingInitialization ends the loop var's
            // TDZ before storing into it.
            if let Some(slot) = local_slot {
                self.chunk.emit_op_u8(OpCode::InitLet, slot, line);
            }
            if is_const {
                if let Some(slot) = local_slot {
                    self.chunk.emit_op_u8(OpCode::SetLocal, slot, line);
                    self.chunk.emit_op(OpCode::Pop, line);
                    self.locals[slot as usize].is_const = true;
                } else if is_global {
                    let idx = self.make_string_constant(name);
                    self.chunk.emit_op_u16(OpCode::SetGlobal, idx, line);
                    self.chunk.emit_op(OpCode::Pop, line);
                    self.const_globals.insert(name);
                }
            } else {
                self.compile_set_variable(name, line)?;
                self.chunk.emit_op(OpCode::Pop, line);
            }
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
            with_depth: self.with_depth,
            is_switch: false,
            for_of_iter_slot: None,
        });
        self.compile_statement(&f.body)?;
        self.chunk.emit_loop(loop_start, line);

        self.chunk.patch_jump(exit_jump);
        self.chunk.emit_op(OpCode::Pop, line); // pop result
        if iter_slot.is_none() {
            // Dup path: the iterator is a stack temp at TOS — pop it. In the
            // reserved-slot path the iterator lives in a scoped local and is
            // reclaimed by end_scope below.
            self.chunk.emit_op(OpCode::Pop, line);
        }

        self.patch_loop_breaks();
        if !is_var { self.end_scope(); }
        Ok(())
    }

    pub(super) fn compile_for_of(&mut self, f: &ForOfStatement) -> Result<(), String> {
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

        // Per spec, the head's let/const bindings are already in scope —
        // uninitialized (TDZ) — while the iterable expression evaluates:
        // `for (let x of [x])` throws ReferenceError. Reserve marker locals
        // shadowing any outer bindings; the surrounding scope pops them.
        if is_let_const
            && let ForInOfLeft::Variable(decl) = &f.left
        {
            for d in &decl.declarations {
                // Simple identifier bindings only: the per-iteration binding
                // shadows this marker with a fresh local. Destructured heads
                // pre-declare their names and write into them directly, so a
                // shadowing marker would capture those writes.
                if let Pattern::Identifier(id) = &d.id {
                    self.chunk.emit_op(OpCode::PushEmpty, line);
                    self.add_local(id.name);
                    self.mark_initialized();
                }
            }
        }

        // Compile the iterable and get its iterator
        self.compile_expr(&f.right)?;
        self.chunk.emit_op(OpCode::GetIterator, line);

        // Track the iterator as an anonymous local: slot accounting stays
        // correct for body locals, and abrupt exits/throws can address it to
        // run IteratorClose. (At global scope it lives in the script frame.)
        let anon = self.interner.intern("(for-of-iter)");
        self.add_local(anon);
        self.mark_initialized();
        let iter_slot = (self.locals.len() - 1) as u16;

        self.emit_completion_reset(line);
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
                    if matches!(&f.left, ForInOfLeft::Variable(decl) if decl.kind == VarKind::Const) {
                        self.locals.last_mut().unwrap().is_const = true;
                    }
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
        // Guard the body with an exception handler whose catch region closes
        // the iterator and rethrows: a throw crossing the loop must run the
        // iterator's return() (e.g. a generator's pending finally blocks).
        self.chunk.emit_byte(OpCode::PushExcHandler as u8, line);
        let handler_placeholder = self.chunk.code.len();
        self.chunk.code.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);

        self.loops.push(LoopCtx {
            continue_target: loop_start,
            break_patches: Vec::new(),
            continue_patches: Vec::new(),
            scope_depth: loop_scope_depth,
            label: None,
            has_deferred_continue: false,
            try_depth: self.finally_stack.len(),
            with_depth: self.with_depth,
            is_switch: false,
            for_of_iter_slot: Some(iter_slot),
        });

        self.compile_statement(&f.body)?;

        self.chunk.emit_op(OpCode::PopExcHandler, line);

        // For fresh-binding-simple: close per-iteration scope before looping back.
        if fresh_binding_simple {
            self.end_scope();
        }

        // Loop back
        self.chunk.emit_loop(loop_start, line);

        // Exit: pop the result and close the iterator.
        self.chunk.patch_jump(exit_jump);
        self.chunk.emit_op(OpCode::Pop, line); // pop result
        // Patch break jumps to land here. Both natural exit and break go through
        // IteratorClose; for natural exit the iter's __iter_done__ flag is set
        // so close becomes a no-op, but for break the iterator's .return()
        // must run. (Break sites popped the body handler themselves.)
        self.patch_loop_breaks();
        self.chunk.emit_op(OpCode::IteratorClose, line); // close + pop iterator
        let skip_catch = self.chunk.emit_jump(OpCode::Jump, line);

        // Catch region: close the iterator, rethrow.
        if self.chunk.len() > u16::MAX as usize {
            self.chunk.jump_overflow = true;
        }
        let catch_target = self.chunk.len() as u16;
        self.chunk.code[handler_placeholder] = (catch_target >> 8) as u8;
        self.chunk.code[handler_placeholder + 1] = (catch_target & 0xFF) as u8;
        if iter_slot <= u8::MAX as u16 {
            self.chunk.emit_op_u8(OpCode::GetLocal, iter_slot as u8, line);
        } else {
            self.chunk.emit_op_u16(OpCode::GetLocalWide, iter_slot, line);
        }
        self.chunk.emit_op(OpCode::IteratorClose, line);
        self.chunk.emit_op(OpCode::Throw, line);
        self.chunk.patch_jump(skip_catch);

        // The iterator's stack slot was consumed by IteratorClose — drop the
        // compiler-side local entry without emitting another pop.
        debug_assert_eq!(self.locals.last().map(|l| l.name), Some(anon));
        self.locals.pop();

        if !is_var { self.end_scope(); }
        Ok(())
    }

    pub(super) fn compile_switch(&mut self, s: &SwitchStatement) -> Result<(), String> {
        let line = s.span.start;
        // Switch completion starts at undefined; matched case bodies overwrite it.
        self.emit_completion_reset(line);
        let outer_depth = self.scope_depth;
        self.compile_expr(&s.discriminant)?;
        // The CaseBlock is its own lexical scope (let/const/class declared in a
        // case are block-scoped to the switch, not leaked to the enclosing scope).
        // The discriminant stays on the stack through the case bodies, so reserve a
        // slot for it as an anonymous local — otherwise the first declared local
        // would alias the discriminant's stack slot.
        self.begin_scope();
        let disc_slot_name = self.interner.intern("(switch discriminant)");
        self.add_local(disc_slot_name);
        self.mark_initialized();

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
            // `break` must pop the case-block locals AND the discriminant slot,
            // so target the depth outside the switch's lexical scope.
            scope_depth: outer_depth,
            label: None,
            has_deferred_continue: false,
            try_depth: self.finally_stack.len(),
            with_depth: self.with_depth,
            is_switch: true,
            for_of_iter_slot: None,
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

        // End the case-block scope: pops all case-declared locals and the
        // discriminant slot reserved above (fall-through / no-match path).
        self.end_scope();

        self.patch_loop_breaks();
        Ok(())
    }

    /// Unwind the constructs crossed by an abrupt exit (break/continue/
    /// return): inlined `finally` bodies and for-of iterator handlers, in
    /// reverse nesting order. `target_loop` is the loop being exited to
    /// (None for return); crossed for-of loops get their handler popped and
    /// iterator closed, while the target loop itself (`pop_target_handler`)
    /// only pops its per-iteration handler — its own epilogue or next
    /// iteration handles the iterator. Runs BEFORE locals are popped: both
    /// finalizer bodies and the iterator reads address live stack slots.
    pub(super) fn compile_abrupt_unwind(
        &mut self,
        target_loop: Option<usize>,
        target_try_depth: usize,
        pop_target_handler: bool,
        line: u32,
    ) -> Result<(), String> {
        // Detach the levels being unwound while their bodies compile — a
        // `return` / `break` INSIDE an inlined finally calls back into this
        // function, and with the levels still on the stack it would
        // re-inline the same blocks forever (stack overflow at compile time).
        let detached = self.finally_stack.split_off(target_try_depth);
        let emit_level = |this: &mut Self, level: &(Option<std::rc::Rc<Vec<Statement>>>, bool)| -> Result<(), String> {
            let (finally_opt, handler_active) = level;
            // Inside a catch block the runtime handler is already gone
            // (handle_throw popped it on entry).
            if *handler_active {
                this.chunk.emit_op(OpCode::PopExcHandler, line);
            }
            if let Some(stmts_rc) = finally_opt {
                let stmts = (**stmts_rc).clone();
                for stmt in &stmts {
                    this.compile_statement(stmt)?;
                }
            }
            Ok(())
        };
        let mut fin_abs = target_try_depth + detached.len();
        let lo = target_loop.map(|i| i + 1).unwrap_or(0);
        let mut li = self.loops.len();
        while li > lo {
            li -= 1;
            // Finallys registered inside this loop unwind before its handler.
            let boundary = self.loops[li].try_depth.max(target_try_depth);
            while fin_abs > boundary {
                fin_abs -= 1;
                let level = detached[fin_abs - target_try_depth].clone();
                emit_level(self, &level)?;
            }
            if let Some(slot) = self.loops[li].for_of_iter_slot {
                self.chunk.emit_op(OpCode::PopExcHandler, line);
                if slot <= u8::MAX as u16 {
                    self.chunk.emit_op_u8(OpCode::GetLocal, slot as u8, line);
                } else {
                    self.chunk.emit_op_u16(OpCode::GetLocalWide, slot, line);
                }
                self.chunk.emit_op(OpCode::IteratorClose, line);
            }
        }
        while fin_abs > target_try_depth {
            fin_abs -= 1;
            let level = detached[fin_abs - target_try_depth].clone();
            emit_level(self, &level)?;
        }
        if pop_target_handler
            && let Some(ti) = target_loop
            && self.loops[ti].for_of_iter_slot.is_some()
        {
            self.chunk.emit_op(OpCode::PopExcHandler, line);
        }
        self.finally_stack.extend(detached);
        Ok(())
    }

    pub(super) fn compile_return(&mut self, r: &ReturnStatement) -> Result<(), String> {
        let line = r.span.start;
        // A return from inside try blocks must unwind their exception
        // handlers (and run finally bodies) — otherwise the handlers go
        // stale on the VM's exc_handlers stack and a LATER unrelated throw
        // "returns" to a dead frame's catch target, jumping mid-instruction.
        // DuckDuckGo's localStorage probe (`try { ...; return e=!0 }`)
        // surfaced exactly that. The argument compiles BEFORE the unwind so
        // a throw during its evaluation still reaches this try's catch.
        if let Some(arg) = &r.argument {
            self.compile_expr(arg)?;
            self.compile_abrupt_unwind(None, 0, false, line)?;
            self.chunk.emit_op(OpCode::Return, line);
        } else {
            self.compile_abrupt_unwind(None, 0, false, line)?;
            self.chunk.emit_op(OpCode::ReturnUndefined, line);
        }
        Ok(())
    }


    pub(super) fn compile_break(&mut self, b: &BreakStatement) -> Result<(), String> {
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
        let target_with_depth = self.loops[target_idx].with_depth;
        // Unwind (finally bodies + crossed for-of iterator closes) runs
        // BEFORE the locals are popped: both reference stack slots that
        // PopN destroys.
        self.compile_abrupt_unwind(Some(target_idx), target_try_depth, true, line)?;
        let pop_n = self.locals_above_depth(loop_depth);
        if pop_n > 0 && pop_n <= u8::MAX as usize {
            self.chunk.emit_op_u8(OpCode::PopN, pop_n as u8, line);
        } else {
            for _ in 0..pop_n {
                self.chunk.emit_op(OpCode::Pop, line);
            }
        }
        // Exit any `with` scopes between here and the target loop.
        for _ in target_with_depth..self.with_depth {
            self.chunk.emit_op(OpCode::WithExit, line);
        }
        let patch = self.chunk.emit_jump(OpCode::Jump, line);
        self.loops[target_idx].break_patches.push(patch);
        Ok(())
    }

    pub(super) fn compile_continue(&mut self, c: &ContinueStatement) -> Result<(), String> {
        let line = c.span.start;
        if self.loops.is_empty() {
            return Err(format!("'continue' outside of loop at offset {line}"));
        }
        // Find the matching loop context. `continue` targets an iteration
        // statement only — never a `switch` (which is break-only), so skip
        // switch contexts when resolving both labeled and unlabeled forms.
        let ctx_idx = if let Some(label) = c.label {
            self.loops.iter().rposition(|ctx| ctx.label == Some(label) && !ctx.is_switch)
                .ok_or_else(|| format!("label not found at offset {line}"))?
        } else {
            self.loops.iter().rposition(|ctx| !ctx.is_switch)
                .ok_or_else(|| format!("'continue' outside of loop at offset {line}"))?
        };
        let target = self.loops[ctx_idx].continue_target;
        let loop_depth = self.loops[ctx_idx].scope_depth;
        let target_try_depth = self.loops[ctx_idx].try_depth;
        let target_with_depth = self.loops[ctx_idx].with_depth;
        let deferred = self.loops[ctx_idx].has_deferred_continue;

        // Unwind before PopN — see compile_break.
        self.compile_abrupt_unwind(Some(ctx_idx), target_try_depth, true, line)?;
        let pop_n = self.locals_above_depth(loop_depth);
        if pop_n > 0 && pop_n <= u8::MAX as usize {
            self.chunk.emit_op_u8(OpCode::PopN, pop_n as u8, line);
        } else {
            for _ in 0..pop_n {
                self.chunk.emit_op(OpCode::Pop, line);
            }
        }
        // Exit any `with` scopes between here and the target loop.
        for _ in target_with_depth..self.with_depth {
            self.chunk.emit_op(OpCode::WithExit, line);
        }
        if deferred {
            // Emit a forward jump; it will be patched to the update position later
            let patch = self.chunk.emit_jump(OpCode::Jump, line);
            self.loops[ctx_idx].continue_patches.push(patch);
        } else {
            self.chunk.emit_loop(target, line);
        }
        Ok(())
    }

    pub(super) fn compile_throw(&mut self, t: &ThrowStatement) -> Result<(), String> {
        self.compile_expr(&t.argument)?;
        self.chunk.emit_op(OpCode::Throw, t.span.start);
        Ok(())
    }

    pub(super) fn compile_try(&mut self, t: &TryStatement) -> Result<(), String> {
        let line = t.span.start;

        // try/catch/finally lowers to try { try/catch } finally — the
        // finalizer must also run when the CATCH block throws or exits
        // early, which the flat form couldn't express (the runtime handler
        // is already popped once the catch is entered).
        if t.handler.is_some() && t.finalizer.is_some() {
            let inner = TryStatement {
                block: t.block.clone(),
                handler: t.handler.clone(),
                finalizer: None,
                span: t.span,
            };
            let outer = TryStatement {
                block: BlockStatement {
                    body: vec![Statement::Try(Box::new(inner))],
                    span: t.span,
                },
                handler: None,
                finalizer: t.finalizer.clone(),
                span: t.span,
            };
            return self.compile_try(&outer);
        }

        // Emit PushExcHandler with placeholder offsets for catch and finally.
        // Layout: [PushExcHandler, catch_hi, catch_lo, finally_hi, finally_lo]
        let _handler_pos = self.chunk.len();
        self.chunk
            .emit_byte(OpCode::PushExcHandler as u8, line);
        let catch_placeholder = self.chunk.code.len();
        self.chunk
            .code
            .extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);

        // Track the finally block so break/continue/return can inline it.
        // handler_active = true: early exits from the try block must also
        // pop the runtime exception handler.
        let finally_rc = t.finalizer.as_ref().map(|f| std::rc::Rc::new(f.body.clone()));
        self.finally_stack.push((finally_rc, true));

        // Compile try block (no scope — var declarations should be global/function-scoped).
        for stmt in &t.block.body {
            self.compile_statement(stmt)?;
        }

        // Entering the catch region: handle_throw pops the handler before
        // jumping here, so early exits from the CATCH block must inline the
        // finally but NOT pop a handler. The entry stays on the stack with
        // the flag flipped so `return`/`break` inside catch still run the
        // finalizer.
        if let Some(entry) = self.finally_stack.last_mut() {
            entry.1 = false;
        }

        self.chunk.emit_op(OpCode::PopExcHandler, line);
        let skip_catch = self.chunk.emit_jump(OpCode::Jump, line);

        // Patch the catch target. PushExcHandler encodes ABSOLUTE u16
        // targets — past 64 KiB of bytecode they'd wrap and the handler
        // would land mid-instruction; poison the chunk instead.
        if self.chunk.len() > u16::MAX as usize {
            self.chunk.jump_overflow = true;
        }
        let catch_target = self.chunk.len() as u16;
        if t.handler.is_some() || t.finalizer.is_some() {
            self.chunk.code[catch_placeholder] = (catch_target >> 8) as u8;
            self.chunk.code[catch_placeholder + 1] = (catch_target & 0xFF) as u8;
        }

        // No catch clause: the handler region is a synthetic catch that runs
        // the finalizer and rethrows the pending exception. A return/break/
        // continue inside the finalizer exits normally, swallowing it.
        if t.handler.is_none()
            && let Some(finalizer) = &t.finalizer
        {
            // Popped here so early exits inside the finalizer body don't
            // re-inline this same finalizer.
            self.finally_stack.pop();
            // handle_throw pushed the exception value; keep it across the
            // finalizer body.
            if self.scope_depth > 0 {
                self.begin_scope();
                let exc_name = self.interner.intern("__pending_exc__");
                self.add_local(exc_name);
                self.mark_initialized();
                let exc_slot = (self.locals.len() - 1) as u16;
                for stmt in &finalizer.body {
                    self.compile_statement(stmt)?;
                }
                if exc_slot <= u8::MAX as u16 {
                    self.chunk.emit_op_u8(OpCode::GetLocal, exc_slot as u8, line);
                } else {
                    self.chunk.emit_op_u16(OpCode::GetLocalWide, exc_slot, line);
                }
                self.chunk.emit_op(OpCode::Throw, line);
                self.end_scope();
            } else {
                let exc_name = self.interner.intern("__pending_exc__");
                let idx = self.make_string_constant(exc_name);
                self.chunk.emit_op_u16(OpCode::DefineGlobal, idx, line);
                for stmt in &finalizer.body {
                    self.compile_statement(stmt)?;
                }
                self.chunk.emit_op_u16(OpCode::GetGlobal, idx, line);
                self.chunk.emit_op(OpCode::Throw, line);
            }
        }

        // Compile catch block.
        if let Some(handler) = &t.handler {
            self.begin_scope();
            match &handler.param {
                Some(Pattern::Identifier(id)) => {
                    self.check_strict_restricted(id.name, "catch parameter")?;
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

        // Leaving try+catch: the finally region itself (below) and any code
        // after must not re-inline this finalizer. (The synthetic no-catch
        // handler above already popped it.)
        if t.handler.is_some() || t.finalizer.is_none() {
            self.finally_stack.pop();
        }

        self.chunk.patch_jump(skip_catch);

        // Compile finally block.
        if let Some(finalizer) = &t.finalizer {
            if self.chunk.len() > u16::MAX as usize {
                self.chunk.jump_overflow = true;
            }
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

    pub(super) fn compile_labeled(&mut self, l: &LabeledStatement) -> Result<(), String> {
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
                with_depth: self.with_depth,
                is_switch: false,
                for_of_iter_slot: None,
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
                with_depth: self.with_depth,
                is_switch: false,
                for_of_iter_slot: None,
            });
            self.compile_statement(&l.body)?;
            self.patch_loop_breaks();
        }
        Ok(())
    }

    pub(super) fn compile_with(&mut self, w: &WithStatement) -> Result<(), String> {
        let line = w.span.start;
        self.compile_expr(&w.object)?;
        self.chunk.emit_op(OpCode::WithEnter, line);
        // WithStatement completion is UpdateEmpty(body, undefined).
        self.emit_completion_reset(line);
        self.with_depth += 1;
        self.with_local_floor.push(self.locals.len());
        self.compile_statement(&w.body)?;
        self.with_local_floor.pop();
        self.with_depth -= 1;
        self.chunk.emit_op(OpCode::WithExit, line);
        Ok(())
    }

    pub(super) fn compile_import(&mut self, i: &ImportDeclaration) -> Result<(), String> {
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

    pub(super) fn compile_export(&mut self, e: &ExportDeclaration) -> Result<(), String> {
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
}
