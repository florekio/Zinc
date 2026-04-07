/// Trait implemented by all types that can be managed by the garbage collector.
///
/// # Safety
/// Implementors must correctly trace ALL reachable GC references.
/// Missing a reference will cause the GC to free a live object -> use-after-free.
pub unsafe trait Trace {
    /// Mark all GC-managed objects reachable from `self`.
    fn trace(&self, tracer: &mut Tracer);
}

/// Passed to `Trace::trace` to mark reachable objects.
pub struct Tracer {
    /// Objects to process (worklist for BFS marking)
    pub(crate) worklist: Vec<*mut GcHeader>,
}

impl Tracer {
    pub(crate) fn new() -> Self {
        Self {
            worklist: Vec::new(),
        }
    }

    /// Mark an object as reachable. If it hasn't been marked yet,
    /// it's added to the worklist for further tracing.
    ///
    /// # Safety
    /// The pointer must be valid and point to a GcHeader.
    pub unsafe fn mark(&mut self, ptr: *mut GcHeader) {
        if ptr.is_null() {
            return;
        }
        unsafe {
            let header = &mut *ptr;
            if !header.marked {
                header.marked = true;
                self.worklist.push(ptr);
            }
        }
    }
}

/// Header prepended to every GC-managed allocation.
#[repr(C)]
pub struct GcHeader {
    /// Mark bit for mark-and-sweep.
    pub marked: bool,
    /// Intrusive linked list of all GC objects.
    pub next: *mut GcHeader,
    /// Size of the allocation (header + data) for sweep/stats.
    pub size: usize,
    /// Function pointer to trace this object's children.
    /// We need this because the GC iterates GcHeaders and doesn't know the concrete type.
    pub trace_fn: unsafe fn(*mut u8, &mut Tracer),
    /// Function pointer to drop this object.
    pub drop_fn: unsafe fn(*mut u8),
}

impl GcHeader {
    pub fn new<T: Trace>(size: usize) -> Self {
        Self {
            marked: false,
            next: std::ptr::null_mut(),
            size,
            trace_fn: trace_fn_for::<T>,
            drop_fn: drop_fn_for::<T>,
        }
    }
}

/// Type-erased trace function.
unsafe fn trace_fn_for<T: Trace>(ptr: *mut u8, tracer: &mut Tracer) {
    unsafe {
        let obj = &*(ptr as *const T);
        obj.trace(tracer);
    }
}

/// Type-erased drop function.
unsafe fn drop_fn_for<T>(ptr: *mut u8) {
    unsafe {
        std::ptr::drop_in_place(ptr as *mut T);
    }
}

// ---- Trace implementations for primitive types ----

unsafe impl Trace for () {
    fn trace(&self, _tracer: &mut Tracer) {}
}

unsafe impl Trace for bool {
    fn trace(&self, _tracer: &mut Tracer) {}
}

unsafe impl Trace for i32 {
    fn trace(&self, _tracer: &mut Tracer) {}
}

unsafe impl Trace for f64 {
    fn trace(&self, _tracer: &mut Tracer) {}
}

unsafe impl Trace for String {
    fn trace(&self, _tracer: &mut Tracer) {}
}

unsafe impl<T: Trace> Trace for Vec<T> {
    fn trace(&self, tracer: &mut Tracer) {
        for item in self {
            item.trace(tracer);
        }
    }
}

unsafe impl<T: Trace> Trace for Option<T> {
    fn trace(&self, tracer: &mut Tracer) {
        if let Some(val) = self {
            val.trace(tracer);
        }
    }
}
