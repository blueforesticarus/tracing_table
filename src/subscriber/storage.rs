//! Tracing row policy: timestamp bases, packed span identities and deferred references.
use crate::data::column::{self, Layout as ColumnLayout};
use crate::data::dataframe::Frame;
use crate::data::stringbuffer::StringBufferId;
use crate::data::util::smallbox::SmallBox;
use crate::data::{CompressionStats, blockbuffer::BlockStore};

/// Tracing policy and active-only scratch wrap the generic store without another allocation.
pub struct BlockCache {
    pub blocks: BlockStore<CollocatedMetadata>,
    active_parent_offsets: Vec<u8>,
    active_parent_row_offsets: smallvec::SmallVec<[u16; 2]>,
    active_timestamp_header: bool,
    parent_timestamp_cache: bool,
    active_parent_timestamps_valid: bool,
    // Active time is domain state; archived descriptors contain only row indexes.
    active_start: usize,
}
pub type ArchivedBlock = crate::data::blockbuffer::ArchivedBlock<CollocatedMetadata>;
pub type Data = crate::data::blockbuffer::Data<CollocatedMetadata>;
type Row<'a> = crate::data::dataframe::Row<'a, CollocatedMetadata>;
type AnyFrame<'a> = crate::data::dataframe::AnyFrame<'a, CollocatedMetadata>;
pub use crate::data::blockbuffer::{ArchivedFrame, ArchivedFrameIter, Descriptor, DropChunkError};
pub type ArchivedBlockIter<'a> = crate::data::dataframe::ArchivedBlockIter<'a, CollocatedMetadata>;
impl std::ops::Deref for BlockCache {
    type Target = BlockStore<CollocatedMetadata>;
    fn deref(&self) -> &Self::Target {
        &self.blocks
    }
}
impl std::ops::DerefMut for BlockCache {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.blocks
    }
}
/// Location in the same callsite. Block ordinals are one-based and survive eviction.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, bytemuck::NoUninit)]
pub struct SpanFieldsRef {
    pub archetype: u32,
    pub block: u32,
}

impl BlockCache {
    pub fn new(unit: usize, start_cap: usize, max_cap: usize) -> Self {
        Self {
            blocks: BlockStore::new(unit, start_cap, max_cap),
            active_parent_offsets: Vec::new(),
            active_parent_row_offsets: smallvec::SmallVec::new(),
            active_timestamp_header: false,
            parent_timestamp_cache: false,
            active_parent_timestamps_valid: true,
            active_start: 0,
        }
    }

    /// Configure before appending. The cache column is physical scratch only;
    /// callers and archived frames continue to use the original row width.
    pub fn cache_parent_timestamps(&mut self, enabled: bool) {
        assert!(self.blocks.ring.is_empty() && self.blocks.active.length == 0);
        self.parent_timestamp_cache = enabled;
        self.blocks.active = Frame::new(
            self.blocks.unit + usize::from(enabled) * 8,
            self.blocks.active.capacity(),
        );
    }

    pub fn caches_parent_timestamps(&self) -> bool {
        self.parent_timestamp_cache
    }

    /// Convert every active row in one pass. The descriptor, span references,
    /// parent dictionary and all archived allocations remain untouched.
    pub fn promote(&mut self, logical: &[u8], min_rows: usize) -> bool {
        self.blocks.promote(
            logical,
            crate::subscriber::MESSAGE_BLOCK_TARGET_SIZE,
            min_rows,
            usize::from(self.parent_timestamp_cache) * 8,
        )
    }

