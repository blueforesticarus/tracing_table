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

    // starting index of active frame
    pub count: usize,

    /// BufferCollection ID used by string offsets in the active chunk.
    pub string_buffer: usize,

    // timestamp
    timestamp: Range<usize>,

    // used for get_mut
    edit_buffer: Vec<u8>,
}

pub struct Data {
    pub data: Box<ArchivedFrame>,
    pub index: usize,
    /// BufferCollection ID shared by every string field in this chunk.
    pub string_buffer: usize,
    pub(crate) timestamp: Range<usize>,
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
            timestamp: Range { start: 0, end: 0 },
            count: 1,
            string_buffer: 0,
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
                // active frame has reached max size, compact put in ring buffer.
                // active frame cleared, retains allocated capacity
                let frame = ArchivedFrame::new_from(&self.active);

                // mask and first element
                assert_eq!(&frame.mask()[..], &self.active.buffer[..self.unit]);
                assert_eq!(frame.first(), &self.active.buffer[self.unit..2 * self.unit]);

                let length = frame.len();
                self.ring.push(Data {
                    data: frame,
                    timestamp: self.timestamp.clone(),
                    index: self.count,
                    string_buffer: self.string_buffer,
                });
                self.count += length;
                self.timestamp = Range { start: 0, end: 0 };
                self.active.clear();
            }
        }
    }

    pub fn allocate(&mut self) -> (&mut [u8], usize) {
        self.cycle_if_needed();
        let (buffer, index) = self.active.allocate().unwrap();
        (buffer, index + self.count)
    }

    pub fn push(&mut self, data: &[u8], ts: usize) {
        self.allocate().0.copy_from_slice(data);
        self.update_mask_after_append();
        self.update_ts_range(ts, None);
    }

    /// Drop the oldest completed chunk by its first row's absolute index.
    /// Reject stale indexes and later/active chunks without changing the cache.
    pub fn drop_chunk(&mut self, index: usize) -> Result<(), DropChunkError> {
        let oldest = self.ring.first().map_or(self.count, |chunk| chunk.index);
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

    pub fn get_mut<'a>(&'a mut self, index: usize) -> Option<Row<'a>> {
        if index >= self.count {
            Row {
                init: false, // we aren't copying the data, just providing the buffer
                buffer: RowBuffer::Owned(Box::default()),
                index: index - self.count,
                frame: crate::data::dataframe::AnyFrame::Frame(&mut self.active),
            }
            .into()
        } else {
            let Some(data) = self.ring.iter_mut().rev().find(|d| d.index <= index) else {
                return None;
            };

            #[cfg(feature = "bit-compression")]
            let frame = AnyFrame::BitCompact(&mut data.data);
            #[cfg(not(feature = "bit-compression"))]
            let frame = AnyFrame::Compact(&mut data.data);
            Row {
                init: false, // Decode only if the caller accesses the archived row.
                buffer: RowBuffer::Borrowed(self.edit_buffer.as_mut_slice()),
                index: index - data.index,
                frame,
            }
            .into()
        }
    }

    /// NOTE: could simplify by computing mask when compacting. Pretty sure it's the same amount of processing.
    pub fn update_mask_after_append(&mut self) {
        self.active.update_mask(self.active.length - 1, false /*only need to check for newly constant bits when changing data, not when appending. */);
    }

    // track earliest and latest ts for block
    // called when a span closes
    pub fn update_ts_range(&mut self, ts: usize, index: Option<usize>) {
        let index = index.unwrap_or(self.count + self.active.length);

        let timestamp = if index >= self.count {
            &mut self.timestamp
        } else {
            &mut self
                .ring
                .iter_mut()
                .rev()
                .find(|d| d.index <= index)
                .unwrap()
                .timestamp
        };

        if timestamp.start == 0 || timestamp.start > ts {
            timestamp.start = ts;
        }
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

    /// Split resident storage by lifetime; shared cache state belongs to active storage.
    pub fn compression_parts(&self) -> (CompressionStats, CompressionStats) {
        let active = self.active.compression_ratio()
            + CompressionStats {
                overhead: size_of::<Self>() + self.edit_buffer.len(),
                overalloc: self.edit_buffer.capacity() - self.edit_buffer.len(),
                ..Default::default()
            };
        let archived = self
            .ring
            .iter()
            .map(|v| {
                v.data.compression_ratio()
                    + CompressionStats {
                        overhead: size_of_val(v),
                        ..Default::default()
                    }
            })
            .fold(CompressionStats::default(), |a, b| a + b)
            + CompressionStats {
                overalloc: (self.ring.capacity() - self.ring.len()) * size_of::<Data>(),
                ..Default::default()
            };
        (active, archived)
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
            cache.get_mut(index).unwrap().get_mut()[0] ^= bit;
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
