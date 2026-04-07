/// NaN-boxed JavaScript value representation.
///
/// Every JS value fits in a single `u64`. We use the **negative** quiet NaN
/// space (sign bit set) so that real f64 values -- including positive NaN --
/// are never confused with tagged values.
///
/// Encoding:
///   - f64 values: stored as-is. All normal doubles, +Inf, -Inf, and NaN
///     (which is positive: 0x7FF8...) pass through unchanged.
///   - Tagged values: SIGN_BIT | QNAN | 3-bit type tag | 48-bit payload
///     These occupy the "negative quiet NaN" space which no IEEE 754 operation
///     ever produces.
///
/// Tags (bits 50-48):
///   000 = pointer to GC-managed object
///   001 = 32-bit signed integer (SMI optimization)
///   010 = boolean (bit 0 of payload)
///   011 = null
///   100 = undefined
///   101 = interned string id (u32)
///   110 = symbol id (u32)
///   111 = (reserved)
use std::fmt;

use crate::runtime::object::ObjectId;
use crate::util::interner::StringId;

const SIGN_BIT: u64 = 1 << 63;
/// Quiet NaN prefix: exponent all 1s + quiet bit set
const QNAN: u64 = 0x7FF8_0000_0000_0000;
/// Our NaN-box prefix: sign bit + quiet NaN. All tagged values have these bits set.
const NANBOX: u64 = SIGN_BIT | QNAN;
/// Mask for the 3-bit type tag (bits 50-48)
const TAG_MASK: u64 = 0x0007_0000_0000_0000;
/// Mask for the 48-bit payload
const PAYLOAD_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
/// Combined mask: NANBOX + tag bits
const NANBOX_MASK: u64 = NANBOX | TAG_MASK;

const TAG_OBJECT: u64 = NANBOX;
const TAG_INT: u64 = NANBOX | (0b001 << 48);
const TAG_BOOL: u64 = NANBOX | (0b010 << 48);
const TAG_NULL: u64 = NANBOX | (0b011 << 48);
const TAG_UNDEFINED: u64 = NANBOX | (0b100 << 48);
const TAG_STRING: u64 = NANBOX | (0b101 << 48);
const TAG_SYMBOL: u64 = NANBOX | (0b110 << 48);

/// A JavaScript value packed into 64 bits via NaN-boxing.
#[derive(Clone, Copy, PartialEq)]
#[repr(transparent)]
pub struct Value(u64);

/// Sentinel values
pub const UNDEFINED: Value = Value(TAG_UNDEFINED);
pub const NULL: Value = Value(TAG_NULL);
pub const TRUE: Value = Value(TAG_BOOL | 1);
pub const FALSE: Value = Value(TAG_BOOL);

impl Value {
    // ---- Constructors ----

    #[inline]
    pub fn number(n: f64) -> Self {
        Self(n.to_bits())
    }

    #[inline]
    pub fn int(i: i32) -> Self {
        Self(TAG_INT | (i as u32 as u64))
    }

    #[inline]
    pub fn boolean(b: bool) -> Self {
        if b { TRUE } else { FALSE }
    }

    #[inline]
    pub fn null() -> Self {
        NULL
    }

    #[inline]
    pub fn undefined() -> Self {
        UNDEFINED
    }

    #[inline]
    pub fn string(id: StringId) -> Self {
        Self(TAG_STRING | id.0 as u64)
    }

    #[inline]
    pub fn symbol(id: u32) -> Self {
        Self(TAG_SYMBOL | id as u64)
    }

    /// Create a Value from an ObjectId (stored in the object tag slot).
    /// The ObjectId's u32 is used as the payload.
    #[inline]
    pub fn object_id(id: ObjectId) -> Self {
        Self(TAG_OBJECT | (id.0 as u64))
    }

    /// Create a Value from a raw pointer to a GC-managed object.
    ///
    /// # Safety
    /// The pointer must be valid and managed by the GC.
    #[inline]
    pub unsafe fn object(ptr: *mut u8) -> Self {
        debug_assert!(!ptr.is_null());
        Self(TAG_OBJECT | (ptr as u64 & PAYLOAD_MASK))
    }

    // ---- Type checks ----

    /// Returns true if this value is a f64 number (not an integer SMI).
    #[inline]
    pub fn is_float(&self) -> bool {
        // A float is anything that doesn't have our NANBOX prefix (sign + qnan).
        // Real NaN (0x7FF8...) does NOT have the sign bit, so it's correctly
        // identified as a float.
        (self.0 & NANBOX) != NANBOX
    }

    /// Returns true if this is a SMI (small integer) value.
    #[inline]
    pub fn is_int(&self) -> bool {
        (self.0 & NANBOX_MASK) == TAG_INT
    }

    /// Returns true if this is any numeric value (float or int).
    #[inline]
    pub fn is_number(&self) -> bool {
        self.is_float() || self.is_int()
    }

