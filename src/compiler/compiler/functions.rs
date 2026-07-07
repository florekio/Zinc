//! Function, arrow, and class compilation: body chunks, hoisting of vars
//! and function declarations, and strict-mode/flag propagation.

use super::*;

impl<'a> Compiler<'a> {
    pub(super) fn compile_function_decl(&mut self, f: &FunctionDeclaration) -> Result<(), String> {
        let name = f.id.unwrap_or_else(|| self.interner.intern("<anonymous>"));
        let line = f.span.start;

        // At function scope, reserve the local slot BEFORE compiling the body so
        // the function is in scope for itself — recursion (`function e(){…e()…}`)
        // and any nested closure that captures the declaration both resolve it as
        // an upvalue rather than a missing global. This path runs when the
        // top-level hoist was skipped (body has top-level let/const), which modern
        // strict bundles routinely hit. At global scope the name is a global, so
        // self-reference resolves via GetGlobal and no slot is needed.
        let reserved_slot = if self.scope_depth == 0 {
            None
        } else if let Some(slot) = self.resolve_local(name) {
            Some(slot)
        } else {
            self.chunk.emit_op(OpCode::Undefined, line);
            self.add_local(name);
            self.mark_initialized();
            Some(self.locals.len() - 1)
        };

        let child_chunk =
            self.compile_function_body(name, &f.params, &f.body, f.is_async, f.is_generator)?;
        let chunk_idx = self.chunk.child_chunks.len() as u16;
        let upvalue_descs = child_chunk.upvalue_descriptors.clone();
        self.chunk.child_chunks.push(child_chunk);
        self.chunk.emit_op_u16(OpCode::Closure, chunk_idx, line);
        // Emit upvalue descriptors inline after the Closure opcode
        for desc in &upvalue_descs {
            self.chunk.emit_byte(if desc.is_local { 1 } else { 0 }, line);
            self.chunk.emit_byte((desc.index >> 8) as u8, line);
            self.chunk.emit_byte((desc.index & 0xFF) as u8, line);
        }

        match reserved_slot {
            None => {
                let idx = self.make_string_constant(name);
                self.chunk.emit_op_u16(OpCode::DefineGlobal, idx, line);
            }
            Some(slot) => {
                if slot <= u8::MAX as usize {
                    self.chunk.emit_op_u8(OpCode::SetLocal, slot as u8, line);
                } else {
                    self.chunk.emit_op_u16(OpCode::SetLocalWide, slot as u16, line);
                }
                self.chunk.emit_op(OpCode::Pop, line);
            }
        }
        Ok(())
    }