    pub fn active_parent_offsets(&self) -> ParentOffsets<'_> {
        ParentOffsets::new(&self.active_parent_offsets)
    }

    /// Active dictionary heap belongs to writable storage, including spare capacity.
    pub fn active_parent_offsets_heap_bytes(&self) -> usize {
        self.active_parent_offsets.capacity()
            + if self.active_parent_row_offsets.spilled() {
                self.active_parent_row_offsets.capacity() * size_of::<u16>()
            } else {
                0
            }
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
            self.blocks.active.length < self.blocks.active.capacity(),
            "prepare chunk before encoding parent"
        );
        assert!(row_offset + ParentOffsets::ROW_BYTES <= self.blocks.unit);
        let row_offset = u16::try_from(row_offset).expect("parent position exceeds u16");
        // Update targets and follows-from may coexist. Share identities across
        // columns; their exact residuals tolerate a common fitted predictor.
        if !self.active_parent_row_offsets.contains(&row_offset) {
            self.active_parent_row_offsets.push(row_offset);
        }
        let table_index = match self.active_parent_offsets().find(identity.to_bits()) {
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
                let offset = crate::data::util::float_u16::FloatU16::from_u64_lossy(index);
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
            self.active_parent_offsets()
                .entry(table_index)
                .encode_index(
                    index,
                    self.blocks.active.length,
                    self.blocks.active.length + 1,
                ),
            table_index,
        )
    }

    pub fn cycle_if_needed(&mut self) {
        self.cycle_if_needed_with_columns(&[]);
    }

    /// The subscriber supplies selectable-column positions only at rollover;
    /// archives retain metadata bytes without owning the caller's schema.
    pub fn cycle_if_needed_with_columns(&mut self, columns: &[ColumnLayout]) {
        if self.blocks.grow_if_needed() {
            self.cycle_with_columns(columns);
        }
    }

    /// Archive a populated chunk early when its owner must change string buffers.
    pub(crate) fn cycle(&mut self) {
        self.cycle_with_columns(&[]);
    }

    pub(crate) fn cycle_with_columns(&mut self, columns: &[ColumnLayout]) {
        if self.blocks.active.length == 0 {
            return;
        }
        // The common path borrows the shared physical schema directly. Only
        // standalone callers missing an origin field need a temporary copy.
        let fallback;
        let columns = if let Some(layout) = self.blocks.layouts.last() {
            layout.columns.as_slice()
        } else if self.active_timestamp_header && !columns.iter().any(|c| c.is_unsigned()) {
            let mut complete = smallvec::SmallVec::<[ColumnLayout; 8]>::from_slice(columns);
            complete.insert(0, ColumnLayout::tagged_origin(0));
            fallback = complete;
            &fallback
        } else {
            columns
        };
        assert!(
            !self.active_timestamp_header
                || columns
                    .first()
                    .is_some_and(|c| c.is_tagged_origin() && c.offset == 0),
            "tracing timestamp must begin the field directory"
        );
        let mut data = if self.parent_timestamp_cache {
            self.parent_timestamp_archive(columns)
        } else {
            if !self.active_parent_offsets.is_empty() {
                fit_strides(
                    &mut self.blocks.active,
                    &mut self.active_parent_offsets,
                    &self.active_parent_row_offsets,
                );
            }
            let metadata = CollocatedMetadata::default();
            append_retention_start(&mut self.active_parent_offsets, columns, self.active_start);
            column::fit(
                &mut self.blocks.active,
                &mut self.active_parent_offsets,
                columns,
            );
            ArchivedBlock::new_with_trailer(
                &self.blocks.active,
                metadata,
                &self.active_parent_offsets,
            )
        };
        let mut next_metadata = CollocatedMetadata::default();
        next_metadata.string_buffer = self.blocks.active_metadata.string_buffer;
        let metadata = std::mem::replace(&mut self.blocks.active_metadata, next_metadata);
        data.metadata = metadata;
        self.blocks.push_archive(data);
        self.active_parent_offsets.clear();
        self.active_parent_row_offsets.clear();
        self.active_timestamp_header = false;
        self.active_parent_timestamps_valid = true;
        self.active_start = 0;
    }

    /// Initialize the origin after rollover and validate before committing a row.
    /// Return its encoded timestamp alongside the bytes and absolute row index.
    pub fn allocate(&mut self, ts: u64) -> (&mut [u8], usize, u64) {
        self.allocate_inner(ts, false, None)
    }

    /// Allocate an event/span start with a timestamp in the first eight bytes.
    /// Generic payloads and deferred span updates continue to use allocate().
    pub fn allocate_timestamp(&mut self, ts: u64) -> (&mut [u8], usize, u64) {
        self.allocate_timestamp_with_parent(ts, None)
    }

    /// Cache an exact parent start alongside the active row. Missing parents
    /// disable relative encoding for this block; zero is a valid parent start.
    pub fn allocate_timestamp_with_parent(
        &mut self,
        ts: u64,
        parent: Option<u64>,
    ) -> (&mut [u8], usize, u64) {
        self.allocate_inner(ts, true, parent)
    }

    fn allocate_inner(
        &mut self,
        ts: u64,
        timestamp_header: bool,
        parent: Option<u64>,
    ) -> (&mut [u8], usize, u64) {
        column::OriginEncoding::check_origin(ts);
        self.cycle_if_needed();
        // A block has one row schema; generic payloads must never be rewritten.
        assert!(self.blocks.active.length == 0 || self.active_timestamp_header == timestamp_header);
        if self.blocks.active.length == 0 {
            let ts = usize::try_from(ts).expect("block timestamp exceeds usize");
            self.active_start = ts;
        }
        assert!(
            ts >= self.active_start as u64,
            "timestamp precedes block timestamp base"
        );
        let offset = ts;
        if timestamp_header {
            assert!(self.blocks.unit >= size_of::<u64>());
        }
        self.active_timestamp_header = timestamp_header;
        if self.parent_timestamp_cache && parent.is_none() {
            self.active_parent_timestamps_valid = false;
        }
        let (buffer, index) = self.blocks.active.allocate().unwrap();
        if self.parent_timestamp_cache {
            buffer[self.blocks.unit..self.blocks.unit + 8]
                .copy_from_slice(&parent.unwrap_or(0).to_ne_bytes());
        }
        (
            &mut buffer[..self.blocks.unit],
            index + self.blocks.active_descriptor.index,
            offset,
        )
    }

    /// Find metadata by absolute retained row index, including after eviction.

    /// Resolve one physical timestamp through the same ordinary field codec as iteration.
    pub fn decode_timestamp(&self, encoded: u64, index: usize) -> Option<u64> {
        if index >= self.blocks.active_descriptor.index {
            return (index - self.blocks.active_descriptor.index < self.blocks.active.length)
                .then_some(encoded);
        }
        let (ordinal, block) = self.blocks.archived_at(index)?;
        let fallback = [ColumnLayout::tagged_origin(0)];
        let columns = self.blocks.columns_for_block(ordinal, &fallback);
        if block.data.timestamp_parent_relative(columns) {
            return None;
        }
        Some(
            block
                .data
                .timestamp_column(columns)
                .decode(encoded as u128, index - block.descriptor.index) as u64,
        )
    }

    pub fn push(&mut self, data: &[u8], ts: usize) {
        self.allocate(ts as u64).0.copy_from_slice(data);
        self.update_mask_after_append();
    }

    /// Drop the oldest completed chunk by its first row's absolute index.
    /// Reject stale indexes and later/active chunks without changing the cache.

    /// Borrow a row and its descriptor together, so decoding needs one lookup.

    /// Span closes edit only fixed structural bytes. Keep that common path
    /// direct for active rows and reuse physical scratch for archived rows.

    /// Candidates can include other spans' updates; readers must match the full SpanId.
    pub fn span_fields_refs(&self, index: usize) -> &[SpanFieldsRef] {
        if index >= self.blocks.active_descriptor.index {
            return if index - self.blocks.active_descriptor.index < self.blocks.active.length {
                self.blocks
                    .active_metadata
                    .span_fields()
                    .unwrap_or_default()
            } else {
                &[]
            };
        }
        self.blocks
            .ring
            .iter()
            .rev()
            .find(|block| block.descriptor.index <= index)
            .and_then(|block| block.data.metadata.span_fields())
            .unwrap_or_default()
    }

    /// Deduplicate candidates and rebuild the exact-size list only for a new reference.
    pub(crate) fn add_span_fields_ref(&mut self, index: usize, reference: SpanFieldsRef) {
        let metadata = if index >= self.blocks.active_descriptor.index {
            if index - self.blocks.active_descriptor.index >= self.blocks.active.length {
                return;
            }
            &mut self.blocks.active_metadata
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
        std::iter::once(&self.blocks.active_metadata)
            .chain(self.blocks.ring.iter().map(|block| &block.data.metadata))
            .map(CollocatedMetadata::heap_bytes)
            .sum()
    }

    /// NOTE: could simplify by computing mask when compacting. Pretty sure it's the same amount of processing.

    /// Estimate the oldest archive's interval from consecutive block starts.
    /// An empty active tail supplies no timestamp; use one tick in that case.
    pub(crate) fn eviction_timestamps(
        &self,
        fallback: &[ColumnLayout],
    ) -> Option<std::ops::Range<usize>> {
        let first = self.blocks.ring.front()?;
        let columns = self.blocks.columns_for_block(self.blocks.offset, fallback);
        let start = first.data.timestamp_start(columns);
        let next = if let Some(next) = self.blocks.ring.get(1) {
            let columns = self
                .blocks
                .columns_for_block(self.blocks.offset + 1, fallback);
            next.data.timestamp_start(columns)
        } else if self.blocks.active.length != 0 {
            self.active_start
        } else {
            start
        };
        Some(start..next)
    }

    /// Active frame, descriptor and shared cache/scratch state; no archived traversal.
    pub fn active_compression_ratio(&self) -> CompressionStats {
        self.blocks.active_compression_ratio()
            + CompressionStats {
                overhead: size_of::<Self>() - size_of::<BlockStore<CollocatedMetadata>>()
                    + self.active_parent_offsets.len()
                    + if self.active_parent_row_offsets.spilled() {
                        self.active_parent_row_offsets.len() * 2
                    } else {
                        0
                    },
                overalloc: self.active_parent_offsets_heap_bytes()
                    - self.active_parent_offsets.len()
                    - if self.active_parent_row_offsets.spilled() {
                        self.active_parent_row_offsets.len() * 2
                    } else {
                        0
                    },
                ..Default::default()
            }
    }

    /// Domain scratch belongs to active storage, never to the archived payloads.
    pub fn compression_parts(&self) -> (CompressionStats, CompressionStats) {
        (
            self.active_compression_ratio(),
            self.blocks.compression_parts().1,
        )
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
        time::{SystemTime, UNIX_EPOCH},
    };

    use bytemuck::{Pod, Zeroable, bytes_of};
    use streaming_iterator::StreamingIterator;

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
            assert_eq!(offset, 12);
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
        // Zero is an ordinary immutable start, including after rollover.
        assert_eq!(cache.ring[0].data.timestamp_start(&[]), 0);
        assert_eq!(cache.eviction_timestamps(&[]), Some(0..20));
        assert_eq!(cache.active_start, 20);
        // Growing a starter frame also preserves the first row's origin.
        let mut growing = BlockCache::new(8, 1, 8);
        growing.push(&[0; 8], 100);
        growing.push(&[0; 8], 110);
        assert!(growing.active.capacity() > 1);
        assert_eq!(growing.active_start, 100);
    }

    #[test]
    fn eviction_intervals_follow_archived_and_active_starts_after_eviction() {
        let mut cache = BlockCache::new(8, 2, 2);
        for ts in [0, 7, 20, 21, 50] {
            cache.push(&[0; 8], ts);
        }
        // The next archive bounds the first interval; the active tail bounds
        // the next one. No row maxima or late close timestamps participate.
        assert_eq!(cache.eviction_timestamps(&[]), Some(0..20));
        cache.drop_chunk(1).unwrap();
        assert_eq!(cache.eviction_timestamps(&[]), Some(20..50));
        cache.cycle();
        assert_eq!(cache.eviction_timestamps(&[]), Some(20..50));
        cache.drop_chunk(3).unwrap();
        assert_eq!(cache.eviction_timestamps(&[]), Some(50..50));
        // An empty active frame has no start; the eviction scorer clamps to a tick.
        assert_eq!(cache.ring[0].data.frame.trailer().len(), 8);
    }

    #[test]
    fn oversized_timestamp_is_rejected_before_rollover_or_row_allocation() {
        let mut cache = BlockCache::new(8, 1, 1);
        let max = crate::data::column::OriginEncoding::MAX_ORIGIN;
        cache
            .allocate_timestamp(max)
            .0
            .copy_from_slice(&max.to_ne_bytes());
        cache.update_mask_after_append();
        // Rejecting a tag collision must leave even a full active frame intact.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cache.allocate_timestamp(max + 1);
        }));
        assert!(result.is_err());
        assert_eq!(cache.active.length, 1);
        assert!(cache.ring.is_empty());
        cache.cycle();
        let columns = [crate::data::column::Layout::tagged_origin(0)];
        assert_eq!(cache.ring[0].data.timestamp_start(&columns), max as usize);
        // Ten-byte predictor plus its one-byte directory; no retention end.
        assert_eq!(cache.ring[0].data.frame.trailer().len(), 11);
        assert_eq!(cache.decode_timestamp(0, 1), Some(max));
    }

    #[test]
    fn timestamp_offsets_cover_unsigned_limits_without_truncation() {
        use crate::data::column::{Context, Encoding, Footer, PredictionEncoding};
        // The first ordinary field footer, rather than Descriptor, owns the basis.
        for origin in [0, 1u64 << 63] {
            let mut footer = Footer::new();
            footer.extend_from_slice(&origin.to_le_bytes());
            footer.extend_from_slice(&[0, 0]);
            let mut frame = crate::data::dataframe::Frame::new(8, 3);
            for _ in 0..3 {
                frame.push(&[0; 8]);
            }
            let archived = super::ArchivedFrame::new_from(&frame);
            let layout = crate::data::column::Layout::unsigned(0, 8);
            let context = Context {
                frame: &archived,
                layout,
            };
            let encoding = <PredictionEncoding<true> as Encoding>::parse(&footer, layout, 3);
            for ts in [origin, origin + 1, u64::MAX] {
                let offset = encoding.encode(ts as u128, 0, context).unwrap();
                assert_eq!(offset, (ts - origin) as u128);
                assert_eq!(encoding.decode(offset, 0, context), ts as u128);
            }
        }
        assert_eq!(size_of::<super::Descriptor>(), 8);
        assert_eq!(size_of::<super::Data>(), 16);
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
        assert_eq!(archived.data.timestamp_start(&[]), 1);
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
        assert_eq!(cache.active_start, 3);
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
            cache.get_mut(index, &[]).unwrap().1.get_mut()[0] ^= bit;
            expected[index - 1][0] ^= bit; // Absolute row indexes start at one.
        }
        let mut rows = cache.iter(&[]);
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
        let mut c_iter = cache.iter(&[]);
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

