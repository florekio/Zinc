use crate::runtime::value::Value;
use crate::util::interner::StringId;

pub type PropertyKey = StringId;
/// Host-supplied native function. Receives `&mut Vm` so the host code can
/// allocate strings, call back into JS, throw via VmError, and inspect/mutate
/// the heap. Boxed so closures with embedder state are usable too.
///
/// Returning Err(value) re-throws `value` as a JS exception in the caller's
/// frame.
pub type NativeFn = std::sync::Arc<
    dyn Fn(&mut crate::vm::vm::Vm, Value, &[Value]) -> Result<Value, Value> + Send + Sync,
>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectId(pub u32);

/// Property flags: writable (W), enumerable (E), configurable (C).
/// Default for user-created properties: all true (0b111).
#[derive(Debug, Clone, Copy)]
pub struct Property {
    pub value: Value,
    pub flags: u8,
}

impl Property {
    pub const WRITABLE: u8     = 0b001;
    pub const ENUMERABLE: u8   = 0b010;
    pub const CONFIGURABLE: u8 = 0b100;
    pub const ALL: u8          = 0b111;

    #[inline(always)]
    pub fn data(value: Value) -> Self {
        Self { value, flags: Self::ALL }
    }

    #[inline(always)]
    pub fn with_flags(value: Value, flags: u8) -> Self {
        Self { value, flags }
    }

    #[inline(always)]
    pub fn is_writable(self) -> bool { self.flags & Self::WRITABLE != 0 }
    #[inline(always)]
    pub fn is_enumerable(self) -> bool { self.flags & Self::ENUMERABLE != 0 }
    #[inline(always)]
    pub fn is_configurable(self) -> bool { self.flags & Self::CONFIGURABLE != 0 }
}

pub struct JsObject {
    /// Properties stored as a flat Vec for cache-friendly linear scan.
    /// Most JS objects have <=4 properties; linear scan beats HashMap.
    pub properties: Vec<(StringId, Property)>,
    pub prototype: Option<ObjectId>,
    pub kind: ObjectKind,
    pub marked: bool,
    /// Whether Object.preventExtensions() has been called
    pub extensible: bool,
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

/// A generator frame's exception handler, saved across suspension.
#[derive(Debug, Clone, Copy)]
pub struct SavedExcHandler {
    pub catch_target: u16,
    pub finally_target: u16,
    pub rel_stack_depth: usize,
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

/// The element type of a typed array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypedArrayKind {
    Int8, Uint8, Uint8Clamped, Int16, Uint16, Int32, Uint32, Float32, Float64,
    BigInt64, BigUint64,
}

impl TypedArrayKind {
    pub fn bytes_per_element(self) -> usize {
        match self {
            TypedArrayKind::Int8 | TypedArrayKind::Uint8 | TypedArrayKind::Uint8Clamped => 1,
            TypedArrayKind::Int16 | TypedArrayKind::Uint16 => 2,
            TypedArrayKind::Int32 | TypedArrayKind::Uint32 | TypedArrayKind::Float32 => 4,
            TypedArrayKind::Float64 | TypedArrayKind::BigInt64 | TypedArrayKind::BigUint64 => 8,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            TypedArrayKind::Int8 => "Int8Array",
            TypedArrayKind::Uint8 => "Uint8Array",
            TypedArrayKind::Uint8Clamped => "Uint8ClampedArray",
            TypedArrayKind::Int16 => "Int16Array",
            TypedArrayKind::Uint16 => "Uint16Array",
            TypedArrayKind::Int32 => "Int32Array",
            TypedArrayKind::Uint32 => "Uint32Array",
            TypedArrayKind::Float32 => "Float32Array",
            TypedArrayKind::Float64 => "Float64Array",
            TypedArrayKind::BigInt64 => "BigInt64Array",
            TypedArrayKind::BigUint64 => "BigUint64Array",
        }
    }

    pub fn is_bigint(self) -> bool {
        matches!(self, TypedArrayKind::BigInt64 | TypedArrayKind::BigUint64)
    }

