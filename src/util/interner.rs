use std::collections::HashMap;

/// An interned string identifier. Comparison is O(1).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct StringId(pub u32);

/// String interner: maps strings to unique `StringId` values.
/// All identifier names, property keys, and string literals go through here.
///
/// StringId(0) is always the empty string "".
pub struct Interner {
    map: HashMap<String, StringId>,
    strings: Vec<String>,
}

impl Interner {
    pub fn new() -> Self {
        let mut interner = Self {
            map: HashMap::new(),
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
        self.map.insert(s.to_owned(), id);
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
