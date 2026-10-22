use std::{
    alloc::Layout,
    clone,
    collections::{HashMap, HashSet},
    default,
    hash::{DefaultHasher, Hash, Hasher},
    mem::MaybeUninit,
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
use super::blockbuffer::ArchivedBlock;
use super::smallbox::{Initializer, SmallBox, boxed_with};

mod bitcompact;
pub use bitcompact::{BitCompactFrame, BitCompactFrameIter};

/// Pack varying bits across row boundaries in archived bit frames. Set false and
/// rebuild to retain byte-aligned packed rows; byte-compaction builds are unchanged.
pub const CONTINUOUS_BIT_PACKING: bool = true;

pub enum AnyFrame<'a> {
    Frame(&'a mut Box<Frame>),
    Compact(&'a mut Box<CompactFrame>),
    BitCompact(&'a mut Box<BitCompactFrame>),
    Archived(&'a mut SmallBox<ArchivedBlock>),
}

/// The edit changes omitted data and requires rebuilding; the frame is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NeedsRebuild;

impl AnyFrame<'_> {
    /// Write a row, rebuilding and replacing its owning box only when necessary.
    pub fn write_row(&mut self, index: usize, row: &[u8]) {
        match self {
            Self::Frame(frame) => {
                assert!(index < frame.length, "row index out of bounds");
                let start = (index + 1) * frame.unit;
                frame.buffer[start..start + frame.unit].copy_from_slice(row);
                frame.update_mask(index, true);
            }
            Self::Compact(frame) => {
                if frame.try_write_row(index, row).is_ok() {
                    return;
                }
                let mut unpacked = Frame::new(frame.unit(), frame.len());
                frame.uncompact(&mut unpacked);
                AnyFrame::Frame(&mut unpacked).write_row(index, row);
                **frame = CompactFrame::new_from(&unpacked);
            }
            Self::BitCompact(frame) => {
                if frame.try_write_row(index, row).is_ok() {
                    return;
                }
                let mut unpacked = Frame::new(frame.unit(), frame.len());
                frame.uncompact(&mut unpacked);
                AnyFrame::Frame(&mut unpacked).write_row(index, row);
                **frame = BitCompactFrame::new_from(&unpacked);
            }
            Self::Archived(block) => {
                if block.frame.try_write_row(index, row).is_ok() {
                    return;
                }
                let mut unpacked = Frame::new(block.frame.unit(), block.frame.len());
                block.frame.uncompact(&mut unpacked);
                AnyFrame::Frame(&mut unpacked).write_row(index, row);
                // Rebuilding replaces only the archive; Data's descriptor stays put.
                let metadata = std::mem::take(&mut block.metadata);
                **block = ArchivedBlock::new(&unpacked, metadata);
            }
        }
    }
}

/// Mutable scratch storage, either lent by the caller or owned by the row.
pub enum RowBuffer<'a> {
    Borrowed(&'a mut [u8]),
    Owned(Box<[u8]>),
}

impl RowBuffer<'_> {
    fn get_or_init(&mut self, size: usize) -> &mut [u8] {
        // An empty owned box defers allocation until a compact row is accessed.
        if let Self::Owned(buffer) = self {
            if buffer.is_empty() {
                // TODO: Decode into MaybeUninit<u8> storage to avoid this zero fill.
                // SAFETY: All allocated bytes are zeroed, which is valid for u8.
                *buffer = unsafe { Box::<[u8]>::new_zeroed_slice(size).assume_init() };
            }
        }
        self
    }
}

impl Deref for RowBuffer<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Self::Borrowed(buffer) => buffer,
            Self::Owned(buffer) => buffer,
        }
    }
}

impl DerefMut for RowBuffer<'_> {
    fn deref_mut(&mut self) -> &mut [u8] {
        match self {
            Self::Borrowed(buffer) => buffer,
            Self::Owned(buffer) => buffer,
        }
    }
}

pub struct Row<'a> {
    pub init: bool,
    pub buffer: RowBuffer<'a>,
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
            AnyFrame::BitCompact(frame) => {
                let slice = self.buffer.get_or_init(frame.unit());
                if !self.init {
                    frame.read_row(self.index, slice);
                    self.init = true;
                }
                slice
            }
            AnyFrame::Compact(compact_frame) => {
                let slice = self.buffer.get_or_init(compact_frame.unit());
                if !self.init {
                    compact_frame.read_row(self.index, slice);
                    self.init = true;
                }
                slice
            }
            AnyFrame::Archived(block) => {
                let slice = self.buffer.get_or_init(block.frame.unit());
                if !self.init {
                    block.frame.read_row(self.index, slice);
                    self.init = true;
                }
                slice
            }
        }
    }
}