    /// Coerce a numeric value to this element type's stored representation
    /// (integer wrap / clamp / float rounding), matching the spec's conversions.
    pub fn coerce(self, n: f64) -> f64 {
        fn to_int(n: f64) -> f64 { if n.is_finite() { n.trunc() } else { 0.0 } }
        match self {
            TypedArrayKind::Float32 => (n as f32) as f64,
            TypedArrayKind::Float64 => n,
            TypedArrayKind::Uint8Clamped => {
                if n.is_nan() { 0.0 } else { n.round_ties_even().clamp(0.0, 255.0) }
            }
            TypedArrayKind::Int8 => (to_int(n) as i64 as u8) as i8 as f64,
            TypedArrayKind::Uint8 => (to_int(n) as i64 as u8) as f64,
            TypedArrayKind::Int16 => (to_int(n) as i64 as u16) as i16 as f64,
            TypedArrayKind::Uint16 => (to_int(n) as i64 as u16) as f64,
            TypedArrayKind::Int32 => (to_int(n) as i64 as u32) as i32 as f64,
            TypedArrayKind::Uint32 => (to_int(n) as i64 as u32) as f64,
            // BigInt-backed kinds store the (already 64-bit-wrapped) value as f64
            // only for non-bigint paths; real values flow through the BigInt store.
            TypedArrayKind::BigInt64 | TypedArrayKind::BigUint64 => to_int(n),
        }
    }
}

pub enum ObjectKind {
    Ordinary,
    Array(Vec<Value>),
    Function(FunctionKind),
    /// Array iterator: (source_array_id, current_index)
    ArrayIterator(ObjectId, usize),
    /// Map iterator: (source_map_id, current_index)
    MapIterator(ObjectId, usize),
    /// Set iterator: (source_set_id, current_index)
    SetIterator(ObjectId, usize),
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
        /// Snapshot of the actual call arguments — used by `arguments` references
        /// inside the generator body across suspensions.
        saved_args: Vec<Value>,
        /// Exception handlers belonging to the generator frame at suspension
        /// (innermost last), with stack depth relative to the frame base.
        /// Removed from the VM handler stack on suspend (they'd be stale) and
        /// re-pushed with fresh absolute positions on resume.
        saved_handlers: Vec<SavedExcHandler>,
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
    /// Promise combinator tracking state (Promise.all, race, allSettled, any)
    PromiseCombinator {
        kind: CombinatorKind,
        remaining: usize,
        values: Vec<Value>,
        result_promise: ObjectId,
        /// For Promise.any: collect rejection reasons
        errors: Vec<Value>,
    },
    /// Tracks a .finally() callback for propagation
    FinallyTracker {
        callback: Value,
        is_reject: bool,
    },
    /// ES6 Map: ordered key-value pairs
    Map {
        entries: Vec<(Value, Value)>,
    },
    /// ES6 Set: ordered unique values
    Set {
        entries: Vec<Value>,
    },
    /// WeakMap: object keys only, not traced by GC
    WeakMap {
        entries: Vec<(ObjectId, Value)>,
    },
    /// WeakSet: object values only, not traced by GC
    WeakSet {
        entries: Vec<ObjectId>,
    },
    /// Date: milliseconds since UNIX epoch
    Date(f64),
    /// BigInt: arbitrary-precision integer. A `typeof === "bigint"` primitive,
    /// stored on the heap because the NaN-boxed Value tag space is full. These
    /// objects are immutable and compared by mathematical value, not identity.
    BigInt(num_bigint::BigInt),
    /// ArrayBuffer: a raw byte store backing typed-array / DataView views.
    ArrayBuffer(Vec<u8>),
    /// Typed array view. Elements are kept pre-coerced as f64 (BigInt kinds use
    /// the BigInt value range); `kind` drives element coercion and BYTES_PER_ELEMENT.
    /// `buffer` is the associated ArrayBuffer (kept in sync on writes).
    TypedArray { kind: TypedArrayKind, elements: Vec<f64>, buffer: ObjectId },
    /// DataView over an ArrayBuffer.
    DataView { buffer: ObjectId, byte_offset: usize, byte_length: usize },
    /// Lazy concatenated string. Left and right are either TAG_STRING (StringId)
    /// or another ConsString ObjectId. `len` caches the total char count for O(1) .length.
    ConsString { left: Value, right: Value, len: u32 },
    /// A flat, owned string value that is NOT interned. Produced by string
    /// operations (charAt, substr, fromCharCode, concat flattening, …) so that
    /// transient string values don't pollute (and unboundedly grow) the
    /// interner. `char_len` caches the Unicode scalar count for O(1) `.length`.
    /// Interned only on demand, when used as a property key (flatten_to_string_id).
    FlatString { data: Box<str>, char_len: u32 },
    /// Host-owned object: tag identifies the host class (assigned by the
    /// embedder via `Engine::register_host_class`); payload is an opaque
    /// 64-bit handle the host uses to find the backing data (typically an
    /// index into a side table). Not traced as a Value graph by the GC.
    Host { tag: u32, payload: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CombinatorKind {
    All,
    Race,
    AllSettled,
    Any,
}

pub enum FunctionKind {
    /// Packed function value (closure_id << 16 | chunk_idx) for bytecode
    /// functions — the FULL packing, so wrapping (e.g. Function.bind) and
    /// unwrapping round-trips the closure identity. Dropping the high bits
    /// detached bound functions from their captured upvalues.
    Bytecode { chunk_idx: usize, name: StringId },
    /// Native/builtin function
    Native { name: StringId, func: NativeFn },
    /// Bound function
    Bound {
        target: ObjectId,
        this_val: Value,
        args: Vec<Value>,
    },
    /// Native sentinel function (negative i32 ID), for binding native methods
    NativeSentinel { sentinel: i32 },
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
        // Combinator resolve callbacks: tracker_oid encoded as (encoded / 1024)
        if s <= -800_000 && s > -900_000 {
            let encoded = (-800_000 - s) as u32;
            return Some(ObjectId(encoded / 1024));
        }
        if s <= -900_000 && s > -1_000_000 {
            let encoded = (-900_000 - s) as u32;
            return Some(ObjectId(encoded / 1024));
        }
        // Finally tracker sentinels
        if s <= -1_100_000 && s > -1_200_000 {
            return Some(ObjectId((-1_100_000 - s) as u32));
        }
        if s <= -1_200_000 && s > -1_300_000 {
            return Some(ObjectId((-1_200_000 - s) as u32));
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
            gc_threshold: 4096,
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
        for (_, prop) in &obj.properties {
            if let Some(oid) = trace_value(prop.value) { refs.push(oid); }
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
            ObjectKind::ArrayIterator(source_id, _)
            | ObjectKind::MapIterator(source_id, _)
            | ObjectKind::SetIterator(source_id, _) => {
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
            ObjectKind::PromiseCombinator { values, result_promise, errors, .. } => {
                for val in values {
                    if let Some(oid) = trace_value(*val) { refs.push(oid); }
                }
                for val in errors {
                    if let Some(oid) = trace_value(*val) { refs.push(oid); }
                }
                refs.push(*result_promise);
            }
            ObjectKind::FinallyTracker { callback, .. } => {
                if let Some(oid) = trace_value(*callback) { refs.push(oid); }
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
            ObjectKind::Map { entries } => {
                for (k, v) in entries {
                    if let Some(oid) = trace_value(*k) { refs.push(oid); }
                    if let Some(oid) = trace_value(*v) { refs.push(oid); }
                }
            }
            ObjectKind::Set { entries } => {
                for v in entries {
                    if let Some(oid) = trace_value(*v) { refs.push(oid); }
                }
            }
            ObjectKind::WeakMap { entries } => {
                // Only trace values, NOT keys (keys are weak references)
                for (_, v) in entries {
                    if let Some(oid) = trace_value(*v) { refs.push(oid); }
                }
            }
            ObjectKind::WeakSet { .. } => {
                // Do not trace entries (weak references)
            }
            ObjectKind::ConsString { left, right, .. } => {
                if let Some(oid) = trace_value(*left) { refs.push(oid); }
                if let Some(oid) = trace_value(*right) { refs.push(oid); }
            }
            ObjectKind::TypedArray { buffer, .. } => refs.push(*buffer),
            ObjectKind::DataView { buffer, .. } => refs.push(*buffer),
            ObjectKind::ArrayBuffer(_)
            | ObjectKind::Ordinary
            | ObjectKind::KeyIterator(_, _)
            | ObjectKind::RegExp { .. }
            | ObjectKind::Host { .. }
            | ObjectKind::BigInt(_)
            | ObjectKind::FlatString { .. }
            | ObjectKind::Date(_) => {}
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
            extensible: true,
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
            extensible: true,
        }
    }

    pub fn array(elements: Vec<Value>) -> Self {
        Self {
            properties: Vec::new(),
            prototype: None,
            kind: ObjectKind::Array(elements),
            marked: false,
            extensible: true,
        }
    }

    pub fn bigint(value: num_bigint::BigInt) -> Self {
        Self {
            properties: Vec::new(),
            prototype: None,
            kind: ObjectKind::BigInt(value),
            marked: false,
            extensible: false,
        }
    }

    pub fn function_bytecode(chunk_idx: usize, name: StringId) -> Self {
        Self {
            properties: Vec::new(),
            prototype: None,
            kind: ObjectKind::Function(FunctionKind::Bytecode { chunk_idx, name }),
            marked: false,
            extensible: true,
        }
    }

    pub fn function_native(name: StringId, func: NativeFn) -> Self {
        Self {
            properties: Vec::new(),
            prototype: None,
            kind: ObjectKind::Function(FunctionKind::Native { name, func }),
            marked: false,
            extensible: true,
        }
    }

    pub fn regexp(pattern: String, flags: String) -> Self {
        Self {
            properties: Vec::new(),
            prototype: None,
            kind: ObjectKind::RegExp { pattern, flags },
            marked: false,
            extensible: true,
        }
    }

    /// Get a property value (ignoring descriptor flags).
    #[inline(always)]
    pub fn get_property(&self, key: StringId) -> Option<Value> {
        for &(k, ref prop) in &self.properties {
            if k == key { return Some(prop.value); }
        }
        None
    }

    /// Get the full property descriptor.
    #[inline(always)]
    pub fn get_property_descriptor(&self, key: StringId) -> Option<Property> {
        for &(k, prop) in &self.properties {
            if k == key { return Some(prop); }
        }
        None
    }

    /// Set a property value with default flags (writable|enumerable|configurable).
    #[inline(always)]
    pub fn set_property(&mut self, key: StringId, value: Value) {
        for entry in &mut self.properties {
            if entry.0 == key {
                // Respect writable flag
                if entry.1.is_writable() {
                    entry.1.value = value;
                }
                return;
            }
        }
        // Don't add new properties to non-extensible (frozen/sealed) objects
        if !self.extensible {
            return;
        }
        self.properties.push((key, Property::data(value)));
    }

    /// Define a property with explicit flags (for Object.defineProperty).
    pub fn define_property(&mut self, key: StringId, prop: Property) {
        for entry in &mut self.properties {
            if entry.0 == key {
                entry.1 = prop;
                return;
            }
        }
        self.properties.push((key, prop));
    }

    /// Check if the object has its own property (not inherited).
    pub fn has_own_property(&self, key: StringId) -> bool {
        self.properties.iter().any(|&(k, _)| k == key)
    }

    /// Delete a property (respects configurable flag).
    pub fn delete_property(&mut self, key: StringId) -> bool {
        if let Some(idx) = self.properties.iter().position(|&(k, _)| k == key) {
            if self.properties[idx].1.is_configurable() {
                self.properties.remove(idx);
                return true;
            }
            return false; // not configurable
        }
        true // property doesn't exist, deletion succeeds
    }
}
