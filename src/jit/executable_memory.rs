/// Allocate executable memory on macOS ARM64.
///
/// Apple Silicon requires MAP_JIT for JIT-compiled code,
/// and we need to flush the instruction cache after writing.
use std::ptr;

const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const PROT_EXEC: i32 = 4;
const MAP_PRIVATE: i32 = 0x02;
const MAP_ANONYMOUS: i32 = 0x1000; // macOS
const MAP_JIT: i32 = 0x0800;      // macOS ARM64

unsafe extern "C" {
    fn mmap(
        addr: *mut u8,
        len: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn sys_icache_invalidate(start: *mut u8, len: usize);
    fn pthread_jit_write_protect_np(enabled: i32);
}

/// A block of memory that is both writable and executable.
pub struct ExecutableBuffer {
    ptr: *mut u8,
    len: usize,
    code_len: usize,
}

impl ExecutableBuffer {
    /// Allocate `size` bytes of RWX memory.
    pub fn new(size: usize) -> Option<Self> {
        let ptr = unsafe {
            mmap(
                ptr::null_mut(),
                size,
                PROT_READ | PROT_WRITE | PROT_EXEC,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_JIT,
                -1,
                0,
            )
        };
        if ptr.is_null() || ptr as isize == -1 {
            return None;
        }
        Some(Self {
            ptr,
            len: size,
            code_len: 0,
        })
    }

    /// Write machine code into the buffer.
    pub fn write_code(&mut self, code: &[u8]) {
        assert!(code.len() <= self.len, "code too large for buffer");
        unsafe {
            // Disable write protection (Apple Silicon W^X)
            pthread_jit_write_protect_np(0);
            // Copy code
            ptr::copy_nonoverlapping(code.as_ptr(), self.ptr, code.len());
            // Re-enable write protection
            pthread_jit_write_protect_np(1);
            // Flush instruction cache
            sys_icache_invalidate(self.ptr, code.len());
        }
        self.code_len = code.len();
    }

    /// Get a function pointer to the start of the code.
    /// The function takes one i64 argument and returns i64.
    ///
    /// # Safety
    /// The caller must ensure the buffer contains valid ARM64 code.
    pub unsafe fn as_fn1(&self) -> fn(i64) -> i64 { unsafe {
        std::mem::transmute::<*mut u8, fn(i64) -> i64>(self.ptr)
    }}

    /// Get a function pointer for a 2-argument function.
    ///
    /// # Safety
    /// The caller must ensure the buffer contains valid ARM64 code.
    pub unsafe fn as_fn2(&self) -> fn(i64, i64) -> i64 { unsafe {
        std::mem::transmute::<*mut u8, fn(i64, i64) -> i64>(self.ptr)
    }}

    /// Get a function pointer for a 3-argument function.
    ///
    /// # Safety
    /// The caller must ensure the buffer contains valid ARM64 code.
    pub unsafe fn as_fn3(&self) -> fn(i64, i64, i64) -> i64 { unsafe {
        std::mem::transmute::<*mut u8, fn(i64, i64, i64) -> i64>(self.ptr)
    }}

    /// Get a function pointer for a globals-only JIT function (takes *mut i64, returns nothing).
    ///
    /// # Safety
    /// The caller must ensure the buffer contains valid ARM64 code.
    pub unsafe fn as_fn_globals(&self) -> fn(*mut i64) { unsafe {
        std::mem::transmute::<*mut u8, fn(*mut i64)>(self.ptr)
    }}

    /// Get the raw pointer to the code (for computing call offsets).
    pub fn ptr(&self) -> *mut u8 {
        self.ptr
    }
}

impl Drop for ExecutableBuffer {
    fn drop(&mut self) {
        unsafe {
            munmap(self.ptr, self.len);
        }
    }
}
