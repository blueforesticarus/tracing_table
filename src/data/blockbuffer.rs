use std::{
    alloc::Layout,
    cmp::{max, min},
    collections::{VecDeque, binary_heap},
    default,
    ops::Range,
    sync::atomic::AtomicU64,
};

use streaming_iterator::{IntoStreamingIterator, StreamingIterator};

use super::CompressionStats;
use super::dataframe::{AnyFrame, Frame, Row, RowBuffer};
use super::smallbox::SmallBox;
use super::stringbuffer::StringBufferId;

mod metadata;
pub use metadata::CollocatedMetadata;
mod parent_offsets;
pub use parent_offsets::{CallsiteArchetype, ParentOffsetEntry, ParentOffsets};
mod timestamps;

// Compile-time selection keeps both modes on the normal rollover/edit/read path.
#[cfg(feature = "bit-compression")]
pub use super::dataframe::{
    BitCompactFrame as ArchivedFrame, BitCompactFrameIter as ArchivedFrameIter,
};
#[cfg(not(feature = "bit-compression"))]
pub use super::dataframe::{CompactFrame as ArchivedFrame, CompactFrameIter as ArchivedFrameIter};

/// a quasi ring buffer of dataframes.

// Public storage views let the standalone example inspect without library helpers.
pub struct BlockCache {
    unit: usize,

    // This is a very simple way to reduce the size of rare or one-off logs.
    // The starter frame is allocated with a smaller amount of space, and once full is reallocated.
    // NOTE: frame size is in rows,
    // as a rule of thumb: frames should be at least as long as they are wide,
    // otherwise the overhead of the mask exceeds the savings when eliding one byte.
    frame_size_max: usize,

    // TODO: more optimistically compact frame, ie. compact before frame is full and unpack as needed.
    pub active: Box<Frame>,

    // Append archives at the back; evict oldest blocks from the front
    pub ring: VecDeque<Data>,

    /// Preserve absolute block references when eviction removes a prefix.
    pub offset: usize,

    /// Metadata moves intact into Data when the active frame is archived.
    pub active_descriptor: Descriptor,

    /// Moves into the archive on rollover, preserving span-list ownership.
    pub active_metadata: CollocatedMetadata,

    // used for get_mut
    edit_buffer: Vec<u8>,

    // Writable dictionary scratch is reused after its entries move into an archive.
    active_parent_offsets: Vec<u8>,
    // These describe writable rows only; archived frames need neither field.
    active_parent_row_offset: u16,
    active_timestamp_header: bool,
}

pub struct Data {
    pub data: SmallBox<ArchivedBlock>,
    pub descriptor: Descriptor,
}

/// Shared metadata and variable-sized frame occupy one two-byte-aligned allocation.
#[repr(C)]
pub struct ArchivedBlock {
    pub metadata: CollocatedMetadata,
    pub frame: ArchivedFrame,
}

impl ArchivedBlock {
    pub fn compression_ratio(&self) -> CompressionStats {
        // Count shared metadata, its span-list heap, and allocation padding once.
        self.frame.compression_ratio()
            + self.metadata.compression_ratio()
            + CompressionStats {
                overhead: SmallBox::<Self>::layout(Layout::for_value(self)).0.size()
                    - size_of_val(&self.frame)
                    - size_of::<CollocatedMetadata>(),
                ..Default::default()
            }
    }
}

/// Metadata shared by active and archived blocks; row indices remain absolute.
#[derive(Debug, Default)]
pub struct Descriptor {
    /// One-based starting row index within the archetype.
    pub index: usize,
    /// Invariant: start is the first row's timestamp and NEVER changes for this block
    /// Rows store exact offsets or stride residuals from this immutable origin.
    /// Appended timestamps must not precede start.
    pub(crate) timestamp: Range<usize>,
}

// Keep per-block predictors in the heap metadata, away from occupied/spare slots.
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(size_of::<Descriptor>() == 24);
    assert!(size_of::<Data>() == 32);
};

/// Location in the same callsite. Block ordinals are one-based and survive eviction.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, bytemuck::NoUninit)]
pub struct SpanFieldsRef {
    pub archetype: u32,
    pub block: u32,
}

