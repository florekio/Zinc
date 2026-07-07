//! Destructuring: binding patterns in declarations, assignment patterns,
//! and iterator/object destructuring with defaults and rest elements.

use super::*;

impl<'a> Compiler<'a> {
    /// value is on stack; if it is `undefined`, replace with the default expression
    pub(super) fn emit_default_check(&mut self, right: &Expression, line: u32) -> Result<(), String> {
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
    pub(super) fn compile_bind_value_local(&mut self, pat: &Pattern, line: u32) -> Result<(), String> {
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
    pub(super) fn compile_bind_arr_elems_local(&mut self, elements: &[Option<Pattern>], iter_slot: u8, line: u32) -> Result<(), String> {
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
    pub(super) fn compile_bind_obj_props_local(&mut self, properties: &[ObjectPatternProperty], src_slot: u8, line: u32) -> Result<(), String> {
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
    pub(super) fn compile_bind_value_global(&mut self, pat: &Pattern, line: u32) -> Result<(), String> {
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
    pub(super) fn compile_bind_arr_elems_global(&mut self, elements: &[Option<Pattern>], line: u32) -> Result<(), String> {
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
    pub(super) fn compile_bind_obj_props_global(&mut self, properties: &[ObjectPatternProperty], line: u32) -> Result<(), String> {
        // RequireObjectCoercible on TOS source, then save to a temp local so
        // we can read it at a known stack slot from each property iteration.
        // This avoids leaving the source on the operand stack where nested
        // array/object destructuring would mistake its slot for the iterator's.
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
        // Source is still on stack — register it as a temp local so iter_slot
        // calculations are stable even when nested destructuring pushes more
        // operands.
        let src_slot = self.locals.len() as u8;
        let anon_src = self.interner.intern("__obj_src__");
        self.add_local(anon_src);
        self.mark_initialized();
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
                        self.chunk.emit_op_u8(OpCode::GetLocal, src_slot, line);
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
        // Drop the temp source local. Caller still expects the original source
        // value on the stack (compile_bind_value_global emits a Pop after us).
        self.locals.pop();
        Ok(())
    }

    /// Convert a destructuring expression (LHS of for-of) into the equivalent
    /// Pattern so it can be compiled via `compile_assign_pat`.
    /// Returns `None` for elisions/unsupported forms.
    pub(super) fn expr_to_pattern(expr: &Expression) -> Option<Pattern> {
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
    pub(super) fn compile_assign_pat(&mut self, pat: &Pattern, line: u32) -> Result<(), String> {
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

    /// Destructure a pattern, using the value stored in local slot `src_slot`.
    /// New identifier bindings are added as locals.
    pub(super) fn destructure_pattern_from_slot(&mut self, pat: &Pattern, src_slot: u8, line: u32) -> Result<(), String> {
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

    /// Destructure the value currently on top of the stack into `pat`, assigning
    /// to existing variables (compile_set_variable). Consumes the stack top.
    /// Used by nested destructuring assignment like `({x: {y}} = obj)`.
    pub(super) fn compile_assign_to_pattern(&mut self, pat: &Pattern, line: u32) -> Result<(), String> {
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
}
