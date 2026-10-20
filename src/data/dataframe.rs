use std::{
    alloc::{self, Layout},
    clone,
    collections::{HashMap, HashSet},
    default,
    hash::{DefaultHasher, Hash, Hasher},
    ops::{Add, Deref, DerefMut, Range},
    ptr, slice,
    sync::{Arc, Weak},
};

use bytemuck::Pod;
use smallvec::SmallVec;
use streaming_iterator::StreamingIterator;
use tracing::{
    callsite::Identifier,
    field::{self, Visit},
};

use super::CompressionStats;

pub enum AnyFrame<'a> {
    Frame(&'a mut Box<Frame>),
    Compact(&'a mut Box<CompactFrame>),
}

pub struct Row<'a> {
    pub init: bool,
    pub buffered: Option<&'a mut [u8]>,
    pub owned: Option<Vec<u8>>,
    pub index: usize,
    pub frame: AnyFrame<'a>,
}

impl<'a> Row<'a> {
    pub fn get_mut<'b>(&'b mut self) -> &'b mut [u8] {
        match &mut self.frame {
            AnyFrame::Frame(frame) => {
                let index = (1 + self.index) * frame.unit;
                &mut frame.buffer[index..index + frame.unit]
            }
            AnyFrame::Compact(compact_frame) => {
                let slice = if self.buffered.is_none() {
                    if self.owned.is_none() {
                        self.owned = Some(vec![0u8; compact_frame.unit]);
                    }
                    self.owned.as_mut().unwrap().as_mut_slice()
                } else {
                    self.buffered.as_mut().unwrap()
                };

                //todo cleanup
                if !self.init {
                    self.init = true;
                    slice.copy_from_slice(compact_frame.first());
                    if self.index != 0 {
                        let index = 2 * compact_frame.unit
                            + (compact_frame.unit - compact_frame.mask().constant_bytes())
                                * (self.index - 1);
                        for (a, b) in MaskIter::new(compact_frame.mask()) {
                            slice[b].copy_from_slice(&compact_frame.buffer[index..][a]);
                        }
                    }
                }

                slice
            }
        }
    }
}

impl<'a> Drop for Row<'a> {
    fn drop(&mut self) {
        match &mut self.frame {
            AnyFrame::Frame(frame) => frame.compute_mask(self.index),
            AnyFrame::Compact(compact_frame) => {
                if !self.init {
                    return;
                }
                let unit = compact_frame.unit;
                let buffer = match self.buffered.as_mut() {
                    Some(v) => v,
                    None => self.owned.as_mut().unwrap().as_mut_slice(),
                };

                let mut rebuild = false;
                for (i, b) in compact_frame.mask().iter().enumerate() {
                    if *b == 0 && buffer[i] != compact_frame.buffer[unit + i] {
                        rebuild = true;
                        break;
                    }
                }

                if !rebuild {
                    let layout = compact_frame.buffer();
                    if self.index != 0 {
                        let index = (self.index - 1) * layout.mask.compact_unit();
                        for (a, b) in MaskIter::new(layout.mask) {
                            layout.data[index..][a].copy_from_slice(&buffer[b]);
                        }
                    } else {
                        layout.first.copy_from_slice(&buffer);
                    }
                } else {
                    let mut unpacked = Frame::new(unit, compact_frame.length);
                    compact_frame.uncompact(&mut unpacked);
                    unpacked.buffer[unit..][unit * self.index..][..unit].copy_from_slice(&buffer);
                    // A formerly constant header byte can change when a span closes.
                    unpacked.compute_mask(self.index);
                    let new = CompactFrame::new_from(&unpacked);
                    **compact_frame = new;
                }
            }
        }
    }
}

/// This is a 2d byte array which supports deduped columns.
/// supports both in place and non in place compaction
/// these should be made large enough to compact data.
/// theoretically work with all non-pin types.
///
/// TODO: const generic unit Frame?
// Box must use the same alignment as the raw allocation when freeing this DST.
#[repr(C, align(16))]
pub struct Frame {
    // NOTE: these could be stored with the pointer.
    // in fact unit could be stored once for the whole block_buffer
    pub unit: usize,
    pub length: usize,