/// Timestamp fields already own their absolute block start, including relative
/// representations. Only schemas without timestamps need a separate start word.
fn append_retention_start(trailer: &mut Vec<u8>, columns: &[ColumnLayout], start: usize) {
    if !columns
        .first()
        .is_some_and(|c| c.is_tagged_origin() && c.offset == 0)
    {
        trailer.extend_from_slice(&(start as u64).to_le_bytes());
    }
}
impl Row<'_> {
    /// Physical edits borrow the existing archive; the start field is decoded once.
    pub(crate) fn decode_timestamp(&mut self) -> u64 {
        let value = u64::from_ne_bytes(self.get_mut()[..8].try_into().unwrap());
        match &self.frame {
            AnyFrame::Archived(block, columns, _) => block
                .timestamp_column(columns)
                .decode(value as u128, self.index)
                as u64,
            _ => value,
        }
    }
}
impl ArchivedBlock {
    /// Only update/reference-only schemas need an independent absolute start.
    pub fn retention_footer_bytes(&self, columns: &[ColumnLayout]) -> usize {
        usize::from(!Self::has_timestamp(columns)) * 8
    }
    fn has_timestamp(columns: &[ColumnLayout]) -> bool {
        columns
            .first()
            .is_some_and(|c| c.is_tagged_origin() && c.offset == 0)
    }
    pub fn timestamp_parent_relative(&self, columns: &[ColumnLayout]) -> bool {
        Self::has_timestamp(columns) && self.timestamp_column(columns).encoding().is_relative()
    }
    pub fn parent_offsets(&self, columns: &[ColumnLayout]) -> ParentOffsets<'_> {
        let prefix = column::Metadata::split(self.frame.trailer(), columns).0;
        let retained = self.retention_footer_bytes(columns);
        ParentOffsets::new(
            &prefix[..prefix
                .len()
                .checked_sub(retained)
                .expect("tracing retention footer missing")],
        )
    }
    pub fn timestamp_column<'a>(
        &'a self,
        columns: &'a [ColumnLayout],
    ) -> column::Column<'a> {
        // Entry zero counts backward from the directory boundary. Its own size
        // locates the timestamp without summing any other record lengths.
        debug_assert!(
            columns
                .first()
                .is_some_and(|c| c.is_tagged_origin() && c.offset == 0)
        );
        let trailer = self.frame.trailer();
        let end = trailer.len() - columns.len();
        let bytes = usize::from(*trailer.get(end).expect("timestamp directory missing")) * 2;
        let start = end
            .checked_sub(bytes)
            .expect("timestamp footer exceeds records");
        column::Column::parse(
            &self.frame,
            ColumnLayout::tagged_origin(0),
            &trailer[start..end],
        )
    }
    /// Read the absolute block start without scanning rows or resolving parents.
    pub fn timestamp_start(&self, columns: &[ColumnLayout]) -> usize {
        if Self::has_timestamp(columns) {
            self.timestamp_column(columns).encoding().origin() as usize
        } else {
            let prefix = column::Metadata::split(self.frame.trailer(), columns).0;
            u64::from_le_bytes(prefix[prefix.len() - 8..].try_into().unwrap()) as usize
        }
    }
}