impl Descriptor {
    /// Store an unsigned offset from the block's permanent origin.
    pub(crate) fn encode_timestamp(&self, ts: u64) -> u64 {
        let base = self.timestamp.start as u64;
        assert!(ts >= base, "timestamp precedes block timestamp base");
        // Active and disabled-stride blocks retain the complete unsigned range.
        ts - base
    }

    /// Recover nominal time from an offset after any stride has been decoded.
    pub fn decode_timestamp(&self, offset: u64) -> u64 {
        (self.timestamp.start as u64)
            .checked_add(offset)
            .expect("timestamp offset exceeds u64")
    }

    /// Descriptor fields remain inline in Data or BlockCache.
    pub fn compression_ratio(&self) -> CompressionStats {
        CompressionStats {
            overhead: size_of::<Self>(),
            ..Default::default()
        }
    }
}

impl Data {
    /// Share descriptor accounting with active blocks, excluding its inline bytes here.
    pub fn compression_ratio(&self) -> CompressionStats {
        self.data.compression_ratio()
            + self.descriptor.compression_ratio()
            + CompressionStats {
                overhead: size_of::<Self>() - size_of::<Descriptor>(),
                ..Default::default()
            }
    }
}

/// Dropping chunks must preserve the retained suffix and protect the active chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropChunkError {
    AlreadyDropped,
    NotOldest,
}

impl BlockCache {
    pub fn new(unit: usize, start_cap: usize, max_cap: usize) -> Self {
        BlockCache {
            unit,
            frame_size_max: max_cap,
            active: Frame::new(unit, start_cap),
            ring: VecDeque::new(),
            offset: 0,
            active_descriptor: Descriptor {
                index: 1,
                ..Default::default()
            },
            active_metadata: CollocatedMetadata::default(),
            edit_buffer: vec![0; unit],
            active_parent_offsets: Vec::new(),
            active_parent_row_offset: 0,
            active_timestamp_header: false,
        }
    }

