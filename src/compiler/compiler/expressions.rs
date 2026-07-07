//! Expression compilation: literals, operators, assignments (simple,
//! compound, logical, member), calls, object/array literals, templates,
//! optional chains, and yield/await.

use super::*;

impl<'a> Compiler<'a> {
    pub(super) fn compile_expr(&mut self, expr: &Expression) -> Result<(), String> {
        match expr {
            Expression::NumberLiteral(n) => self.compile_number(n),
            Expression::BigIntLiteral(b) => {
                // Store the digit string as a constant; LoadBigInt parses it
                // into a heap BigInt at runtime (Value can't hold one directly).
                let idx = self.chunk.add_constant(Value::string(b.raw));
                self.chunk.emit_op_u16(OpCode::LoadBigInt, idx, b.span.start);
                Ok(())
            }
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

    pub(super) fn compile_number(&mut self, n: &NumberLiteral) -> Result<(), String> {
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

    pub(super) fn compile_string_lit(&mut self, s: &StringLiteral) -> Result<(), String> {
        let idx = self.chunk.add_constant(Value::string(s.value));
        self.chunk.emit_op_u16(OpCode::Const, idx, s.span.start);
        Ok(())
    }

    pub(super) fn compile_identifier(&mut self, id: &Identifier) -> Result<(), String> {
        let line = id.span.start;
        let name_str = self.interner.resolve(id.name);
        if name_str == "undefined" {
            self.chunk.emit_op(OpCode::Undefined, line);
            return Ok(());
        }
        self.compile_get_variable(id.name, line)
    }

    pub(super) fn compile_binary(&mut self, b: &BinaryExpression) -> Result<(), String> {
        let line = b.span.start;
        // `#name in obj` — PrivateIdentifier is parsed as an Identifier with a
        // leading `#`. Detect that exact form here and emit HasPrivate.
        if b.operator == BinaryOperator::In
            && let Expression::Identifier(id) = &b.left
        {
            let name = self.interner.resolve(id.name).to_owned();
            if name.starts_with('#') {
                self.compile_expr(&b.right)?;
                let cidx = self.chunk.add_constant(crate::runtime::value::Value::string(id.name));
                self.chunk.emit_op_u16(OpCode::HasPrivate, cidx, b.span.start);
                return Ok(());
            }
        }
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

    pub(super) fn compile_unary(&mut self, u: &UnaryExpression) -> Result<(), String> {
        let line = u.span.start;

        // typeof <identifier> must not throw ReferenceError on undeclared
        // globals — but only genuinely global/undeclared names go through
        // TypeOfGlobal. A name that resolves to a local *or an upvalue* must
        // read that binding normally; otherwise `typeof <captured-var>` looked
        // the name up as a (missing) global and wrongly returned "undefined"
        // (e.g. Babel's `typeof SomeClass === "function"` checks on a closed-
        // over class, which then took a fallback branch and left the class
        // undefined downstream). resolve_upvalue is deduplicated, so calling
        // it here and again during the normal compile below is safe.
        if u.operator == UnaryOperator::TypeOf
            && let Expression::Identifier(id) = &u.argument
                && self.resolve_local(id.name).is_none()
                && self.resolve_upvalue(id.name).is_none() {
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

    pub(super) fn compile_delete(&mut self, argument: &Expression, line: u32) -> Result<(), String> {
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

    pub(super) fn compile_update(&mut self, u: &UpdateExpression) -> Result<(), String> {
        let line = u.span.start;
        let inc_op = match u.operator {
            UpdateOperator::Increment => OpCode::Inc,
            UpdateOperator::Decrement => OpCode::Dec,
        };

        match &u.argument {
            Expression::Identifier(id) => {
                // Inside a `with` body, ++/-- must resolve its reference once
                // and write back through it (see compound assignment).
                if self.ident_needs_with_ref(id.name) {
                    let idx = self.make_string_constant(id.name);
                    self.chunk.emit_op_u16(OpCode::WithRefResolve, idx, line); // [ref]
                    self.compile_get_variable_resolved(id.name, line)?;       // [ref, old]
                    if u.prefix {
                        self.chunk.emit_op(inc_op, line);                      // [ref, new]
                        self.compile_set_variable_resolved(id.name, line)?;    // [new]
                    } else {
                        self.chunk.emit_op(OpCode::ToNumeric, line);           // [ref, oldn]
                        self.chunk.emit_op(OpCode::Dup, line);                 // [ref, oldn, oldn]
                        self.chunk.emit_op(inc_op, line);                      // [ref, oldn, new]
                        self.chunk.emit_op(OpCode::Rot3, line);                // [new, ref, oldn]
                        self.chunk.emit_op(OpCode::Rot3, line);                // [oldn, new, ref]
                        self.chunk.emit_op(OpCode::Swap, line);                // [oldn, ref, new]
                        self.compile_set_variable_resolved(id.name, line)?;    // [oldn, new]
                        self.chunk.emit_op(OpCode::Pop, line);                 // [oldn]
                    }
                    return Ok(());
                }
                self.compile_get_variable(id.name, line)?;
                if u.prefix {
                    self.chunk.emit_op(inc_op, line);
                    self.compile_set_variable(id.name, line)?;
                } else {
                    // postfix: apply ToNumeric to old value, then inc/dec the copy
                    self.chunk.emit_op(OpCode::ToNumeric, line); // keeps BigInt; coerces else
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
                        self.compile_expr(key)?;              // [obj, key]
                        if u.prefix {
                            self.chunk.emit_op(OpCode::Dup2, line);       // [obj, key, obj, key]
                            self.chunk.emit_op(OpCode::GetElement, line); // [obj, key, old]
                            self.chunk.emit_op(inc_op, line);             // [obj, key, new]
                            self.chunk.emit_op(OpCode::SetElement, line); // [new]
                        } else {
                            // postfix: result is ToNumeric(old); store old±1.
                            self.chunk.emit_op(OpCode::Dup2, line);       // [obj, key, obj, key]
                            self.chunk.emit_op(OpCode::GetElement, line); // [obj, key, old]
                            self.chunk.emit_op(OpCode::ToNumeric, line);  // [obj, key, n]
                            self.chunk.emit_op(OpCode::Rot3, line);       // [n, obj, key]
                            self.chunk.emit_op(OpCode::Dup2, line);       // [n, obj, key, obj, key]
                            self.chunk.emit_op(OpCode::GetElement, line); // [n, obj, key, old]
                            self.chunk.emit_op(inc_op, line);             // [n, obj, key, new]
                            self.chunk.emit_op(OpCode::SetElement, line); // [n, new]
                            self.chunk.emit_op(OpCode::Pop, line);        // [n]
                        }
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

    pub(super) fn compile_logical(&mut self, l: &LogicalExpression) -> Result<(), String> {
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

    pub(super) fn compile_conditional(&mut self, c: &ConditionalExpression) -> Result<(), String> {
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

    pub(super) fn compile_assignment(&mut self, a: &AssignmentExpression) -> Result<(), String> {
        let line = a.span.start;
        match &a.left {
            AssignmentTarget::Identifier(id) => {
                if a.operator == AssignmentOperator::Assign {
                    if Self::is_anonymous_fn_def(&a.right) {
                        self.pending_function_name = Some(id.name);
                    }
                    // Inside a `with` body, the target reference must be
                    // resolved BEFORE the RHS runs: `x = (scope.x = 2, 1)`
                    // must not see the with-object property the RHS creates.
                    if self.ident_needs_with_ref(id.name) {
                        let idx = self.make_string_constant(id.name);
                        self.chunk.emit_op_u16(OpCode::WithRefResolve, idx, line);
                        self.compile_expr(&a.right)?;
                        self.compile_set_variable_resolved(id.name, line)?;
                        return Ok(());
                    }
                    self.compile_expr(&a.right)?;
                } else {
                    // Arithmetic compound assignment inside a `with` body:
                    // resolve the reference ONCE, read and write through it.
                    // (A getter that deletes the property must not make the
                    // write-back fall through to the static binding.)
                    let is_logical = matches!(
                        a.operator,
                        AssignmentOperator::AndAssign
                            | AssignmentOperator::OrAssign
                            | AssignmentOperator::NullishAssign
                    );
                    if !is_logical && self.ident_needs_with_ref(id.name) {
                        let idx = self.make_string_constant(id.name);
                        self.chunk.emit_op_u16(OpCode::WithRefResolve, idx, line); // [ref]
                        self.compile_get_variable_resolved(id.name, line)?;       // [ref, old]
                        self.compile_expr(&a.right)?;                             // [ref, old, rhs]
                        self.emit_compound_arith(a.operator, line)?;              // [ref, result]
                        self.compile_set_variable_resolved(id.name, line)?;       // [result]
                        return Ok(());
                    }
                    self.compile_get_variable(id.name, line)?;

                    // Logical assignment operators need short-circuit. Per spec,
                    // when the RHS is an anonymous function definition and the
                    // LHS is an identifier, NamedEvaluation gives it the LHS name.
                    let lazy_name = if Self::is_anonymous_fn_def(&a.right) {
                        Some(id.name)
                    } else { None };
                    match a.operator {
                        AssignmentOperator::AndAssign => {
                            let jump = self.chunk.emit_jump(OpCode::JumpIfFalsePeek, line);
                            self.chunk.emit_op(OpCode::Pop, line);
                            if let Some(n) = lazy_name { self.pending_function_name = Some(n); }
                            self.compile_expr(&a.right)?;
                            self.chunk.patch_jump(jump);
                            self.compile_set_variable(id.name, line)?;
                            return Ok(());
                        }
                        AssignmentOperator::OrAssign => {
                            let jump = self.chunk.emit_jump(OpCode::JumpIfTruePeek, line);
                            self.chunk.emit_op(OpCode::Pop, line);
                            if let Some(n) = lazy_name { self.pending_function_name = Some(n); }
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
                            if let Some(n) = lazy_name { self.pending_function_name = Some(n); }
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
                                    Pattern::Member(m) => {
                                        // Stack: [..., source, value]. Assign value to obj.prop.
                                        self.compile_expr(&m.object)?;
                                        match &m.property {
                                            MemberProperty::Identifier(id) => {
                                                let idx = self.make_string_constant(*id);
                                                self.chunk.emit_op(OpCode::Swap, line);
                                                self.emit_set_property(idx, line);
                                                self.chunk.emit_op(OpCode::Pop, line);
                                            }
                                            MemberProperty::Expression(expr) => {
                                                self.compile_expr(expr)?;
                                                // [value, obj, key] -> [obj, key, value]
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
                                        match &a.left {
                                            Pattern::Identifier(id) => {
                                                self.compile_set_variable(id.name, line)?;
                                                self.chunk.emit_op(OpCode::Pop, line);
                                            }
                                            Pattern::Member(_)
                                            | Pattern::Object(_)
                                            | Pattern::Array(_) => {
                                                self.compile_assign_to_pattern(&a.left, line)?;
                                            }
                                            _ => { self.chunk.emit_op(OpCode::Pop, line); }
                                        }
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

    pub(super) fn compile_member_assignment(
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
    pub(super) fn emit_logical_member_assign_inline(
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
    pub(super) fn emit_logical_elem_assign(
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

    pub(super) fn emit_logical_priv_assign(
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

    pub(super) fn emit_compound_arith(&mut self, op: AssignmentOperator, line: u32) -> Result<(), String> {
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

    pub(super) fn compile_sequence(&mut self, s: &SequenceExpression) -> Result<(), String> {
        for (i, expr) in s.expressions.iter().enumerate() {
            self.compile_expr(expr)?;
            if i < s.expressions.len() - 1 {
                self.chunk.emit_op(OpCode::Pop, self.current_line());
            }
        }
        Ok(())
    }

    pub(super) fn compile_member(&mut self, m: &MemberExpression) -> Result<(), String> {
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

    pub(super) fn compile_call(&mut self, c: &CallExpression) -> Result<(), String> {
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
            let has_spread = c.arguments.iter().any(|a| matches!(a, Expression::Spread(_)));
            if has_spread {
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
                self.chunk.emit_op_u8(OpCode::SpreadCall, 0, line);
            } else {
                for arg in &c.arguments {
                    self.compile_expr(arg)?;
                }
                self.chunk.emit_op_u8(OpCode::Call, argc, line);
            }
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

    /// Mark the most recently added child chunk as a concise method.
    /// Concise methods have no [[Construct]] slot.
    pub(super) fn mark_last_child_as_method(&mut self) {
        if let Some(child) = self.chunk.child_chunks.last_mut() {
            child.flags |= ChunkFlags::METHOD;
        }
    }

    pub(super) fn compile_new(&mut self, n: &NewExpression) -> Result<(), String> {
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

    pub(super) fn compile_array(&mut self, a: &ArrayExpression) -> Result<(), String> {
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

    pub(super) fn compile_object(&mut self, o: &ObjectExpression) -> Result<(), String> {
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

    pub(super) fn compile_object_property(&mut self, p: &Property, line: u32) -> Result<(), String> {
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
                // Concise methods (`{ method() {} }`) are not constructable.
                if p.method {
                    self.mark_last_child_as_method();
                }
                self.chunk.emit_op(OpCode::DefineDataProp, line);
            }
            PropertyKindVal::Get => {
                self.compile_expr(&p.value)?;
                self.mark_last_child_as_method();
                self.chunk.emit_op(OpCode::DefineGetter, line);
            }
            PropertyKindVal::Set => {
                self.compile_expr(&p.value)?;
                self.mark_last_child_as_method();
                self.chunk.emit_op(OpCode::DefineSetter, line);
            }
        }
        Ok(())
    }

    pub(super) fn compile_property_key(&mut self, key: &PropertyKey, line: u32) -> Result<(), String> {
        match key {
            PropertyKey::Identifier(id) | PropertyKey::StringLiteral(id) | PropertyKey::Private(id) => {
                self.emit_constant(Value::string(*id), line);
            }
            PropertyKey::NumberLiteral(n) => {
                self.emit_constant(Value::number(*n), line);
            }
            PropertyKey::Computed(expr) => {
                self.compile_expr(expr)?;
                // Per spec, ComputedPropertyName runs ToPropertyKey before the value
                // expression, so any toString side effects are observable.
                self.chunk.emit_op(OpCode::ToPropertyKey, line);
            }
        }
        Ok(())
    }

    pub(super) fn property_key_name(&mut self, key: &PropertyKey) -> StringId {
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

    pub(super) fn compile_template_literal(&mut self, t: &TemplateLiteral) -> Result<(), String> {
        let line = t.span.start;
        // Always ensure the result of a TemplateLiteral is a string. We do this
        // by prefixing an empty string, so concatenation with the first
        // expression goes through Add's string-coercion path (which calls
        // ToString on each operand and propagates throws).
        let empty = self.interner.intern("");
        self.emit_constant(Value::string(empty), line);
        let mut parts = 1u32;

        for (i, quasi) in t.quasis.iter().enumerate() {
            let str_id = quasi.cooked.unwrap_or(quasi.raw);
            let text = self.interner.resolve(str_id);
            let is_empty = text.is_empty();

            if !is_empty {
                self.emit_constant(Value::string(str_id), line);
                self.chunk.emit_op(OpCode::Add, line);
                parts += 1;
            }

            if i < t.expressions.len() {
                self.compile_expr(&t.expressions[i])?;
                self.chunk.emit_op(OpCode::Add, line);
                parts += 1;
            }
        }

        let _ = parts;
        Ok(())
    }

    pub(super) fn compile_tagged_template(&mut self, t: &TaggedTemplateExpression) -> Result<(), String> {
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

    pub(super) fn compile_optional_chain(&mut self, o: &OptionalChainExpression) -> Result<(), String> {
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

    pub(super) fn compile_yield(&mut self, y: &YieldExpression) -> Result<(), String> {
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

    pub(super) fn compile_await(&mut self, a: &AwaitExpression) -> Result<(), String> {
        self.compile_expr(&a.argument)?;
        self.chunk.emit_op(OpCode::Await, a.span.start);
        Ok(())
    }

    pub(super) fn compile_regexp(&mut self, r: &RegExpLiteral) -> Result<(), String> {
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

    pub(super) fn compile_meta_property(&mut self, m: &MetaProperty) -> Result<(), String> {
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
