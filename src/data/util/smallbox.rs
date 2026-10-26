//! CODEX GENERATED: reviewed
//! Thin ownership for slice-tailed values: a u16 length lives before the value.
use super::unalign::Unalign;
use std::{
    alloc::{self, Layout},
    marker::PhantomData,
    ops::{Deref, DerefMut},
    ptr::{self, NonNull, Pointee},
};

/// One-pointer owner for DSTs whose metadata is a slice length fitting in u16.
/// Lengths count bytes for archived byte tails, elements for ordinary slices.
/// Future option: encode length in units of the pointee's alignment to extend
/// the range. That requires preserving discarded low bits (for example in tagged
/// pointer bits); rounding alone loses exact frame/trailer boundaries.
#[repr(C)]
pub struct SmallBox<T: ?Sized + Pointee<Metadata = usize>> {
    // Only the stored address is unaligned; allocations still satisfy T's alignment.
    pointer: Unalign<NonNull<u8>, 2>,
    owner: PhantomData<T>,
}

// SAFETY: Ownership, borrowing and mutation follow Box<T>; only the stored
// pointer representation differs. No shared mutation is exposed.
unsafe impl<T: ?Sized + Pointee<Metadata = usize> + Send> Send for SmallBox<T> {}
unsafe impl<T: ?Sized + Pointee<Metadata = usize> + Sync> Sync for SmallBox<T> {}

impl<T: ?Sized + Pointee<Metadata = usize>> SmallBox<T> {
    /// Initialize a DST tail directly beside its owning header.
    ///
    /// SAFETY: T must have plain repr(C), with no extra packing/alignment, and
    /// exactly two fields, H followed by U. Its metadata is the tail's slice length.
    pub(crate) unsafe fn with_header<
        H,
        U: ?Sized + Pointee<Metadata = usize>,
        F: FnOnce(*mut U),
    >(
        header: H,
        tail: Initializer<U, F>,
    ) -> Self {
        let Initializer {
            layout: tail_layout,
            length,
            initialize,
            ..
        } = tail;
        u16::try_from(length).expect("SmallBox length exceeds u16");
        let (value_layout, tail_offset) = Layout::new::<H>().extend(tail_layout).unwrap();
        let (layout, offset) = Self::layout(value_layout.pad_to_align());
        let allocation = Allocation::new(layout);
        // SAFETY: The caller guarantees the field layout, and Initializer guarantees
        // the tail's layout and initialization. Keep the header on the stack until
        // initialization succeeds so both it and the allocation are freed on panic.
        unsafe {
            let value = allocation.0.add(offset);
            initialize(ptr::from_raw_parts_mut(value.add(tail_offset), length));
            value.cast::<H>().write(header);
            let owner = Self::from_raw(ptr::from_raw_parts_mut(value, length));
            std::mem::forget(allocation);
            owner
        }
    }

    /// Full retained allocation, including length prefix and alignment padding.
    pub fn allocation_size(&self) -> usize {
        Self::layout(Layout::for_value(&**self)).0.size()
    }

    pub(crate) fn layout(value: Layout) -> (Layout, usize) {
        let (layout, offset) = Layout::new::<u16>().extend(value).unwrap();
        (layout.pad_to_align(), offset)
    }

    /// Adopt an initialized value allocated using Self::layout. The two bytes
    /// immediately before the value must be writable; its slice length must fit u16.
    /// The caller transfers sole ownership of both value and allocation.
    unsafe fn from_raw(value: *mut T) -> Self {
        let length = u16::try_from(ptr::metadata(value)).expect("SmallBox length exceeds u16");
        let pointer = value.cast::<u8>();
        // SAFETY: The caller reserved the prefix in this same allocation.
        unsafe {
            pointer
                .sub(size_of::<u16>())
                .cast::<u16>()
                .write_unaligned(length);
        }
        Self {
            pointer: Unalign::new(unsafe { NonNull::new_unchecked(pointer) }),
            owner: PhantomData,
        }
    }

    /// Move a boxed slice-tailed value into a thin allocation without cloning T.
    pub fn from_box(value: Box<T>) -> Self {
        let length = ptr::metadata(value.as_ref());
        u16::try_from(length).expect("SmallBox length exceeds u16");
        let value_layout = Layout::for_value(value.as_ref());
        let (layout, offset) = Self::layout(value_layout);
        let allocation = Allocation::new(layout);
        // SAFETY: The layout aligns T and reserves its full storage plus prefix.
        // Raw copying moves ownership; the source allocation is freed without
        // dropping its moved value. Zero-sized Box values have no allocation.
        unsafe {
            let source = Box::into_raw(value);
            let destination = allocation.0.add(offset);
            ptr::copy_nonoverlapping(source.cast::<u8>(), destination, value_layout.size());
            if value_layout.size() != 0 {
                alloc::dealloc(source.cast(), value_layout);
            }
            let owner = Self::from_raw(ptr::from_raw_parts_mut(destination, length));
            std::mem::forget(allocation);
            owner
        }
    }