impl crate::data::blockbuffer::BlockMetadata for CollocatedMetadata {
    fn heap_bytes(&self) -> usize {
        self.heap_bytes()
    }
}
/// Shared active/archive metadata; both the thin owner and string ID align to two bytes.
#[repr(C)]
#[derive(Default)]
pub struct CollocatedMetadata {
    // Put the pointer first so no small field introduces padding before it.
    /// most spans have no extra fields (recorded after creation),
    /// if they do a record is created for the fields with the spanid of the span
    /// we only store the set of extra fields blocks associated with any row of this block.
    /// reader is responsible for rebuilding association if desired, this limits search space (likely to one block, if not zero)
    span_fields: crate::data::util::exact_set::ExactSet<SpanFieldsRef>,

    /// index of the string buffer in this arch's collection which indices in this block refer to
    // TODO: Rework into shared footer metadata, stored once per block only when
    // its schema contains string references (buffer indices or offsets).
    pub string_buffer: StringBufferId,
}

const _: () = assert!(align_of::<CollocatedMetadata>() == 2);
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(size_of::<crate::data::util::exact_set::ExactSet<SpanFieldsRef>>() == 8);
    assert!(size_of::<CollocatedMetadata>() == 12);
    assert!(std::mem::offset_of!(CollocatedMetadata, span_fields) == 0);
};