// when the row is dropped we have to write the scratchpad back to frame storage (if it exists)
impl<'a> Drop for Row<'a> {
    fn drop(&mut self) {
        match &mut self.frame {
            AnyFrame::Frame(frame) => frame.update_mask(self.index, true),
            frame if self.init => frame.write_row(self.index, &self.buffer),
            _ => {}
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

// Active frames retain usize counters. Include trailing padding so Box's drop
// layout matches the allocation; slice metadata must exclude that padding.
fn frame_allocation_layout(buffer_size: usize) -> Layout {
    let total_size = size_of::<[usize; 2]>()
        .checked_add(buffer_size)
        .expect("frame allocation size overflow");
    Layout::from_size_align(total_size, 16)
        .expect("frame allocation exceeds layout limits")
        .pad_to_align()
}

// Archived byte frames have a separate four-byte header. Their buffer starts
// immediately after the counters, before any final allocation alignment padding.
fn compact_frame_allocation_layout(buffer_size: usize) -> Layout {
    let total_size = size_of::<[u16; 2]>()
        .checked_add(buffer_size)
        .expect("compact frame allocation size overflow");
    // Match the header's natural alignment so Box deallocates with the same layout.
    Layout::from_size_align(total_size, align_of::<u16>())
        .expect("compact frame allocation exceeds layout limits")
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

        // SAFETY: The layout matches Frame's counters and trailing buffer.
        // Only field initialization belongs here; allocation and ownership stay in smallbox.
        let mut v = unsafe {
            boxed_with(layout, buffer_size, |frame_ptr: *mut Frame| {
                ptr::addr_of_mut!((*frame_ptr).unit).write(unit);
                ptr::addr_of_mut!((*frame_ptr).length).write(0);
            })
        };
        v.buffer[..unit].fill(0); // Mask
        v
    }

    /// set the mask for these bytes, preventing them from being compacted
    /// usefull if a field needs to remain mutable in the compacted frame
    /// TODO really need to figure out rule of thumb for int sizes in cases like these
    /// TODO will be undone if update_mask is called with check_for_new_const=true
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
        // drop(buffer);
        self.update_mask(self.length - 1, false)
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
    // XXX: check_for_new_consts is ONLY used by Row and AnyFrame::write_row, it is questionable whether we want it there.
    //      undoes lock_editible
    //      mask is asked to be both source of truth for compaction and actual variability which we might want to differ if we prefer editing to not trigger recompaction.
    //      FOR NOW avoid passing check_for_new_const=true
    pub fn update_mask(&mut self, index: usize, check_for_new_const: bool) {
        assert!(index < self.length, "row index out of bounds");
        let (mask, buffer) = self.buffer.split_at_mut(self.unit);
        let buffer = &buffer[..self.unit * self.length];

        if check_for_new_const {
            // update newly constant bits
            for (i, m) in mask.iter_mut().enumerate() {
                if *m == 0 {
                    continue;
                }
                let first = buffer[i];
                let mut varying = 0;
                for row in buffer.chunks_exact(self.unit).skip(1) {
                    varying |= first ^ row[i];
                    // Stop once every previously marked bit has shown variation.
                    if varying & *m == *m {
                        break;
                    }
                }
                *m &= varying;
            }
        }

        // update newly varying bits
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

/// Byte-wise compaction; see BitCompactFrame for bit-level packing.
/// TODO implement bit per byte mask because frames may have as many rows as they have bytes
/// TODO store the mask as regions to avoid repeatedly scanning it.
// Keep the two u16 fields in sync with compact_frame_allocation_layout.
#[repr(C)]
pub struct CompactFrame {
    pub unit: u16,
    pub length: u16,

    pub buffer: [u8],
}

pub struct CompactFrameLayout<'a> {
    mask: &'a mut Mask,
    first: &'a mut [u8],
    data: &'a mut [u8],
}

impl CompactFrame {
    /// Common archived-frame metadata, also exposed by BitCompactFrame.
    pub fn unit(&self) -> usize {
        usize::from(self.unit)
    }
    pub fn len(&self) -> usize {
        usize::from(self.length)
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn new_from(frame: &Frame) -> Box<Self> {
        Self::initializer(frame).into_box()
    }

    /// Size the byte archive once and let its eventual owner allocate storage.
    pub(crate) fn initializer(frame: &Frame) -> Initializer<Self, impl FnOnce(*mut Self) + '_> {
        assert!(frame.unit > 0, "frame unit must be nonzero");
        let unit = u16::try_from(frame.unit).expect("compact frame unit exceeds u16");
        let length = u16::try_from(frame.length).expect("compact frame length exceeds u16");
        // The checked u16 counters bound source arithmetic on 32-bit targets too.
        let source = &frame.buffer[..(frame.length + 1) * frame.unit];
        let mask = frame.mask();
        let compact_unit = mask.compact_unit();
        // Store only the mask, first row (if any), and subsequent packed rows.
        let header_size = frame.unit * if length == 0 { 1 } else { 2 };
        let buffer_size = frame
            .length
            .saturating_sub(1)
            .checked_mul(compact_unit)
            .and_then(|size| size.checked_add(header_size))
            .expect("compact frame buffer size overflow");
        let layout = compact_frame_allocation_layout(buffer_size);
        // SAFETY: The checked layout matches the counters and tail. Borrow the
        // payload as MaybeUninit until packing has initialized every byte.
        unsafe {
            Initializer::new(layout, buffer_size, move |raw: *mut Self| {
                ptr::addr_of_mut!((*raw).unit).write(unit);
                ptr::addr_of_mut!((*raw).length).write(length);
                let buffer = slice::from_raw_parts_mut(
                    ptr::addr_of_mut!((*raw).buffer).cast::<MaybeUninit<u8>>(),
                    buffer_size,
                );
                copy_bytes(&mut buffer[..header_size], &source[..header_size]);
                for (index, row) in source[header_size..].chunks_exact(frame.unit).enumerate() {
                    let offset = header_size + index * compact_unit;
                    for (packed, unpacked) in MaskIter::new(mask) {
                        copy_bytes(&mut buffer[offset..][packed], &row[unpacked]);
                    }
                }
            })
        }
    }

    pub fn mask(&self) -> &Mask {
        &Mask::get(&self.buffer[..self.unit()])
    }

    pub fn first(&self) -> &[u8] {
        let end = if self.is_empty() {
            self.unit()
        } else {
            2 * self.unit()
        };
        &self.buffer[self.unit()..end]
    }

    /// Decode into caller-provided storage; Row handles writing edits back on drop.
    pub fn read_row(&self, index: usize, output: &mut [u8]) {
        assert!(index < self.len(), "row index out of bounds");
        assert_eq!(output.len(), self.unit(), "row size mismatch");
        output.copy_from_slice(self.first());
        if index != 0 {
            let mask = self.mask();
            let offset = 2 * self.unit() + mask.compact_unit() * (index - 1);
            for (packed, unpacked) in MaskIter::new(mask) {
                output[unpacked].copy_from_slice(&self.buffer[offset..][packed]);
            }
        }
    }

    /// Update in place, or return NeedsRebuild without changes if a constant byte changes.
    pub fn try_write_row(&mut self, index: usize, row: &[u8]) -> Result<(), NeedsRebuild> {
        assert!(index < self.len(), "row index out of bounds");
        assert_eq!(row.len(), self.unit(), "row size mismatch");
        let first = self.first();
        let constants_changed = self
            .mask()
            .iter()
            .enumerate()
            .any(|(i, &mask)| mask == 0 && row[i] != first[i]);
        if constants_changed {
            return Err(NeedsRebuild);
        }

        let layout = self.buffer();
        if index == 0 {
            layout.first.copy_from_slice(row);
        } else {
            let offset = (index - 1) * layout.mask.compact_unit();
            for (packed, unpacked) in MaskIter::new(layout.mask) {
                layout.data[offset..][packed].copy_from_slice(&row[unpacked]);
            }
        }
        Ok(())
    }

    pub fn buffer(&mut self) -> CompactFrameLayout {
        let unit = self.unit();
        let header_size = unit + self.first().len();
        let (header, data) = self.buffer.split_at_mut(header_size);
        let (mask, first) = header.split_at_mut(unit);
        CompactFrameLayout {
            mask: Mask::get_mut(mask),
            first,
            data,
        }
    }

    pub fn uncompact(&self, new: &mut Frame) {
        // TODO check size once
        new.unit = self.unit();
        new.length = self.len();

        // Empty frames contain only the mask.
        let offset = self.unit() + self.first().len();
        new.buffer[..offset].copy_from_slice(&self.buffer[..offset]);

        let mut write_ptr = &mut new.buffer[offset..];
        let mut read_ptr = &self.buffer[offset..];
        let mask = Mask::get(&self.buffer[..self.unit()]);
        let inc = self.unit() - mask.constant_bytes();
        let first = &self.buffer[self.unit()..offset];

        // TODO perhaps copy the mask iterable to a stack allocated buffer.
        for _row in 1..self.len() {
            write_ptr[..self.unit()].copy_from_slice(first);
            for (a, b) in MaskIter::new(mask) {
                write_ptr[b].copy_from_slice(&read_ptr[a]);
            }
            write_ptr = &mut write_ptr[self.unit()..];
            read_ptr = &read_ptr[inc..];
        }
    }

    pub fn compression_ratio(&self) -> CompressionStats {
        CompressionStats {
            data: self.len() * self.unit(),
            // Every payload byte after the mask belongs to a stored row.
            compressed: self.buffer.len() - self.unit(),
            overhead: size_of_val(self) - self.buffer.len() + self.unit(),
            overalloc: 0,
        }
    }
}

// Raw copying initializes fresh payload storage without reading or zeroing it first.
fn copy_bytes(destination: &mut [MaybeUninit<u8>], source: &[u8]) {
    assert_eq!(destination.len(), source.len());
    // SAFETY: Both slices cover the checked byte count and cannot overlap.
    unsafe {
        ptr::copy_nonoverlapping(
            source.as_ptr(),
            destination.as_mut_ptr().cast(),
            source.len(),
        );
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
    yielded: usize,
}

impl<'a> CompactFrameIter<'a> {
    pub fn new(frame: &'a CompactFrame) -> Self {
        let compact_unit = frame.unit() - frame.mask().constant_bytes();
        Self {
            frame,
            buffer: StackVec::new(frame.first()),
            index: frame.unit() * 2,
            compact_unit,
            yielded: 0,
        }
    }
}

impl<'a> StreamingIterator for CompactFrameIter<'a> {
    type Item = [u8];

    fn advance(&mut self) {
        // Count rows explicitly: a fully constant frame has zero packed bytes
        // per row, so a byte-offset sentinel cannot detect its end.
        if self.yielded >= self.frame.len() {
            self.yielded = self.frame.len() + 1;
            return;
        }
        if self.yielded > 0 {
            for (a, b) in MaskIter::new(self.frame.mask()) {
                self.buffer[b].copy_from_slice(&self.frame.buffer[self.index..][a]);
            }
            self.index += self.compact_unit;
        }
        self.yielded += 1;
    }

    fn get(&self) -> Option<&Self::Item> {
        (self.yielded > 0 && self.yielded <= self.frame.len()).then(|| self.buffer.as_ref())
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

    use super::{
        AnyFrame, Frame, NeedsRebuild, Row, RowBuffer, StackVec, compact_frame_allocation_layout,
        frame_allocation_layout,
    };

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
            assert_eq!(compact_layout.align(), align_of::<u16>());
            assert_eq!(
                compact_frame_allocation_layout(compact.buffer.len()),
                compact_layout
            );
            assert_eq!(compact.buffer.len(), 3 * unit);
            let compact_stats = compact.compression_ratio();
            assert_eq!(
                compact_stats.compressed + compact_stats.overhead + compact_stats.overalloc,
                compact_layout.size()
            );
            assert_eq!(compact_stats.overalloc, 0);
        }
    }

    #[test]
    fn compact_empty_single_and_constant_rows() {
        // Removing spare storage must preserve empty frames and zero-byte packed rows.
        for length in [0, 1, 4] {
            let mut frame = Frame::new(3, length);
            for _ in 0..length {
                frame.push(&[7, 8, 9]);
            }
            let mut compact = CompactFrame::new_from(&frame);
            assert_eq!(compact.buffer.len(), if length == 0 { 3 } else { 6 });
            assert!(compact.buffer().data.is_empty());
            let stats = compact.compression_ratio();
            assert_eq!(stats.compressed, if length == 0 { 0 } else { 3 });
            assert_eq!(stats.overalloc, 0);
            assert_eq!(stats.compressed + stats.overhead, size_of_val(&*compact));
            let mut rows = CompactFrameIter::new(&compact);
            for _ in 0..length {
                assert_eq!(rows.next(), Some(&[7, 8, 9][..]));
            }
            assert!(rows.next().is_none());
            let mut restored = Frame::new(3, length);
            compact.uncompact(&mut restored);
            assert_eq!(restored.length, length);
            assert_eq!(restored.buffer, frame.buffer);
        }
    }

    #[test]
    fn frame_allocations_reject_invalid_sizes() {
        // These inputs must fail before allocation even when overflow checks are disabled.
        assert!(catch_unwind(|| Frame::new(usize::MAX, 2)).is_err());
        assert!(catch_unwind(|| Frame::new(usize::MAX, 1)).is_err());
        assert!(catch_unwind(|| frame_allocation_layout(usize::MAX)).is_err());
        assert!(catch_unwind(|| frame_allocation_layout(isize::MAX as usize)).is_err());
        for (unit, length) in [
            (0, 0),
            (u16::MAX as usize + 1, 0),
            (1, u16::MAX as usize + 1),
        ] {
            // Invalid counters must be rejected before accessing the source rows.
            assert!(
                catch_unwind(|| {
                    let mut frame = Frame::new(1, 0);
                    frame.unit = unit;
                    frame.length = length;
                    CompactFrame::new_from(&frame)
                })
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
    fn compact_row_writeback_reuses_storage_until_constants_change() {
        for index in [0, 1] {
            let mut expected = [[0xAA, 0x55], [0xAB, 0x55], [0xAA, 0x55]];
            let mut frame = Frame::new(2, expected.len());
            for row in &expected {
                frame.push(row);
            }
            let mut compact = CompactFrame::new_from(&frame);
            for (byte, change, reuse) in [(0, 0x80, true), (1, 2, false)] {
                if !reuse {
                    // A rejected in-place write must leave the mask and every row intact.
                    let mask = compact.mask().to_vec();
                    let mut edit = expected[index];
                    edit[byte] ^= change;
                    assert_eq!(compact.try_write_row(index, &edit), Err(NeedsRebuild));
                    assert_eq!(&**compact.mask(), mask.as_slice());
                    for (i, expected) in expected.iter().enumerate() {
                        let mut decoded = [0; 2];
                        compact.read_row(i, &mut decoded);
                        assert_eq!(&decoded, expected);
                    }
                }
                let address = &*compact as *const CompactFrame;
                let mut row = Row {
                    init: false,
                    buffer: RowBuffer::Owned(Box::default()),
                    index,
                    frame: AnyFrame::Compact(&mut compact),
                };
                assert_eq!(row.get_mut(), &expected[index]);
                row.get_mut()[byte] ^= change;
                expected[index][byte] ^= change;
                assert_eq!(row.get_mut(), &expected[index]);
                drop(row);
                if reuse {
                    // Byte compaction stores the entire varying byte, including bit 7.
                    assert!(std::ptr::eq(address, &*compact));
                }
                for (index, expected) in expected.iter().enumerate() {
                    let mut decoded = [0; 2];
                    compact.read_row(index, &mut decoded);
                    assert_eq!(&decoded, expected);
                }
            }
        }
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
        assert_eq!(compact.buffer().data.len(), 2 * 3);
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

    #[test]
    fn mask_revalidation_removes_constants_before_adding_new_variation() {
        // Cover baseline edits and variation found only in a later row, with spare
        // capacity left unwritten so validation must examine committed rows only.
        for index in [0, 1, 3] {
            let mut frame = Frame::new(3, 6);
            for row in 0..4 {
                frame.push(&[if row == index { 3 } else { 0 }, 0x40, 0x80]);
            }
            assert_eq!(&**frame.mask(), &[3, 0, 0]);
            AnyFrame::Frame(&mut frame).write_row(index, &[1, 0x44, 0x80]);
            assert_eq!(&**frame.mask(), &[1, 4, 0]);
            AnyFrame::Frame(&mut frame).write_row(index, &[0, 0x40, 0x80]);
            assert_eq!(&**frame.mask(), &[0, 0, 0]);
        }
    }
}