    fn raw(&self) -> *mut T {
        // SAFETY: Every constructor stores the immutable length immediately
        // before the aligned value. Prefix padding accommodates higher alignments.
        let length = unsafe {
            self.pointer
                .get()
                .as_ptr()
                .sub(size_of::<u16>())
                .cast::<u16>()
                .read_unaligned()
        };
        ptr::from_raw_parts_mut(self.pointer.get().as_ptr(), length as usize)
    }
}

/// A checked layout and one-shot writer, independent of the destination owner.
/// Frame code can share packing between standalone boxes and collocated archives.
pub(crate) struct Initializer<T: ?Sized + Pointee<Metadata = usize>, F: FnOnce(*mut T)> {
    layout: Layout,
    length: usize,
    initialize: F,
    value: PhantomData<T>,
}

impl<T: ?Sized + Pointee<Metadata = usize>, F: FnOnce(*mut T)> Initializer<T, F> {
    /// SAFETY: layout must match T with this length. Given fresh, aligned storage,
    /// initialize must leave every field valid before returning, and clean up any
    /// partially initialized owners if it panics. It must not free the storage.
    pub(crate) unsafe fn new(layout: Layout, length: usize, initialize: F) -> Self {
        Self {
            layout,
            length,
            initialize,
            value: PhantomData,
        }
    }

    pub(crate) fn into_box(self) -> Box<T> {
        // SAFETY: Construction established the layout and initializer contract.
        unsafe { boxed_with(self.layout, self.length, self.initialize) }
    }
}

impl<T: ?Sized + Pointee<Metadata = usize>> Deref for SmallBox<T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: This owner keeps T alive; &self allows shared access only.
        unsafe { &*self.raw() }
    }
}

impl<T: ?Sized + Pointee<Metadata = usize>> DerefMut for SmallBox<T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: The owner is not Clone/Copy, and &mut self ensures exclusivity.
        unsafe { &mut *self.raw() }
    }
}

impl<T: ?Sized + Pointee<Metadata = usize>> AsRef<T> for SmallBox<T> {
    fn as_ref(&self) -> &T {
        self
    }
}

// Free even if the owned value's destructor unwinds.
struct Allocation(*mut u8, Layout);
impl Allocation {
    fn new(layout: Layout) -> Self {
        assert!(
            layout.size() != 0,
            "container allocation must include a header"
        );
        // SAFETY: Every caller reserves a nonempty, checked Layout.
        let pointer = unsafe { alloc::alloc(layout) };
        if pointer.is_null() {
            alloc::handle_alloc_error(layout);
        }
        Self(pointer, layout)
    }
}

/// Allocate a conventional boxed DST while keeping raw ownership logic here.
/// SAFETY: layout must match T with this length; initialize must leave T valid.
/// If initialization panics, it must clean up any fields it already initialized.
pub(crate) unsafe fn boxed_with<T: ?Sized + Pointee<Metadata = usize>>(
    layout: Layout,
    length: usize,
    initialize: impl FnOnce(*mut T),
) -> Box<T> {
    let allocation = Allocation::new(layout);
    let raw = ptr::from_raw_parts_mut(allocation.0, length);
    initialize(raw);
    // SAFETY: The caller's initializer has established the complete value.
    let owner = unsafe { Box::from_raw(raw) };
    std::mem::forget(allocation);
    owner
}

impl Drop for Allocation {
    fn drop(&mut self) {
        // SAFETY: This guard owns the allocation with its original layout.
        unsafe {
            alloc::dealloc(self.0, self.1);
        }
    }
}