impl CollocatedMetadata {
    pub fn span_fields(&self) -> Option<&[SpanFieldsRef]> {
        self.span_fields.get()
    }

    fn add_span_fields_ref(&mut self, reference: SpanFieldsRef) {
        self.span_fields.insert(reference);
    }

    pub fn compression_ratio(&self) -> CompressionStats {
        CompressionStats {
            overhead: size_of::<Self>() + self.heap_bytes(),
            ..Default::default()
        }
    }

    /// Exclude inline metadata already charged to its active or archived owner.
    pub fn heap_bytes(&self) -> usize {
        self.span_fields.heap_bytes()
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;

    #[test]
    fn thin_span_list_preserves_entries_and_deduplicates_across_growth() {
        let mut metadata = CollocatedMetadata::default();
        assert_eq!(align_of::<CollocatedMetadata>(), 2);
        assert!(metadata.span_fields().is_none());
        assert_eq!(metadata.heap_bytes(), 0);
        // Revisit every entry after each replacement; duplicates must not allocate.
        for block in 1..=257 {
            let reference = SpanFieldsRef {
                archetype: 3,
                block,
            };
            metadata.add_span_fields_ref(reference);
            let bytes = metadata.heap_bytes();
            metadata.add_span_fields_ref(reference);
            assert_eq!(metadata.heap_bytes(), bytes);
            let refs = metadata.span_fields().unwrap();
            assert_eq!(refs.len(), block as usize);
            assert!(
                refs.iter()
                    .enumerate()
                    .all(|(i, r)| r.archetype == 3 && r.block == i as u32 + 1)
            );
        }
        assert_eq!(
            metadata.heap_bytes(),
            // The u16 prefix occupies four bytes after aligning the slice.
            4 + 257 * size_of::<SpanFieldsRef>()
        );
    }
}

// Tracing-specific interpretation of the generic reference dictionary key.
/// Packed identity: callsite in the low 20 bits, archetype in the high 12 bits.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallsiteArchetype(u32);

impl CallsiteArchetype {
    pub const CALLSITE_BITS: u32 = 20;
    pub const ARCHETYPE_BITS: u32 = 12;
    pub const MAX_CALLSITE: u32 = (1 << Self::CALLSITE_BITS) - 1;
    pub const MAX_ARCHETYPE: u32 = (1 << Self::ARCHETYPE_BITS) - 1;

    /// Reject overflow instead of silently aliasing a different parent identity.
    pub const fn new(callsite: u32, archetype: u32) -> Self {
        assert!(callsite <= Self::MAX_CALLSITE, "callsite exceeds 20 bits");
        assert!(
            archetype <= Self::MAX_ARCHETYPE,
            "archetype exceeds 12 bits"
        );
        Self(callsite | (archetype << Self::CALLSITE_BITS))
    }

    pub const fn callsite(self) -> u32 {
        self.0 & Self::MAX_CALLSITE
    }

    pub const fn archetype(self) -> u32 {
        self.0 >> Self::CALLSITE_BITS
    }

    /// Every u32 bit pattern encodes a valid pair.
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    pub const fn to_bits(self) -> u32 {
        self.0
    }
}

pub(crate) use crate::data::encoding::reference::fit_strides;
pub use crate::data::encoding::reference::{
    ReferenceEncoding as ParentOffsetEntry, ReferenceTable as ParentOffsets,
};

#[cfg(test)]
mod parent_offsets_tests {
    use super::*;
    #[test]
    fn packed_identity_preserves_both_fields_at_their_boundaries() {
        assert_eq!(size_of::<CallsiteArchetype>(), 4);
        assert_eq!(size_of::<ParentOffsetEntry>(), 8);
        assert_eq!(ParentOffsets::ENTRY_BYTES, 8);
        // Cross-product catches a shift or mask that leaks across either field.
        for callsite in [0, 1, CallsiteArchetype::MAX_CALLSITE] {
            for archetype in [0, 1, CallsiteArchetype::MAX_ARCHETYPE] {
                let identity = CallsiteArchetype::new(callsite, archetype);
                assert_eq!(identity.callsite(), callsite);
                assert_eq!(identity.archetype(), archetype);
                assert_eq!(CallsiteArchetype::from_bits(identity.to_bits()), identity);
            }
        }
        assert_eq!(
            CallsiteArchetype::from_bits(u32::MAX).callsite(),
            (1 << 20) - 1
        );
        assert_eq!(
            CallsiteArchetype::from_bits(u32::MAX).archetype(),
            (1 << 12) - 1
        );
    }

    #[test]
    #[should_panic(expected = "callsite exceeds 20 bits")]
    fn packed_identity_rejects_callsite_overflow() {
        CallsiteArchetype::new(1 << 20, 0);
    }

