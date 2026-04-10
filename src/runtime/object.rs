use crate::runtime::value::Value;
use crate::util::interner::StringId;

pub type PropertyKey = StringId;
pub type NativeFn = fn(&mut ObjectHeap, Value, &[Value]) -> Result<Value, Value>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectId(pub u32);

pub struct JsObject {
    /// Properties stored as a flat Vec for cache-friendly linear scan.
    /// Most JS objects have <=4 properties; linear scan beats HashMap.
    pub properties: Vec<(StringId, Value)>,
    pub prototype: Option<ObjectId>,
    pub kind: ObjectKind,
    pub marked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GeneratorState {
    /// Created but `.next()` not yet called.
    SuspendedStart,
    /// Paused at a `yield` expression.
    SuspendedYield,
    /// Currently running (re-entrancy guard).
    Executing,
    /// Finished (returned or threw).
    Completed,
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
    /// Generator (suspendable function)
    Generator {
        state: GeneratorState,
        chunk_idx: usize,
        ip: usize,
        saved_stack: Vec<Value>,
        saved_upvalues: Vec<Value>,
        this_value: Value,
    },
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

/// Extract an ObjectId from a Value, if it references one.
/// Handles both object-tagged values and promise sentinel functions.
pub fn trace_value(val: Value) -> Option<ObjectId> {
    if val.is_object() {
        return val.as_object_id();
    }
    // Promise resolve/reject sentinels encode ObjectIds in function values
    if val.is_function() {
        let s = val.as_function().unwrap();
        if s <= -600_000 && s > -700_000 {
            return Some(ObjectId((-600_000 - s) as u32));
        }
        if s <= -700_000 && s > -800_000 {
            return Some(ObjectId((-700_000 - s) as u32));
        }
    }
    None
}

/// Simple arena-based object storage with mark-and-sweep GC.
pub struct ObjectHeap {
    objects: Vec<Option<JsObject>>,
    pub gc_threshold: usize,
    free_list: Vec<u32>,
}

impl ObjectHeap {
    pub fn new() -> Self {
        Self {
            objects: Vec::new(),
            gc_threshold: 256,
            free_list: Vec::new(),
        }
    }

    pub fn allocate(&mut self, obj: JsObject) -> ObjectId {
        if let Some(idx) = self.free_list.pop() {
            self.objects[idx as usize] = Some(obj);
            ObjectId(idx)
        } else {
            let id = ObjectId(self.objects.len() as u32);
            self.objects.push(Some(obj));
            id
        }
    }

    pub fn needs_gc(&self) -> bool {
        self.objects.len() > self.gc_threshold
    }

    pub fn get(&self, id: ObjectId) -> Option<&JsObject> {
        self.objects.get(id.0 as usize).and_then(|o| o.as_ref())
    }

    pub fn get_mut(&mut self, id: ObjectId) -> Option<&mut JsObject> {
        self.objects.get_mut(id.0 as usize).and_then(|o| o.as_mut())
    }

    // ---- Garbage Collection ----

    /// Mark all objects reachable from the given root ObjectIds.
    pub fn mark_from_roots(&mut self, root_ids: &[ObjectId]) {
        let mut worklist: Vec<ObjectId> = Vec::new();

        for &id in root_ids {
            if let Some(Some(obj)) = self.objects.get_mut(id.0 as usize)
                && !obj.marked
            {
                obj.marked = true;
                worklist.push(id);
            }
        }

        while let Some(id) = worklist.pop() {
            let refs = self.collect_refs(id);
            for ref_id in refs {
                if let Some(Some(obj)) = self.objects.get_mut(ref_id.0 as usize)
                    && !obj.marked
                {
                    obj.marked = true;
                    worklist.push(ref_id);
                }
            }
        }
    }

    /// Collect all ObjectIds referenced by a single object.
    fn collect_refs(&self, id: ObjectId) -> Vec<ObjectId> {
        let obj = match self.objects.get(id.0 as usize).and_then(|o| o.as_ref()) {
            Some(o) => o,
            None => return Vec::new(),
        };

        let mut refs = Vec::new();

        // Properties
        for &(_, val) in &obj.properties {
            if let Some(oid) = trace_value(val) { refs.push(oid); }
        }

        // Prototype chain
        if let Some(proto_id) = obj.prototype {
            refs.push(proto_id);
        }

        // Kind-specific references
        match &obj.kind {
            ObjectKind::Array(elements) => {
                for val in elements {
                    if let Some(oid) = trace_value(*val) { refs.push(oid); }
                }
            }
            ObjectKind::ArrayIterator(source_id, _) => {
                refs.push(*source_id);
            }
            ObjectKind::Wrapper(val) => {
                if let Some(oid) = trace_value(*val) { refs.push(oid); }
            }
            ObjectKind::Generator { saved_stack, saved_upvalues, this_value, .. } => {
                for val in saved_stack {
                    if let Some(oid) = trace_value(*val) { refs.push(oid); }
                }
                for val in saved_upvalues {
                    if let Some(oid) = trace_value(*val) { refs.push(oid); }
                }
                if let Some(oid) = trace_value(*this_value) { refs.push(oid); }
            }
            ObjectKind::Promise { result, reactions, .. } => {
                if let Some(oid) = trace_value(*result) { refs.push(oid); }
                for reaction in reactions {
                    if let Some(on_f) = reaction.on_fulfilled
                        && let Some(oid) = trace_value(on_f)
                    {
                        refs.push(oid);
                    }
                    if let Some(on_r) = reaction.on_rejected
                        && let Some(oid) = trace_value(on_r)
                    {
                        refs.push(oid);
                    }
                    refs.push(reaction.promise);
                }
            }
            ObjectKind::Function(fk) => {
                if let FunctionKind::Bound { target, this_val, args } = fk {
                    refs.push(*target);
                    if let Some(oid) = trace_value(*this_val) { refs.push(oid); }
                    for val in args {
                        if let Some(oid) = trace_value(*val) { refs.push(oid); }
                    }
                }
            }
            ObjectKind::Ordinary
            | ObjectKind::KeyIterator(_, _)
            | ObjectKind::RegExp { .. } => {}
        }

        refs
    }

    /// Sweep: free unmarked objects and reset marks on survivors.
    pub fn sweep(&mut self) {
        for i in 0..self.objects.len() {
            if let Some(obj) = &mut self.objects[i] {
                if obj.marked {
                    obj.marked = false;
                } else {
                    self.objects[i] = None;
                    self.free_list.push(i as u32);
                }
            }
        }
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
            properties: Vec::new(),
            prototype: None,
            kind: ObjectKind::Ordinary,
            marked: false,
        }
    }

    pub fn promise() -> Self {
        Self {
            properties: Vec::new(),
            prototype: None,
            kind: ObjectKind::Promise {
                state: PromiseState::Pending,
                result: Value::undefined(),
                reactions: Vec::new(),
            },
            marked: false,
        }
    }

    pub fn array(elements: Vec<Value>) -> Self {
        Self {
            properties: Vec::new(),
            prototype: None,
            kind: ObjectKind::Array(elements),
            marked: false,
        }
    }

    pub fn function_bytecode(chunk_idx: usize, name: StringId) -> Self {
        Self {
            properties: Vec::new(),
            prototype: None,
            kind: ObjectKind::Function(FunctionKind::Bytecode { chunk_idx, name }),
            marked: false,
        }
    }

    pub fn function_native(name: StringId, func: NativeFn) -> Self {
        Self {
            properties: Vec::new(),
            prototype: None,
            kind: ObjectKind::Function(FunctionKind::Native { name, func }),
            marked: false,
        }
    }

    pub fn regexp(pattern: String, flags: String) -> Self {
        Self {
            properties: Vec::new(),
            prototype: None,
            kind: ObjectKind::RegExp { pattern, flags },
            marked: false,
        }
    }

    #[inline(always)]
    pub fn get_property(&self, key: StringId) -> Option<Value> {
        for &(k, v) in &self.properties {
            if k == key { return Some(v); }
        }
        None
    }

    #[inline(always)]
    pub fn set_property(&mut self, key: StringId, value: Value) {
        for entry in &mut self.properties {
            if entry.0 == key {
                entry.1 = value;
                return;
            }
        }
        self.properties.push((key, value));
    }
}