    pub(super) fn compile_class_decl(&mut self, c: &ClassDeclaration) -> Result<(), String> {
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

    pub(super) fn compile_class_body(&mut self, body: &ClassBody, line: u32) -> Result<(), String> {
        // Class bodies (including method bodies) are implicitly strict.
        self.class_depth += 1;
        let r = self.compile_class_body_inner(body, line);
        self.class_depth -= 1;
        r
    }

    pub(super) fn compile_class_body_inner(&mut self, body: &ClassBody, line: u32) -> Result<(), String> {
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
                            // Per spec, computed property names run ToPropertyKey
                            // before the method body / value expression is evaluated.
                            self.chunk.emit_op(OpCode::ToPropertyKey, line);
                        }
                        self.compile_expr(&m.value)?;
                        // Concise methods / getters / setters are not constructable.
                        // Constructors are an exception (compiled via the explicit ctor path).
                        if !matches!(m.kind, MethodKind::Constructor) {
                            self.mark_last_child_as_method();
                        }
                        match m.kind {
                            MethodKind::Get => self.chunk.emit_op(OpCode::DefineGetter, line),
                            MethodKind::Set => self.chunk.emit_op(OpCode::DefineSetter, line),
                            _ => self.chunk.emit_op(OpCode::DefineDataProp, line),
                        }
                        // After accessor-define, the target object is still on stack — pop it.
                        self.chunk.emit_op(OpCode::Pop, line);
                    } else {
                        self.compile_expr(&m.value)?;
                        if !matches!(m.kind, MethodKind::Constructor) {
                            self.mark_last_child_as_method();
                        }
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
                        // Computed key: emit key expr, then value, then computed opcode.
                        // (compile_property_key already emits ToPropertyKey for the
                        // Computed variant.)
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
                        self.chunk.emit_byte((desc.index >> 8) as u8, line);
            self.chunk.emit_byte((desc.index & 0xFF) as u8, line);
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

    /// Hoist `var` names of a function/arrow body: reserve an
    /// undefined-initialized local slot for every var declared anywhere in
    /// the body (parameters keep their slots).
    pub(super) fn hoist_body_vars(&mut self, body_stmts: &[Statement]) {
        let mut hoisted_names = Vec::new();
        for stmt in body_stmts {
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

    /// Hoist top-level function declarations of a function/arrow body:
    /// each `function f() {...}` is initialized at the top with its closure,
    /// shadowing any same-named parameter or var binding (per spec, function
    /// declarations have higher precedence than params/vars in function code).
    /// Returns the hoisted names so the statement loop can skip the
    /// declarations. Shared by compile_function_body_with_self and
    /// compile_arrow_body — webpack module wrappers are arrows whose bodies
    /// rely on hoisting (React's renderer calls helpers declared later).
    ///
    /// Top-level `let` / `const` complicate the hoist: pre-compiling inner
    /// functions would resolve those bindings as missing upvalues since
    /// their slots are not yet allocated. When every top-level lexical is a
    /// simple identifier, we reserve their slots (undefined) BEFORE the
    /// hoist so function bodies resolve them as locals/upvalues; the
    /// declaration statement later assigns into the reserved slot (tracked
    /// in `predeclared_lex`). DuckDuckGo's SSG script needs this — an IIFE
    /// opening with `let e = ...` calls memoized helpers declared after the
    /// call site; skipping the hoist left them undefined at the call.
    ///
    /// Destructuring lexicals (`let {a} = ...`) still skip the hoist —
    /// their binding path allocates slots mid-pattern and can't be
    /// pre-reserved without reworking it. Note TDZ is currently NOT
    /// enforced at runtime (InitLet is a no-op), so reserving as undefined
    /// does not change observable TDZ behavior.
    pub(super) fn hoist_body_functions(
        &mut self,
        body_stmts: &[Statement],
        params: &[Pattern],
    ) -> Result<Vec<StringId>, String> {
        let has_top_level_lex = body_stmts.iter().any(|s| matches!(
            s,
            Statement::Variable(d) if matches!(d.kind, VarKind::Let | VarKind::Const)
        ));
        let lex_all_simple = body_stmts.iter().all(|s| match s {
            Statement::Variable(d) if matches!(d.kind, VarKind::Let | VarKind::Const) => d
                .declarations
                .iter()
                .all(|dec| matches!(&dec.id, Pattern::Identifier(_))),
            _ => true,
        });
        let has_fn_decls = body_stmts
            .iter()
            .any(|s| matches!(s, Statement::Function(f) if f.id.is_some()));
        let do_hoist = !has_top_level_lex || (lex_all_simple && has_fn_decls);
        let mut hoisted_fns: Vec<StringId> = Vec::new();
        // Function declaration named `arguments` shouldn't shadow the arguments
        // object when params have expressions (defaults/destructuring/rest), per
        // spec — the arguments object is required and the user-visible binding.
        let has_param_exprs = params.iter().any(|p| matches!(
            p,
            Pattern::Assignment(_) | Pattern::Array(_) | Pattern::Object(_) | Pattern::Rest(_)
        ));
        let arguments_id = self.interner.intern("arguments");
        if do_hoist {
            // Reserve slots for top-level lexicals first (see above)
            // so the hoisted function bodies can capture them.
            if has_top_level_lex {
                for stmt in body_stmts {
                    if let Statement::Variable(d) = stmt
                        && matches!(d.kind, VarKind::Let | VarKind::Const)
                    {
                        for dec in &d.declarations {
                            if let Pattern::Identifier(id) = &dec.id
                                && self.resolve_local(id.name).is_none()
                            {
                                self.chunk.emit_op(OpCode::Undefined, dec.span.start);
                                self.add_local(id.name);
                                self.mark_initialized();
                                self.predeclared_lex.push(id.name);
                            }
                        }
                    }
                }
            }
            // Two-pass hoist so a function declaration is in scope *before* its
            // own body compiles — otherwise a nested function that references
            // the declaration (recursion, or a closure that calls back into a
            // sibling) resolves it as a missing global. Pass 1 reserves a local
            // slot for every hoisted name; pass 2 compiles the bodies and
            // assigns each closure into its reserved slot.
            let mut hoist_targets: Vec<(StringId, usize, usize)> = Vec::new(); // (name, slot, stmt_idx)
            for (idx, stmt) in body_stmts.iter().enumerate() {
                if let Statement::Function(f) = stmt
                    && let Some(name) = f.id
                {
                    if name == arguments_id
                        && (has_param_exprs || !params.iter().any(|p| matches!(p, Pattern::Identifier(id) if id.name == arguments_id)))
                    {
                        continue;
                    }
                    let line = f.span.start;
                    let slot = if let Some(slot) = self.resolve_local(name) {
                        slot
                    } else {
                        // Reserve a slot initialized to undefined; pass 2 overwrites it.
                        self.chunk.emit_op(OpCode::Undefined, line);
                        self.add_local(name);
                        self.mark_initialized();
                        self.locals.len() - 1
                    };
                    hoist_targets.push((name, slot, idx));
                    hoisted_fns.push(name);
                }
            }
            for (name, slot, idx) in hoist_targets {
                let Statement::Function(f) = &body_stmts[idx] else { unreachable!() };
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
                if slot <= u8::MAX as usize {
                    self.chunk.emit_op_u8(OpCode::SetLocal, slot as u8, line);
                } else {
                    self.chunk.emit_op_u16(OpCode::SetLocalWide, slot as u16, line);
                }
                self.chunk.emit_op(OpCode::Pop, line);
            }
        }
        Ok(hoisted_fns)
    }

    pub(super) fn compile_function_body(
        &mut self,
        name: StringId,
        params: &[Pattern],
        body: &BlockStatement,
        is_async: bool,
        is_generator: bool,
    ) -> Result<Chunk, String> {
        self.compile_function_body_with_self(name, params, body, is_async, is_generator, None)
    }

    pub(super) fn compile_function_body_with_self(
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
        // Inherit strict mode from parent, or detect "use strict" directive,
        // or if we're inside a class body (class methods are implicitly strict).
        if self.chunk.flags.contains(ChunkFlags::STRICT)
            || self.has_use_strict_directive(&body.body)
            || self.class_depth > 0
        {
            flags |= ChunkFlags::STRICT;
        }
        child_chunk.flags = flags;

        // Record binding names for direct-eval-in-parameter early errors.
        let mut pnames: Vec<StringId> = Vec::new();
        for p in params { collect_pattern_names(p, &mut pnames); }
        let has_param_exprs = params.iter().any(|p| matches!(p,
            Pattern::Assignment(_) | Pattern::Array(_) | Pattern::Object(_) | Pattern::Rest(_)));
        if has_param_exprs {
            // Non-arrow functions bind `arguments` in the parameter scope, so a
            // direct eval in a parameter default cannot redeclare it.
            pnames.push(self.interner.intern("arguments"));
        }
        child_chunk.param_names = pnames;
        child_chunk.lexical_names = collect_body_lexical_names(&body.body);

        // Swap compiler state -- push parent's locals + upvalues onto the
        // enclosing chain so nested functions can capture them transitively.
        let parent_chunk = std::mem::replace(&mut self.chunk, child_chunk);
        let parent_locals = std::mem::take(&mut self.locals);
        let parent_upvalues = std::mem::take(&mut self.upvalues);
        let parent_depth = self.scope_depth;
        let parent_loops = std::mem::take(&mut self.loops);
        let parent_predeclared_lex = std::mem::take(&mut self.predeclared_lex);
        // Fresh per-function: a `return` must only unwind try handlers of
        // ITS OWN function, never the enclosing one's.
        let parent_finally_stack = std::mem::take(&mut self.finally_stack);
        // `with` nesting does not cross function boundaries.
        let parent_with_depth = std::mem::take(&mut self.with_depth);
        let parent_with_local_floor = std::mem::take(&mut self.with_local_floor);
        self.enclosing_chain.push(EnclosingFrame {
            locals: parent_locals,
            upvalues: parent_upvalues,
        });

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
                // Default value expression (a direct eval here is in the
                // parameter scope — mark it so var/function decls early-error).
                self.chunk.emit_op(OpCode::BeginParamExpr, line);
                self.compile_expr(&a.right)?;
                self.chunk.emit_op(OpCode::EndParamExpr, line);
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
        self.hoist_body_vars(&body.body);

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

        let hoisted_fns = self.hoist_body_functions(&body.body, params)?;

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

        // Pop the parent's frame off the chain. Its locals (captured flags
        // updated) and upvalues (any transitively-threaded upvalues added
        // while compiling this child) are restored directly — no copy-back
        // needed, the frame *is* the parent's state.
        let parent_frame = self.enclosing_chain.pop().expect("enclosing frame");
        self.locals = parent_frame.locals;
        self.upvalues = parent_frame.upvalues;
        self.scope_depth = parent_depth;
        self.loops = parent_loops;
        self.predeclared_lex = parent_predeclared_lex;
        self.finally_stack = parent_finally_stack;
        self.with_depth = parent_with_depth;
        self.with_local_floor = parent_with_local_floor;

        if compiled.jump_overflow {
            let name = self.interner.resolve(compiled.name).to_owned();
            return Err(format!(
                "function '{name}' too large: a jump offset exceeded the i16 encoding"
            ));
        }
        Ok(compiled)
    }

    pub(super) fn compile_arrow_body(
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
        // Inherit strict mode from the enclosing code (or detect the arrow's
        // own "use strict" directive; class bodies are implicitly strict).
        let arrow_strict = self.chunk.flags.contains(ChunkFlags::STRICT)
            || self.class_depth > 0
            || matches!(body, ArrowBody::Block(b) if self.has_use_strict_directive(&b.body));
        if arrow_strict {
            child_chunk.flags |= ChunkFlags::STRICT;
        }
        // Binding names for direct-eval-in-parameter early errors. Arrows have no
        // implicit `arguments`, so only explicit params / body lexicals collide.
        let mut pnames: Vec<StringId> = Vec::new();
        for p in params { collect_pattern_names(p, &mut pnames); }
        child_chunk.param_names = pnames;
        if let ArrowBody::Block(b) = body {
            child_chunk.lexical_names = collect_body_lexical_names(&b.body);
        }

        let parent_chunk = std::mem::replace(&mut self.chunk, child_chunk);
        let parent_locals = std::mem::take(&mut self.locals);
        let parent_upvalues = std::mem::take(&mut self.upvalues);
        let parent_depth = self.scope_depth;
        let parent_loops = std::mem::take(&mut self.loops);
        let parent_predeclared_lex = std::mem::take(&mut self.predeclared_lex);
        let parent_finally_stack = std::mem::take(&mut self.finally_stack);
        // `with` nesting does not cross function boundaries.
        let parent_with_depth = std::mem::take(&mut self.with_depth);
        let parent_with_local_floor = std::mem::take(&mut self.with_local_floor);
        // Push parent's scope onto the chain so the arrow body (and anything
        // nested in it) can capture transitively.
        self.enclosing_chain.push(EnclosingFrame {
            locals: parent_locals,
            upvalues: parent_upvalues,
        });

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
                self.chunk.emit_op(OpCode::BeginParamExpr, line);
                self.compile_expr(&a.right)?;
                self.chunk.emit_op(OpCode::EndParamExpr, line);
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
                // Same var + function-declaration hoisting as plain function
                // bodies — webpack module wrappers `(e,t,n)=>{...}` declare
                // helpers after their first use (React's framework chunk).
                self.hoist_body_vars(&block.body);
                let hoisted_fns = self.hoist_body_functions(&block.body, params)?;
                for stmt in &block.body {
                    if let Statement::Function(f) = stmt
                        && let Some(name) = f.id
                        && hoisted_fns.contains(&name)
                    {
                        continue;
                    }
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

        // Pop the parent frame; its locals (captured flags set during
        // resolution) and upvalues (transitively threaded) are restored
        // directly.
        let parent_frame = self.enclosing_chain.pop().expect("enclosing frame");
        self.locals = parent_frame.locals;
        self.upvalues = parent_frame.upvalues;
        self.scope_depth = parent_depth;
        self.loops = parent_loops;
        self.predeclared_lex = parent_predeclared_lex;
        self.finally_stack = parent_finally_stack;
        self.with_depth = parent_with_depth;
        self.with_local_floor = parent_with_local_floor;

        if compiled.jump_overflow {
            let name = self.interner.resolve(compiled.name).to_owned();
            return Err(format!(
                "function '{name}' too large: a jump offset exceeded the i16 encoding"
            ));
        }
        Ok(compiled)
    }

    pub(super) fn compile_function_expr(&mut self, f: &FunctionExpression) -> Result<(), String> {
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
            self.chunk.emit_byte((desc.index >> 8) as u8, line);
            self.chunk.emit_byte((desc.index & 0xFF) as u8, line);
        }
        Ok(())
    }

    pub(super) fn is_anonymous_fn_def(expr: &Expression) -> bool {
        match expr {
            Expression::Function(f) => f.id.is_none(),
            Expression::ArrowFunction(_) => true,
            Expression::Class(c) => c.id.is_none(),
            _ => false,
        }
    }

    pub(super) fn compile_arrow_expr(&mut self, a: &ArrowFunctionExpression) -> Result<(), String> {
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
            self.chunk.emit_byte((desc.index >> 8) as u8, line);
            self.chunk.emit_byte((desc.index & 0xFF) as u8, line);
        }
        Ok(())
    }

    pub(super) fn compile_class_expr(&mut self, c: &ClassExpression) -> Result<(), String> {
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
}