    #[inline]
    pub fn is_boolean(&self) -> bool {
        (self.0 & NANBOX_MASK) == TAG_BOOL
    }

    #[inline]
    pub fn is_null(&self) -> bool {
        self.0 == TAG_NULL
    }

    #[inline]
    pub fn is_undefined(&self) -> bool {
        self.0 == TAG_UNDEFINED
    }

    #[inline]
    pub fn is_nullish(&self) -> bool {
        self.is_null() || self.is_undefined()
    }

    #[inline]
    pub fn is_string(&self) -> bool {
        (self.0 & NANBOX_MASK) == TAG_STRING
    }

    #[inline]
    pub fn is_symbol(&self) -> bool {
        (self.0 & NANBOX_MASK) == TAG_SYMBOL
    }

    #[inline]
    pub fn is_object(&self) -> bool {
        (self.0 & NANBOX_MASK) == TAG_OBJECT
    }

    // ---- Extractors ----

    /// Extract as f64. Returns the number whether it's stored as float or SMI.
    #[inline]
    pub fn as_number(&self) -> Option<f64> {
        if self.is_float() {
            Some(f64::from_bits(self.0))
        } else if self.is_int() {
            Some(self.as_int().unwrap() as f64)
        } else {
            None
        }
    }

    #[inline]
    pub fn as_float(&self) -> Option<f64> {
        if self.is_float() {
            Some(f64::from_bits(self.0))
        } else {
            None
        }
    }

    #[inline]
    pub fn as_int(&self) -> Option<i32> {
        if self.is_int() {
            Some((self.0 & 0xFFFF_FFFF) as u32 as i32)
        } else {
            None
        }
    }

    #[inline]
    pub fn as_bool(&self) -> Option<bool> {
        if self.is_boolean() {
            Some((self.0 & 1) != 0)
        } else {
            None
        }
    }

    #[inline]
    pub fn as_string_id(&self) -> Option<StringId> {
        if self.is_string() {
            Some(StringId((self.0 & 0xFFFF_FFFF) as u32))
        } else {
            None
        }
    }

    #[inline]
    pub fn as_symbol_id(&self) -> Option<u32> {
        if self.is_symbol() {
            Some((self.0 & 0xFFFF_FFFF) as u32)
        } else {
            None
        }
    }

    /// Extract an ObjectId from an object-tagged value.
    #[inline]
    pub fn as_object_id(&self) -> Option<ObjectId> {
        if self.is_object() {
            Some(ObjectId((self.0 & PAYLOAD_MASK) as u32))
        } else {
            None
        }
    }

    /// Extract object pointer.
    ///
    /// # Safety
    /// The caller must ensure the pointer is still valid (not GC'd).
    #[inline]
    pub unsafe fn as_object_ptr(&self) -> Option<*mut u8> {
        if self.is_object() {
            Some((self.0 & PAYLOAD_MASK) as *mut u8)
        } else {
            None
        }
    }

    // ---- JS semantics ----

    /// ToBoolean (ES spec): returns the JS truthiness of this value.
    pub fn to_boolean(&self) -> bool {
        if self.is_float() {
            let n = f64::from_bits(self.0);
            n != 0.0 && !n.is_nan()
        } else if self.is_int() {
            self.as_int().unwrap() != 0
        } else if self.is_boolean() {
            (self.0 & 1) != 0
        } else if self.is_null() || self.is_undefined() {
            false
        } else if self.is_string() {
            // Empty string is falsy -- we'll need interner context for this.
            // For now, all strings are truthy (refined in runtime with interner).
            // StringId(0) will be reserved for the empty string.
            self.as_string_id().unwrap().0 != 0
        } else {
            // Objects (including functions) are always truthy
            true
        }
    }

