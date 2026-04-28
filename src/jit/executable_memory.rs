/// Allocate executable memory for JIT-compiled code.
///
/// macOS ARM64: requires MAP_JIT and W^X via pthread_jit_write_protect_np.
/// Linux x86-64: plain mmap(PROT_READ|WRITE|EXEC), no special W^X protocol.
use std::ptr;

const PROT_READ:  i32 = 1;
const PROT_WRITE: i32 = 2;
const PROT_EXEC:  i32 = 4;
const MAP_PRIVATE: i32 = 0x02;

#[cfg(target_os = "macos")]
const MAP_ANONYMOUS: i32 = 0x1000;
#[cfg(not(target_os = "macos"))]
const MAP_ANONYMOUS: i32 = 0x20;

// MAP_JIT is macOS-only (required for JIT on Apple Silicon)
#[cfg(target_os = "macos")]
const MAP_JIT: i32 = 0x0800;
#[cfg(not(target_os = "macos"))]
const MAP_JIT: i32 = 0;

unsafe extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}

// macOS-only: W^X write-protection and instruction cache invalidation
#[cfg(target_os = "macos")]
unsafe extern "C" {
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
        Some(Self { ptr, len: size, code_len: 0 })
    }

    /// Write machine code into the buffer.
    pub fn write_code(&mut self, code: &[u8]) {
        assert!(code.len() <= self.len, "code too large for buffer");
        unsafe {
            #[cfg(target_os = "macos")]
            pthread_jit_write_protect_np(0);

            ptr::copy_nonoverlapping(code.as_ptr(), self.ptr, code.len());

            #[cfg(target_os = "macos")]
            {
                pthread_jit_write_protect_np(1);
                sys_icache_invalidate(self.ptr, code.len());
            }
        }
        self.code_len = code.len();
    }

    pub unsafe fn as_fn1(&self) -> fn(i64) -> i64 { unsafe {
        std::mem::transmute::<*mut u8, fn(i64) -> i64>(self.ptr)
    }}

    pub unsafe fn as_fn2(&self) -> fn(i64, i64) -> i64 { unsafe {
        std::mem::transmute::<*mut u8, fn(i64, i64) -> i64>(self.ptr)
    }}

    pub unsafe fn as_fn3(&self) -> fn(i64, i64, i64) -> i64 { unsafe {
        std::mem::transmute::<*mut u8, fn(i64, i64, i64) -> i64>(self.ptr)
    }}

    pub unsafe fn as_fn_globals(&self) -> fn(*mut i64) { unsafe {
        std::mem::transmute::<*mut u8, fn(*mut i64)>(self.ptr)
    }}

    pub fn ptr(&self) -> *mut u8 { self.ptr }
}

impl Drop for ExecutableBuffer {
    fn drop(&mut self) {
        unsafe { munmap(self.ptr, self.len); }
    }
}
