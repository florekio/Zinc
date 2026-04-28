#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
mod jit_tests_arm64 {
    use zinc::jit::arm64::*;
    use zinc::jit::executable_memory::ExecutableBuffer;

    #[test]
    fn test_jit_identity() {
        // Simplest possible JIT: return the argument
        // fn identity(x: i64) -> i64 { x }
        let mut asm = Assembler::new();
        // X0 already has the argument, just return it
        asm.ret();

        let mut buf = ExecutableBuffer::new(4096).expect("mmap failed");
        buf.write_code(&asm.code);
        let f = unsafe { buf.as_fn1() };
        assert_eq!(f(42), 42);
        assert_eq!(f(0), 0);
        assert_eq!(f(-1), -1);
    }

    #[test]
    fn test_jit_add_one() {
        // fn add_one(x: i64) -> i64 { x + 1 }
        let mut asm = Assembler::new();
        asm.add_imm(X0, X0, 1);
        asm.ret();

        let mut buf = ExecutableBuffer::new(4096).expect("mmap failed");
        buf.write_code(&asm.code);
        let f = unsafe { buf.as_fn1() };
        assert_eq!(f(41), 42);
        assert_eq!(f(0), 1);
        assert_eq!(f(-1), 0);
    }

    #[test]
    fn test_jit_double() {
        // fn double(x: i64) -> i64 { x + x }
        let mut asm = Assembler::new();
        asm.add_reg(X0, X0, X0);
        asm.ret();

        let mut buf = ExecutableBuffer::new(4096).expect("mmap failed");
        buf.write_code(&asm.code);
        let f = unsafe { buf.as_fn1() };
        assert_eq!(f(21), 42);
        assert_eq!(f(0), 0);
    }

    #[test]
    fn test_jit_fibonacci() {
        // Hand-written JIT fibonacci:
        // fn fib(n: i64) -> i64 {
        //     if n <= 1 return n;
        //     return fib(n-1) + fib(n-2);
        // }
        let mut asm = Assembler::new();

        // Prologue: save frame pointer, link register, and callee-saved regs
        asm.stp_pre(X29, X30, SP, -48);
        asm.mov_reg(X29, SP);
        asm.str_imm(X19, SP, 16);
        asm.str_imm(X20, SP, 24);

        // Save argument in callee-saved register
        asm.mov_reg(X19, X0);

        // if n <= 1, return n
        asm.cmp_imm(X19, 1);
        let branch_to_return = asm.offset();
        asm.b_le(0); // placeholder — will patch

        // fib(n-1)
        asm.sub_imm(X0, X19, 1);
        let call1_offset = asm.offset();
        asm.bl(-(call1_offset as i32)); // call self (offset 0 = start of function)

        // Save result in X20
        asm.mov_reg(X20, X0);

        // fib(n-2)
        asm.sub_imm(X0, X19, 2);
        let call2_offset = asm.offset();
        asm.bl(-(call2_offset as i32)); // call self

        // result = fib(n-1) + fib(n-2)
        asm.add_reg(X0, X20, X0);

        // Epilogue
        asm.ldr_imm(X19, SP, 16);
        asm.ldr_imm(X20, SP, 24);
        asm.ldp_post(X29, X30, SP, 48);
        asm.ret();

        // Patch the "return n" branch
        let return_n = asm.offset();
        // return n: X0 = X19, then epilogue
        asm.mov_reg(X0, X19);
        asm.ldr_imm(X19, SP, 16);
        asm.ldr_imm(X20, SP, 24);
        asm.ldp_post(X29, X30, SP, 48);
        asm.ret();

        // Patch the b.le to jump to return_n
        asm.patch_branch(branch_to_return, return_n);

        // Emit and run!
        let mut buf = ExecutableBuffer::new(4096).expect("mmap failed");
        buf.write_code(&asm.code);
        let fib = unsafe { buf.as_fn1() };

        assert_eq!(fib(0), 0);
        assert_eq!(fib(1), 1);
        assert_eq!(fib(2), 1);
        assert_eq!(fib(5), 5);
        assert_eq!(fib(10), 55);
        assert_eq!(fib(20), 6765);
    }

    #[test]
    fn test_jit_fibonacci_performance() {
        // Same as above but benchmark fib(35)
        let mut asm = Assembler::new();
        asm.stp_pre(X29, X30, SP, -48);
        asm.mov_reg(X29, SP);
        asm.str_imm(X19, SP, 16);
        asm.str_imm(X20, SP, 24);
        asm.mov_reg(X19, X0);
        asm.cmp_imm(X19, 1);
        let branch = asm.offset();
        asm.b_le(0);
        asm.sub_imm(X0, X19, 1);
        let c1 = asm.offset();
        asm.bl(-(c1 as i32));
        asm.mov_reg(X20, X0);
        asm.sub_imm(X0, X19, 2);
        let c2 = asm.offset();
        asm.bl(-(c2 as i32));
        asm.add_reg(X0, X20, X0);
        asm.ldr_imm(X19, SP, 16);
        asm.ldr_imm(X20, SP, 24);
        asm.ldp_post(X29, X30, SP, 48);
        asm.ret();
        let ret_n = asm.offset();
        asm.mov_reg(X0, X19);
        asm.ldr_imm(X19, SP, 16);
        asm.ldr_imm(X20, SP, 24);
        asm.ldp_post(X29, X30, SP, 48);
        asm.ret();
        asm.patch_branch(branch, ret_n);

        let mut buf = ExecutableBuffer::new(4096).expect("mmap failed");
        buf.write_code(&asm.code);
        let fib = unsafe { buf.as_fn1() };

        let start = std::time::Instant::now();
        let result = fib(35);
        let elapsed = start.elapsed();

        assert_eq!(result, 9227465);
        eprintln!("JIT fib(35) = {} in {:?}", result, elapsed);
    }
}

