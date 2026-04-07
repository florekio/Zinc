/// Bytecode opcodes for the Zinc VM.
///
/// Variable-length encoding: 1-byte opcode + 0-4 byte operands.
/// Operand notation in comments:
///   u8  = 1-byte unsigned
///   u16 = 2-byte unsigned big-endian
///   u32 = 4-byte unsigned big-endian
///   i16 = 2-byte signed (for jump offsets)
///   i32 = 4-byte signed (for long jump offsets)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum OpCode {
    // ---- Constants & Literals ----
    /// Push constants[u16 index]
    Const = 0x01,
    /// Push constants[u32 index] (wide)
    ConstLong = 0x02,
    /// Push undefined
    Undefined = 0x03,
    /// Push null
    Null = 0x04,
    /// Push true
    True = 0x05,
    /// Push false
    False = 0x06,
    /// Push 0 (as SMI)
    Zero = 0x07,
    /// Push 1 (as SMI)
    One = 0x08,

    // ---- Stack Manipulation ----
    /// Discard top of stack
    Pop = 0x10,
    /// Discard top N values (u8 count)
    PopN = 0x11,
    /// Duplicate top of stack
    Dup = 0x12,
    /// Duplicate top two values
    Dup2 = 0x13,
    /// Swap top two values
    Swap = 0x14,
    /// Rotate top 3: [a,b,c] -> [c,a,b]
    Rot3 = 0x15,

    // ---- Arithmetic ----
    /// a + b (numeric add or string concat)
    Add = 0x20,
    /// a - b
    Sub = 0x21,
    /// a * b
    Mul = 0x22,
    /// a / b
    Div = 0x23,
    /// a % b
    Rem = 0x24,
    /// a ** b
    Exp = 0x25,
    /// -a (unary negation)
    Neg = 0x26,
    /// +a (unary plus / ToNumber)
    Pos = 0x27,
    /// Increment (for ++)
    Inc = 0x28,
    /// Decrement (for --)
    Dec = 0x29,

    // ---- Bitwise ----
    /// a & b
    BitAnd = 0x30,
    /// a | b
    BitOr = 0x31,
    /// a ^ b
    BitXor = 0x32,
    /// ~a
    BitNot = 0x33,
    /// a << b
    Shl = 0x34,
    /// a >> b (sign-extending)
    Shr = 0x35,
    /// a >>> b (zero-fill)
    UShr = 0x36,

    // ---- Comparison ----
    /// a == b (abstract equality)
    Eq = 0x40,
    /// a != b
    Ne = 0x41,
    /// a === b (strict equality)
    StrictEq = 0x42,
    /// a !== b
    StrictNe = 0x43,
    /// a < b
    Lt = 0x44,
    /// a <= b
    Le = 0x45,
    /// a > b
    Gt = 0x46,
    /// a >= b
    Ge = 0x47,
    /// a instanceof b
    InstanceOf = 0x48,
    /// a in b
    In = 0x49,

    // ---- Logical / Unary ----
    /// !a
    Not = 0x50,
    /// typeof a
    TypeOf = 0x51,
    /// typeof globalVar (u16 name) -- no ReferenceError for undeclared
    TypeOfGlobal = 0x52,
    /// void a -> evaluate, replace with undefined
    Void = 0x53,
    /// delete obj[key]
    DeleteProp = 0x54,
    /// delete globalName (u16 name)
    DeleteGlobal = 0x55,

    // ---- Control Flow ----
    /// Unconditional relative jump (i16 offset)
    Jump = 0x60,
    /// Long unconditional jump (i32 offset)
    JumpLong = 0x61,
    /// Pop, jump if falsy (i16 offset)
    JumpIfFalse = 0x62,
    /// Pop, jump if truthy (i16 offset)
    JumpIfTrue = 0x63,
    /// Jump if falsy, don't pop -- for && (i16 offset)
    JumpIfFalsePeek = 0x64,
    /// Jump if truthy, don't pop -- for || (i16 offset)
    JumpIfTruePeek = 0x65,
    /// Jump if null/undefined, don't pop -- for ?? (i16 offset)
    JumpIfNullishPeek = 0x66,
    /// Backward jump for loops (u16 offset)
    Loop = 0x67,

    // ---- Variable Access ----
    /// Push local variable (u8 slot)
    GetLocal = 0x70,
    /// Set local variable (u8 slot), TOS stays
    SetLocal = 0x71,
    /// Push local variable (u16 slot) -- wide
    GetLocalWide = 0x72,
    /// Set local variable (u16 slot) -- wide
    SetLocalWide = 0x73,
    /// Push captured upvalue (u8 index)
    GetUpvalue = 0x74,
    /// Set captured upvalue (u8 index)
    SetUpvalue = 0x75,
    /// Close over local on stack top
    CloseUpvalue = 0x76,
    /// Push global by name (u16 constant index)
    GetGlobal = 0x78,
    /// Set global by name (u16 constant index)
    SetGlobal = 0x79,
    /// Define global variable (u16 constant index)
    DefineGlobal = 0x7A,
    /// Mark let/const slot as initialized (u8 slot)
    InitLet = 0x7B,
    /// Throw ReferenceError if slot uninitialized (u8 slot)
    CheckTdz = 0x7C,

    // ---- Property Access ----
    /// obj.name -- replace obj with value (u16 name constant)
    GetProperty = 0x80,
    /// obj.name = val (u16 name constant)
    SetProperty = 0x81,
    /// obj[key] -- pop key & obj, push value
    GetElement = 0x82,
    /// obj[key] = val
    SetElement = 0x83,
    /// super.name (u16 name)
    GetSuper = 0x84,
    /// super[key]
    GetSuperElem = 0x85,
    /// Optional chain: if TOS null/undefined, jump & push undefined (i16 offset)
    OptionalChain = 0x86,
    /// Get private field #name (u16 name)
    GetPrivate = 0x87,
    /// Set private field #name (u16 name)
    SetPrivate = 0x88,

    // ---- Function Calls ----
    /// Call function (u8 argc): stack=[fn, arg0..argN]
    Call = 0x90,
    /// Method call (u8 argc): stack=[obj, fn, arg0..argN]
    CallMethod = 0x91,
    /// new Constructor(args) (u8 argc)
    Construct = 0x92,
    /// Call with spread arguments (u8 argc)
    SpreadCall = 0x93,
    /// new with spread (u8 argc)
    SpreadConstruct = 0x94,
    /// Return TOS from current function
    Return = 0x95,
    /// Return undefined (implicit return)
    ReturnUndefined = 0x96,

    // ---- Object / Array Creation ----
    /// Push new empty object {}
    CreateObject = 0xA0,
    /// Push new array (u16 hint_length)
    CreateArray = 0xA1,
    /// arr[u32 index] = TOS
    SetArrayItem = 0xA2,
    /// Spread iterable into array being built
    ArraySpread = 0xA3,
    /// Define data property: stack=[obj, key, val]
    DefineDataProp = 0xA4,
    /// Define getter: stack=[obj, key, fn]
    DefineGetter = 0xA5,
    /// Define setter: stack=[obj, key, fn]
    DefineSetter = 0xA6,
    /// Define method with name (u16 name)
    DefineMethod = 0xA7,
    /// Object spread: {...source}
    ObjectSpread = 0xA8,
    /// Create RegExp (u16 pattern, u16 flags)
    CreateRegExp = 0xA9,

    // ---- Closures ----
    /// Create closure from child chunk (u16 chunk_index + upvalue descriptors)
    Closure = 0xB0,
    /// Wide variant (u32 chunk_index)
    ClosureLong = 0xB1,

    // ---- Classes ----
    /// Create class (u16 name)
    Class = 0xB8,
    /// Set up prototype chain: [subclass, superclass]
    Inherit = 0xB9,
    /// Define static method (u16 name)
    ClassStaticMethod = 0xBA,
    /// Define prototype method (u16 name)
    ClassMethod = 0xBB,
    /// Define instance field initializer (u16 name)
    ClassField = 0xBC,
    /// Define static field (u16 name)
    ClassStaticField = 0xBD,
    /// Define private method (u16 name)
    ClassPrivateMethod = 0xBE,
    /// Push super constructor for super() calls
    GetSuperConstructor = 0xBF,

    // ---- Exception Handling ----
    /// Throw TOS as exception
    Throw = 0xC0,
    /// Register exception handler (u16 catch_offset, u16 finally_offset)
    PushExcHandler = 0xC1,
    /// Unregister innermost handler
    PopExcHandler = 0xC2,
    /// Push completion type for finally
    EnterFinally = 0xC3,
    /// Rethrow/continue based on completion type
    LeaveFinally = 0xC4,

    // ---- Iterators ----
    /// Replace TOS with its [Symbol.iterator]()
    GetIterator = 0xD0,
    /// [Symbol.asyncIterator]()
    GetAsyncIterator = 0xD1,
    /// Call .next(), push result
    IteratorNext = 0xD2,
    /// Push .done field of iterator result
    IteratorDone = 0xD3,
    /// Push .value field of iterator result
    IteratorValue = 0xD4,
    /// Call .return() on iterator
    IteratorClose = 0xD5,

    // ---- Generator / Async ----
    /// Yield TOS (suspend generator)
    Yield = 0xD8,
    /// yield* expression (delegate)
    YieldStar = 0xD9,
    /// Await TOS (suspend async function)
    Await = 0xDA,
    /// Wrap current frame as generator object
    CreateGenerator = 0xDB,
    /// Return from async function (resolve promise)
    AsyncReturn = 0xDC,
    /// Reject the async function's promise
    AsyncThrow = 0xDD,

    // ---- Destructuring ----
    /// Pop iterable, push N values (u8 count)
    DestructureArray = 0xE0,
    /// Collect remaining into array (u8 already_consumed)
    DestructureRest = 0xE1,
    /// Pop object, push named values (u8 count)
    DestructureObject = 0xE2,
    /// If TOS is undefined, jump to default (i16 offset)
    DestructureDefault = 0xE3,

    // ---- Modules ----
    /// Static import (u16 specifier)
    ImportModule = 0xE8,
    /// Dynamic import() -- returns promise
    ImportDynamic = 0xE9,
    /// Export binding (u16 name, u8 slot)
    Export = 0xEA,
    /// Export default value
    ExportDefault = 0xEB,
    /// Read imported binding (u16 module, u16 binding)
    GetModuleVar = 0xEC,

    // ---- Miscellaneous ----
    /// No operation
    Nop = 0x00,
    /// debugger statement
    Debugger = 0xF0,
    /// Push new.target
    NewTarget = 0xF1,
    /// Push import.meta
    ImportMeta = 0xF2,
    /// Tagged template call (u8 count)
    TemplateTag = 0xF3,
    /// Collect remaining args into array (u8 from_arg)
    CreateRestParam = 0xF4,
    /// Convert TOS to property key
    ToPropertyKey = 0xF5,
    /// Set inferred name on TOS function (u16 name)
    SetFunctionName = 0xF7,
    /// Enter with scope (non-strict)
    WithEnter = 0xF9,
    /// Exit with scope
    WithExit = 0xFA,
    /// Stop execution
    Halt = 0xFF,
}

