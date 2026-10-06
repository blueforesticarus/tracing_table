use std::{
    alloc::{self, Layout},
    mem,
    ops::{Add, Deref, DerefMut, Range},
};
use streaming_iterator::StreamingIterator;

pub enum AnyFrame<'a> {
    Frame(&'a mut Box<Frame>),
    Compact(&'a mut Box<CompactFrame>),
}

pub struct Row<'a> {
    pub init: bool,
    pub buffered: Option<&'a mut [u8]>,
    pub owned: Option<Vec<u8>>,
    pub index: usize,
    // Set only for completed frames; changed rows invalidate external caches.
    pub revision: Option<&'a mut u64>,
    pub frame: AnyFrame<'a>,
}

impl<'a> Row<'a> {
    pub fn get_mut<'b>(&'b mut self) -> &'b mut [u8] {
        match &mut self.frame {
            AnyFrame::Frame(frame) => {
                self.init = true;
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
        if !self.init {
            return;
        }
        if let Some(revision) = self.revision.as_deref_mut() {
            *revision += 1;
        }
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
/// TODO: aligned Frame
/// TODO: const generic unit Frame?
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

impl Frame {
    pub fn new(unit: usize, cap: usize) -> Box<Frame> {
        assert!(unit > 0 && cap > 0);
        let buffer_size = cap.checked_add(1).unwrap().checked_mul(unit).unwrap();
        // Match the DST's padded layout exactly: Box uses it again at deallocation.
        let layout = frame_layout(buffer_size);
        let raw = unsafe { alloc::alloc_zeroed(layout) };
        if raw.is_null() {
            alloc::handle_alloc_error(layout);
        }
        let frame_ptr = std::ptr::from_raw_parts_mut::<Frame>(raw, buffer_size);
        let mut frame = unsafe { Box::from_raw(frame_ptr) };
        frame.unit = unit;
        frame.length = 0;
        frame
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

    pub fn buffer(&mut self) -> FrameBufferLayout<'_> {
        // TODO check safety in constructor
        let (mask, data) = unsafe { self.buffer.split_at_mut_unchecked(self.unit) };
        FrameBufferLayout { mask, data }
    }

    pub fn mask(&self) -> &Mask {
        &Mask::get(&self.buffer[..self.unit])
    }

    pub fn push(&mut self, data: &[u8]) {
        let (buffer, _) = self.allocate().unwrap();
        buffer.copy_from_slice(data);
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

    pub fn compute_mask(&mut self, index: usize) {
        let (mask, buffer) = self.buffer.split_at_mut(self.unit);
        // Compare an edited row with both neighbours; updating row zero must
        // also mark bytes that now differ from the rest of the frame.
        for neighbour in [index.checked_sub(1), index.checked_add(1)] {
            if let Some(other) = neighbour.filter(|other| *other < self.length) {
                for i in 0..self.unit {
                    mask[i] |= buffer[index * self.unit + i] ^ buffer[other * self.unit + i];
                }
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
        frame_layout((self.unit - cbytes) * self.length.saturating_sub(1) + self.unit * 2).size()
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
            overhead: mask.0.len() + size_of_val(self) - self.buffer.len(),
            overalloc: self.buffer.len() - mask.0.len() - (self.length * self.unit),
        }
    }
}

/// TODO implement bit compacted frame for even more density at the cost of compute
/// TODO implement bit per byte mask because frames may have as many rows as they have bytes
/// TODO this NEEDS to store the compact unit, but unfortionately that breaks compatibility with Frame.
///      should also store the mask as regions. it doesn't need the bitwise... does it?
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
        assert!(unit > 0 && length > 0 && const_bytes <= unit);
        let buffer_size = (length - 1)
            .checked_mul(unit - const_bytes)
            .unwrap()
            .checked_add(unit.checked_mul(2).unwrap())
            .unwrap();
        let layout = frame_layout(buffer_size);
        let raw = unsafe { alloc::alloc_zeroed(layout) };
        if raw.is_null() {
            alloc::handle_alloc_error(layout);
        }
        let frame_ptr = std::ptr::from_raw_parts_mut::<CompactFrame>(raw, buffer_size);
        let mut frame = unsafe { Box::from_raw(frame_ptr) };
        frame.unit = unit;
        frame.length = length;
        frame
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

    pub fn buffer(&mut self) -> CompactFrameLayout<'_> {
        let (header, data) = unsafe { self.buffer.split_at_mut_unchecked(self.unit * 2) };
        let (mask, first) = unsafe { header.split_at_mut_unchecked(self.unit) };
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
            overhead: mask.0.len() + size_of_val(self) - self.buffer.len(),
            compressed,
            overalloc: self.buffer.len() - mask.0.len() - compressed,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompressionStats {
    pub data: usize,
    pub compressed: usize,
    pub overhead: usize,
    pub overalloc: usize,
}

impl Add for CompressionStats {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        CompressionStats {
            data: self.data + rhs.data,
            compressed: self.compressed + rhs.compressed,
            overhead: self.overhead + rhs.overhead,
            overalloc: self.overalloc + rhs.overalloc,
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
    buffer: StackVec<128, u128>,
    // Count rows, not bytes: a fully constant frame has a zero-byte stride.
    next_row: usize,
    current: bool,
}

impl<'a> CompactFrameIter<'a> {
    pub fn new(frame: &'a CompactFrame) -> Self {
        Self {
            frame,
            buffer: StackVec::new(frame.first(), None),
            next_row: 0,
            current: false,
        }
    }
}

impl StreamingIterator for CompactFrameIter<'_> {
    type Item = [u8];
    fn advance(&mut self) {
        self.current = self.next_row < self.frame.length;
        if !self.current {
            return;
        }
        if self.next_row > 0 {
            let offset =
                2 * self.frame.unit + (self.next_row - 1) * self.frame.mask().compact_unit();
            for (packed, full) in MaskIter::new(self.frame.mask()) {
                self.buffer[full].copy_from_slice(&self.frame.buffer[offset..][packed]);
            }
        }
        self.next_row += 1;
    }
    fn get(&self) -> Option<&[u8]> {
        self.current.then(|| self.buffer.as_ref())
    }
}

// Both DST headers contain two usize fields and have explicit 16-byte alignment.
fn frame_layout(buffer_size: usize) -> Layout {
    Layout::from_size_align(size_of::<usize>() * 2 + buffer_size, 16)
        .unwrap()
        .pad_to_align()
}

/// utility to store an buffer up to len S on the stack,
/// falls back to the heap, if size or alignment requirements are exceeded
/// different than smallvec crate because this isn't variable size: no .push()
enum StackVec<const S: usize, A = usize> {
    Inline([u8; S], usize, [A; 0]),
    Heap(Vec<u128>, usize),
}

impl<const S: usize, A> StackVec<S, A> {
    fn new(data: &[u8], alignment: Option<usize>) -> Self {
        let size = data.len();
        let alignment = alignment.unwrap_or(mem::align_of::<A>());
        let v = if size <= S && alignment <= align_of::<A>() {
            let mut array = [0u8; S];
            array[..size].copy_from_slice(data);
            Self::Inline(array, size, [])
        } else {
            // Keep the aligned owner, rather than deallocating as Box<[u8]>.
            assert!(alignment <= align_of::<u128>());
            let mut array = vec![0u128; size.div_ceil(size_of::<u128>())];
            bytemuck::cast_slice_mut::<u128, u8>(&mut array)[..size].copy_from_slice(data);
            Self::Heap(array, size)
        };

        assert_eq!(align_of::<Self>(), align_of::<A>());
        assert!(is_aligned_to(v.deref(), alignment));
        v
    }
}

impl<const S: usize, A> Deref for StackVec<S, A> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        match self {
            StackVec::Inline(data, size, _) => &data[..*size],
            StackVec::Heap(data, size) => &bytemuck::cast_slice::<u128, u8>(data)[..*size],
        }
    }
}

impl<const S: usize, A> DerefMut for StackVec<S, A> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            StackVec::Inline(data, size, _) => &mut data[..*size],
            StackVec::Heap(data, size) => &mut bytemuck::cast_slice_mut::<u128, u8>(data)[..*size],
        }
    }
}

#[inline]
pub fn is_aligned_to<T: ?Sized>(reference: &T, alignment: usize) -> bool {
    let ptr = reference as *const T as *const () as usize;
    ptr % alignment == 0
}

/// Allocate byte-owned storage and check its actual alignment. A Box<[u8]>
/// cannot request an over-aligned deallocation layout; use a typed owner (as
/// StackVec does) when a guaranteed alignment above one is required.
pub fn aligned_boxed_slice(size: usize, align: usize) -> Box<[u8]> {
    assert!(align.is_power_of_two());
    let bytes = vec![0u8; size].into_boxed_slice();
    assert!(
        is_aligned_to(bytes.as_ref(), align),
        "allocator did not supply requested alignment"
    );
    bytes
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
    use streaming_iterator::StreamingIterator;

    use crate::data::dataframe::{CompactFrame, CompactFrameIter};

    use super::Frame;
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