    pub fn active_parent_offsets(&self) -> ParentOffsets<'_> {
        ParentOffsets::new(&self.active_parent_offsets)
    }

    /// Active dictionary heap belongs to writable storage, including spare capacity.
    pub fn active_parent_offsets_heap_bytes(&self) -> usize {
        self.active_parent_offsets.capacity()
    }

    /// Call after cycle_if_needed/prepare_chunk and immediately before allocate,
    /// so the dictionary and the new row always belong to the same block.
    pub fn encode_parent(
        &mut self,
        index: u64,
        identity: CallsiteArchetype,
        row_offset: usize,
    ) -> (u64, u16) {
        assert!(
            self.active.length < self.active.capacity(),
            "prepare chunk before encoding parent"
        );
        assert!(row_offset + ParentOffsets::ROW_BYTES <= self.unit);
        let row_offset = u16::try_from(row_offset).expect("parent position exceeds u16");
        assert!(
            self.active_parent_offsets.is_empty() || self.active_parent_row_offset == row_offset
        );
        self.active_parent_row_offset = row_offset;
        let table_index = match self.active_parent_offsets().find(identity) {
            Some(index) => index,
            None => {
                let count = self.active_parent_offsets().len() as u16;
                // Reject overflow before appending an entry or changing row state.
                count
                    .checked_add(1)
                    .expect("parent identity count exceeds u16");
                self.active_parent_offsets
                    .extend_from_slice(&identity.to_bits().to_le_bytes());
                // Store a two-byte predictor scaled by FloatU16::UNIT; every row uses
                // its decoded value, so rounding never changes the original ID.
                let offset = super::float_u16::FloatU16::from_u64_lossy(index);
                self.active_parent_offsets
                    .extend_from_slice(&offset.to_bits().to_le_bytes());
                // Active rows use only the offset. Fit the final stride once
                // the block length and each identity's last occurrence are known.
                self.active_parent_offsets
                    .extend_from_slice(&0u16.to_le_bytes());
                count
            }
        };
        (
            self.active_parent_offsets().entry(table_index).encode(
                index,
                self.active.length,
                self.active.length + 1,
            ),
            table_index,
        )
    }

    pub fn cycle_if_needed(&mut self) {
        if self.active.capacity() == self.active.length {
            if self.active.capacity() < self.frame_size_max {
                // starts at 1, jumps to 8, then double capacity untill max
                let mut frame = Frame::new(
                    self.active.unit,
                    max(min(self.active.capacity() * 2, self.frame_size_max), 8),
                );
                frame.buffer[..self.active.buffer.len()].copy_from_slice(&self.active.buffer);
                frame.length = self.active.length;
                self.active = frame;
            } else {
                self.cycle();
            }
        }
    }

    /// Archive a populated chunk early when its owner must change string buffers.
    pub(crate) fn cycle(&mut self) {
        if self.active.length == 0 {
            return;
        }
        if self.active_timestamp_header {
            self.active_metadata.fit_timestamp_stride(&mut self.active);
        }
        if !self.active_parent_offsets.is_empty() {
            parent_offsets::fit_strides(
                &mut self.active,
                &mut self.active_parent_offsets,
                usize::from(self.active_parent_row_offset),
            );
        }
        let length = self.active.length;
        // The active buffer ID stays until prepare_chunk selects the next one;
        // timestamp range and deferred references start fresh for the new frame.
        let next = Descriptor {
            index: self.active_descriptor.index + length,
            ..Default::default()
        };
        let mut next_metadata = CollocatedMetadata::default();
        next_metadata.string_buffer = self.active_metadata.string_buffer;
        // Pack directly into the final archive and retain the active capacity for reuse.
        let metadata = std::mem::replace(&mut self.active_metadata, next_metadata);
        let data =
            ArchivedBlock::new_with_trailer(&self.active, metadata, &self.active_parent_offsets);
        self.ring.push_back(Data {
            data,
            descriptor: std::mem::replace(&mut self.active_descriptor, next),
        });
        self.active.clear();
        self.active_parent_offsets.clear();
        self.active_parent_row_offset = 0;
        self.active_timestamp_header = false;
    }

    /// Initialize the origin after rollover and validate before committing a row.
    /// Return its encoded timestamp alongside the bytes and absolute row index.
    pub fn allocate(&mut self, ts: u64) -> (&mut [u8], usize, u64) {
        self.allocate_inner(ts, false)
    }

    /// Allocate an event/span start with a timestamp in the first eight bytes.
    /// Generic payloads and deferred span updates continue to use allocate().
    pub fn allocate_timestamp(&mut self, ts: u64) -> (&mut [u8], usize, u64) {
        self.allocate_inner(ts, true)
    }

    fn allocate_inner(&mut self, ts: u64, timestamp_header: bool) -> (&mut [u8], usize, u64) {
        self.cycle_if_needed();
        // A block has one row schema; generic payloads must never be rewritten.
        assert!(self.active.length == 0 || self.active_timestamp_header == timestamp_header);
        if self.active.length == 0 {
            let ts = usize::try_from(ts).expect("block timestamp exceeds usize");
            self.active_descriptor.timestamp = ts..ts;
        }
        let offset = self.active_descriptor.encode_timestamp(ts);
        if timestamp_header {
            assert!(self.unit >= size_of::<u64>());
        }
        self.active_timestamp_header = timestamp_header;
        let (buffer, index) = self.active.allocate().unwrap();
        (buffer, index + self.active_descriptor.index, offset)
    }

    /// Find metadata by absolute retained row index, including after eviction.
    pub fn descriptor(&self, index: usize) -> Option<&Descriptor> {
        if index >= self.active_descriptor.index {
            return (index - self.active_descriptor.index < self.active.length)
                .then_some(&self.active_descriptor);
        }
        self.ring.iter().rev().find_map(|block| {
            (index >= block.descriptor.index
                && index - block.descriptor.index < block.data.frame.len())
            .then_some(&block.descriptor)
        })
    }

    /// Decode a retained timestamp with its metadata and existing frame length.
    pub fn decode_timestamp(&self, encoded: u64, index: usize) -> Option<u64> {
        if index >= self.active_descriptor.index {
            return (index - self.active_descriptor.index < self.active.length)
                .then(|| self.active_descriptor.decode_timestamp(encoded));
        }
        self.ring.iter().rev().find_map(|block| {
            let row = index.checked_sub(block.descriptor.index)?;
            (row < block.data.frame.len()).then(|| {
                block.data.metadata.decode_timestamp(
                    &block.descriptor,
                    encoded,
                    row,
                    block.data.frame.len(),
                )
            })
        })
    }

    pub fn push(&mut self, data: &[u8], ts: usize) {
        self.allocate(ts as u64).0.copy_from_slice(data);
        self.update_mask_after_append();
        self.update_ts_range(ts, None);
    }

    /// Drop the oldest completed chunk by its first row's absolute index.
    /// Reject stale indexes and later/active chunks without changing the cache.
    pub fn drop_chunk(&mut self, index: usize) -> Result<(), DropChunkError> {
        let oldest = self
            .ring
            .front()
            .map_or(self.active_descriptor.index, |chunk| chunk.descriptor.index);
        if index < oldest {
            return Err(DropChunkError::AlreadyDropped);
        }
        if self.ring.is_empty() || index != oldest {
            return Err(DropChunkError::NotOldest);
        }
        self.ring.pop_front();
        self.offset += 1;
        Ok(())
    }

    /// Borrow a row and its descriptor together, so decoding needs one lookup.
    pub fn get_mut(&mut self, index: usize) -> Option<(&Descriptor, Row<'_>)> {
        if index >= self.active_descriptor.index {
            if index - self.active_descriptor.index >= self.active.length {
                return None;
            }
            let row = Row {
                init: false, // we aren't copying the data, just providing the buffer
                buffer: RowBuffer::Owned(Box::default()),
                index: index - self.active_descriptor.index,
                frame: crate::data::dataframe::AnyFrame::Frame(&mut self.active),
            };
            Some((&self.active_descriptor, row))
        } else {
            let Some(data) = self.ring.iter_mut().rev().find(|d| {
                d.descriptor.index <= index && index - d.descriptor.index < d.data.frame.len()
            }) else {
                return None;
            };

            let frame = AnyFrame::Archived(&mut data.data);
            let row = Row {
                init: false, // Decode only if the caller accesses the archived row.
                buffer: RowBuffer::Borrowed(self.edit_buffer.as_mut_slice()),
                index: index - data.descriptor.index,
                frame,
            };
            Some((&data.descriptor, row))
        }
    }

    // TODO abstract metadata, move span handling to subscriber.rs

    /// Candidates can include other spans' updates; readers must match the full SpanId.
    pub fn span_fields_refs(&self, index: usize) -> &[SpanFieldsRef] {
        if index >= self.active_descriptor.index {
            return if index - self.active_descriptor.index < self.active.length {
                self.active_metadata.span_fields().unwrap_or_default()
            } else {
                &[]
            };
        }
        self.ring
            .iter()
            .rev()
            .find(|block| block.descriptor.index <= index)
            .and_then(|block| block.data.metadata.span_fields())
            .unwrap_or_default()
    }

    /// Deduplicate candidates and rebuild the exact-size list only for a new reference.
    pub(crate) fn add_span_fields_ref(&mut self, index: usize, reference: SpanFieldsRef) {
        let metadata = if index >= self.active_descriptor.index {
            if index - self.active_descriptor.index >= self.active.length {
                return;
            }
            &mut self.active_metadata
        } else if let Some(block) = self
            .ring
            .iter_mut()
            .rev()
            .find(|b| b.descriptor.index <= index)
        {
            &mut block.data.metadata
        } else {
            return; // A late close may outlive its evicted start block.
        };
        metadata.add_span_fields_ref(reference);
    }

    /// Owning handles are already charged to shared metadata; count their heaps once.
    pub fn span_fields_heap_bytes(&self) -> usize {
        std::iter::once(&self.active_metadata)
            .chain(self.ring.iter().map(|block| &block.data.metadata))
            .map(CollocatedMetadata::heap_bytes)
            .sum()
    }

    /// NOTE: could simplify by computing mask when compacting. Pretty sure it's the same amount of processing.
    pub fn update_mask_after_append(&mut self) {
        self.active.update_mask(self.active.length - 1, false /*only need to check for newly constant bits when changing data, not when appending. */);
    }

    // Extend the upper bound on append/close; NEVER move the decoding origin.
    pub fn update_ts_range(&mut self, ts: usize, index: Option<usize>) {
        let index = index.unwrap_or(self.active_descriptor.index + self.active.length);

        let timestamp = if index >= self.active_descriptor.index {
            &mut self.active_descriptor.timestamp
        } else {
            &mut self
                .ring
                .iter_mut()
                .rev()
                .find(|d| d.descriptor.index <= index)
                .unwrap()
                .descriptor
                .timestamp
        };

        if timestamp.end < ts {
            timestamp.end = ts;
        }
    }

    pub fn iter(&self) -> impl StreamingIterator<Item = [u8]> {
        self.ring
            .iter()
            .into_streaming_iter()
            .flat_map(|frame| ArchivedFrameIter::new(&frame.data.frame))
            .chain(self.active.iter().into_streaming_iter_ref())
    }

    /// Active frame, descriptor and shared cache/scratch state; no archived traversal.
    pub fn active_compression_ratio(&self) -> CompressionStats {
        self.active.compression_ratio()
            + self.active_descriptor.compression_ratio()
            + self.active_metadata.compression_ratio()
            + CompressionStats {
                overhead: size_of::<Self>()
                    - size_of::<Descriptor>()
                    - size_of::<CollocatedMetadata>()
                    + self.edit_buffer.len()
                    + self.active_parent_offsets.len(),
                overalloc: self.edit_buffer.capacity() - self.edit_buffer.len()
                    + self.active_parent_offsets.capacity()
                    - self.active_parent_offsets.len(),
                ..Default::default()
            }
    }

    /// Split resident storage by lifetime; shared cache state belongs to active storage.
    pub fn compression_parts(&self) -> (CompressionStats, CompressionStats) {
        let archived = self
            .ring
            .iter()
            .map(Data::compression_ratio)
            .fold(CompressionStats::default(), |a, b| a + b)
            + CompressionStats {
                overalloc: (self.ring.capacity() - self.ring.len()) * size_of::<Data>(),
                ..Default::default()
            };
        (self.active_compression_ratio(), archived)
    }

    pub fn compression_ratio(&self) -> CompressionStats {
        let (active, archived) = self.compression_parts();
        active + archived
    }
}