    pub buffer: [u8],
}

pub struct FrameBufferLayout<'a> {
    mask: &'a [u8],
    data: &'a [u8],
}

// Both frame types have two usize fields followed by a byte slice. Include the
// trailing padding in the allocation so Box's dynamically computed drop layout
// matches exactly; the slice metadata must still exclude that padding.
fn frame_allocation_layout(buffer_size: usize) -> Layout {
    let total_size = size_of::<[usize; 2]>()
        .checked_add(buffer_size)
        .expect("frame allocation size overflow");
    Layout::from_size_align(total_size, 16)
        .expect("frame allocation exceeds layout limits")
        .pad_to_align()
}

impl Frame {
    pub fn new(unit: usize, cap: usize) -> Box<Frame> {
        // TODO(soundness): Only the mask is initialized, but the public buffer and
        // allocate() expose unwritten row bytes through safe APIs. Hide spare bytes
        // from safe reads and use MaybeUninit for allocation without zeroing capacity.
        // Reject overflow before it can produce an undersized allocation in release builds.
        let buffer_size = cap
            .checked_mul(unit)
            .and_then(|size| size.checked_add(unit))
            .expect("frame buffer size overflow");
        let layout = frame_allocation_layout(buffer_size);

        // Allocate raw memory
        let ptr = unsafe { alloc::alloc(layout) };
        assert!(!ptr.is_null());

        // Initialize the header
        let mut v = unsafe {
            let frame_ptr = ptr as *mut u8;
            let frame_ptr: *mut Frame = std::ptr::from_raw_parts_mut(frame_ptr, buffer_size);
            Box::from_raw(frame_ptr)
        };

        v.length = 0;
        v.unit = unit;
        v.buffer[..unit].fill(0); // Mask
        v
    }

    /// set the mask for these bytes, preventing them from being compacted
    /// usefull if a field needs to remain mutable in the compacted frame
    /// TODO really need to figure out rule of thumb for int sizes in cases like these
    pub fn lock_editable(&mut self, i1: usize, i2: usize) {
        for i in i1..i2 {
            self.buffer[i] = 0xFF;
        }
    }

    pub fn clear(&mut self) {
        self.length = 0;
        self.buffer[..self.unit].fill(0);
    }

    pub fn buffer(&mut self) -> FrameBufferLayout {
        // TODO: Before restoring unchecked splits, encapsulate unit and ensure every
        // constructor/mutation preserves unit <= buffer.len(), including on panic.
        // let (mask, data) = unsafe { self.buffer.split_at_mut_unchecked(self.unit) };
        let (mask, data) = self.buffer.split_at_mut(self.unit);
        FrameBufferLayout { mask, data }
    }

    pub fn mask(&self) -> &Mask {
        &Mask::get(&self.buffer[..self.unit])
    }

    pub fn push(&mut self, data: &[u8]) {
        let (buffer, _) = self.allocate().unwrap();
        buffer.copy_from_slice(data);
        drop(buffer);
        self.compute_mask(self.length - 1)
    }

    pub fn allocate(&mut self) -> Option<(&mut [u8], usize)> {
        if self.length == self.capacity() {
            return None;
        }
        let (_mask, buffer) = self.buffer.split_at_mut(self.unit);
        let index = self.unit * self.length;
        self.length += 1;
        Some((&mut buffer[index..index + self.unit], self.length - 1))
    }

    // Appends inspect the previous row; edits inspect both neighboring pairs.
    // This also catches edits to row zero, whose new constants affect every row.
    pub fn compute_mask(&mut self, index: usize) {
        let (mask, buffer) = self.buffer.split_at_mut(self.unit);
        for next in index.max(1)..=(index + 1).min(self.length.saturating_sub(1)) {
            let offset = self.unit * next;
            for ((m, a), b) in mask
                .iter_mut()
                .zip(&buffer[offset - self.unit..offset])
                .zip(&buffer[offset..offset + self.unit])
            {
                *m |= *a ^ *b;
            }
        }
    }