    /// Raw bits (for debugging/comparison).
    #[inline]
    pub fn raw(&self) -> u64 {
        self.0
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_undefined() {
            write!(f, "undefined")
        } else if self.is_null() {
            write!(f, "null")
        } else if self.is_boolean() {
            write!(f, "{}", self.as_bool().unwrap())
        } else if self.is_int() {
            write!(f, "{}i", self.as_int().unwrap())
        } else if self.is_float() {
            write!(f, "{}", f64::from_bits(self.0))
        } else if self.is_string() {
            write!(f, "str#{}", self.as_string_id().unwrap().0)
        } else if self.is_symbol() {
            write!(f, "sym#{}", self.as_symbol_id().unwrap())
        } else if self.is_object() {
            write!(f, "obj@{:#x}", self.0 & PAYLOAD_MASK)
        } else {
            write!(f, "Value({:#018x})", self.0)
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_undefined() {
            write!(f, "undefined")
        } else if self.is_null() {
            write!(f, "null")
        } else if self.is_boolean() {
            write!(f, "{}", self.as_bool().unwrap())
        } else if self.is_int() {
            write!(f, "{}", self.as_int().unwrap())
        } else if self.is_float() {
            let n = f64::from_bits(self.0);
            if n.is_nan() {
                write!(f, "NaN")
            } else if n.is_infinite() {
                if n > 0.0 { write!(f, "Infinity") } else { write!(f, "-Infinity") }
            } else {
                write!(f, "{n}")
            }
        } else if self.is_string() {
            write!(f, "<string#{}>", self.as_string_id().unwrap().0)
        } else if self.is_symbol() {
            write!(f, "Symbol({})", self.as_symbol_id().unwrap())
        } else if self.is_object() {
            write!(f, "[object Object]")
        } else {
            write!(f, "???")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_undefined() {
        let v = Value::undefined();
        assert!(v.is_undefined());
        assert!(!v.is_null());
        assert!(!v.is_number());
        assert!(!v.is_object());
        assert!(!v.to_boolean());
    }

    #[test]
    fn test_null() {
        let v = Value::null();
        assert!(v.is_null());
        assert!(v.is_nullish());
        assert!(!v.to_boolean());
    }

    #[test]
    fn test_booleans() {
        assert!(Value::boolean(true).is_boolean());
        assert_eq!(Value::boolean(true).as_bool(), Some(true));
        assert_eq!(Value::boolean(false).as_bool(), Some(false));
        assert!(Value::boolean(true).to_boolean());
        assert!(!Value::boolean(false).to_boolean());
        assert_eq!(TRUE, Value::boolean(true));
        assert_eq!(FALSE, Value::boolean(false));
    }

    #[test]
    fn test_integers() {
        let v = Value::int(42);
        assert!(v.is_int());
        assert!(v.is_number());
        assert!(!v.is_float());
        assert_eq!(v.as_int(), Some(42));
        assert_eq!(v.as_number(), Some(42.0));
        assert!(v.to_boolean());

        let zero = Value::int(0);
        assert!(!zero.to_boolean());

        let neg = Value::int(-1);
        assert_eq!(neg.as_int(), Some(-1));
        assert_eq!(neg.as_number(), Some(-1.0));

        let max = Value::int(i32::MAX);
        assert_eq!(max.as_int(), Some(i32::MAX));

        let min = Value::int(i32::MIN);
        assert_eq!(min.as_int(), Some(i32::MIN));
    }

    #[test]
    fn test_floats() {
        let v = Value::number(3.14);
        assert!(v.is_float());
        assert!(v.is_number());
        assert!(!v.is_int());
        assert_eq!(v.as_float(), Some(3.14));
        assert_eq!(v.as_number(), Some(3.14));
        assert!(v.to_boolean());

        // Zero
        let zero = Value::number(0.0);
        assert!(!zero.to_boolean());

        // Negative zero
        let neg_zero = Value::number(-0.0);
        assert!(!neg_zero.to_boolean());

        // NaN
        let nan = Value::number(f64::NAN);
        assert!(nan.is_float());
        assert!(!nan.to_boolean());

        // Infinity
        let inf = Value::number(f64::INFINITY);
        assert!(inf.is_float());
        assert!(inf.to_boolean());
        assert_eq!(inf.as_number(), Some(f64::INFINITY));

        // Negative infinity
        let neg_inf = Value::number(f64::NEG_INFINITY);
        assert!(neg_inf.is_float());
        assert_eq!(neg_inf.as_number(), Some(f64::NEG_INFINITY));
    }

    #[test]
    fn test_strings() {
        let v = Value::string(StringId(42));
        assert!(v.is_string());
        assert_eq!(v.as_string_id(), Some(StringId(42)));
        assert!(v.to_boolean());

        // Empty string (id 0) is falsy
        let empty = Value::string(StringId(0));
        assert!(!empty.to_boolean());
    }

    #[test]
    fn test_symbols() {
        let v = Value::symbol(7);
        assert!(v.is_symbol());
        assert_eq!(v.as_symbol_id(), Some(7));
    }

    #[test]
    fn test_type_discrimination() {
        // Each type should only match its own check
        let values = [
            Value::undefined(),
            Value::null(),
            Value::boolean(true),
            Value::int(1),
            Value::number(1.5),
            Value::string(StringId(1)),
            Value::symbol(1),
        ];

        for (i, v) in values.iter().enumerate() {
            assert_eq!(v.is_undefined(), i == 0, "undefined check failed for {v:?}");
            assert_eq!(v.is_null(), i == 1, "null check failed for {v:?}");
            assert_eq!(v.is_boolean(), i == 2, "boolean check failed for {v:?}");
            assert_eq!(v.is_int(), i == 3, "int check failed for {v:?}");
            assert_eq!(v.is_float(), i == 4, "float check failed for {v:?}");
            assert_eq!(v.is_string(), i == 5, "string check failed for {v:?}");
            assert_eq!(v.is_symbol(), i == 6, "symbol check failed for {v:?}");
        }
    }
}