#[cfg(test)]
mod test {
    use std::{
        hash::{DefaultHasher, Hash, Hasher},
        time::{Instant, SystemTime, UNIX_EPOCH},
    };

    use bytemuck::{Pod, Zeroable, bytes_of};
    use streaming_iterator::StreamingIterator;

    use crate::data::dataframe::Frame;

    use super::BlockCache;

    #[test]
    fn collocated_archive_layout_and_accounting_match_the_allocation() {
        use super::{CollocatedMetadata, Data, StringBufferId};
        use std::alloc::Layout;

        // Cross padding boundaries and verify both archive formats with feature runs.
        for unit in 1..=33 {
            let mut cache = BlockCache::new(unit, 2, 2);
            cache.active_metadata.string_buffer = StringBufferId::new(7);
            cache.push(&vec![0; unit], 0);
            cache.push(&vec![1; unit], 1);
            cache.cycle();
            let block = &cache.ring[0];
            let frame_layout = Layout::for_value(&block.data.frame);
            let (value_layout, offset) = Layout::new::<CollocatedMetadata>()
                .extend(frame_layout)
                .unwrap();
            let (expected, _) =
                super::SmallBox::<super::ArchivedBlock>::layout(value_layout.pad_to_align());
            // No fixed padding remains between metadata, frame, or u16 prefix.
            assert_eq!(offset, 14);
            assert_eq!(expected.align(), 2);
            assert_eq!(
                Layout::for_value(block.data.as_ref()),
                value_layout.pad_to_align()
            );
            assert_eq!(
                &block.data.frame as *const _ as *const u8 as usize
                    - block.data.as_ref() as *const _ as *const u8 as usize,
                offset
            );
            let stats = block.compression_ratio();
            assert_eq!(
                stats.compressed + stats.overhead + stats.overalloc,
                size_of::<Data>() + expected.pad_to_align().size()
            );
            assert_eq!(block.data.metadata.string_buffer, StringBufferId::new(7));
        }
    }

