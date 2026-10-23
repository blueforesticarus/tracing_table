//! Values with an explicit alignment and access by value, never unaligned references.
use std::{
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
    mem,
};

/// Selects a supported power-of-two alignment for [`Unalign`].
#[doc(hidden)]
pub struct Alignment<const A: usize>;

/// Sealed layout mapping: callers cannot substitute a marker with the wrong alignment.
#[doc(hidden)]
pub trait SupportedAlignment: sealed::Sealed {
    type Marker: Copy;
}

mod sealed {
    pub trait Sealed {}
}

// repr(align) requires a literal, so map const arguments to zero-sized markers.
// Cover Rust's entire supported alignment range, rejecting other values by bounds.
macro_rules! alignments {
    ($($name:ident = $alignment:literal),* $(,)?) => {$(
        #[doc(hidden)]
        #[derive(Clone, Copy)]
        #[repr(align($alignment))]
        pub struct $name;

        impl sealed::Sealed for Alignment<$alignment> {}
        impl SupportedAlignment for Alignment<$alignment> {
            type Marker = $name;
        }
    )*};
}

alignments! {
    A1 = 1, A2 = 2, A4 = 4, A8 = 8, A16 = 16, A32 = 32, A64 = 64,
    A128 = 128, A256 = 256, A512 = 512, A1024 = 1024, A2048 = 2048,
    A4096 = 4096, A8192 = 8192, A16384 = 16384, A32768 = 32768,
    A65536 = 65536, A131072 = 131072, A262144 = 262144, A524288 = 524288,
    A1048576 = 1048576, A2097152 = 2097152, A4194304 = 4194304,
    A8388608 = 8388608, A16777216 = 16777216, A33554432 = 33554432,
    A67108864 = 67108864, A134217728 = 134217728, A268435456 = 268435456,
    A536870912 = 536870912,
}

// Keep packing private. Rust moves a packed field to aligned storage before
// running its destructor, so owned, non-Copy values retain ordinary drop behavior.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Packed<T>(T);

/// Owns `T` with exactly `A`-byte alignment (a power of two from 1 through 2^29).
/// Size is `size_of::<T>()` rounded up to `A`; T's internal layout is unchanged.
/// Borrows of this wrapper are safe, but no reference to its inner T is exposed.
///
/// ```
/// use tracing_table::data::unalign::Unalign;
/// // A low-alignment field remains accessible through ordinary safe methods.
/// let mut value = Unalign::<u32, 2>::new(42);
/// value.set(value.get() + 1);
/// assert_eq!(value.into_inner(), 43);
/// ```
///
/// ```compile_fail
/// use tracing_table::data::unalign::Unalign;
/// // Reject invalid alignments at compile time rather than during construction.
/// let _ = Unalign::<u32, 3>::new(42);
/// ```
#[repr(C)]
pub struct Unalign<T, const A: usize>
where
    Alignment<A>: SupportedAlignment,
{
    value: Packed<T>,
    // A zero-length array raises alignment without storing a marker value.
    alignment: [<Alignment<A> as SupportedAlignment>::Marker; 0],
}

impl<T, const A: usize> Unalign<T, A>
where
    Alignment<A>: SupportedAlignment,
{
    pub const fn new(value: T) -> Self {
        Self {
            value: Packed(value),
            alignment: [],
        }
    }

    /// Read by value; the compiler knows the packed field may be unaligned.
    pub const fn get(&self) -> T
    where
        T: Copy,
    {
        self.value.0
    }

    /// Move ownership out; no unaligned reference or duplicate owner is created.
    pub fn into_inner(self) -> T {
        self.value.0
    }

    /// Install the replacement before returning the previous owner to the caller.
    pub fn replace(&mut self, value: T) -> T {
        // Packed<T> itself has alignment one, so borrowing the wrapper is safe.
        // Moving its field out by value lets Rust generate the unaligned access.
        mem::replace(&mut self.value, Packed(value)).0
    }

    /// Replace before dropping, keeping self initialized even if T's drop panics.
    pub fn set(&mut self, value: T) {
        drop(self.replace(value));
    }
}

