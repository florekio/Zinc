use crate::runtime::value::Value;
use crate::util::interner::StringId;

use super::opcode::OpCode;

bitflags::bitflags! {
    /// Flags describing a compiled chunk's characteristics.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ChunkFlags: u8 {
        const STRICT    = 0b0000_0001;
        const GENERATOR = 0b0000_0010;
        const ASYNC     = 0b0000_0100;
        const MODULE    = 0b0000_1000;
        const ARROW     = 0b0001_0000;
    }
}

/// A compiled bytecode unit (one per function/script/module).
#[derive(Debug)]
pub struct Chunk {
    /// Bytecode instructions.
    pub code: Vec<u8>,
    /// Constant pool (numbers, strings, nested chunks, etc.)
    pub constants: Vec<Value>,
    /// Run-length encoded line info: (bytecode_offset, source_line).
    pub lines: Vec<(u32, u32)>,
    /// Source file name.
    pub source_name: StringId,
    /// Number of local variable slots needed.
    pub local_count: u16,
    /// Number of upvalues captured.
    pub upvalue_count: u16,
    /// Number of declared parameters.
    pub param_count: u16,
    /// Function.length: params before first default/rest.
    pub formal_length: u16,
    /// Function name (for stack traces).
    pub name: StringId,
    /// Chunk flags (strict, generator, async, etc.)
    pub flags: ChunkFlags,
    /// Upvalue descriptors for this closure.
    pub upvalue_descriptors: Vec<UpvalueDescriptor>,
    /// Exception handler table.
    pub exception_handlers: Vec<ExceptionHandler>,
    /// Nested function chunks (referenced by Closure opcode).
    pub child_chunks: Vec<Chunk>,
    /// Absolute chunk indices of direct children (filled during VM flattening).
    pub children: Vec<usize>,
}

/// Describes how a closure captures one upvalue.
#[derive(Debug, Clone)]
pub struct UpvalueDescriptor {
    /// Index: if `is_local` is true, this is a local slot in the *enclosing* function.
    /// If false, this is an upvalue index in the enclosing function's upvalue list.
    pub index: u8,
    /// True if capturing directly from the enclosing function's locals.
    /// False if capturing from the enclosing function's upvalues (transitive capture).
    pub is_local: bool,
}

/// Exception handler entry in the handler table.
#[derive(Debug, Clone)]
pub struct ExceptionHandler {
    /// Start of the try block (bytecode offset).
    pub try_start: u32,
    /// End of the try block (bytecode offset).
    pub try_end: u32,
    /// Start of catch handler (0 if no catch).
    pub catch_target: u32,
    /// Start of finally handler (0 if no finally).
    pub finally_target: u32,
    /// Operand stack depth at try entry (for unwinding).
    pub stack_depth: u16,
    /// Local slot for catch parameter (-1/0xFFFF if none).
    pub catch_binding: u16,
}

impl Chunk {
    pub fn new(name: StringId, source_name: StringId) -> Self {
        Self {
            code: Vec::new(),
            constants: Vec::new(),
            lines: Vec::new(),
            source_name,
            local_count: 0,
            upvalue_count: 0,
            param_count: 0,
            formal_length: 0,
            name,
            flags: ChunkFlags::empty(),
            upvalue_descriptors: Vec::new(),
            exception_handlers: Vec::new(),
            child_chunks: Vec::new(),
            children: Vec::new(),
        }
    }

    // ---- Emit helpers ----

    /// Write a single byte.
    pub fn emit_byte(&mut self, byte: u8, line: u32) {
        self.code.push(byte);
        self.add_line(line);
    }

    /// Emit an opcode.
    pub fn emit_op(&mut self, op: OpCode, line: u32) {
        self.emit_byte(op as u8, line);
    }

    /// Emit an opcode followed by a u8 operand.
    pub fn emit_op_u8(&mut self, op: OpCode, operand: u8, line: u32) {
        self.emit_byte(op as u8, line);
        self.emit_byte(operand, line);
    }

