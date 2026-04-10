use std::collections::HashMap;

use crate::runtime::value::Value;
use crate::util::interner::StringId;

pub type PropertyKey = StringId;
pub type NativeFn = fn(&mut ObjectHeap, Value, &[Value]) -> Result<Value, Value>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectId(pub u32);

pub struct JsObject {
    pub properties: HashMap<StringId, Value>,
    pub prototype: Option<ObjectId>,
    pub kind: ObjectKind,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PromiseState {
    Pending,
    Fulfilled,
    Rejected,
}

#[derive(Clone)]
pub struct PromiseReaction {
    pub on_fulfilled: Option<Value>,
    pub on_rejected: Option<Value>,
    pub promise: ObjectId, // child promise returned by .then()
}

pub enum ObjectKind {
    Ordinary,
    Array(Vec<Value>),
    Function(FunctionKind),
    /// Array iterator: (source_array_id, current_index)
    ArrayIterator(ObjectId, usize),
    /// Object key iterator: (list of key StringIds, current_index)
    KeyIterator(Vec<crate::util::interner::StringId>, usize),
    /// Primitive wrapper object (new Number(5), new Boolean(true), new String("x"))
    Wrapper(Value),
    /// Regular expression
    RegExp {
        pattern: String,
        flags: String,
    },
    /// Promise with state machine
    Promise {
        state: PromiseState,
        result: Value,
        reactions: Vec<PromiseReaction>,
    },
}

pub enum FunctionKind {
    /// Index into VM's chunk list for bytecode functions
    Bytecode { chunk_idx: usize, name: StringId },
    /// Native/builtin function
    Native { name: StringId, func: NativeFn },
    /// Bound function
    Bound {
        target: ObjectId,
        this_val: Value,
        args: Vec<Value>,
    },
}

/// Simple arena-based object storage (no GC yet - just grow).
pub struct ObjectHeap {
    objects: Vec<Option<JsObject>>,
}

impl ObjectHeap {
    pub fn new() -> Self {
        Self {
            objects: Vec::new(),
        }
    }

    pub fn allocate(&mut self, obj: JsObject) -> ObjectId {
        let id = ObjectId(self.objects.len() as u32);
        self.objects.push(Some(obj));
        id
    }

    pub fn get(&self, id: ObjectId) -> Option<&JsObject> {
        self.objects.get(id.0 as usize).and_then(|o| o.as_ref())
    }

    pub fn get_mut(&mut self, id: ObjectId) -> Option<&mut JsObject> {
        self.objects.get_mut(id.0 as usize).and_then(|o| o.as_mut())
    }

    /// Look up a property by walking the prototype chain.
    /// Returns the value if found, or None if not on any prototype.
    pub fn get_property_chain(&self, start: ObjectId, key: StringId) -> Option<Value> {
        let mut current = Some(start);
        let mut depth = 0;
        while let Some(oid) = current {
            if depth > 64 { break; } // prevent infinite loops
            if let Some(obj) = self.get(oid) {
                if let Some(val) = obj.get_property(key) {
                    return Some(val);
                }
                current = obj.prototype;
                depth += 1;
            } else {
                break;
            }
        }
        None
    }
}

impl Default for ObjectHeap {
    fn default() -> Self {
        Self::new()
    }
}

impl JsObject {
    pub fn ordinary() -> Self {
        Self {
            properties: HashMap::new(),
            prototype: None,
            kind: ObjectKind::Ordinary,
        }
    }

    pub fn promise() -> Self {
        Self {
            properties: HashMap::new(),
            prototype: None,
            kind: ObjectKind::Promise {
                state: PromiseState::Pending,
                result: Value::undefined(),
                reactions: Vec::new(),
            },
        }
    }

    pub fn array(elements: Vec<Value>) -> Self {
        Self {
            properties: HashMap::new(),
            prototype: None,
            kind: ObjectKind::Array(elements),
        }
    }

    pub fn function_bytecode(chunk_idx: usize, name: StringId) -> Self {
        Self {
            properties: HashMap::new(),
            prototype: None,
            kind: ObjectKind::Function(FunctionKind::Bytecode { chunk_idx, name }),
        }
    }

    pub fn function_native(name: StringId, func: NativeFn) -> Self {
        Self {
            properties: HashMap::new(),
            prototype: None,
            kind: ObjectKind::Function(FunctionKind::Native { name, func }),
        }
    }

    pub fn regexp(pattern: String, flags: String) -> Self {
        Self {
            properties: HashMap::new(),
            prototype: None,
            kind: ObjectKind::RegExp { pattern, flags },
        }
    }

    pub fn get_property(&self, key: StringId) -> Option<Value> {
        self.properties.get(&key).copied()
    }

    pub fn set_property(&mut self, key: StringId, value: Value) {
        self.properties.insert(key, value);
    }
}
