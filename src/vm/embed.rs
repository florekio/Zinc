//! Embedder-facing convenience API on `Vm`.
//!
//! These methods are the public surface intended for host code (e.g. the
//! Copper browser bindings) that needs to construct values, read/write
//! properties, and raise typed errors without reaching into the internal
//! interner / heap modules. The receiver is `&mut Vm` so they're callable
//! from inside `Engine::register_host_fn` callbacks.

use crate::runtime::object::{JsObject, ObjectKind, Property};
use crate::runtime::value::Value;

use super::vm::{Vm, VmError};

impl Vm {
    /// Intern a Rust string and return a JS string `Value`.
    pub fn value_from_str(&mut self, s: &str) -> Value {
        Value::string(self.interner.intern(s))
    }

    /// Allocate a JS array from a `Vec<Value>`.
    pub fn alloc_array(&mut self, items: Vec<Value>) -> Value {
        let obj = JsObject {
            properties: Vec::new(),
            prototype: None,
            kind: ObjectKind::Array(items),
            marked: false,
            extensible: true,
        };
        Value::object_id(self.heap.allocate(obj))
    }

    /// Allocate an empty JS object (`{}`) with `Object.prototype` as its
    /// prototype. Use `set_property` to populate it.
    pub fn alloc_object(&mut self) -> Value {
        let mut obj = JsObject::ordinary();
        obj.prototype = Some(self.object_prototype);
        Value::object_id(self.heap.allocate(obj))
    }

    /// Read a named own-or-inherited property on a JS object value.
    /// Returns `None` if `target` isn't an object or the property is missing.
    /// Walks the prototype chain. Does NOT invoke getters.
    pub fn get_property(&mut self, target: Value, name: &str) -> Option<Value> {
        let oid = target.as_object_id()?;
        let key = self.interner.intern(name);
        self.heap.get_property_chain(oid, key)
    }

    /// Read a named own property on a JS object value (no prototype walk).
    /// Returns `None` if `target` isn't an object or the property is missing.
    pub fn get_own_property(&mut self, target: Value, name: &str) -> Option<Value> {
        let oid = target.as_object_id()?;
        let key = self.interner.intern(name);
        self.heap.get(oid).and_then(|o| o.get_property(key))
    }

    /// Set a named property on a JS object value with default attributes
    /// (writable | enumerable | configurable). Returns `true` on success,
    /// `false` if `target` isn't an object.
    pub fn set_property(&mut self, target: Value, name: &str, value: Value) -> bool {
        let Some(oid) = target.as_object_id() else { return false };
        let key = self.interner.intern(name);
        if let Some(obj) = self.heap.get_mut(oid) {
            obj.set_property(key, value);
            true
        } else {
            false
        }
    }

    /// Define a named property on a JS object value with explicit attribute
    /// flags. Use `Property::WRITABLE | Property::ENUMERABLE | Property::CONFIGURABLE`
    /// for the equivalent of a plain assignment, or pass `0` for a read-only,
    /// non-enumerable, non-configurable slot.
    pub fn define_property(&mut self, target: Value, name: &str, value: Value, flags: u8) -> bool {
        let Some(oid) = target.as_object_id() else { return false };
        let key = self.interner.intern(name);
        if let Some(obj) = self.heap.get_mut(oid) {
            obj.define_property(key, Property::with_flags(value, flags));
            true
        } else {
            false
        }
    }

    /// Build a new `Error` instance with the given message. Returns the value
    /// — the caller decides whether to throw it (return `Err(value)` from a
    /// host fn) or attach it elsewhere.
    pub fn error(&mut self, message: &str) -> Value {
        self.make_native_error("Error", message)
    }

    /// Build a new `TypeError` instance with the given message.
    pub fn type_error(&mut self, message: &str) -> Value {
        self.make_native_error("TypeError", message)
    }

    /// Build a new `RangeError` instance with the given message.
    pub fn range_error(&mut self, message: &str) -> Value {
        self.make_native_error("RangeError", message)
    }

    /// Build a new `ReferenceError` instance with the given message.
    pub fn reference_error(&mut self, message: &str) -> Value {
        self.make_native_error("ReferenceError", message)
    }

    /// Build a new `SyntaxError` instance with the given message.
    pub fn syntax_error(&mut self, message: &str) -> Value {
        self.make_native_error("SyntaxError", message)
    }

    /// Convenience: call a JS function from a host callback and return the
    /// result in the host callback's `Result<Value, Value>` shape, so a
    /// thrown JS exception propagates as `Err(thrown_value)`.
    ///
    /// Use this instead of `call_function` when you're inside a
    /// `register_host_fn` body and want to use `?`:
    /// ```ignore
    /// let result = vm.host_call(callback, &[arg])?;
    /// ```
    pub fn host_call(&mut self, func_val: Value, args: &[Value]) -> Result<Value, Value> {
        match self.call_function(func_val, args) {
            Ok(v) => Ok(v),
            Err(VmError::Throw(reason)) => Err(reason),
            Err(VmError::TypeError(msg)) => Err(self.type_error(&msg)),
            Err(VmError::ReferenceError(msg)) => Err(self.reference_error(&msg)),
            Err(VmError::RuntimeError(msg)) => Err(self.error(&msg)),
        }
    }

    /// Like `host_call`, but with an explicit `this` binding.
    pub fn host_call_this(
        &mut self,
        func_val: Value,
        this_value: Value,
        args: &[Value],
    ) -> Result<Value, Value> {
        match self.call_function_this(func_val, this_value, args) {
            Ok(v) => Ok(v),
            Err(VmError::Throw(reason)) => Err(reason),
            Err(VmError::TypeError(msg)) => Err(self.type_error(&msg)),
            Err(VmError::ReferenceError(msg)) => Err(self.reference_error(&msg)),
            Err(VmError::RuntimeError(msg)) => Err(self.error(&msg)),
        }
    }
}