    pub fn capacity(&self) -> usize {
        let size = self.buffer.len();
        size / self.unit - 1
    }

    // This is possible if we add an extra row to the beginning of the buffer.
    // Why would we want to do this thought?
    // pub fn compact_in_place(&mut self) {

    pub fn compact_size_needed(&self) -> usize {
        let cbytes = self.mask().constant_bytes();
        (self.unit - cbytes) * (self.length - 1) + self.unit * 2 + 8 * 2
    }

    pub fn compact(&self, new: &mut CompactFrame) {
        // TODO check at start and use unchecked for rest
        new.unit = self.unit;
        new.length = self.length;

        // copy mask and first row
        let offset = self.unit * 2;
        new.buffer[..offset].copy_from_slice(&self.buffer[..offset]);

        let mut write_ptr = &mut new.buffer[offset..];
        let mut read_ptr = &self.buffer[offset..];
        let mask = Mask::get(&self.buffer[..self.unit]);
        let inc = self.unit - mask.constant_bytes();

        // TODO perhaps copy the mask iterable to a stack allocated buffer.
        for _row in 1..self.length {
            for (a, b) in MaskIter::new(mask) {
                write_ptr[a].copy_from_slice(&read_ptr[b]);
            }
            write_ptr = &mut write_ptr[inc..];
            read_ptr = &read_ptr[self.unit..];
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &[u8]> {
        // if the data we cast to needs alignment and unit is not even mult, then this also needs to use a buffer.
        self.buffer[self.unit..].chunks(self.unit).take(self.length)
    }

    pub fn compression_ratio(&self) -> CompressionStats {
        let mask = self.mask();
        CompressionStats {
            data: self.length * self.unit,
            compressed: self.length * self.unit,
            // The DST size includes the usize header and trailing alignment padding.
            overhead: size_of_val(self) - self.buffer.len() + mask.0.len(),
            overalloc: self.buffer.len() - mask.0.len() - (self.length * self.unit),
        }
    }
}

/// TODO implement bit compacted frame for even more density at the cost of compute
/// TODO implement bit per byte mask because frames may have as many rows as they have bytes
/// TODO this NEEDS to store the compact unit, but unfortionately that breaks compatibility with Frame.
///      should also store the mask as regions. it doesn't need the bitwise... does it?
// Keep this representation in sync with Frame and frame_allocation_layout.
#[repr(C, align(16))]
pub struct CompactFrame {
    pub unit: usize,
    pub length: usize,

    pub buffer: [u8],
}

pub struct CompactFrameLayout<'a> {
    mask: &'a mut Mask,
    first: &'a mut [u8],
    data: &'a mut [u8],
}

impl CompactFrame {
    pub fn new_uninitialized(unit: usize, length: usize, const_bytes: usize) -> Box<CompactFrame> {
        // Validate subtraction as well as multiplication/addition before allocating.
        let compact_unit = unit
            .checked_sub(const_bytes)
            .expect("constant byte count exceeds frame unit");
        let header_size = unit
            .checked_mul(2)
            .expect("compact frame header size overflow");
        let buffer_size = length
            .checked_mul(compact_unit)
            .and_then(|size| size.checked_add(header_size))
            .expect("compact frame buffer size overflow");
        let layout = frame_allocation_layout(buffer_size);

        // Allocate raw memory
        let ptr = unsafe { alloc::alloc(layout) };
        assert!(!ptr.is_null());

        // Cast the pointer
        let v = unsafe {
            let frame_ptr = ptr as *mut u8;
            let frame_ptr: *mut CompactFrame = std::ptr::from_raw_parts_mut(frame_ptr, buffer_size);
            Box::from_raw(frame_ptr)
        };

        // TODO(soundness): This safe constructor returns uninitialized unit/length
        // fields and buffer bytes. Initialize headers before forming the typed Box
        // and prevent safe reads of payload bytes until they have been written.
        v
    }