    #[test]
    #[should_panic(expected = "archetype exceeds 12 bits")]
    fn packed_identity_rejects_archetype_overflow() {
        CallsiteArchetype::new(0, 1 << 12);
    }
}

// Tracing chooses absolute or parent-relative starts; generic encodings own
// fitting, footer layout and row transforms for the selected ordinary field.
impl BlockCache {
    /// Strip active-only parent starts before fitting either complete archive.
    /// Compare real aligned allocations, retaining the old scheme on ties.
    fn parent_timestamp_archive(&self, columns: &[column::Layout]) -> SmallBox<ArchivedBlock> {
        let source = || {
            let mut frame = Frame::new(self.blocks.unit, self.blocks.active.length);
            for row in self.blocks.active.iter() {
                frame.push(&row[..self.blocks.unit]);
            }
            // Preserve editable end slots and other structural mask locks.
            frame.buffer[..self.blocks.unit]
                .copy_from_slice(&self.blocks.active.mask()[..self.blocks.unit]);
            frame
        };
        let finish = |mut frame: Box<Frame>, relative: bool| {
            let metadata = CollocatedMetadata::default();
            let mut trailer = self.active_parent_offsets.clone();
            if !trailer.is_empty() {
                fit_strides(&mut frame, &mut trailer, &self.active_parent_row_offsets);
            }
            append_retention_start(&mut trailer, columns, self.active_start);
            if relative {
                column::fit_relative(&mut frame, &mut trailer, columns, self.active_start as u64);
            } else {
                column::fit(&mut frame, &mut trailer, columns);
            }
            ArchivedBlock::new_with_trailer(&frame, metadata, &trailer)
        };
        let absolute = finish(source(), false);
        if !self.active_parent_timestamps_valid || !self.active_timestamp_header {
            return absolute;
        }
        let mut relative = source();
        let mut elapsed = Vec::with_capacity(self.blocks.active.length);
        for row in self.blocks.active.iter() {
            let offset = u64::from_ne_bytes(row[..8].try_into().unwrap());
            let parent = u64::from_ne_bytes(
                row[self.blocks.unit..self.blocks.unit + 8]
                    .try_into()
                    .unwrap(),
            );
            let Some(value) = offset.checked_sub(parent) else {
                return absolute;
            };
            elapsed.push(value);
        }
        column::rewrite(
            &mut relative,
            column::Layout::new(0, 8, false),
            |_, row, _| u128::from(elapsed[row]),
        );
        let relative = finish(relative, true);
        if relative.allocation_size() < absolute.allocation_size() {
            relative
        } else {
            absolute
        }
    }
}

#[cfg(test)]
mod timestamps_tests {
    use super::*;
    use crate::data::util::float_u16::FloatU16;
    use crate::subscriber::storage::{BlockCache, CallsiteArchetype};

    const COLUMNS: [column::Layout; 1] = [column::Layout {
        offset: 0,
        width: 8,
        policy: column::Policy::TaggedOrigin,
    }];
    fn archived_stride(block: &ArchivedBlock) -> FloatU16 {
        block.timestamp_column(&COLUMNS).encoding().stride_float()
    }
    fn append(cache: &mut BlockCache, ts: u64) {
        // Match the subscriber's timestamp-header write and mask update order.
        let (row, _, offset) = cache.allocate_timestamp(ts);
        row.fill(0);
        row[..8].copy_from_slice(&offset.to_ne_bytes());
        cache.update_mask_after_append();
    }

    #[test]
    fn active_parent_column_is_omitted_and_smaller_archives_win() {
        let mut cache = BlockCache::new(8, 32, 32);
        cache.cache_parent_timestamps(true);
        for r in 0..32u64 {
            let parent = 1_000_000 + r * r * 10_000;
            let (row, _, offset) = cache.allocate_timestamp_with_parent(parent + 7, Some(parent));
            row.copy_from_slice(&offset.to_ne_bytes());
            cache.update_mask_after_append();
        }
        assert_eq!(cache.active.unit, 16);
        assert_eq!(
            u64::from_ne_bytes(cache.active.iter().nth(7).unwrap()[8..].try_into().unwrap()),
            1_490_000
        );
        cache.cycle();
        let block = &cache.ring[0];
        assert!(block.data.timestamp_parent_relative(&COLUMNS));
        assert_eq!(block.data.frame.unit(), 8);
        assert_eq!(block.data.frame.mask()[..8], [0; 8]);
        assert_eq!(
            u64::from_ne_bytes(block.data.frame.first()[..8].try_into().unwrap()),
            7
        );
        assert_eq!(cache.active.unit, 16);
    }

    #[test]
    fn cache_keeps_absolute_prediction_when_relative_is_larger_or_unavailable() {
        for missing in [false, true] {
            let mut cache = BlockCache::new(8, 50, 50);
            cache.cache_parent_timestamps(true);
            for r in 0..50 {
                let (row, _, offset) = cache.allocate_timestamp_with_parent(
                    1_000 + r * 2,
                    if missing { None } else { Some(0) },
                );
                row.copy_from_slice(&offset.to_ne_bytes());
                cache.update_mask_after_append();
            }
            cache.cycle();
            assert!(!cache.ring[0].data.timestamp_parent_relative(&COLUMNS));
            assert_eq!(cache.ring[0].data.frame.unit(), 8);
            check_archived(&cache, &(0..50).map(|r| 1_000 + r * 2).collect::<Vec<_>>());
        }
    }

    fn check_archived(cache: &BlockCache, times: &[u64]) {
        let block = cache.ring.back().unwrap();
        assert_eq!(block.data.frame.len(), times.len());
        let mut row = vec![0; block.data.frame.unit()];
        let mut mask = 0u64;
        let mut first = 0;
        for (r, &ts) in times.iter().enumerate() {
            block.data.frame.read_row(r, &mut row);
            let encoded = u64::from_ne_bytes(row[..8].try_into().unwrap());
            assert_eq!(
                block
                    .data
                    .timestamp_column(&COLUMNS)
                    .decode(encoded as u128, r) as u64,
                ts
            );
            // Random access must use the same block metadata and local row origin.
            assert_eq!(
                cache.decode_timestamp(encoded, block.descriptor.index + r),
                Some(ts)
            );
            if r == 0 {
                first = encoded;
            }
            mask |= encoded ^ first;
        }
        // Fitting must clear stale timestamp mask bits to realize the savings.
        assert_eq!(&block.data.frame.mask()[..8], mask.to_ne_bytes());
    }

    #[test]
    fn coarse_ticks_keep_small_strides_exact_and_remove_linear_variation() {
        for shift in [0, 10, 20, 40] {
            let mut cache = BlockCache::new(8, 50, 50);
            let times: Vec<_> = (0..50)
                .map(|r| (1_000_000 + (r << shift)) >> shift)
                .collect();
            for &ts in &times {
                append(&mut cache, ts);
                assert_eq!(
                    u64::from_ne_bytes(
                        cache.active.iter().last().unwrap()[..8].try_into().unwrap()
                    ),
                    ts
                );
            }
            // Fifty roughly millisecond ticks at shift 20 need a stride of 50.
            cache.cycle();
            assert_eq!(archived_stride(&cache.ring[0].data).to_u64(), 50);
            check_archived(&cache, &times);
            assert_eq!(&cache.ring[0].data.frame.mask()[..8], &[0; 8]);
        }
    }

