use crate::compiler::chunk::ChunkFlags;
use crate::runtime::value::Value;

use super::vm::{Vm, VmError, CallFrame};

impl Vm {
    /// Call a closure value with the given arguments and run it to completion.
    pub(crate) fn call_function(&mut self, func_val: Value, args: &[Value]) -> Result<Value, VmError> {
        self.call_function_this(func_val, Value::undefined(), args)
    }

    /// Call a closure with a specific `this` binding.
    pub(crate) fn call_function_this(&mut self, func_val: Value, this_value: Value, args: &[Value]) -> Result<Value, VmError> {
        if !func_val.is_function() {
            return Ok(Value::undefined());
        }
        let packed = func_val.as_function().unwrap();
        // Native global function sentinels
        if (-536..=-500).contains(&packed) {
            return Ok(self.exec_global_fn(packed, args));
        }
        let closure_id = ((packed as u32) >> 16) as usize;
        let chunk_idx = (packed & 0xFFFF) as usize;
        if chunk_idx < 1 || chunk_idx >= self.chunks.len() {
            return Ok(Value::undefined());
        }

        let func_pos = self.stack.len();
        self.push(func_val);
        for arg in args {
            self.push(*arg);
        }
        let expected = self.chunks[chunk_idx].param_count as usize;
        let mut argc = args.len();
        while argc < expected {
            self.push(Value::undefined());
            argc += 1;
        }

        let upvalues = if closure_id < self.closure_upvalues.len() {
            self.closure_upvalues[closure_id].clone()
        } else {
            Vec::new()
        };

        // Arrow functions inherit `this` from the enclosing scope.
        let effective_this = if self.chunks[chunk_idx].flags.contains(ChunkFlags::ARROW) {
            self.frames.last().map(|f| f.this_value).unwrap_or(Value::undefined())
        } else {
            this_value
        };

        let stop_depth = self.frames.len();

        self.frames.push(CallFrame {
            chunk_idx, ip: 0, base: func_pos + 1,
            upvalues, this_value: effective_this, is_constructor: false,
            pending_super_call: false, generator_id: None, argc: args.len(),
        });

        // Run using the full main dispatch loop, stopping when our frame returns.
        self.run_until(stop_depth)
    }
}