    #[test]
    fn timestamp_origin_zero_is_not_an_uninitialized_sentinel() {
        let mut cache = BlockCache::new(8, 2, 2);
        for ts in [0, 7, 20] {
            cache.push(&[0; 8], ts);
        }
        // Later appends, archived closes, and active closes only grow the end.
        cache.update_ts_range(100, Some(1));
        cache.update_ts_range(200, Some(3));
        assert_eq!(cache.ring[0].descriptor.timestamp, 0..100);
        assert_eq!(cache.active_descriptor.timestamp, 20..200);
        // Growing a starter frame also preserves the first row's origin.
        let mut growing = BlockCache::new(8, 1, 8);
        growing.push(&[0; 8], 100);
        growing.push(&[0; 8], 110);
        assert!(growing.active.capacity() > 1);
        assert_eq!(growing.active_descriptor.timestamp, 100..110);
    }

    #[test]
    fn timestamp_offsets_cover_unsigned_limits_without_truncation() {
        use super::Descriptor;
        let descriptor = Descriptor {
            timestamp: (1usize << 63)..usize::MAX,
            ..Default::default()
        };
        // Large absolute times and offsets retain all 64 bits without zigzag.
        for (ts, expected) in [
            (1 << 63, 0),
            ((1 << 63) + 1, 1),
            (u64::MAX, i64::MAX as u64),
        ] {
            let offset = descriptor.encode_timestamp(ts);
            assert_eq!(offset, expected);
            assert_eq!(descriptor.decode_timestamp(offset), ts);
        }
        let zero = Descriptor::default();
        assert_eq!(zero.encode_timestamp(u64::MAX), u64::MAX);
        assert_eq!(zero.decode_timestamp(u64::MAX), u64::MAX);
    }

