use std::alloc::{self, Layout};

use super::trace::{GcHeader, Trace, Tracer};

/// The GC heap: manages all garbage-collected allocations.
pub struct Heap {
    /// Head of the intrusive linked list of all GC objects.
    all_objects: *mut GcHeader,
    /// Total bytes allocated on the GC heap.
    bytes_allocated: usize,
    /// When bytes_allocated exceeds this, trigger a collection.
    next_gc_threshold: usize,
    /// Total number of live objects.
    object_count: usize,
}

/// Initial GC threshold (64 KB).
const INITIAL_GC_THRESHOLD: usize = 64 * 1024;
/// Growth factor after each GC.
const GC_GROWTH_FACTOR: usize = 2;

impl Heap {
    pub fn new() -> Self {
        Self {
            all_objects: std::ptr::null_mut(),
            bytes_allocated: 0,
            next_gc_threshold: INITIAL_GC_THRESHOLD,
            object_count: 0,
        }
    }

    /// Allocate a GC-managed object of type T.
    /// Returns a raw pointer to the data (past the header).
    ///
    /// The object is prepended to the all-objects linked list.
    pub fn allocate<T: Trace>(&mut self, value: T) -> *mut T {
        let layout = Layout::new::<GcAlloc<T>>();
        let total_size = layout.size();

        // Allocate memory
        let ptr = unsafe { alloc::alloc(layout) as *mut GcAlloc<T> };
        if ptr.is_null() {
            alloc::handle_alloc_error(layout);
        }

        // Initialize header
        let alloc = unsafe { &mut *ptr };
        alloc.header = GcHeader::new::<T>(total_size);
        // Write value (using ptr::write to avoid dropping uninitialized memory)
        unsafe { std::ptr::write(&mut alloc.data, value) };

        // Prepend to linked list
        alloc.header.next = self.all_objects;
        self.all_objects = &mut alloc.header as *mut GcHeader;

        self.bytes_allocated += total_size;
        self.object_count += 1;

        &mut alloc.data as *mut T
    }

    /// Check if we should trigger a GC.
    pub fn should_collect(&self) -> bool {
        self.bytes_allocated >= self.next_gc_threshold
    }

    /// Run a mark-and-sweep collection.
    /// `roots` is called to trace all root references.
    pub fn collect(&mut self, roots: impl FnOnce(&mut Tracer)) {
        // Mark phase
        let mut tracer = Tracer::new();
        roots(&mut tracer);

        // Process worklist (BFS)
        while let Some(header_ptr) = tracer.worklist.pop() {
            unsafe {
                let header = &*header_ptr;
                // Get pointer to data (right after header)
                let data_ptr = (header_ptr as *mut u8).add(std::mem::size_of::<GcHeader>());
                (header.trace_fn)(data_ptr, &mut tracer);
            }
        }

        // Sweep phase
        self.sweep();

        // Adjust threshold
        self.next_gc_threshold = (self.bytes_allocated * GC_GROWTH_FACTOR).max(INITIAL_GC_THRESHOLD);
    }

    fn sweep(&mut self) {
        let mut prev: *mut *mut GcHeader = &mut self.all_objects;
        let mut current = self.all_objects;

        while !current.is_null() {
            let header = unsafe { &mut *current };
            let next = header.next;

            if header.marked {
                // Alive: clear mark for next cycle
                header.marked = false;
                prev = &mut header.next;
                current = next;
            } else {
                // Dead: unlink and free
                unsafe {
                    *prev = next;

                    // Drop the object
                    let data_ptr = (current as *mut u8).add(std::mem::size_of::<GcHeader>());
                    (header.drop_fn)(data_ptr);

                    // Deallocate
                    let layout = Layout::from_size_align_unchecked(header.size, 8);
                    self.bytes_allocated -= header.size;
                    self.object_count -= 1;
                    alloc::dealloc(current as *mut u8, layout);
                }
                current = next;
            }
        }
    }

    pub fn bytes_allocated(&self) -> usize {
        self.bytes_allocated
    }

    pub fn object_count(&self) -> usize {
        self.object_count
    }

    /// Get the head of the all-objects list (for testing/debugging).
    pub fn all_objects(&self) -> *mut GcHeader {
        self.all_objects
    }
}

impl Default for Heap {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Heap {
    fn drop(&mut self) {
        // Free all remaining objects
        let mut current = self.all_objects;
        while !current.is_null() {
            unsafe {
                let header = &*current;
                let next = header.next;
                let data_ptr = (current as *mut u8).add(std::mem::size_of::<GcHeader>());
                (header.drop_fn)(data_ptr);
                let layout = Layout::from_size_align_unchecked(header.size, 8);
                alloc::dealloc(current as *mut u8, layout);
                current = next;
            }
        }
    }
}

/// Internal allocation layout: header + data.
#[repr(C)]
struct GcAlloc<T> {
    header: GcHeader,
    data: T,
}

#[cfg(test)]
mod tests {
    use super::*;

    // A simple GC-managed type for testing
    struct TestObj {
        value: i32,
    }

    unsafe impl Trace for TestObj {
        fn trace(&self, _tracer: &mut Tracer) {}
    }

    #[test]
    fn test_allocate_and_read() {
        let mut heap = Heap::new();
        let ptr = heap.allocate(TestObj { value: 42 });
        unsafe {
            assert_eq!((*ptr).value, 42);
        }
        assert_eq!(heap.object_count(), 1);
        assert!(heap.bytes_allocated() > 0);
    }

    #[test]
    fn test_collect_frees_unreachable() {
        let mut heap = Heap::new();
        let _ptr1 = heap.allocate(TestObj { value: 1 });
        let _ptr2 = heap.allocate(TestObj { value: 2 });
        assert_eq!(heap.object_count(), 2);

        // Collect with no roots -- everything should be freed
        heap.collect(|_tracer| {});
        assert_eq!(heap.object_count(), 0);
    }

    #[test]
    fn test_collect_keeps_reachable() {
        let mut heap = Heap::new();
        let ptr1 = heap.allocate(TestObj { value: 1 });
        let _ptr2 = heap.allocate(TestObj { value: 2 });
        assert_eq!(heap.object_count(), 2);

        // Keep only ptr1 as a root
        let header_ptr = unsafe {
            (ptr1 as *mut u8).sub(std::mem::size_of::<GcHeader>()) as *mut GcHeader
        };
        heap.collect(|tracer| unsafe {
            tracer.mark(header_ptr);
        });

        assert_eq!(heap.object_count(), 1);
        // ptr1 should still be valid
        unsafe {
            assert_eq!((*ptr1).value, 1);
        }
    }
}
