/// stats for string_cache
use std::collections::HashMap;

use crate::data::dataframe::CompressionStats;
// TODO https://en.wikipedia.org/wiki/Dynamic_perfect_hashing

pub type Atom = string_cache::Atom<string_cache::EmptyStaticAtomSet>;

/// WHY: we compact bytes. this results in much more likely compaction as upper bits will be mostly unused.
/// of course this is worse if we have lots of unique strings.
/// we inline short strings the same way Atom does
///
/// avoid cloning.
#[derive(Debug, Clone)]
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
    pub fn as_str(&self) -> &str {
        assert!(!self.is_inlined());
        let ptr = (self.0 & !1) as *const u8;
        let len = unsafe { std::ptr::read(ptr as *const usize) };
        let data_ptr = unsafe { ptr.add(std::mem::size_of::<usize>()) };
        let slice = unsafe { std::slice::from_raw_parts(data_ptr, len) };
        std::str::from_utf8(slice).unwrap()
    }

    pub fn from_index(index: usize) -> Self {
        assert!(index >> 63 == 0); // ensure we dont lose a bit when shifting left
        AtomIndexed(index << 1)
    }
    pub fn from_str(s: &str) -> Self {
        assert!(s.len() <= 7);
        let mut data = [0; 8];

        // A short string up to 7 bytes, stored inline in most-significant 56 bits. Bits #4 to #7 (the upper nibble of the lower byte) are the length of the string.
        data[..s.len()].copy_from_slice(s.as_bytes());
        data[7] = s.len() as u8;
        let value = u64::from_le_bytes(data);
        AtomIndexed(value as usize | 1)
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
        if atom.len() <= 7 {
            return AtomIndexed::from_str(&atom);
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
            Some(Atom::from(index.as_str()))
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
            let indexed = AtomIndexed::from_str(s);
            assert!(indexed.is_inlined());
            assert_eq!(indexed.as_str(), s);
        }

        for index in [2, 0, 1, 2234, 253463, 65754646, 44444444444] {
            let indexed = AtomIndexed::from_index(index);
            assert!(!indexed.is_inlined());
            assert_eq!(indexed.as_index(), index);
        }
    }
}
