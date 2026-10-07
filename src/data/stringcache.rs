/// stats for string_cache
use std::{collections::HashMap, fmt, ops::Deref, sync::LazyLock};

use parking_lot::RwLock;

use crate::data::dataframe::CompressionStats;
// TODO https://en.wikipedia.org/wiki/Dynamic_perfect_hashing

pub type Atom = string_cache::Atom<string_cache::EmptyStaticAtomSet>;

// Process-wide caches used by AtomIndexed::from and index lookup.
pub static STRING_INDEX: LazyLock<RwLock<Index>> = LazyLock::new(|| RwLock::new(Index::default()));
pub static STRING_STATS: LazyLock<RwLock<Stats>> = LazyLock::new(|| RwLock::new(Stats::default()));

/// WHY: we compact bytes. this results in much more likely compaction as upper bits will be mostly unused.
/// of course this is worse if we have lots of unique strings.
/// we inline short strings the same way Atom does
///
/// avoid cloning.
///
/// NOTE: inlining small strings can make storage worse if those strings are used alot, since 8 bytes will vary where-as with inlining you only need up the max id of unique strings.
/// we likely want to come up with something with seperate storage for reused and non-reused strings.

#[derive(Debug, Clone, Copy)]
pub struct AtomIndexed(usize);

impl AtomIndexed {
    pub fn is_inlined(&self) -> bool {
        // get first bit as bool
        self.0 & 1 == 1
    }
    pub fn as_index(&self) -> usize {
        assert!(!self.is_inlined());
        self.0 >> 1
    }
    pub fn as_small_str(&self) -> &str {
        assert!(
            self.is_inlined(),
            "resolve indexed strings through Index::get"
        );
        // A short string up to 7 bytes, stored inline in most-significant 56 bits. Bits #4 to #7 (the upper nibble of the lower byte) are the length of the string.
        let len = (self.0 >> 4 & 0xf) as usize;
        let bytes = bytemuck::bytes_of(&self.0);
        let text = if cfg!(target_endian = "little") {
            &bytes[1..]
        } else {
            &bytes[..bytes.len() - 1]
        };
        std::str::from_utf8(&text[..len]).unwrap()
    }

    pub fn from_index(index: usize) -> Self {
        assert!(index <= usize::MAX >> 1); // ensure we dont lose a bit when shifting left
        AtomIndexed(index << 1)
    }
    pub fn from_small_str(s: &str) -> Self {
        assert!(s.len() <= 7);
        // A short string up to 7 bytes, stored inline in most-significant 56 bits. Bits #4 to #7 (the upper nibble of the lower byte) are the length of the string.
        let mut value = 0usize;
        let bytes = bytemuck::bytes_of_mut(&mut value);
        let text = if cfg!(target_endian = "little") {
            &mut bytes[1..]
        } else {
            let end = bytes.len() - 1;
            &mut bytes[..end]
        };
        text[..s.len()].copy_from_slice(s.as_bytes());
        AtomIndexed(value | (s.len() << 4) | 1)
    }
}

mod _impls {
    use super::*;
    use std::ops::Deref;

    // Match Atom's string access and construction without exposing cache locks.
    impl Deref for AtomIndexed {
        type Target = str;

        fn deref(&self) -> &str {
            self.as_ref()
        }
    }

    impl AsRef<str> for AtomIndexed {
        fn as_ref(&self) -> &str {
            if self.is_inlined() {
                return self.as_small_str();
            }
            let atom = Atom::from(*self);
            let text = atom.deref() as *const str;
            // SAFETY: STRING_INDEX retains the dynamic Atom's string allocation.
            // Its entries must never be removed or replaced while references exist.
            // Inline strings instead borrow self above, never a temporary Atom.
            unsafe { &*text }
        }
    }

    impl fmt::Display for AtomIndexed {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(Atom::from(self.clone()).deref())
        }
    }

    impl From<Atom> for AtomIndexed {
        fn from(atom: Atom) -> Self {
            // Count constructions in either inline or indexed form.
            STRING_STATS.write().inc(&atom);
            STRING_INDEX.write().insert(atom)
        }
    }

    impl From<&str> for AtomIndexed {
        fn from(value: &str) -> Self {
            Self::from(Atom::from(value))
        }
    }

    impl From<String> for AtomIndexed {
        fn from(value: String) -> Self {
            Self::from(value.as_str())
        }
    }

    impl From<&String> for AtomIndexed {
        fn from(value: &String) -> Self {
            Self::from(value.as_str())
        }
    }

    impl From<AtomIndexed> for Atom {
        fn from(value: AtomIndexed) -> Self {
            STRING_INDEX
                .read()
                .get(value)
                .expect("string index must exist in the global cache")
        }
    }
}

// to enable us to have mostly sequential items in the block
#[derive(Debug, Default)]
pub struct Index {
    index: HashMap<Atom, usize>,
    items: HashMap<usize, Atom>,
}