    pub fn new_from(frame: &Frame) -> Box<Self> {
        // allocate space for packed frame and move
        // TODO special handling of uncompactible frames
        let mut new = CompactFrame::new_uninitialized(
            frame.unit,
            frame.length,
            frame.mask().constant_bytes(),
        );

        frame.compact(&mut new);
        new
    }

    pub fn mask(&self) -> &Mask {
        &Mask::get(&self.buffer[..self.unit])
    }

    pub fn first(&self) -> &[u8] {
        &self.buffer[self.unit..2 * self.unit]
    }

    pub fn buffer(&mut self) -> CompactFrameLayout {
        // TODO: Before restoring unchecked splits, encapsulate unit and ensure every
        // constructor/mutation preserves unit <= buffer.len() / 2, including on panic.
        // let (header, data) = unsafe { self.buffer.split_at_mut_unchecked(self.unit * 2) };
        // let (mask, first) = unsafe { header.split_at_mut_unchecked(self.unit) };
        let (header, data) = self.buffer.split_at_mut(self.unit * 2);
        let (mask, first) = header.split_at_mut(self.unit);
        CompactFrameLayout {
            mask: Mask::get_mut(mask),
            first,
            data,
        }
    }

    pub fn uncompact(&self, new: &mut Frame) {
        // TODO check size once
        new.unit = self.unit;
        new.length = self.length;

        // copy mask and first value
        let offset = self.unit * 2;
        new.buffer[..offset].copy_from_slice(&self.buffer[..offset]);

        let mut write_ptr = &mut new.buffer[offset..];
        let mut read_ptr = &self.buffer[offset..];
        let mask = Mask::get(&self.buffer[..self.unit]);
        let inc = self.unit - mask.constant_bytes();
        let first = &self.buffer[self.unit..offset];

        // TODO perhaps copy the mask iterable to a stack allocated buffer.
        for _row in 1..self.length {
            write_ptr[..self.unit].copy_from_slice(first);
            for (a, b) in MaskIter::new(mask) {
                write_ptr[b].copy_from_slice(&read_ptr[a]);
            }
            write_ptr = &mut write_ptr[self.unit..];
            read_ptr = &read_ptr[inc..];
        }
    }

    pub fn compression_ratio(&self) -> CompressionStats {
        let mask = self.mask();
        let compressed = (self.length - 1) * (self.unit - mask.constant_bytes()) + self.unit;
        CompressionStats {
            data: self.length * self.unit,
            // Count header and alignment padding as overhead, separate from spare bytes.
            overhead: size_of_val(self) - self.buffer.len() + mask.0.len(),
            compressed,
            overalloc: self.buffer.len() - mask.0.len() - compressed,
        }
    }
}

/// yields (packed range, unpacked range) of non-constant bytes according to the mask
/// ie. for mask [101100] yields ((0,1)(0,1)),((1,3)(2,4))
struct MaskIter<'a> {
    mask: &'a [u8],
    index: usize,
    accum: usize,
}

impl<'a> MaskIter<'a> {
    fn new(mask: &'a [u8]) -> Self {
        Self {
            mask,
            index: 0,
            accum: 0,
        }
    }
}

impl<'a> Iterator for MaskIter<'a> {
    /// (packed, uncompacted)
    type Item = (Range<usize>, Range<usize>);

    fn next(&mut self) -> Option<Self::Item> {
        let mut index = self.index;
        loop {
            match self.mask.get(index) {
                Some(0) => index += 1,
                Some(_) => break, // Found start index
                None => return None,
            }
        }
        let base = index;
        loop {
            index += 1;
            match self.mask.get(index) {
                Some(0) => break, // found end
                None => break,
                Some(_) => continue,
            }
        }
        self.index = index;
        let accum = self.accum;
        self.accum += index - base;
        Some((accum..self.accum, base..index))
    }
}

/// Mask representing which bits/bytes of the buffer vary (1) or are constant (0)
/// currently byte for byte (and so could be used for bit level compaction, though we don't)
/// could be make bit for byte, to save space when rows are wide relative to the size of the frame.
#[repr(C)]
pub struct Mask([u8]);