    #[test]
    fn span_candidates_deduplicate_move_with_the_block_and_drop_with_it() {
        use super::{SpanFieldsRef, StringBufferId};
        let mut cache = BlockCache::new(1, 2, 2);
        cache.active_metadata.string_buffer = StringBufferId::new(7);
        cache.push(&[1], 1);
        let first = SpanFieldsRef {
            archetype: 2,
            block: 1,
        };
        let second = SpanFieldsRef {
            archetype: 3,
            block: 4,
        };
        cache.add_span_fields_ref(1, first);
        let allocation = cache.active_metadata.span_fields().unwrap().as_ptr();
        let initial_bytes = cache.span_fields_heap_bytes();
        cache.add_span_fields_ref(1, first);
        assert_eq!(
            cache.active_metadata.span_fields().unwrap().as_ptr(),
            allocation
        );
        assert_eq!(cache.span_fields_heap_bytes(), initial_bytes);
        cache.add_span_fields_ref(1, second);
        assert_eq!(cache.span_fields_refs(1), &[first, second]);
        let allocation = cache.active_metadata.span_fields().unwrap().as_ptr();
        let archived_bytes = cache.span_fields_heap_bytes();
        // Rollover moves the owning handle, without copying the list allocation.
        cache.push(&[2], 2);
        let metadata_stats = cache.active_metadata.compression_ratio();
        cache.push(&[3], 3);
        let archived = &cache.ring[0];
        assert_eq!(archived.descriptor.index, 1);
        assert_eq!(archived.descriptor.timestamp, 1..2);
        assert_eq!(archived.data.metadata.string_buffer, StringBufferId::new(7));
        assert_eq!(
            archived.data.metadata.span_fields().unwrap().as_ptr(),
            allocation
        );
        let archived_stats = archived.data.metadata.compression_ratio();
        assert_eq!(
            (archived_stats.overhead, archived_stats.overalloc),
            (metadata_stats.overhead, metadata_stats.overalloc)
        );
        assert_eq!(cache.active_descriptor.index, 3);
        assert_eq!(cache.active_descriptor.timestamp, 3..3);
        assert_eq!(cache.active_metadata.string_buffer, StringBufferId::new(7));
        assert!(cache.active_metadata.span_fields().is_none());
        assert_eq!(cache.span_fields_refs(2), &[first, second]);
        assert!(cache.span_fields_refs(3).is_empty());
        cache.add_span_fields_ref(2, second);
        assert_eq!(cache.span_fields_heap_bytes(), archived_bytes);
        cache.drop_chunk(1).unwrap();
        cache.add_span_fields_ref(1, first); // A late close cannot attach to another block.
        assert!(cache.span_fields_refs(1).is_empty());
        assert_eq!(cache.span_fields_heap_bytes(), 0);
    }

