use std::{
    cmp::{max, min},
    collections::binary_heap,
    default,
    ops::Range,
    sync::atomic::AtomicU64,
};

use streaming_iterator::{IntoStreamingIterator, StreamingIterator};

use super::CompressionStats;
use super::dataframe::{AnyFrame, Frame, Row, RowBuffer};

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
    // TODO: The data should be compacted at this point. and recompacted later if needed.
    // NOTE: frame size is in rows,
    // as a rule of thumb: frames should be at least as long as they are wide,
    // otherwise the overhead of the mask exceeds the savings when eliding one byte.
    frame_size_max: usize,

    // TODO: more optimistically compact frame, ie. compact before frame is full and unpack as needed.
    // TODO: partial allocation of active frame. Let the active start small and reallocate itself as it grows.
    // This will save on the overhead of rare logs. otherwise we will allocate at 4kib (or whatever the frame size is) min per log!
    // another way to do this is to increase the size of the active frame after pushing it to the ring, for the first couple frames.
    pub active: Box<Frame>,

    // NOTE: this could be made contiguous, since frames don't change size after being compacted.
    pub ring: Vec<Data>,

    /// Preserve absolute block references when eviction removes a prefix.
    pub offset: usize,

    /// Metadata moves intact into Data when the active frame is archived.
    pub active_descriptor: Descriptor,

    // used for get_mut
    edit_buffer: Vec<u8>,
}

pub struct Data {
    pub data: Box<ArchivedFrame>,
    pub descriptor: Descriptor,
}

/// Metadata shared by active and archived blocks; row indices remain absolute.
#[derive(Debug, Default)]
pub struct Descriptor {
    /// One-based starting row index within the archetype.
    pub index: usize,
    /// BufferCollection ID shared by every string field in this chunk.
    pub string_buffer: usize,
    /// Deduplicated deferred-span block candidates; the boxed Vec is allocated lazily.
    pub span_fields: Option<Box<Vec<SpanFieldsRef>>>,
    /// Invariant: start is the first row's timestamp and NEVER changes for this block
    /// ts are stored as offsets from start, preserving exact start times forever.
    /// if negative offsets are enabled, start need not be the minimum timestamp.
    pub(crate) timestamp: Range<usize>,
}

/// Location in the same callsite. Block ordinals are one-based and survive eviction.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, bytemuck::NoUninit)]
pub struct SpanFieldsRef {
    pub archetype: u32,
    pub block: u32,
}

impl Descriptor {
    /// Zigzag preserves small offsets of either sign for the frame compressor.
    pub(crate) fn encode_timestamp(&self, ts: u64, allow_negative: bool) -> u64 {
        let base = self.timestamp.start as u64;
        assert!(
            allow_negative || ts >= base,
            "timestamp precedes block timestamp base"
        );
        let delta =
            i64::try_from(i128::from(ts) - i128::from(base)).expect("timestamp offset exceeds i64");
        ((delta as u64) << 1) ^ ((delta >> 63) as u64)
    }

    /// Recover nominal time without consulting the mutable end of the range.
    pub fn decode_timestamp(&self, offset: u64) -> u64 {
        let delta = ((offset >> 1) as i64) ^ -((offset & 1) as i64);
        (self.timestamp.start as u64)
            .checked_add_signed(delta)
            .expect("timestamp offset exceeds u64")
    }

    /// Count inline metadata, the rare boxed Vec header, and used/spare entries once.
    pub fn compression_ratio(&self) -> CompressionStats {
        let mut stats = CompressionStats {
            overhead: size_of::<Self>(),
            ..Default::default()
        };
        if let Some(refs) = self.span_fields.as_deref() {
            stats.overhead += size_of_val(refs) + refs.len() * size_of::<SpanFieldsRef>();
            stats.overalloc += (refs.capacity() - refs.len()) * size_of::<SpanFieldsRef>();
        }
        stats
    }

    /// The inline descriptor is already charged by its owning block/cache.
    pub fn heap_bytes(&self) -> usize {
        let stats = self.compression_ratio();
        stats.overhead - size_of::<Self>() + stats.overalloc
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
            ring: Vec::new(),
            offset: 0,
            active_descriptor: Descriptor {
                index: 1,
                ..Default::default()
            },
            edit_buffer: vec![0; unit],
        }
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
        // Compact the populated frame and retain its allocated capacity for reuse.
        let frame = ArchivedFrame::new_from(&self.active);