impl<T: ?Sized + Pointee<Metadata = usize>> Drop for SmallBox<T> {
    fn drop(&mut self) {
        let raw = self.raw();
        let (layout, offset) = Self::layout(Layout::for_value(&**self));
        // SAFETY: Constructors allocate the prefix and T together. Drop T once,
        // then release the whole allocation, including during destructor unwinding.
        unsafe {
            let _allocation = Allocation(self.pointer.get().as_ptr().sub(offset), layout);
            ptr::drop_in_place(raw);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, rc::Rc};

    #[test]
    fn u16_prefix_preserves_exact_byte_lengths_at_the_limit() {
        // Odd lengths must not be rounded into the DST metadata or its trailer.
        for length in [0, 1, 2, 3, u16::MAX as usize - 1, u16::MAX as usize] {
            let mut value = SmallBox::from_box(vec![37u8; length].into_boxed_slice());
            assert_eq!(value.len(), length);
            assert_eq!(value.allocation_size(), (2 + length).next_multiple_of(2));
            if length != 0 {
                value[length - 1] = 91;
                assert_eq!(value[length - 1], 91);
            }
        }
    }

    #[test]
    #[should_panic(expected = "SmallBox length exceeds u16")]
    fn oversized_boxed_tail_is_rejected_before_ownership_moves() {
        SmallBox::from_box(vec![0u8; u16::MAX as usize + 1].into_boxed_slice());
    }

    #[test]
    fn oversized_direct_tail_drops_header_without_running_initializer() {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        struct Header(Rc<Cell<usize>>);
        impl Drop for Header {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        #[repr(C)]
        struct Combined {
            header: Header,
            tail: [u8],
        }
        let drops = Rc::new(Cell::new(0));
        let called = Cell::new(false);
        let result = catch_unwind(AssertUnwindSafe(|| {
            let length = u16::MAX as usize + 1;
            // SAFETY: The byte-tail layout matches; the length check must reject
            // it before allocating or invoking this deliberately failing writer.
            let tail = unsafe {
                Initializer::new(
                    Layout::array::<u8>(length).unwrap(),
                    length,
                    |_: *mut [u8]| {
                        called.set(true);
                        panic!("oversized tail initializer must not run");
                    },
                )
            };
            // SAFETY: Combined is repr(C) with exactly this header and tail.
            let _: SmallBox<Combined> =
                unsafe { SmallBox::with_header(Header(drops.clone()), tail) };
        }));
        assert!(result.is_err());
        assert!(!called.get());
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn thin_slices_preserve_alignment_mutation_and_owned_drops() {
        #[repr(align(16))]
        struct Tracked(Rc<Cell<usize>>);
        impl Drop for Tracked {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let drops = Rc::new(Cell::new(0));
        for length in [0, 1, 7] {
            let source: Box<[_]> = (0..length).map(|_| Tracked(drops.clone())).collect();
            let mut value = SmallBox::from_box(source);
            assert_eq!(value.len(), length);
            assert_eq!(value.as_ptr() as usize % 16, 0);
            if length != 0 {
                value.swap(0, length - 1);
            }
        }
        assert_eq!(drops.get(), 8);
        assert_eq!(size_of::<SmallBox<[Tracked]>>(), size_of::<usize>());
    }

    #[test]
    fn direct_initialization_preserves_tail_address_layout_and_owned_drops() {
        struct Header(Rc<Cell<usize>>);
        impl Drop for Header {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        #[repr(align(32))]
        struct Tracked(Header);
        #[repr(C)]
        struct Combined {
            header: Header,
            tail: [Tracked],
        }
        let drops = Rc::new(Cell::new(0));
        for length in [0, 1, 7] {
            let destination = Cell::new(ptr::null_mut());
            let tail_layout = Layout::array::<Tracked>(length).unwrap();
            // SAFETY: The array layout matches the tail; every element is written
            // into supplied storage, with no intermediate payload allocation.
            let tail = unsafe {
                Initializer::new(tail_layout, length, |raw: *mut [Tracked]| {
                    let raw = raw.cast::<Tracked>();
                    destination.set(raw);
                    for index in 0..length {
                        raw.add(index).write(Tracked(Header(drops.clone())));
                    }
                })
            };
            // SAFETY: Combined is repr(C) with exactly this header and slice tail.
            let value: SmallBox<Combined> =
                unsafe { SmallBox::with_header(Header(drops.clone()), tail) };
            assert_eq!(value.tail.len(), length);
            assert_eq!(value.tail.as_ptr().cast_mut(), destination.get());
            assert_eq!(value.tail.as_ptr() as usize % 32, 0);
            let expected = Layout::new::<Header>()
                .extend(tail_layout)
                .unwrap()
                .0
                .pad_to_align();
            assert_eq!(Layout::for_value(&*value), expected);
            assert_eq!(
                value.allocation_size(),
                SmallBox::<Combined>::layout(expected).0.size()
            );
            // Read both owners through their final location before destruction.
            assert!(Rc::ptr_eq(&value.header.0, &drops));
            assert!(value.tail.iter().all(|item| Rc::ptr_eq(&item.0.0, &drops)));
            let before = drops.get();
            drop(value);
            assert_eq!(drops.get() - before, length + 1);
        }
        assert_eq!(drops.get(), 11);
    }

    #[test]
    fn failed_tail_initialization_drops_the_uncommitted_header() {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        struct Header(Rc<Cell<usize>>);
        impl Drop for Header {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        #[repr(C)]
        struct Combined {
            header: Header,
            tail: [u8],
        }
        let drops = Rc::new(Cell::new(0));
        let result = catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: The layout matches [u8]; no owned tail fields need cleanup
            // when initialization deliberately unwinds after writing one byte.
            let tail = unsafe {
                Initializer::new(Layout::array::<u8>(4).unwrap(), 4, |raw: *mut [u8]| {
                    raw.cast::<u8>().write(42);
                    panic!("tail initialization failed");
                })
            };
            // SAFETY: The combined fields match the header and tail exactly.
            let _: SmallBox<Combined> =
                unsafe { SmallBox::with_header(Header(drops.clone()), tail) };
        }));
        assert!(result.is_err());
        assert_eq!(drops.get(), 1);
    }
}