    #[test]
    fn stride_limits_and_flat_blocks_round_trip() {
        for (times, stride) in [
            (vec![0, FloatU16::MAX / 2], FloatU16::MAX),
            (
                vec![0, FloatU16::MAX / 2 + 1],
                if column::UNSIGNED_MINIMAX_STRIDE {
                    FloatU16::MAX
                } else {
                    0
                },
            ),
            (
                vec![0, column::OriginEncoding::MAX_ORIGIN],
                if column::UNSIGNED_MINIMAX_STRIDE {
                    FloatU16::MAX
                } else {
                    0
                },
            ),
            (vec![column::OriginEncoding::MAX_ORIGIN], 0),
            (vec![7, 7, 7], 0),
            (
                vec![0, 9, 0],
                if column::UNSIGNED_MINIMAX_STRIDE {
                    8
                } else {
                    0
                },
            ),
        ] {
            let mut cache = BlockCache::new(8, times.len(), times.len());
            for &ts in &times {
                append(&mut cache, ts);
            }
            cache.cycle();
            // Mask fitting may choose a very different coefficient with fewer bits.
            // Keep legacy coefficient checks when that optional policy is selected.
            if !column::UNSIGNED_MASK_STRIDE {
                assert_eq!(archived_stride(&cache.ring[0].data).to_u64(), stride);
            }
            check_archived(&cache, &times);
        }
    }

    #[test]
    fn irregular_full_range_times_and_quantized_strides_round_trip() {
        for times in [
            vec![0, column::OriginEncoding::MAX_ORIGIN, 2],
            vec![1 << 59, (1 << 59) + 5_000, (1 << 59) + 2, (1 << 59) + 20],
            vec![0, 0, 5_003], // A fractional fit also loses a FloatU16 precision bit.
            vec![
                column::OriginEncoding::MAX_ORIGIN - 2,
                column::OriginEncoding::MAX_ORIGIN - 1,
                column::OriginEncoding::MAX_ORIGIN,
            ],
        ] {
            let mut cache = BlockCache::new(8, times.len(), times.len());
            for &ts in &times {
                append(&mut cache, ts);
            }
            cache.cycle();
            assert!(archived_stride(&cache.ring[0].data).to_u64() > 0);
            check_archived(&cache, &times);
        }
    }

    #[test]
    fn early_archive_edits_late_closes_and_eviction_preserve_predictions() {
        let mut cache = BlockCache::new(10, 32, 32);
        let times = [100, 102, 103, 106];
        for ts in times {
            append(&mut cache, ts);
        }
        cache.active.lock_editable(8, 9);
        // Duration edits must not influence the immutable start-time fit.
        cache.active.buffer[18] = 17;
        cache.cycle();
        assert_eq!(
            archived_stride(&cache.ring[0].data).to_u64(),
            if column::UNSIGNED_MASK_STRIDE {
                8 // Endpoint's residual mask is smaller than the minimax mask.
            } else if column::UNSIGNED_MINIMAX_STRIDE {
                6
            } else {
                8
            }
        );
        assert_eq!(cache.ring[0].data.frame.mask()[8], 0xff);
        check_archived(&cache, &times);
        // Changing a constant byte rebuilds the archive without refitting times.
        cache.get_mut_physical(1, &COLUMNS).unwrap().1.get_mut()[9] = 42;
        check_archived(&cache, &times);
        for ts in [500, 501] {
            append(&mut cache, ts);
        }
        cache.cycle();
        cache.drop_chunk(1).unwrap();
        check_archived(&cache, &[500, 501]);
        assert_eq!(cache.ring[0].descriptor.index, 5);
    }

    #[test]
    fn timestamp_and_parent_strides_fit_independently_in_the_same_frame() {
        let mut cache = BlockCache::new(18, 4, 4);
        for r in 0..4 {
            let (parent, group) = cache.encode_parent(200 + r * 3, CallsiteArchetype::new(7, 3), 8);
            let (row, _, offset) = cache.allocate_timestamp(100 + r * 2);
            row[..8].copy_from_slice(&offset.to_ne_bytes());
            row[8..16].copy_from_slice(&parent.to_ne_bytes());
            row[16..18].copy_from_slice(&group.to_ne_bytes());
            cache.update_mask_after_append();
        }
        cache.cycle();
        check_archived(&cache, &[100, 102, 104, 106]);
        let block = &cache.ring[0];
        let entry = block.data.parent_offsets(&COLUMNS).entry(0);
        assert_eq!(entry.stride.to_u64(), 12);
        let mut row = [0; 18];
        for r in 0..4 {
            block.data.frame.read_row(r, &mut row);
            assert_eq!(
                entry.decode_index(u64::from_ne_bytes(row[8..16].try_into().unwrap()), r, 4),
                200 + r as u64 * 3
            );
        }
    }
}

// Tracing integration coverage stays beside the storage policy.
#[cfg(test)]
mod columns {
    //! Integration coverage for generic field edits beside tracing reference metadata.
    use super::{BlockCache, CallsiteArchetype, ParentOffsets};
    use crate::data::column::*;
    use crate::data::column::{read_word, write_word};
    use crate::data::encoding::common::{DICTIONARY, DICTIONARY_CYCLE, DICTIONARY_STRIDE, ROW};
    fn irregular(count: usize, rows: usize, base: u128, step: u128) -> Vec<u128> {
        // Discover all entries before an irregular tail; dictionary IDs follow first occurrence.
        let mut state = 7u32;
        (0..rows)
            .map(|row| {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                let id = if row < count {
                    (row + 7) % count
                } else {
                    (state >> 16) as usize % count
                };
                base + id as u128 * step
            })
            .collect()
    }

