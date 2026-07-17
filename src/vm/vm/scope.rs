//! `with`-statement object environments: HasBinding (honoring
//! Symbol.unscopables), GetValue/PutValue through a with-scope object, and
//! the frame-visible slice of the with stack.

use super::*;

impl Vm {
    /// HasBinding for a with-scope object: true when the object owns the
    /// property (data or accessor) and it is not blocked by the object's
    /// Symbol.unscopables.
    pub(crate) fn with_scope_has_binding(
        &mut self,
        oid: ObjectId,
        name_id: crate::util::interner::StringId,
    ) -> Result<bool, VmError> {
        let name_str = self.interner.resolve(name_id).to_owned();
        let getter_key = self.interner.intern(&format!("__get_{name_str}__"));
        let setter_key = self.interner.intern(&format!("__set_{name_str}__"));
        let owns = self.heap.get(oid).is_some_and(|o| {
            o.get_property(name_id).is_some()
                || o.get_property(getter_key).is_some()
                || o.get_property(setter_key).is_some()
        });
        if !owns {
            return Ok(false);
        }
        // Get(env, @@unscopables) and Get(unscopables, name) are both
        // OBSERVABLE — throwing getters propagate to the identifier lookup.
        let unscopables_sym = format!("__sym_{}__", self.sym_unscopables);
        let unscopables = self.getter_aware_get(oid, &unscopables_sym)?;
        let blocked = if let Some(uo) = unscopables.and_then(|v| v.as_object_id()) {
            let flag = self.getter_aware_get(uo, &name_str)?;
            flag.map(|v| self.truthy(v)).unwrap_or(false)
        } else {
            false
        };
        Ok(!blocked)
    }

    /// Start of the with-stack slice visible to the current frame: entries
    /// pushed within it plus its closure's captured chain (both sit at
    /// `frame.with_base..`). Entries below belong to caller frames and are
    /// not in this function's scope chain.
    pub(crate) fn frame_with_base(&self) -> usize {
        self.frames
            .last()
            .map(|f| f.with_base)
            .unwrap_or(0)
            .min(self.with_stack.len())
    }

    /// Innermost with-scope object (searching `with_stack[from..]`, innermost
    /// first) that has a binding for `name_id`, honoring Symbol.unscopables.
    pub(crate) fn with_scope_lookup(
        &mut self,
        from: usize,
        name_id: crate::util::interner::StringId,
    ) -> Option<ObjectId> {
        for i in (from..self.with_stack.len()).rev() {
            let oid = self.with_stack[i];
            if self.with_scope_has_binding(oid, name_id).unwrap_or(false) {
                return Some(oid);
            }
        }
        None
    }

    /// with_scope_lookup with observable unscopables (throws propagate).
    pub(crate) fn with_scope_lookup_checked(
        &mut self,
        from: usize,
        name_id: crate::util::interner::StringId,
    ) -> Result<Option<ObjectId>, VmError> {
        for i in (from..self.with_stack.len()).rev() {
            let oid = self.with_stack[i];
            if self.with_scope_has_binding(oid, name_id)? {
                return Ok(Some(oid));
            }
        }
        Ok(None)
    }

    /// GetValue through a with-scope binding: run the getter if the property
    /// is an accessor, else read the data property.
    pub(crate) fn with_scope_get(
        &mut self,
        oid: ObjectId,
        name_id: crate::util::interner::StringId,
    ) -> Result<Value, VmError> {
        let name_str = self.interner.resolve(name_id).to_owned();
        let getter_key = self.interner.intern(&format!("__get_{name_str}__"));
        if let Some(gfn) = self.heap.get(oid).and_then(|o| o.get_property(getter_key))
            && gfn.is_function()
        {
            return self.call_function_this(gfn, Value::object_id(oid), &[]);
        }
        Ok(self.heap.get(oid)
            .and_then(|o| o.get_property(name_id))
            .unwrap_or(Value::undefined()))
    }

    /// PutValue through a with-scope binding: run the setter if the property
    /// is an accessor (a getter-only property is silently ignored, non-strict
    /// semantics), else write/recreate the data property.
    pub(crate) fn with_scope_set(
        &mut self,
        oid: ObjectId,
        name_id: crate::util::interner::StringId,
        val: Value,
    ) -> Result<(), VmError> {
        let name_str = self.interner.resolve(name_id).to_owned();
        let setter_key = self.interner.intern(&format!("__set_{name_str}__"));
        if let Some(sfn) = self.heap.get(oid).and_then(|o| o.get_property(setter_key))
            && sfn.is_function()
        {
            self.call_function_this(sfn, Value::object_id(oid), &[val])?;
            return Ok(());
        }
        let getter_key = self.interner.intern(&format!("__get_{name_str}__"));
        if self.heap.get(oid).and_then(|o| o.get_property(getter_key)).is_some() {
            return Ok(());
        }
        if let Some(obj) = self.heap.get_mut(oid) {
            obj.set_property(name_id, val);
        }
        Ok(())
    }
}