impl<T: Copy, const A: usize> Copy for Unalign<T, A> where Alignment<A>: SupportedAlignment {}
impl<T: Copy, const A: usize> Clone for Unalign<T, A>
where
    Alignment<A>: SupportedAlignment,
{
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: Default, const A: usize> Default for Unalign<T, A>
where
    Alignment<A>: SupportedAlignment,
{
    fn default() -> Self {
        Self::new(T::default())
    }
}
impl<T: Copy + fmt::Debug, const A: usize> fmt::Debug for Unalign<T, A>
where
    Alignment<A>: SupportedAlignment,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.get().fmt(f)
    }
}
impl<T: Copy + PartialEq, const A: usize> PartialEq for Unalign<T, A>
where
    Alignment<A>: SupportedAlignment,
{
    fn eq(&self, other: &Self) -> bool {
        self.get() == other.get()
    }
}
impl<T: Copy + Eq, const A: usize> Eq for Unalign<T, A> where Alignment<A>: SupportedAlignment {}
impl<T: Copy + PartialOrd, const A: usize> PartialOrd for Unalign<T, A>
where
    Alignment<A>: SupportedAlignment,
{
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.get().partial_cmp(&other.get())
    }
}
impl<T: Copy + Ord, const A: usize> Ord for Unalign<T, A>
where
    Alignment<A>: SupportedAlignment,
{
    fn cmp(&self, other: &Self) -> Ordering {
        self.get().cmp(&other.get())
    }
}
impl<T: Copy + Hash, const A: usize> Hash for Unalign<T, A>
where
    Alignment<A>: SupportedAlignment,
{
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.get().hash(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, ptr::NonNull, rc::Rc};

    #[test]
    fn layouts_and_pointer_niches_survive_reduced_alignment() {
        // Test both lowering and raising alignment, including array stride.
        macro_rules! check {
            ($($a:literal),*) => {$(
                assert_eq!(align_of::<Unalign<u32, $a>>(), $a);
                assert_eq!(size_of::<Unalign<u32, $a>>(), 4usize.next_multiple_of($a));
            )*};
        }
        check!(1, 2, 4, 8, 16, 32, 64);
        assert_eq!(align_of::<Unalign<u8, 536870912>>(), 536870912);
        assert_eq!(
            size_of::<Option<Unalign<NonNull<u8>, 2>>>(),
            size_of::<usize>()
        );
    }

    #[test]
    fn reads_and_writes_work_at_an_address_not_aligned_for_u32() {
        #[repr(C, align(4))]
        struct Offset {
            prefix: u16,
            value: Unalign<u32, 2>,
        }
        let mut item = Offset {
            prefix: 0,
            value: Unalign::new(u32::MAX),
        };
        assert_eq!(&item.value as *const _ as usize % align_of::<u32>(), 2);
        assert_eq!(item.value.get(), u32::MAX);
        assert_eq!(item.value.replace(7), u32::MAX);
        item.value.set(11);
        assert_eq!(item.value.into_inner(), 11);
    }

    #[test]
    fn non_copy_owners_are_moved_and_dropped_exactly_once() {
        #[repr(align(16))]
        struct Tracked(Rc<Cell<usize>>);
        impl Drop for Tracked {
            fn drop(&mut self) {
                // A packed field's destructor must receive an aligned temporary.
                assert_eq!(self as *const _ as usize % 16, 0);
                self.0.set(self.0.get() + 1);
            }
        }
        let drops = Rc::new(Cell::new(0));
        #[repr(C, align(16))]
        struct Offset {
            prefix: u8,
            values: [Unalign<Tracked, 1>; 2],
        }
        let mut item = Offset {
            prefix: 0,
            values: [
                Unalign::new(Tracked(drops.clone())),
                Unalign::new(Tracked(drops.clone())),
            ],
        };
        assert_eq!(&item.values[0] as *const _ as usize % 16, 1);
        drop(item.values[0].replace(Tracked(drops.clone())));
        assert_eq!(drops.get(), 1);
        item.values[1].set(Tracked(drops.clone()));
        assert_eq!(drops.get(), 2);
        let [first, second] = item.values;
        drop(first.into_inner());
        drop(second);
        assert_eq!(drops.get(), 4);
    }

    #[test]
    fn replacement_remains_initialized_when_the_old_destructor_panics() {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        struct Tracked {
            panic: bool,
            drops: Rc<Cell<usize>>,
        }
        impl Drop for Tracked {
            fn drop(&mut self) {
                self.drops.set(self.drops.get() + 1);
                assert!(!self.panic, "old value deliberately panics on drop");
            }
        }
        let drops = Rc::new(Cell::new(0));
        let mut value = Unalign::<_, 2>::new(Tracked {
            panic: true,
            drops: drops.clone(),
        });
        let result = catch_unwind(AssertUnwindSafe(|| {
            // set must install the new owner before running the old destructor.
            value.set(Tracked {
                panic: false,
                drops: drops.clone(),
            });
        }));
        assert!(result.is_err());
        assert_eq!(drops.get(), 1);
        assert!(!value.into_inner().panic);
        assert_eq!(drops.get(), 2);
    }
}
