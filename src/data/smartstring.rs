/// UNUSED
/// originally we used string_cache crate, and now we use our own stringbuffer which goes in a different direction
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
/// implements a string that either stores an Arc<String> or a small string packed into the word boundary
///
/// Alternatives:
/// Strings could be archetyped by power of 2 length, and stored directly.
/// Combined with block masks this could work well. However, it would lead to potentially far more archetypes.
/// Additionally it presents a problem for reused strings interleaved within the same archetype.
/// And this problem is aggregious enough in the case of large strings to warrent a seperate solution
///
/// However, what we have is not enough.
/// Consider the pathological case of `error!("{} {}", timestamp, e)`
/// Ultimately we will want a better system to representing strings, which can parse out reused tokens
/// If implemented this type will have to be changed from Arc to whatever type represents a pointer to a fragmented string.
use std::mem::ManuallyDrop;

/// XXX cannot be str, because that makes Arc<str> a fat pointer.
/// we want max efficiency for reused strings, even &'static str is too big! (though Arc<&'static str> is a thin pointer and could work)
///
/// it also should not be Arc<String> since this results in 8(arc) -> 16(ref counts) + 24(pointer, len, capacity) -> [DATA] overhead for one-off strings.
/// best case would be 8(arc) -> 16(ref counts) + 8(size) + [DATA] with the size inside the arc instead of outside.
///
/// compare all this to baseline of just storing a Box<str> which would be 8(pointer) + 8(size) -> [DATA] our overhead is always double :( but that is the cost of having replicated strings.
/// NOTE that each pointer incurs ~16 bytes of allocator overhead depending on implementation.

type Stored = str;

use std::str;
use std::sync::{Arc, Weak};

/// Either an Arc<T> or an inline string of up to 7 bytes.
/// uses the least signifigant bit as the discriminator
/// works because Arc is a pointer that must be aligned
///
/// unfortionately there is good reason not to use a custom allocator
/// https://github.com/rust-lang/rfcs/issues/3540
///
/// although we could setup a custom global allocator, because that could be Size 0
pub union ArcOrTinyStr {
    data: u64,
    arc: ManuallyDrop<Arc<Stored>>,
}

impl ArcOrTinyStr {
    /// Try to create from a string of up to 7 bytes.
    pub fn from_str(s: &str) -> Option<Self> {
        let bytes = s.as_bytes();
        if bytes.len() > 7 {
            return None;
        }

        dbg!(bytes);
        let mut buf = [0u8; 8];
        buf[0] = ((bytes.len() as u8) << 1) + 1; // length
        buf[1..=bytes.len()].copy_from_slice(bytes); // inline data
        dbg!(buf);

        let data = bytemuck::cast::<_, u64>(buf);
        assert_eq!((data & 0xFF) >> 1, bytes.len() as u64, "wrong len {data}");
        assert_eq!(data & 1, 1, "Packed string missing discriminator {data}");

        Some(Self { data })
    }

    /// If this is a string, return it.
    pub fn as_str<'a>(&'a self) -> Option<&'a str> {
        if unsafe { self.data } & 1 == 1 {
            let bytes = bytemuck::cast_ref::<u64, [u8; 8]>(unsafe { &self.data });
            let len = (bytes[0] >> 1) as usize;
            Some(str::from_utf8(&bytes[1..=len]).unwrap())
        } else {
            None
        }
    }

    /// Create from an Arc<T>.
    pub fn from_arc(arc: Arc<Stored>) -> Self {
        assert_eq!(size_of::<Self>(), size_of::<Arc<Stored>>());
        let arc = ManuallyDrop::new(arc);
        unsafe {
            let ptr = &arc as *const _ as *const u64;
            assert_eq!(*ptr & 1, 0, "Arc pointer not aligned, cannot tag");
        }

        Self { arc }
    }

    /// If this is an Arc<T>, get a reference (without taking ownership).
    pub fn as_arc<'a>(&'a self) -> Option<&'a Arc<Stored>> {
        if unsafe { self.data } & 1 == 0 {
            // SAFETY: we know this was created from Arc<T>
            let arc_ref: &'a Arc<Stored> = unsafe { &self.arc };
            Some(arc_ref)
        } else {
            None
        }
    }

    pub fn get(&self) -> &str {
        match unsafe { self.data & 1 == 0 } {
            true => self.as_arc().unwrap(),
            false => self.as_str().unwrap(),
        }
    }

    pub fn new(&self, src: &str) -> Self {
        Self::from_str(src).unwrap_or_else(|| Self::from_arc(src.to_string().into()))
    }
}