    #[test]
    fn archived_edits_and_constant_blocks_preserve_the_complete_stream() {
        let mut cache = BlockCache::new(2, 8, 8);
        cache.active_metadata.string_buffer = super::StringBufferId::new(7);
        let mut expected: Vec<_> = (0..35).map(|i| [0xA0 | (i / 8) as u8, 0x55]).collect();
        for row in &expected {
            cache.push(row, 0);
        }
        assert_eq!(cache.ring.len(), 4);
        let reference = super::SpanFieldsRef {
            archetype: 3,
            block: 2,
        };
        cache.add_span_fields_ref(1, reference);
        let allocation = cache.span_fields_refs(1).as_ptr();
        // Patch an archived baseline, then another row with an already-varying
        // bit. Other archived frames remain fully constant and must terminate.
        for (index, bit) in [(1, 0x80), (2, 0x80), (17, 0)] {
            cache.get_mut(index).unwrap().1.get_mut()[0] ^= bit;
            expected[index - 1][0] ^= bit; // Absolute row indexes start at one.
        }
        let mut rows = cache.iter();
        for expected in expected {
            assert_eq!(rows.next(), Some(expected.as_slice()));
        }
        assert!(rows.next().is_none());
        assert!(rows.next().is_none());
        // Recompaction must carry the collocated ID into the replacement allocation.
        assert!(
            cache
                .ring
                .iter()
                .all(|block| block.data.metadata.string_buffer == super::StringBufferId::new(7))
        );
        assert_eq!(cache.span_fields_refs(1), &[reference]);
        assert_eq!(cache.span_fields_refs(1).as_ptr(), allocation);
        let (active, archived) = cache.compression_parts();
        assert_eq!(active.data, 3 * 2);
        assert_eq!(archived.data, 32 * 2);
        assert!(archived.compressed < archived.data);
    }

    #[test]
    fn block_cache_push_and_retrieve() {
        #[derive(Debug, Copy, Clone, Pod, Zeroable)]
        #[repr(C)]
        struct TestData {
            i: usize,
            random: usize,
            t: u128,
        }

        let unit = size_of::<TestData>();

        let mut cache = BlockCache::new(unit, 1, 4096 / unit);

        // write a bunch of stuff in.
        let mut h = DefaultHasher::new();
        let mut times: Vec<u128> = Vec::with_capacity(1000);
        for i in 0..1000usize {
            let t = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                + (i * 1000_000_000) as u128;
            times.push(t);
            i.hash(&mut h);
            let data = TestData {
                i,
                t,
                random: h.finish() as usize,
            };

            cache.push(bytes_of(&data), 0 /*ignore ts for this test*/);
        }

        // for f in cache.ring.iter() {
        //     let len = f.unit - f.mask().constant_bytes();
        //     let bit_count = (f.mask().iter().map(|byte| byte.count_ones()).sum::<u32>() + 8) / 8;

        //     println!("byte_compact: {}, bit_compact: {}", len, bit_count);
        // }

        // read a bunch of stuff out
        let mut c_iter = cache.iter();
        let mut i = 0usize;
        let mut h = DefaultHasher::new();
        while let Some(item) = c_iter.next() {
            i.hash(&mut h);

            let v: &TestData = bytemuck::from_bytes(item);
            // println!("{:?}", v);
            assert_eq!((v.i, v.random, v.t), (i, h.finish() as usize, times[i]));
            i += 1;
        }
    }
}
