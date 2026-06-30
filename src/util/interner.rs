use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

/// An interned string identifier. Comparison is O(1).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct StringId(pub u32);

/// FxHash — the fast, non-cryptographic hash rustc itself uses. The default
/// `HashMap` hasher is SipHash (DoS-resistant but slow); the interner is on the
/// hot path of *every* string operation (each string value is interned), and
/// has no adversarial-input concern, so a fast hash is a large win. ~5–10×
/// faster than SipHash for the short keys the interner sees.
#[derive(Default)]
pub struct FxHasher {
    hash: u64,
}

const FX_SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl FxHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(FX_SEED);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, mut bytes: &[u8]) {
        while bytes.len() >= 8 {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&bytes[..8]);
            self.add(u64::from_le_bytes(buf));
            bytes = &bytes[8..];
        }
        if !bytes.is_empty() {
            let mut buf = [0u8; 8];
            buf[..bytes.len()].copy_from_slice(bytes);
            self.add(u64::from_le_bytes(buf));
        }
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

type FxBuildHasher = BuildHasherDefault<FxHasher>;

/// String interner: maps strings to unique `StringId` values.
/// All identifier names, property keys, and string literals go through here.
///
/// StringId(0) is always the empty string "".
pub struct Interner {
    map: HashMap<Box<str>, StringId, FxBuildHasher>,
    strings: Vec<String>,
}

impl Interner {
    pub fn new() -> Self {
        let mut interner = Self {
            map: HashMap::default(),
            strings: Vec::new(),
        };
        // Reserve id 0 for empty string (falsy in JS)
        interner.intern("");
        interner
    }

    /// Intern a string, returning its unique id.
    /// If the string was already interned, returns the existing id.
    pub fn intern(&mut self, s: &str) -> StringId {
        if let Some(&id) = self.map.get(s) {
            return id;
        }
        let id = StringId(self.strings.len() as u32);
        self.strings.push(s.to_owned());
        self.map.insert(Box::from(s), id);
        id
    }

    /// Resolve a StringId back to its string.
    pub fn resolve(&self, id: StringId) -> &str {
        &self.strings[id.0 as usize]
    }

    /// Number of interned strings.
    pub fn len(&self) -> usize {
        self.strings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.strings.is_empty()
    }
}

impl Default for Interner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_string_is_zero() {
        let interner = Interner::new();
        assert_eq!(interner.resolve(StringId(0)), "");
    }

    #[test]
    fn test_intern_and_resolve() {
        let mut interner = Interner::new();
        let id = interner.intern("hello");
        assert_eq!(interner.resolve(id), "hello");
    }

    #[test]
    fn test_deduplication() {
        let mut interner = Interner::new();
        let id1 = interner.intern("foo");
        let id2 = interner.intern("foo");
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_different_strings() {
        let mut interner = Interner::new();
        let a = interner.intern("a");
        let b = interner.intern("b");
        assert_ne!(a, b);
    }
}