impl Drop for ArcOrTinyStr {
    fn drop(&mut self) {
        if unsafe { self.data } & 1 == 0 {
            unsafe {
                ManuallyDrop::drop(&mut self.arc);
            }
        }
    }
}
impl Clone for ArcOrTinyStr {
    fn clone(&self) -> Self {
        if unsafe { self.data } & 1 == 0 {
            let arc = unsafe { self.arc.clone() };
            Self::from_arc(ManuallyDrop::into_inner(arc))
        } else {
            Self {
                data: unsafe { self.data },
            }
        }
    }
}
impl PartialEq for ArcOrTinyStr {
    fn eq(&self, other: &Self) -> bool {
        self.get() == other.get()
    }
}

// TODO https://iliabylich.github.io/arena-based-parsers/tldr.html
// TODO https://en.wikipedia.org/wiki/Dynamic_perfect_hashing
// struct StringStorage {
//     data: HashMap<u64, Weak<str>>,
// }

// impl StringStorage {
//     fn get(&mut self, s: &str) -> ArcOrTinyStr {
//         if let Some(tiny) = ArcOrTinyStr::from_str(s) {
//             return tiny;
//         }

//         let mut hasher = DefaultHasher::new();
//         s.hash(&mut hasher);
//         let hash = hasher.finish();

//         if let Some(v) = self.data.get_mut(&hash) {
//             let arc = match v.upgrade() {
//                 Some(arc) => arc,
//                 None => {
//                     let arc: Arc<str> = Arc::from(s);
//                     *v = Arc::<str>::downgrade(&arc);
//                     arc
//                 }
//             };

//             return ArcOrTinyStr::from_arc(arc);
//         }

//         let arc: Arc<str> = Arc::from(s);
//         self.data.insert(hash, Arc::<str>::downgrade(&arc));
//         ArcOrTinyStr::from_arc(arc)
//     }

//     fn cleanup(&mut self) {
//         self.data.retain(|_, v| v.strong_count() > 0);
//     }
// }

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn test_inline_str() {
        let s = "hello!";
        let val = ArcOrTinyStr::from_str(s).expect("should fit");

        assert_eq!(val.as_str(), Some("hello!"));
        assert!(val.as_arc().is_none());

        let s = "this is too long"; // > 7 bytes
        assert!(ArcOrTinyStr::from_str(s).is_none());
    }

    #[test]
    fn test_arc_storage_and_retrieval() {
        let arc1: Arc<str> = Arc::from("hello world");

        let val = ArcOrTinyStr::from_arc(arc1.clone());
        let arc = Arc::<str>::downgrade(&arc1);
        drop(arc1);

        assert_eq!(val.as_str(), None);

        let arc_back = val.as_arc().unwrap();
        assert_eq!(arc_back.as_ref(), "hello world");

        let arc_back2 = val.as_arc().unwrap().clone();
        assert_eq!(Arc::strong_count(&arc_back2), 2); //  arc_back + arc_back2

        assert_eq!(val.get(), "hello world");
        assert_eq!(arc_back2.as_ref(), "hello world");

        drop(arc_back2);
        assert_eq!(Arc::strong_count(arc_back), 1); // arc_back
        assert!(arc.upgrade().is_some());
        drop(val);
        assert!(arc.upgrade().is_none());
    }

    #[test]
    fn test_drop_arc_decrements_count() {
        let counter = Arc::<str>::from("test");

        {
            let boxed = ArcOrTinyStr::from_arc(counter.clone());
            assert_eq!(Arc::strong_count(&counter), 2);

            assert_eq!(boxed.get(), "test");
        }

        // After _boxed is dropped, Arc count should go down
        assert_eq!(Arc::strong_count(&counter), 1);
    }
}
