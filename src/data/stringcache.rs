use std::collections::HashMap;

use string_cache::{Atom, EmptyStaticAtomSet};

/// implements a droppable string interner
/// strings can be lookedup by hash.
/// stores a ref count per string, refs are held by frames.
/// old strings are manually garbage collected.
///
/// whether this should be specific to the callsite is an interesting question

// TODO https://en.wikipedia.org/wiki/Dynamic_perfect_hashing
struct StringStorage {
    data: HashMap<usize, ()>,
}

impl StringStorage {
    fn get(&mut self, s: &str) -> Atom<EmptyStaticAtomSet> {
        // well that's underwelming. It seems that string_cache can just handle it.
        // we must be carefull to drop this iff we drop the frame
        Atom::from(s)
    }
}