#[cfg(target_arch = "x86_64")]
mod jit_tests_x86_64 {
    use zinc::jit::x86_64::*;
    use zinc::jit::executable_memory::ExecutableBuffer;

    #[test]
    fn test_jit_identity() {
        // fn identity(x: i64) -> i64 { x }
        // System V AMD64: arg in RDI, return value in RAX
        let mut asm = Assembler::new();
        asm.mov_reg(RAX, RDI);
        asm.ret();

        let mut buf = ExecutableBuffer::new(4096).expect("mmap failed");
        buf.write_code(&asm.code);
        let f = unsafe { buf.as_fn1() };
        assert_eq!(f(42), 42);
        assert_eq!(f(0), 0);
        assert_eq!(f(-1), -1);
    }

    #[test]
    fn test_jit_add_one() {
        // fn add_one(x: i64) -> i64 { x + 1 }
        let mut asm = Assembler::new();
        asm.add_imm(RAX, RDI, 1); // RAX = RDI + 1
        asm.ret();

        let mut buf = ExecutableBuffer::new(4096).expect("mmap failed");
        buf.write_code(&asm.code);
        let f = unsafe { buf.as_fn1() };
        assert_eq!(f(41), 42);
        assert_eq!(f(0), 1);
        assert_eq!(f(-1), 0);
    }

    #[test]
    fn test_jit_double() {
        // fn double(x: i64) -> i64 { x + x }
        let mut asm = Assembler::new();
        asm.add_reg(RAX, RDI, RDI); // RAX = RDI + RDI (emits mov rax,rdi then add rax,rdi)
        asm.ret();

        let mut buf = ExecutableBuffer::new(4096).expect("mmap failed");
        buf.write_code(&asm.code);
        let f = unsafe { buf.as_fn1() };
        assert_eq!(f(21), 42);
        assert_eq!(f(0), 0);
    }

    #[test]
    fn test_jit_fibonacci() {
        // fn fib(n: i64) -> i64 { if n <= 1 { n } else { fib(n-1) + fib(n-2) } }
        let mut asm = Assembler::new();

        // Prologue: push rbp; push r13; push r12  (3 pushes → RSP 16-byte aligned before CALL)
        asm.push_reg(RBP);
        asm.push_reg(R13);
        asm.push_reg(R12);

        asm.mov_reg(R12, RDI); // save n in R12

        asm.cmp_imm(R12, 1);
        let branch_to_base = asm.offset();
        asm.b_le(0); // placeholder → patched to base case

        // fib(n-1)
        asm.sub_imm(RDI, R12, 1);
        let call1 = asm.offset();
        asm.bl(-(call1 as i32));
        asm.mov_reg(R13, RAX); // save fib(n-1)

        // fib(n-2)
        asm.sub_imm(RDI, R12, 2);
        let call2 = asm.offset();
        asm.bl(-(call2 as i32));
        asm.add_reg(RAX, RAX, R13); // RAX = fib(n-1) + fib(n-2)

        // Epilogue
        asm.pop_reg(R12);
        asm.pop_reg(R13);
        asm.pop_reg(RBP);
        asm.ret();

        // Base case: return n
        let base = asm.offset();
        asm.mov_reg(RAX, R12);
        asm.pop_reg(R12);
        asm.pop_reg(R13);
        asm.pop_reg(RBP);
        asm.ret();

        asm.patch_branch(branch_to_base, base);

        let mut buf = ExecutableBuffer::new(4096).expect("mmap failed");
        buf.write_code(&asm.code);
        let fib = unsafe { buf.as_fn1() };

        assert_eq!(fib(0), 0);
        assert_eq!(fib(1), 1);
        assert_eq!(fib(2), 1);
        assert_eq!(fib(5), 5);
        assert_eq!(fib(10), 55);
        assert_eq!(fib(20), 6765);
    }

    #[test]
    fn test_jit_fibonacci_performance() {
        // Same as above, benchmark fib(35)
        let mut asm = Assembler::new();
        asm.push_reg(RBP);
        asm.push_reg(R13);
        asm.push_reg(R12);
        asm.mov_reg(R12, RDI);
        asm.cmp_imm(R12, 1);
        let branch = asm.offset();
        asm.b_le(0);
        asm.sub_imm(RDI, R12, 1);
        let c1 = asm.offset();
        asm.bl(-(c1 as i32));
        asm.mov_reg(R13, RAX);
        asm.sub_imm(RDI, R12, 2);
        let c2 = asm.offset();
        asm.bl(-(c2 as i32));
        asm.add_reg(RAX, RAX, R13);
        asm.pop_reg(R12);
        asm.pop_reg(R13);
        asm.pop_reg(RBP);
        asm.ret();
        let ret_n = asm.offset();
        asm.mov_reg(RAX, R12);
        asm.pop_reg(R12);
        asm.pop_reg(R13);
        asm.pop_reg(RBP);
        asm.ret();
        asm.patch_branch(branch, ret_n);

        let mut buf = ExecutableBuffer::new(4096).expect("mmap failed");
        buf.write_code(&asm.code);
        let fib = unsafe { buf.as_fn1() };

        let start = std::time::Instant::now();
        let result = fib(35);
        let elapsed = start.elapsed();

        assert_eq!(result, 9227465);
        eprintln!("JIT fib(35) = {} in {:?}", result, elapsed);
    }
}