impl Mask {
    fn get(data: &[u8]) -> &Mask {
        unsafe { &*(data as *const [u8] as *const Mask) }
    }
    fn get_mut(data: &mut [u8]) -> &mut Mask {
        unsafe { &mut *(data as *mut [u8] as *mut Mask) }
    }

    pub fn constant_bytes(&self) -> usize {
        self.0
            .iter()
            .map(|a| (*a == 0) as usize) //TODO would u32 be faster?
            .sum()
    }

    pub fn compact_unit(&self) -> usize {
        self.len() - self.constant_bytes()
    }
}

impl Deref for Mask {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Mask {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

pub struct CompactFrameIter<'a> {
    frame: &'a CompactFrame,
    // Inline capacity is 128 bytes; both storage paths align the row to u128.
    buffer: StackVec<128, u128>,
    index: usize,
    compact_unit: usize,
    capacity: usize,
}

impl<'a> CompactFrameIter<'a> {
    pub fn new(frame: &'a CompactFrame) -> Self {
        let compact_unit = frame.unit - frame.mask().constant_bytes();
        Self {
            frame,
            buffer: StackVec::new(&frame.buffer[frame.unit..2 * frame.unit]),
            index: 0,
            compact_unit,
            capacity: frame.unit * 2 + (frame.length - 1) * compact_unit,
        }
    }
}

impl<'a> StreamingIterator for CompactFrameIter<'a> {
    type Item = [u8];

    fn advance(&mut self) {
        if self.index == 0 {
            self.index = self.frame.unit * 2; // skip mask and first
            return;
        }

        if self.index < self.capacity {
            for (a, b) in MaskIter::new(self.frame.mask()) {
                self.buffer[b].copy_from_slice(&self.frame.buffer[self.index..][a]);
            }
        }
        self.index += self.compact_unit;
    }

    fn get(&self) -> Option<&Self::Item> {
        if self.index > self.capacity {
            return None;
        }

        Some(self.buffer.as_ref())
    }
}

/// Keep the byte array at offset zero in a value aligned for A. Raising the
/// outer enum's alignment alone would not guarantee its byte field's offset.
#[repr(C)]
struct InlineBytes<const S: usize, A> {
    // A zero-length array contributes A's alignment without storing any A values.
    _alignment: [A; 0],
    bytes: [u8; S],
}

/// Store up to S bytes on stack, with fallback to heap. Unlike SmallVec the length is immutable.
/// Use A for alignment.
/// InlineBytes establishes inline alignment; Vec<A> establishes heap alignment.
enum StackVec<const S: usize, A = usize> {
    Inline(InlineBytes<S, A>, usize),
    Heap(Vec<A>, usize),
}

impl<const S: usize, A: Pod> StackVec<S, A> {
    fn new(data: &[u8]) -> Self {
        assert!(
            size_of::<A>() > 0,
            "alignment storage type must have nonzero size"
        );
        let size = data.len();
        let mut v = if size <= S {
            // Initialize the entire array, including its unused suffix; constructing
            // an uninitialized [u8; S] would be undefined behavior.
            Self::Inline(
                InlineBytes {
                    _alignment: [],
                    bytes: [0u8; S],
                },
                size,
            )
        } else {
            // Allocate whole A elements; slice views hide the unused tail bytes.
            // Keeping Vec<A> as the owner also preserves the deallocation alignment.
            Self::Heap(vec![A::zeroed(); size.div_ceil(size_of::<A>())], size)
        };
        v.copy_from_slice(data);
        v
    }
}

impl<const S: usize, A: Pod> Deref for StackVec<S, A> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        match self {
            StackVec::Inline(data, size) => &data.bytes[..*size],
            StackVec::Heap(data, size) => &bytemuck::cast_slice::<A, u8>(data)[..*size],
        }
    }
}

impl<const S: usize, A: Pod> DerefMut for StackVec<S, A> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            StackVec::Inline(data, size) => &mut data.bytes[..*size],
            StackVec::Heap(data, size) => &mut bytemuck::cast_slice_mut::<A, u8>(data)[..*size],
        }
    }
}

