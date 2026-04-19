use crate::util::interner::Interner;

use super::chunk::Chunk;
use super::opcode::OpCode;

/// Pretty-print a chunk's bytecode for debugging.
pub fn disassemble(chunk: &Chunk, interner: &Interner) -> String {
    let mut out = String::new();
    let name = interner.resolve(chunk.name);
    out.push_str(&format!("=== {} ===\n", if name.is_empty() { "<script>" } else { name }));
    out.push_str(&format!(
        "locals: {} params: {} upvalues: {}\n",
        chunk.local_count, chunk.param_count, chunk.upvalue_count
    ));

    let mut offset = 0;
    while offset < chunk.code.len() {
        offset = disassemble_instruction(chunk, offset, interner, &mut out);
    }

    // Disassemble child chunks
    for (i, child) in chunk.child_chunks.iter().enumerate() {
        out.push_str(&format!("\n--- child chunk {i} ---\n"));
        out.push_str(&disassemble(child, interner));
    }

    out
}

fn disassemble_instruction(chunk: &Chunk, offset: usize, interner: &Interner, out: &mut String) -> usize {
    let byte = chunk.code[offset];
    let line = chunk.get_line(offset as u32);

    out.push_str(&format!("{offset:04}  L{line:<4}  "));

    let Some(op) = OpCode::from_byte(byte) else {
        out.push_str(&format!("UNKNOWN({byte:#04x})\n"));
        return offset + 1;
    };

    match op {
        // No operands
        OpCode::Nop | OpCode::Undefined | OpCode::Null | OpCode::True | OpCode::False
        | OpCode::Zero | OpCode::One | OpCode::Pop | OpCode::Dup | OpCode::Dup2
        | OpCode::Swap | OpCode::Rot3
        | OpCode::Add | OpCode::Sub | OpCode::Mul | OpCode::Div | OpCode::Rem
        | OpCode::Exp | OpCode::Neg | OpCode::Pos | OpCode::Inc | OpCode::Dec
        | OpCode::BitAnd | OpCode::BitOr | OpCode::BitXor | OpCode::BitNot
        | OpCode::Shl | OpCode::Shr | OpCode::UShr
        | OpCode::Eq | OpCode::Ne | OpCode::StrictEq | OpCode::StrictNe
        | OpCode::Lt | OpCode::Le | OpCode::Gt | OpCode::Ge
        | OpCode::InstanceOf | OpCode::In
        | OpCode::Not | OpCode::TypeOf | OpCode::Void | OpCode::DeleteProp
        | OpCode::CloseUpvalue
        | OpCode::Return | OpCode::ReturnUndefined
        | OpCode::CreateObject | OpCode::ArraySpread | OpCode::DefineDataProp
        | OpCode::DefineGetter | OpCode::DefineSetter | OpCode::ObjectSpread
        | OpCode::Inherit | OpCode::GetSuperConstructor
        | OpCode::Throw | OpCode::PopExcHandler | OpCode::EnterFinally | OpCode::LeaveFinally
        | OpCode::GetIterator | OpCode::GetForInIterator | OpCode::GetAsyncIterator
        | OpCode::GetSuperClass
        | OpCode::IteratorNext | OpCode::IteratorDone | OpCode::IteratorValue | OpCode::IteratorClose
        | OpCode::Yield | OpCode::YieldStar | OpCode::Await | OpCode::CreateGenerator
        | OpCode::AsyncReturn | OpCode::AsyncThrow
        | OpCode::ImportDynamic | OpCode::ExportDefault
        | OpCode::Debugger | OpCode::NewTarget | OpCode::ImportMeta
        | OpCode::ToPropertyKey | OpCode::WithEnter | OpCode::WithExit
        | OpCode::GetSuperElem | OpCode::Halt => {
            out.push_str(&format!("{op}\n"));
            offset + 1
        }

        // u8 operand
        OpCode::PopN | OpCode::Call | OpCode::Construct
        | OpCode::SpreadCall | OpCode::SpreadConstruct
        | OpCode::DestructureArray | OpCode::DestructureRest | OpCode::DestructureObject
        | OpCode::TemplateTag | OpCode::CreateRestParam => {
            let operand = chunk.code[offset + 1];
            out.push_str(&format!("{op:<20} {operand}\n"));
            offset + 2
        }

        OpCode::GetLocal | OpCode::SetLocal | OpCode::InitLet | OpCode::CheckTdz => {
            let slot = chunk.code[offset + 1];
            out.push_str(&format!("{op:<20} slot:{slot}\n"));
            offset + 2
        }

        OpCode::GetUpvalue | OpCode::SetUpvalue => {
            let idx = chunk.code[offset + 1];
            out.push_str(&format!("{op:<20} upvalue:{idx}\n"));
            offset + 2
        }

        OpCode::CallMethod => {
            let argc = chunk.code[offset + 1];
            let name_idx = chunk.read_u16(offset + 2);
            out.push_str(&format!("{op:<20} argc:{argc} name:[{name_idx}]\n"));
            offset + 4
        }

        // u16 operand (constant index or name)
        OpCode::Const => {
            let idx = chunk.read_u16(offset + 1);
            let val = &chunk.constants[idx as usize];
            out.push_str(&format!("{op:<20} [{idx}] = {val:?}\n"));
            offset + 3
        }

        OpCode::GetGlobal | OpCode::SetGlobal | OpCode::DefineGlobal => {
            let idx = chunk.read_u16(offset + 1);
            let val = &chunk.constants[idx as usize];
            let name = if let Some(id) = val.as_string_id() {
                interner.resolve(id).to_string()
            } else {
                format!("{val:?}")
            };
            out.push_str(&format!("{op:<20} [{idx}] '{name}'\n"));
            offset + 3
        }

        OpCode::GetProperty | OpCode::SetProperty => {
            let idx = chunk.read_u16(offset + 1);
            let ic = chunk.read_u16(offset + 3);
            out.push_str(&format!("{op:<20} [{idx}] ic:{ic}\n"));
            offset + 5
        }

        OpCode::GetSuper
        | OpCode::GetPrivate | OpCode::SetPrivate
        | OpCode::DefineMethod | OpCode::Class
        | OpCode::ClassStaticMethod | OpCode::ClassMethod
        | OpCode::ClassField | OpCode::ClassStaticField | OpCode::ClassPrivateMethod
        | OpCode::SetFunctionName | OpCode::ImportModule | OpCode::ExportAllFrom | OpCode::CollectRest
        | OpCode::TypeOfGlobal | OpCode::DeleteGlobal => {
            let idx = chunk.read_u16(offset + 1);
            out.push_str(&format!("{op:<20} [{idx}]\n"));
            offset + 3
        }

        OpCode::GetLocalWide | OpCode::SetLocalWide => {
            let slot = chunk.read_u16(offset + 1);
            out.push_str(&format!("{op:<20} slot:{slot}\n"));
            offset + 3
        }

        OpCode::CreateArray => {
            let hint = chunk.read_u16(offset + 1);
            out.push_str(&format!("{op:<20} hint_len:{hint}\n"));
            offset + 3
        }

        // Jump instructions (i16 offset)
        OpCode::Jump | OpCode::JumpIfFalse | OpCode::JumpIfTrue
        | OpCode::JumpIfFalsePeek | OpCode::JumpIfTruePeek | OpCode::JumpIfNullishPeek
        | OpCode::OptionalChain | OpCode::DestructureDefault => {
            let jump_offset = chunk.read_i16(offset + 1);
            let target = (offset as i32 + 3 + jump_offset as i32) as usize;
            out.push_str(&format!("{op:<20} {jump_offset:+} -> {target:04}\n"));
            offset + 3
        }

        OpCode::Loop => {
            let back = chunk.read_u16(offset + 1);
            let target = offset + 3 - back as usize;
            out.push_str(&format!("{op:<20} -{back} -> {target:04}\n"));
            offset + 3
        }

        // u32 operand
        OpCode::ConstLong | OpCode::JumpLong | OpCode::SetArrayItem | OpCode::ClosureLong => {
            let val = chunk.read_u32(offset + 1);
            out.push_str(&format!("{op:<20} {val}\n"));
            offset + 5
        }

        // ObjectRest: u8 num_keys + (u16 key_idx) * num_keys
        OpCode::ObjectRest => {
            let n = chunk.code[offset + 1] as usize;
            out.push_str(&format!("{op:<20} excl:{n}\n"));
            offset + 2 + (n * 2)
        }
        // Closure: u16 chunk_index + upvalue descriptors
        OpCode::Closure => {
            let idx = chunk.read_u16(offset + 1);
            out.push_str(&format!("{op:<20} chunk:{idx}\n"));
            // Upvalue descriptors follow
            let child = &chunk.child_chunks[idx as usize];
            let mut off = offset + 3;
            for _ in 0..child.upvalue_count {
                let is_local = chunk.code[off];
                let index = chunk.code[off + 1];
                let kind = if is_local != 0 { "local" } else { "upvalue" };
                out.push_str(&format!("              | {kind} {index}\n"));
                off += 2;
            }
            off
        }

        // Multi-byte operands
        OpCode::PushExcHandler => {
            let catch_off = chunk.read_u16(offset + 1);
            let finally_off = chunk.read_u16(offset + 3);
            out.push_str(&format!("{op:<20} catch:{catch_off} finally:{finally_off}\n"));
            offset + 5
        }

        OpCode::GetModuleVar => {
            let module = chunk.read_u16(offset + 1);
            let binding = chunk.read_u16(offset + 3);
            out.push_str(&format!("{op:<20} module:{module} binding:{binding}\n"));
            offset + 5
        }

        OpCode::Export => {
            let name = chunk.read_u16(offset + 1);
            let slot = chunk.code[offset + 3];
            out.push_str(&format!("{op:<20} name:[{name}] slot:{slot}\n"));
            offset + 4
        }

        OpCode::CreateRegExp => {
            let pattern = chunk.read_u16(offset + 1);
            let flags = chunk.read_u16(offset + 3);
            out.push_str(&format!("{op:<20} pattern:[{pattern}] flags:[{flags}]\n"));
            offset + 5
        }

        OpCode::GetElement | OpCode::SetElement => {
            out.push_str(&format!("{op}\n"));
            offset + 1
        }
    }
}