        // mask and first element
        assert_eq!(&frame.mask()[..], &self.active.buffer[..self.unit]);
        assert_eq!(frame.first(), &self.active.buffer[self.unit..2 * self.unit]);

        let length = frame.len();
        // Keep the string buffer until prepare_chunk selects the next one;
        // timestamp range and deferred references start fresh for the new frame.
        let next = Descriptor {
            index: self.active_descriptor.index + length,
            string_buffer: self.active_descriptor.string_buffer,
            ..Default::default()
        };
        self.ring.push(Data {
            data: frame,
            descriptor: std::mem::replace(&mut self.active_descriptor, next),
        });
        self.active.clear();
    }

    /// Initialize the origin after rollover and validate before committing a row.
    /// Return its encoded timestamp alongside the bytes and absolute row index.
    pub fn allocate(&mut self, ts: u64, allow_negative: bool) -> (&mut [u8], usize, u64) {
        self.cycle_if_needed();
        if self.active.length == 0 {
            let ts = usize::try_from(ts).expect("block timestamp exceeds usize");
            self.active_descriptor.timestamp = ts..ts;
        }
        let offset = self.active_descriptor.encode_timestamp(ts, allow_negative);
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
            (index >= block.descriptor.index && index - block.descriptor.index < block.data.len())
                .then_some(&block.descriptor)
        })
    }

    pub fn push(&mut self, data: &[u8], ts: usize) {
        self.allocate(ts as u64, true).0.copy_from_slice(data);
        self.update_mask_after_append();
        self.update_ts_range(ts, None);
    }

    /// Drop the oldest completed chunk by its first row's absolute index.
    /// Reject stale indexes and later/active chunks without changing the cache.
    pub fn drop_chunk(&mut self, index: usize) -> Result<(), DropChunkError> {
        let oldest = self
            .ring
            .first()
            .map_or(self.active_descriptor.index, |chunk| chunk.descriptor.index);
        if index < oldest {
            return Err(DropChunkError::AlreadyDropped);
        }
        if self.ring.is_empty() || index != oldest {
            return Err(DropChunkError::NotOldest);
        }
        self.ring.remove(0);
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
            let Some(data) =
                self.ring.iter_mut().rev().find(|d| {
                    d.descriptor.index <= index && index - d.descriptor.index < d.data.len()
                })
            else {
                return None;
            };

            #[cfg(feature = "bit-compression")]
            let frame = AnyFrame::BitCompact(&mut data.data);
            #[cfg(not(feature = "bit-compression"))]
            let frame = AnyFrame::Compact(&mut data.data);
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
                self.active_descriptor
                    .span_fields
                    .as_deref()
                    .map(Vec::as_slice)
                    .unwrap_or_default()
            } else {
                &[]
            };
        }
        self.ring
            .iter()
            .rev()
            .find(|block| block.descriptor.index <= index)
            .and_then(|block| block.descriptor.span_fields.as_deref())
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Allocate the boxed Vec only on the first link; deduplicated appends reuse capacity.
    pub(crate) fn add_span_fields_ref(&mut self, index: usize, reference: SpanFieldsRef) {
        let refs = if index >= self.active_descriptor.index {
            if index - self.active_descriptor.index >= self.active.length {
                return;
            }
            &mut self.active_descriptor.span_fields
        } else if let Some(block) = self
            .ring
            .iter_mut()
            .rev()
            .find(|b| b.descriptor.index <= index)
        {
            &mut block.descriptor.span_fields
        } else {
            return; // A late close may outlive its evicted start block.
        };
        let refs = refs.get_or_insert_with(|| Box::new(Vec::new()));
        if !refs.contains(&reference) {
            refs.push(reference);
        }
    }

    /// Inline box handles belong to the cache/descriptors; charge their heap exactly once.
    pub fn span_fields_heap_bytes(&self) -> usize {
        std::iter::once(&self.active_descriptor)
            .chain(self.ring.iter().map(|block| &block.descriptor))
            .map(Descriptor::heap_bytes)
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
            .flat_map(|frame| ArchivedFrameIter::new(&frame.data))
            .chain(self.active.iter().into_streaming_iter_ref())
    }

    /// Active frame, descriptor and shared cache/scratch state; no archived traversal.
    pub fn active_compression_ratio(&self) -> CompressionStats {
        self.active.compression_ratio()
            + self.active_descriptor.compression_ratio()
            + CompressionStats {
                overhead: size_of::<Self>() - size_of::<Descriptor>() + self.edit_buffer.len(),
                overalloc: self.edit_buffer.capacity() - self.edit_buffer.len(),
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
        growing.push(&[0; 8], 50);
        assert!(growing.active.capacity() > 1);
        assert_eq!(growing.active_descriptor.timestamp, 100..100);
    }

    #[test]
    fn zigzag_timestamp_offsets_cover_signed_limits_without_truncation() {
        use super::Descriptor;
        let descriptor = Descriptor {
            timestamp: (1usize << 63)..usize::MAX,
            ..Default::default()
        };
        // The full signed offset domain round-trips even at large absolute times.
        for (ts, expected) in [(0, u64::MAX), (1 << 63, 0), (u64::MAX, u64::MAX - 1)] {
            let offset = descriptor.encode_timestamp(ts, true);
            assert_eq!(offset, expected);
            assert_eq!(descriptor.decode_timestamp(offset), ts);
        }
        let zero = Descriptor::default();
        assert!(std::panic::catch_unwind(|| zero.encode_timestamp(u64::MAX, true)).is_err());
    }

    #[test]
    fn span_candidates_deduplicate_move_with_the_block_and_drop_with_it() {
        use super::SpanFieldsRef;
        let mut cache = BlockCache::new(1, 2, 2);
        cache.active_descriptor.string_buffer = 7;
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
        let allocation = cache
            .active_descriptor
            .span_fields
            .as_ref()
            .unwrap()
            .as_ptr();
        cache.add_span_fields_ref(1, first);
        assert_eq!(
            cache
                .active_descriptor
                .span_fields
                .as_ref()
                .unwrap()
                .as_ptr(),
            allocation
        );
        let refs = cache.active_descriptor.span_fields.as_deref().unwrap();
        assert_eq!(
            cache.span_fields_heap_bytes(),
            size_of_val(refs) + refs.capacity() * size_of::<SpanFieldsRef>()
        );
        let header = refs as *const Vec<SpanFieldsRef>;
        cache.add_span_fields_ref(1, second);
        assert_eq!(
            cache.active_descriptor.span_fields.as_deref().unwrap() as *const Vec<SpanFieldsRef>,
            header
        );
        assert_eq!(cache.span_fields_refs(1), &[first, second]);
        // Filling/rolling over the frame transfers ownership without losing candidates.
        cache.push(&[2], 2);
        let descriptor_stats = cache.active_descriptor.compression_ratio();
        cache.push(&[3], 3);
        let archived = &cache.ring[0].descriptor;
        assert_eq!(archived.index, 1);
        assert_eq!(archived.string_buffer, 7);
        assert_eq!(archived.timestamp, 1..2);
        assert_eq!(
            archived.span_fields.as_deref().unwrap() as *const Vec<SpanFieldsRef>,
            header
        );
        // Descriptor ownership changes, but its inline/heap/spare accounting does not.
        let archived_stats = archived.compression_ratio();
        assert_eq!(
            (archived_stats.overhead, archived_stats.overalloc),
            (descriptor_stats.overhead, descriptor_stats.overalloc)
        );
        assert_eq!(cache.active_descriptor.index, 3);
        assert_eq!(cache.active_descriptor.string_buffer, 7);
        assert_eq!(cache.active_descriptor.timestamp, 3..3);
        assert!(cache.active_descriptor.span_fields.is_none());
        assert_eq!(cache.span_fields_refs(2), &[first, second]);
        assert!(cache.span_fields_refs(3).is_empty());
        cache.add_span_fields_ref(2, second);
        assert_eq!(
            cache.span_fields_heap_bytes(),
            size_of::<Vec<SpanFieldsRef>>()
                + cache.ring[0]
                    .descriptor
                    .span_fields
                    .as_ref()
                    .unwrap()
                    .capacity()
                    * size_of::<SpanFieldsRef>()
        );
        cache.drop_chunk(1).unwrap();
        cache.add_span_fields_ref(1, first); // A late close must not attach to another block.
        assert!(cache.span_fields_refs(1).is_empty());
        assert_eq!(cache.span_fields_heap_bytes(), 0);
    }

    #[test]
    fn archived_edits_and_constant_blocks_preserve_the_complete_stream() {
        let mut cache = BlockCache::new(2, 8, 8);
        let mut expected: Vec<_> = (0..35).map(|i| [0xA0 | (i / 8) as u8, 0x55]).collect();
        for row in &expected {
            cache.push(row, 0);
        }
        assert_eq!(cache.ring.len(), 4);
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