impl OpCode {
    /// Try to convert a byte to an opcode.
    pub fn from_byte(byte: u8) -> Option<Self> {
        // Safety: we verify the byte is a valid discriminant
        if Self::is_valid(byte) {
            Some(unsafe { std::mem::transmute::<u8, OpCode>(byte) })
        } else {
            None
        }
    }

    fn is_valid(byte: u8) -> bool {
        matches!(
            byte,
            0x00..=0x08
            | 0x10..=0x15
            | 0x20..=0x29
            | 0x30..=0x36
            | 0x40..=0x49
            | 0x50..=0x55
            | 0x60..=0x67
            | 0x70..=0x7C
            | 0x80..=0x88
            | 0x90..=0x96
            | 0xA0..=0xA9
            | 0xB0..=0xB1
            | 0xB8..=0xBF
            | 0xC0..=0xC4
            | 0xD0..=0xD5
            | 0xD8..=0xDD
            | 0xE0..=0xE3
            | 0xE8..=0xEC
            | 0xF0..=0xF5
            | 0xF7
            | 0xF9..=0xFA
            | 0xFF
        )
    }

    /// Size of this instruction in bytes (opcode + operands).
    pub fn instruction_size(&self) -> usize {
        match self {
            // 1 byte (no operands)
            OpCode::Nop
            | OpCode::Undefined
            | OpCode::Null
            | OpCode::True
            | OpCode::False
            | OpCode::Zero
            | OpCode::One
            | OpCode::Pop
            | OpCode::Dup
            | OpCode::Dup2
            | OpCode::Swap
            | OpCode::Rot3
            | OpCode::Add
            | OpCode::Sub
            | OpCode::Mul
            | OpCode::Div
            | OpCode::Rem
            | OpCode::Exp
            | OpCode::Neg
            | OpCode::Pos
            | OpCode::Inc
            | OpCode::Dec
            | OpCode::BitAnd
            | OpCode::BitOr
            | OpCode::BitXor
            | OpCode::BitNot
            | OpCode::Shl
            | OpCode::Shr
            | OpCode::UShr
            | OpCode::Eq
            | OpCode::Ne
            | OpCode::StrictEq
            | OpCode::StrictNe
            | OpCode::Lt
            | OpCode::Le
            | OpCode::Gt
            | OpCode::Ge
            | OpCode::InstanceOf
            | OpCode::In
            | OpCode::Not
            | OpCode::TypeOf
            | OpCode::Void
            | OpCode::DeleteProp
            | OpCode::CloseUpvalue
            | OpCode::Return
            | OpCode::ReturnUndefined
            | OpCode::CreateObject
            | OpCode::ArraySpread
            | OpCode::DefineDataProp
            | OpCode::DefineGetter
            | OpCode::DefineSetter
            | OpCode::ObjectSpread
            | OpCode::Inherit
            | OpCode::GetSuperConstructor
            | OpCode::Throw
            | OpCode::PopExcHandler
            | OpCode::EnterFinally
            | OpCode::LeaveFinally
            | OpCode::GetIterator
            | OpCode::GetAsyncIterator
            | OpCode::IteratorNext
            | OpCode::IteratorDone
            | OpCode::IteratorValue
            | OpCode::IteratorClose
            | OpCode::Yield
            | OpCode::YieldStar
            | OpCode::Await
            | OpCode::CreateGenerator
            | OpCode::AsyncReturn
            | OpCode::AsyncThrow
            | OpCode::ImportDynamic
            | OpCode::ExportDefault
            | OpCode::Debugger
            | OpCode::NewTarget
            | OpCode::ImportMeta
            | OpCode::ToPropertyKey
            | OpCode::WithEnter
            | OpCode::WithExit
            | OpCode::GetSuperElem
            | OpCode::Halt => 1,

            // 2 bytes (u8 operand)
            OpCode::PopN
            | OpCode::GetLocal
            | OpCode::SetLocal
            | OpCode::GetUpvalue
            | OpCode::SetUpvalue
            | OpCode::InitLet
            | OpCode::CheckTdz
            | OpCode::Call
            | OpCode::Construct
            | OpCode::SpreadCall
            | OpCode::SpreadConstruct
            | OpCode::DestructureArray
            | OpCode::DestructureRest
            | OpCode::DestructureObject
            | OpCode::TemplateTag
            | OpCode::CreateRestParam => 2,

            // 3 bytes (u16 or i16 operand)
            OpCode::Const
            | OpCode::TypeOfGlobal
            | OpCode::DeleteGlobal
            | OpCode::Jump
            | OpCode::JumpIfFalse
            | OpCode::JumpIfTrue
            | OpCode::JumpIfFalsePeek
            | OpCode::JumpIfTruePeek
            | OpCode::JumpIfNullishPeek
            | OpCode::Loop
            | OpCode::GetLocalWide
            | OpCode::SetLocalWide
            | OpCode::GetGlobal
            | OpCode::SetGlobal
            | OpCode::DefineGlobal
            | OpCode::GetProperty
            | OpCode::SetProperty
            | OpCode::GetElement
            | OpCode::SetElement
            | OpCode::GetSuper
            | OpCode::OptionalChain
            | OpCode::GetPrivate
            | OpCode::SetPrivate
            | OpCode::CreateArray
            | OpCode::DefineMethod
            | OpCode::Closure
            | OpCode::Class
            | OpCode::ClassStaticMethod
            | OpCode::ClassMethod
            | OpCode::ClassField
            | OpCode::ClassStaticField
            | OpCode::ClassPrivateMethod
            | OpCode::DestructureDefault
            | OpCode::ImportModule
            | OpCode::SetFunctionName => 3,

            // 4 bytes (u16 + u16)
            OpCode::PushExcHandler | OpCode::GetModuleVar => 5,

            // 3 bytes (u16 name, u8 slot)
            OpCode::Export => 4,

            // u8 argc + u16 method name
            OpCode::CallMethod => 4,

            // 5 bytes (u32 operand)
            OpCode::ConstLong
            | OpCode::JumpLong
            | OpCode::SetArrayItem
            | OpCode::ClosureLong => 5,

            // 5 bytes (u16 + u16)
            OpCode::CreateRegExp => 5,
        }
    }
}

impl std::fmt::Display for OpCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