    /// Emit an opcode followed by a u16 operand (big-endian).
    pub fn emit_op_u16(&mut self, op: OpCode, operand: u16, line: u32) {
        self.emit_byte(op as u8, line);
        self.code.push((operand >> 8) as u8);
        self.code.push((operand & 0xFF) as u8);
        self.add_line(line);
        self.add_line(line);
    }

    /// Emit an opcode followed by a u32 operand (big-endian).
    pub fn emit_op_u32(&mut self, op: OpCode, operand: u32, line: u32) {
        self.emit_byte(op as u8, line);
        self.code.push((operand >> 24) as u8);
        self.code.push((operand >> 16) as u8);
        self.code.push((operand >> 8) as u8);
        self.code.push((operand & 0xFF) as u8);
        for _ in 0..4 {
            self.add_line(line);
        }
    }

    /// Emit a jump instruction with a placeholder offset.
    /// Returns the position of the offset bytes for later patching.
    pub fn emit_jump(&mut self, op: OpCode, line: u32) -> usize {
        self.emit_byte(op as u8, line);
        let pos = self.code.len();
        // Placeholder i16 offset
        self.code.push(0xFF);
        self.code.push(0xFF);
        self.add_line(line);
        self.add_line(line);
        pos
    }

    /// Patch a previously emitted jump to target the current position.
    pub fn patch_jump(&mut self, offset_pos: usize) {
        let jump_target = self.code.len();
        let offset = jump_target as i32 - offset_pos as i32 - 2; // -2 for the offset bytes themselves
        debug_assert!(
            offset >= i16::MIN as i32 && offset <= i16::MAX as i32,
            "Jump offset {offset} out of i16 range"
        );
        let offset = offset as i16;
        self.code[offset_pos] = (offset >> 8) as u8;
        self.code[offset_pos + 1] = (offset & 0xFF) as u8;
    }

    /// Emit a backward loop jump to `loop_start`.
    pub fn emit_loop(&mut self, loop_start: usize, line: u32) {
        self.emit_byte(OpCode::Loop as u8, line);
        let offset = self.code.len() - loop_start + 2; // +2 for the offset bytes
        debug_assert!(offset <= u16::MAX as usize, "Loop offset too large");
        self.code.push((offset >> 8) as u8);
        self.code.push((offset & 0xFF) as u8);
        self.add_line(line);
        self.add_line(line);
    }

    // ---- Constant pool ----

    /// Add a constant to the pool and return its index.
    pub fn add_constant(&mut self, value: Value) -> u16 {
        let index = self.constants.len();
        self.constants.push(value);
        debug_assert!(index <= u16::MAX as usize, "Constant pool overflow");
        index as u16
    }

    // ---- Line info ----

    fn add_line(&mut self, line: u32) {
        if let Some(last) = self.lines.last()
            && last.1 == line {
                return; // Same line, no need to add
            }
        self.lines.push((self.code.len() as u32 - 1, line));
    }

    /// Get the source line for a bytecode offset.
    pub fn get_line(&self, offset: u32) -> u32 {
        // Binary search for the last entry with offset <= target
        match self.lines.binary_search_by_key(&offset, |&(o, _)| o) {
            Ok(i) => self.lines[i].1,
            Err(0) => 0,
            Err(i) => self.lines[i - 1].1,
        }
    }

    /// Read a u16 from the bytecode at the given offset (big-endian).
    pub fn read_u16(&self, offset: usize) -> u16 {
        ((self.code[offset] as u16) << 8) | (self.code[offset + 1] as u16)
    }

    /// Read an i16 from the bytecode at the given offset (big-endian).
    pub fn read_i16(&self, offset: usize) -> i16 {
        self.read_u16(offset) as i16
    }

    /// Read a u32 from the bytecode at the given offset (big-endian).
    pub fn read_u32(&self, offset: usize) -> u32 {
        ((self.code[offset] as u32) << 24)
            | ((self.code[offset + 1] as u32) << 16)
            | ((self.code[offset + 2] as u32) << 8)
            | (self.code[offset + 3] as u32)
    }

    /// Current bytecode length.
    pub fn len(&self) -> usize {
        self.code.len()
    }

    pub fn is_empty(&self) -> bool {
        self.code.is_empty()
    }
}