#[inline]
pub fn is_aligned_to<T: ?Sized>(reference: &T, alignment: usize) -> bool {
    let ptr = reference as *const T as *const () as usize;
    ptr % alignment == 0
}

// TODO implement StreamingIteratorMut
// it will need to have a type like bevy Mut to avoid needless copying.

// future things to profile:
// - mask bitwise or bytewise. (could have 1 bit per byte, but need to consider alignment)
// - store first row whole or seperate constants (I think storing whole is faster for reconstruction.)

// enum ValueKind {
//     F64,
//     I64,
//     U64,
//     I128,
//     U128,
//     Bool,
//     Str,
// }

// struct FieldKey {
//     field: usize,
//     kind: ValueKind,
// }

// struct Archetype {
//     callsite: Identifier,
//     fields: [FieldKey],
// }

// struct RowCollector<'a> {
//     fields: Vec<ValueKind>,
//     cache: &'a mut StringStorage,
// }

// impl<'a> Visit for RowCollector<'a> {
//     fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {}

//     fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
//         let s = self.cache.get(value);
//         // NOTE: instead of the ArcOrTinyStr we *could* use string size in the archetype.
//         // budget for packed string (or duplicate arc): 8(ArcOrTinyStr)
//         // budget for arc string. 8(ArcOrTinyStr) | 8(hash) + 8(weak_ptr) + 16(arc counts) + len(string) + overhead of empty hashmap bins
//         // if all out strings are dups or less than 8 chars, this is best. if the opposite is true
//         // a normal smartstring: 8(ptr) + 8(len) + 8(capacity) + len(string) would be better.
//         // which can hold strings up to length 24 inside itself.
//     }
// }

#[cfg(test)]
mod test {
    use std::{alloc::Layout, panic::catch_unwind};

    use streaming_iterator::StreamingIterator;

    use crate::data::dataframe::{CompactFrame, CompactFrameIter};

    use super::{Frame, StackVec, frame_allocation_layout};

    #[test]
    fn frame_allocations_match_dynamic_box_layouts() {
        // Vary sizes across padding boundaries and compare the allocation helper
        // with Rust's independently computed DST layouts, including on 32-bit targets.
        for unit in 1..=33 {
            let mut frame = Frame::new(unit, 2);
            frame.push(&vec![0; unit]);
            frame.push(&vec![1; unit]);
            let compact = CompactFrame::new_from(&frame);

            let frame_layout = Layout::for_value(&*frame);
            assert_eq!(frame_layout.align(), 16);
            assert_eq!(frame_allocation_layout(frame.buffer.len()), frame_layout);
            assert_eq!(frame.buffer.len(), 3 * unit);

            // Statistics must cover the entire allocation, including padding, while
            // overalloc counts only unused buffer capacity in both representations.
            let frame_stats = frame.compression_ratio();
            assert_eq!(
                frame_stats.compressed + frame_stats.overhead + frame_stats.overalloc,
                frame_layout.size()
            );
            assert_eq!(frame_stats.overalloc, 0);

            let compact_layout = Layout::for_value(&*compact);
            assert_eq!(compact_layout.align(), 16);
            assert_eq!(
                frame_allocation_layout(compact.buffer.len()),
                compact_layout
            );
            assert_eq!(compact.buffer.len(), 4 * unit);
            let compact_stats = compact.compression_ratio();
            assert_eq!(
                compact_stats.compressed + compact_stats.overhead + compact_stats.overalloc,
                compact_layout.size()
            );
            assert_eq!(compact_stats.overalloc, unit);
        }
    }

    #[test]
    fn frame_allocations_reject_invalid_sizes() {
        // These inputs must fail before allocation even when overflow checks are disabled.
        assert!(catch_unwind(|| Frame::new(usize::MAX, 2)).is_err());
        assert!(catch_unwind(|| Frame::new(usize::MAX, 1)).is_err());
        assert!(catch_unwind(|| frame_allocation_layout(usize::MAX)).is_err());
        assert!(catch_unwind(|| frame_allocation_layout(isize::MAX as usize)).is_err());
        for (unit, length, const_bytes) in [
            (1, 1, 2),
            (usize::MAX, 0, usize::MAX),
            (2, usize::MAX, 0),
            (1, usize::MAX, 0),
        ] {
            assert!(
                catch_unwind(|| CompactFrame::new_uninitialized(unit, length, const_bytes))
                    .is_err()
            );
        }
    }