    #[test]
    fn dictionary_edits_preserve_other_predictions_and_structural_metadata() {
        let columns = [
            Layout::tagged_origin(0),
            Layout::new(18, 2, true),
            Layout::new(20, 2, true),
        ];
        let mut cache = BlockCache::new(22, 1024, 1024);
        let mut expected = irregular(32, 1024, 1000, 900);
        for (row, &value) in expected.iter().enumerate() {
            let (parent, group) =
                cache.encode_parent(500 + row as u64 * 3, CallsiteArchetype::new(7, 3), 8);
            let (bytes, _, ts) = cache.allocate_timestamp(100 + row as u64 * 2);
            bytes[..8].copy_from_slice(&ts.to_ne_bytes());
            bytes[8..16].copy_from_slice(&parent.to_ne_bytes());
            bytes[16..18].copy_from_slice(&group.to_ne_bytes());
            write_word(&mut bytes[18..20], value);
            write_word(&mut bytes[20..22], 2000 + row as u128);
            cache.update_mask_after_append();
        }
        cache.cycle_if_needed_with_columns(&columns);
        let footer = cache.ring[0].data.frame.trailer().to_vec();
        assert_eq!(
            footer.len(),
            ParentOffsets::ENTRY_BYTES + cache.ring[0].data.column_footer_bytes(&columns)
        );
        // A known dictionary value can be edited without replacing coefficients.
        expected[17] = expected[31];
        write_word(
            &mut cache.get_mut(18, &columns).unwrap().1.get_mut()[18..20],
            expected[17],
        );
        assert_eq!(cache.ring[0].data.frame.trailer(), footer);
        // A new value below the minimum refits columns without changing structural
        // timestamp/parent encodings or the unrelated column's decoded values.
        expected[4] = 7;
        write_word(
            &mut cache.get_mut(5, &columns).unwrap().1.get_mut()[18..20],
            7,
        );
        let block = &cache.ring[0].data;
        let records: Vec<_> = block
            .column_metadata(&columns)
            .entries()
            .map(|(_, r)| r)
            .collect();
        assert!(matches!(
            records[1][0],
            DICTIONARY | DICTIONARY_CYCLE | DICTIONARY_STRIDE
        ));
        assert_eq!(records[2][0], ROW);
        let mut bytes = [0; 22];
        for (row, &value) in expected.iter().enumerate() {
            block.read_row(row, &mut bytes, &columns);
            assert_eq!(read_word(&bytes[18..20]), value);
            assert_eq!(read_word(&bytes[20..22]), 2000 + row as u128);
            assert_eq!(read_word(&bytes[..8]) as u64, 100 + row as u64 * 2);
            assert_eq!(
                block.parent_offsets(&columns).entry(0).decode_index(
                    read_word(&bytes[8..16]) as u64,
                    row,
                    1024
                ),
                500 + row as u64 * 3
            );
        }
    }
}

// Tracing integration coverage stays beside the storage policy.
#[cfg(test)]
mod strings {
    //! String rotation is coordinated by tracing chunk policy, above generic buffers.
    use crate::data::stringbuffer::{Buffer, StringBufferId, StringStorage};
    #[test]
    fn full_buffers_force_chunk_rotation_and_preserve_rows() {
        use crate::subscriber::{InputValue, LocalRecordingLayer, RecordingLayer};
        use std::sync::Arc;
        use tracing_subscriber::{Registry, prelude::*};

        fn emit(a: &str, b: &str) {
            tracing::info!(a, b);
        }
        for mode in [StringStorage::Offset, StringStorage::Index] {
            let mut layer = RecordingLayer::new();
            layer.string_storage = mode;
            layer.callsite_memory_cap = usize::MAX;
            layer.global_memory_cap = usize::MAX;
            let recorder = Arc::new(layer);
            let dispatch = tracing::Dispatch::new(
                Registry::default().with(LocalRecordingLayer(recorder.clone())),
            );
            tracing::dispatcher::with_default(&dispatch, || emit("first a", "first b"));
            let site = recorder.callsites.read().values().next().unwrap().clone();
            {
                let mut site = site.write();
                let buffer = &mut site.cache[0].strings.active;
                // Fill storage directly to avoid quadratic overflow dedup work in this boundary test.
                buffer.fill_unused_entries(Buffer::MAX_ENTRIES - 2);
            }
            tracing::dispatcher::with_default(&dispatch, || emit("last a", "last b"));
            assert_eq!(
                site.read().cache[0].strings.active.len(),
                Buffer::MAX_ENTRIES
            );
            tracing::dispatcher::with_default(&dispatch, || emit("rotated a", "rotated b"));
            let site = site.read();
            let arch = &site.cache[0];
            assert_eq!(arch.data.ring.len(), 1);
            assert_eq!(arch.data.ring[0].data.frame.len(), 2);
            assert_eq!(
                arch.data.ring[0].data.metadata.string_buffer,
                StringBufferId::new(0)
            );
            assert_eq!(
                arch.data.active_metadata.string_buffer,
                StringBufferId::new(1)
            );
            assert_eq!(arch.strings.active.len(), 2);
            let mut rows = Vec::new();
            site.visit_rows(|row| rows.push(row.fields));
            for (row, (a, b)) in rows.iter().zip([
                ("first a", "first b"),
                ("last a", "last b"),
                ("rotated a", "rotated b"),
            ]) {
                assert_eq!(
                    *row,
                    vec![
                        Some(InputValue::Text(a.into())),
                        Some(InputValue::Text(b.into()))
                    ]
                );
            }
            assert_eq!(rows.len(), 3);
        }
    }
}