impl Index {
    pub fn insert(&mut self, atom: Atom) -> AtomIndexed {
        if atom.len() < size_of::<usize>() {
            return AtomIndexed::from_small_str(&atom);
        }

        if let Some(&index) = self.index.get(&atom) {
            AtomIndexed::from_index(index)
        } else {
            let index = self.items.len();
            self.items.insert(index, atom.clone());
            self.index.insert(atom, index);
            AtomIndexed::from_index(index)
        }
    }

    pub fn get(&self, index: AtomIndexed) -> Option<Atom> {
        if index.is_inlined() {
            Some(Atom::from(index.as_small_str()))
        } else {
            self.items.get(&index.as_index()).cloned()
        }
    }

    pub fn stats(&self) -> CompressionStats {
        let mut stats = CompressionStats::default();

        let E = (size_of::<(usize, Atom)>() as f64 + 1.0) / 0.875; // hashmap overhead estimate
        stats.overhead += (E * self.index.len() as f64) as usize;
        stats.overalloc += (E * (self.index.capacity() - self.index.len()) as f64) as usize;

        stats
    }
}

#[derive(Debug, Default)]
pub struct Stats {
    // TODO we are duplicating work string_cache does, but we don't have access.
    // TODO reimplement string_cache here for more control.
    counts: HashMap<Atom, usize>,
}

impl Stats {
    /// Number of distinct tracked interned strings; inline strings are excluded.
    pub fn interned_count(&self) -> usize {
        self.counts.len()
    }

    pub fn inc(&mut self, atom: &Atom) {
        if atom.len() <= 7 {
            // Atom stores it directly, so we don't need to count it.
            return;
        }
        *self.counts.entry(atom.clone()).or_insert(0) += 1;
    }

    pub fn dec(&mut self, atom: &Atom) {
        if atom.len() <= 7 {
            // Atom stores it directly, so we don't need to count it.
            return;
        }
        if let Some(count) = self.counts.get_mut(&atom) {
            *count -= 1;
            if *count == 0 {
                self.counts.remove(&atom);
            }
        }
    }

    pub fn get(&mut self, atom: &Atom) -> usize {
        if atom.len() <= 7 {
            return 0;
        }

        *self.counts.get(&atom).unwrap_or(&0)
    }

    pub fn stats(&self) -> CompressionStats {
        let mut stats = CompressionStats::default();
        stats.overalloc = 4096 * 40; // NOTE This is just the size of the string_cache::AtomSet, which is 4096 entries of 40 bytes each. additional entries put in linked list.
        for (atom, count) in &self.counts {
            if atom.len() <= 7 {
                // Atom stores it directly
                // stats.compressed += 8 * count;
                // stats.data += atom.len() * count;
                // actually don't even consider these.
            } else {
                // TODO there are open questions about how much metadata to store.
                stats.compressed += 8 * count + atom.len();
                stats.data += atom.len() * count; // TODO should perhaps include the 16 bytes a &[u8] would take up.
                stats.overhead += 40; // string_cache::Entry is 40 bytes
                stats.overalloc += 0; // n/a
            }
        }
        stats
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn global_atom_conversions() {
        // Long strings resolve to owned Atoms; inline strings borrow their own bytes.
        for text in ["", "A", "1234567", "éclair", "global indexed long string"] {
            let value = AtomIndexed::from(text);
            assert_eq!(&*Atom::from(value), text);
            assert_eq!(&*value, text);
            assert_eq!(value.as_ref(), text);
        }
        assert_eq!(size_of::<AtomIndexed>(), size_of::<usize>());
    }

    #[test]
    fn test_atom_indexed() {
        let mut db = Index::default();
        let mut stats = Stats::default();

        for s in ["hello", "", "1234567", "A"] {
            let a = Atom::from(s);
            let i = db.insert(a.clone());

            assert!(i.is_inlined());
            assert_eq!(a, db.get(i).unwrap());
        }

        for (vi, count, s) in [
            (0, 1, "some longer strings"),
            (1, 1, "to test out the indexing"),
            (2, 1, "this one is duplicated"),
            (2, 2, "this one is duplicated"),
            (3, 1, "this is different"),
        ] {
            let a = Atom::from(s);

            stats.inc(&a);
            assert_eq!(stats.get(&a), count);

            stats.dec(&a);
            assert_eq!(stats.get(&a), count - 1);

            let i = db.insert(a.clone());

            assert!(!i.is_inlined());
            assert_eq!(i.as_index(), vi);
            assert_eq!(a, db.get(i).unwrap());
        }

        for s in ["hello", "", "1234567", "A"] {
            let indexed = AtomIndexed::from_small_str(s);
            assert!(indexed.is_inlined());
            assert_eq!(indexed.as_small_str(), s);
        }

        for index in [2, 0, 1, 2234, 253463, 65754646, 44444444444] {
            let indexed = AtomIndexed::from_index(index);
            assert!(!indexed.is_inlined());
            assert_eq!(indexed.as_index(), index);
        }
    }
}