    #[test]
    fn stack_vec_alignment_survives_storage_changes_and_moves() {
        // Exercise an alignment greater than any built-in row primitive's.
        #[repr(C, align(64))]
        #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
        struct Alignment64([u8; 64]);

        fn check<A: bytemuck::Pod>() {
            // Include empty, full inline, and heap rows with partial final A elements.
            for size in [0, 1, 127, 128, 129, 257] {
                let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
                let value = StackVec::<128, A>::new(&data);
                assert_eq!(&*value, data.as_slice());
                assert_eq!(value.as_ptr() as usize % align_of::<A>(), 0);

                // Moving inline storage changes its address; its alignment must hold
                // at the destination too. Heap storage must retain the same guarantee.
                let mut moved = Box::new(value);
                let bytes: &mut [u8] = &mut moved;
                assert_eq!(bytes.as_ptr() as usize % align_of::<A>(), 0);
                assert_eq!(bytes, data.as_slice());
                bytes.fill(42);
                assert!(bytes.iter().all(|byte| *byte == 42));
            }
        }

        check::<u8>();
        check::<usize>();
        check::<u128>();
        check::<Alignment64>();
    }

    #[test]
    fn data_compact_and_restore() {
        let mut frame = Frame::new(8, 100);
        frame.push("foobar  ".as_bytes());
        frame.push("foobar2 ".as_bytes());
        frame.push("foobar3 ".as_bytes());
        frame.push("foobar14".as_bytes());
        let compact = CompactFrame::new_from(&frame);
        let mut new = Frame::new(8, 4);
        compact.uncompact(&mut new);

        assert_eq!(frame.buffer[..8 * 4 + 8], new.buffer);

        let mut iter = CompactFrameIter::new(&compact);
        assert_eq!("foobar  ".as_bytes(), iter.next().unwrap());
        assert_eq!("foobar2 ".as_bytes(), iter.next().unwrap());
        assert_eq!("foobar3 ".as_bytes(), iter.next().unwrap());
        assert_eq!("foobar14".as_bytes(), iter.next().unwrap());
    }

    #[test]
    fn compact_correct_size() {
        let mut frame = Frame::new(8, 100);
        frame.push("foobar  ".as_bytes());
        frame.push("foobar2 ".as_bytes());
        frame.push("foobar3 ".as_bytes());
        frame.push("foobar14".as_bytes());
        assert_eq!(frame.unit - frame.mask().constant_bytes(), 2);
        let mut compact = CompactFrame::new_from(&frame);
        assert_eq!(compact.mask().constant_bytes(), 6);
        assert_eq!(compact.buffer().first.len(), 8);
        assert_eq!(compact.buffer().mask.len(), 8);
        // assert_eq!(compact.buffer().data.len(), 2 * 3); //alignment!
    }

    #[test]
    fn mask_is_correct() {
        let mut frame = Frame::new(8, 100);
        frame.push("foobar  ".as_bytes());
        frame.push("foobar2 ".as_bytes());
        frame.push("foobar3 ".as_bytes());
        frame.push("foobar14".as_bytes());
        assert!(frame.mask()[..6].iter().all(|a| *a == 0));
        assert!(frame.mask()[6..].iter().all(|a| *a != 0));
        assert_eq!(frame.mask().constant_bytes(), 6);

        frame.clear();
        frame.push(&[0, 0, 0, 3, 0, 0, 0, 10]);
        frame.push(&[0, 0, 0, 1, 0, 0, 0, 10]);
        assert_eq!(frame.mask().0, [0u8, 0, 0, 2, 0, 0, 0, 0][..]);
        assert_eq!(frame.mask().constant_bytes(), 7);
        assert_eq!(frame.unit, 8);
    }
}
