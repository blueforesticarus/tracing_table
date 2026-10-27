//! Generic frame ownership, indexed retention and archive allocation. Metadata
//! is supplied inline by the caller; no tracing policy or schema is retained here.
pub use super::dataframe::ArchivedBlockIter;
#[cfg(feature = "bit-compression")]
pub use super::dataframe::{
    BitCompactFrame as ArchivedFrame, BitCompactFrameIter as ArchivedFrameIter,
};
#[cfg(not(feature = "bit-compression"))]
pub use super::dataframe::{CompactFrame as ArchivedFrame, CompactFrameIter as ArchivedFrameIter};
use super::{
    CompressionStats,
    column::{Layout as ColumnLayout, physical::PhysicalLayout},
    dataframe::{AnyFrame, Frame, Row, RowBuffer},
    util::smallbox::SmallBox,
};
use std::{
    alloc::Layout,
    cmp::{max, min},
    collections::VecDeque,
};
use streaming_iterator::{IntoStreamingIterator, StreamingIterator};

/// Inline metadata can own sparse auxiliary allocations without widening archives.
pub trait BlockMetadata: Default {
    fn heap_bytes(&self) -> usize {
        0
    }
}
impl BlockMetadata for () {}
pub struct BlockStore<M: BlockMetadata = ()> {
    pub(crate) unit: usize,
    /// Shared per-archetype history; no layout ID/width is added to any block.
    pub layouts: smallvec::SmallVec<[PhysicalLayout; 1]>,

    // This is a very simple way to reduce the size of rare or one-off logs.
    // The starter frame is allocated with a smaller amount of space, and once full is reallocated.
    // NOTE: frame size is in rows,
    // as a rule of thumb: frames should be at least as long as they are wide,
    // otherwise the overhead of the mask exceeds the savings when eliding one byte.
    pub(crate) frame_size_max: usize,

    // TODO: more optimistically compact frame, ie. compact before frame is full and unpack as needed.
    pub active: Box<Frame>,

    // Append archives at the back; evict oldest blocks from the front
    pub ring: VecDeque<Data<M>>,

    /// Preserve absolute block references when eviction removes a prefix.
    pub offset: usize,

    /// Metadata moves intact into Data when the active frame is archived.
    pub active_descriptor: Descriptor,

    /// Moves into the archive on rollover, preserving caller ownership.
    pub active_metadata: M,

    // used for get_mut
    pub(crate) edit_buffer: Vec<u8>,
}
pub struct Data<M: BlockMetadata = ()> {
    pub data: SmallBox<ArchivedBlock<M>>,
    pub descriptor: Descriptor,
}

/// Caller metadata and the variable-sized frame share one allocation, aligned for both.
#[repr(C)]
pub struct ArchivedBlock<M: BlockMetadata = ()> {
    pub metadata: M,
    pub frame: ArchivedFrame,
}

impl<M: BlockMetadata> ArchivedBlock<M> {
    pub fn compression_ratio(&self) -> CompressionStats {
        // Count inline metadata, its auxiliary heap, and allocation padding once.
        self.frame.compression_ratio()
            + CompressionStats {
                overhead: size_of::<M>() + self.metadata.heap_bytes(),
                ..Default::default()
            }
            + CompressionStats {
                overhead: SmallBox::<Self>::layout(Layout::for_value(self)).0.size()
                    - size_of_val(&self.frame)
                    - size_of::<M>(),
                ..Default::default()
            }
    }
}

/// Metadata shared by active and archived blocks; row indices remain absolute.
#[derive(Debug, Default)]
pub struct Descriptor {
    /// One-based starting row index within this store.
    pub index: usize,
}
impl Descriptor {
    pub fn compression_ratio(&self) -> CompressionStats {
        CompressionStats {
            overhead: size_of::<Self>(),
            ..Default::default()
        }
    }
}
impl<M: BlockMetadata> Data<M> {
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

impl<M: BlockMetadata> ArchivedBlock<M> {
    pub fn new(frame: &Frame, metadata: M) -> SmallBox<Self> {
        Self::new_with_trailer(frame, metadata, &[])
    }

    /// Pack an already encoded frame with its caller-supplied trailer. Rollover
    /// and edit rebuilds run column fitting before this allocation; previously encoded
    /// physical words must not be encoded again here.
    pub fn new_with_trailer(frame: &Frame, metadata: M, trailer: &[u8]) -> SmallBox<Self> {
        // SAFETY: ArchivedBlock is repr(C), with metadata followed by its DST frame.
        unsafe {
            SmallBox::with_header(
                metadata,
                ArchivedFrame::initializer_with_trailer(frame, trailer),
            )
        }
    }

    pub fn column_metadata<'a, 's>(
        &'a self,
        layout: &'s [crate::data::column::Layout],
    ) -> crate::data::column::Metadata<'a, 's> {
        crate::data::column::Metadata::split(self.frame.trailer(), layout).1
    }

    pub fn column_footer_bytes(&self, layout: &[crate::data::column::Layout]) -> usize {
        self.column_metadata(layout).storage_bytes()
    }

    /// Structural columns retain their existing domain APIs; fields become logical values here.
    pub fn read_row(
        &self,
        index: usize,
        output: &mut [u8],
        layout: &[crate::data::column::Layout],
    ) {
        self.read_row_selected(index, output, layout, true);
    }
    /// Callers editing mutable payloads can retain immutable unsigned words physically.
    pub fn read_row_selected(
        &self,
        index: usize,
        output: &mut [u8],
        layout: &[ColumnLayout],
        unsigned: bool,
    ) {
        self.frame.read_row(index, output);
        self.column_metadata(layout)
            .decode_selected(&self.frame, output, index, unsigned);
    }

    pub fn iter<'a>(
        &'a self,
        layout: &'a [crate::data::column::Layout],
    ) -> ArchivedBlockIter<'a, M> {
        ArchivedBlockIter::new(self, layout)
    }
}

impl<M: BlockMetadata> BlockStore<M> {
    pub fn new(unit: usize, start_cap: usize, max_cap: usize) -> Self {
        Self {
            unit,
            frame_size_max: max_cap,
            active: Frame::new(unit, start_cap),
            layouts: smallvec::SmallVec::new(),
            ring: VecDeque::new(),
            offset: 0,
            active_descriptor: Descriptor { index: 1 },
            active_metadata: M::default(),
            edit_buffer: vec![0; unit],
        }
    }
    /// Grow a full starter frame; the owner chooses when/how to archive at its cap.
    pub fn grow_if_needed(&mut self) -> bool {
        if self.active.length != self.active.capacity() {
            return false;
        }
        if self.active.capacity() >= self.frame_size_max {
            return true;
        }
        let mut frame = Frame::new(
            self.active.unit,
            max(min(self.active.capacity() * 2, self.frame_size_max), 8),
        );
        frame.buffer[..self.active.buffer.len()].copy_from_slice(&self.active.buffer);
        frame.length = self.active.length;
        self.active = frame;
        false
    }
    /// Allocate uninterpreted bytes; basis selection and row headers belong to callers.
    pub fn allocate_row(&mut self) -> (&mut [u8], usize) {
        let (row, index) = self
            .active
            .allocate()
            .expect("prepare chunk before allocating");
        (row, index + self.active_descriptor.index)
    }
    pub fn configure_layout(&mut self, layout: PhysicalLayout, max_cap: usize) {
        assert!(self.ring.is_empty() && self.active.length == 0 && self.layouts.is_empty());
        self.unit = layout.unit();
        self.active = Frame::new(layout.unit(), self.active.capacity());
        self.frame_size_max = max_cap;
        self.edit_buffer.resize(layout.logical_unit(), 0);
        self.layouts.push(layout);
    }

    pub fn layout_for_block(&self, index: usize) -> Option<&PhysicalLayout> {
        self.layouts
            .iter()
            .rev()
            .find(|layout| layout.block_index <= index)
    }

    pub fn columns_for_block<'a>(
        &'a self,
        index: usize,
        fallback: &'a [ColumnLayout],
    ) -> &'a [ColumnLayout] {
        self.layout_for_block(index)
            .map_or(fallback, |layout| layout.columns.as_slice())
    }

    pub fn layouts_heap_bytes(&self) -> usize {
        usize::from(self.layouts.spilled()) * self.layouts.capacity() * size_of::<PhysicalLayout>()
            + self
                .layouts
                .iter()
                .map(PhysicalLayout::heap_bytes)
                .sum::<usize>()
    }

    /// Descriptors form a sorted retained suffix. Recent edits take the fast
    /// last-block path; older random reads use binary search without an index table.
    pub fn archived_at(&self, index: usize) -> Option<(usize, &Data<M>)> {
        let last = self.ring.back()?;
        let ordinal = if index >= last.descriptor.index {
            self.ring.len() - 1
        } else {
            self.ring
                .partition_point(|b| b.descriptor.index <= index)
                .checked_sub(1)?
        };
        let block = &self.ring[ordinal];
        (index >= block.descriptor.index && index - block.descriptor.index < block.data.frame.len())
            .then_some((self.offset + ordinal, block))
    }

    pub fn descriptor(&self, index: usize) -> Option<&Descriptor> {
        if index >= self.active_descriptor.index {
            return (index - self.active_descriptor.index < self.active.length)
                .then_some(&self.active_descriptor);
        }
        self.archived_at(index).map(|(_, block)| &block.descriptor)
    }

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

    pub fn get_mut<'a>(
        &'a mut self,
        index: usize,
        columns: &'a [ColumnLayout],
    ) -> Option<(&'a Descriptor, Row<'a, M>)> {
        self.get_mut_inner(index, columns, true)
    }

    pub fn get_mut_physical<'a>(
        &'a mut self,
        index: usize,
        columns: &'a [ColumnLayout],
    ) -> Option<(&'a Descriptor, Row<'a, M>)> {
        self.get_mut_inner(index, columns, false)
    }

    fn get_mut_inner<'a>(
        &'a mut self,
        index: usize,
        columns: &'a [ColumnLayout],
        logical: bool,
    ) -> Option<(&'a Descriptor, Row<'a, M>)> {
        if index >= self.active_descriptor.index {
            if index - self.active_descriptor.index >= self.active.length {
                return None;
            }
            let row = Row {
                init: false, // we aren't copying the data, just providing the buffer
                buffer: RowBuffer::Borrowed(&mut self.edit_buffer),
                index: index - self.active_descriptor.index,
                frame: crate::data::dataframe::AnyFrame::Frame(&mut self.active),
                projection: if logical { self.layouts.last() } else { None },
            };
            Some((&self.active_descriptor, row))
        } else {
            // Late closes most often target recent blocks; retain the reverse
            // search while also recovering the absolute layout boundary.
            let ordinal = self.archived_at(index)?.0 - self.offset;
            let projection = self
                .layouts
                .iter()
                .rev()
                .find(|p| p.block_index <= self.offset + ordinal);
            let columns = projection.map_or(columns, |p| p.columns.as_slice());
            let data = &mut self.ring[ordinal];
            let size = if logical {
                projection.map_or(data.data.frame.unit(), |p| p.logical_unit())
            } else {
                data.data.frame.unit()
            };

            let frame = AnyFrame::Archived(&mut data.data, columns, logical);
            let row = Row {
                init: false, // Decode only if the caller accesses the archived row.
                buffer: RowBuffer::Borrowed(&mut self.edit_buffer[..size]),
                index: index - data.descriptor.index,
                frame,
                projection: if logical { projection } else { None },
            };
            Some((&data.descriptor, row))
        }
    }

    pub fn update_mask_after_append(&mut self) {
        self.active.update_mask(self.active.length - 1, false /*only need to check for newly constant bits when changing data, not when appending. */);
    }

    pub fn iter<'a>(
        &'a self,
        columns: &'a [ColumnLayout],
    ) -> impl StreamingIterator<Item = [u8]> + 'a {
        self.iter_with_origins::<true>(columns)
    }
    /// A domain reader can decode immutable origins once outside the generic
    /// payload loop, using a typed Column instead of repeating codec dispatch.
    pub fn iter_encoded_origins<'a>(
        &'a self,
        columns: &'a [ColumnLayout],
    ) -> impl StreamingIterator<Item = [u8]> + 'a {
        self.iter_with_origins::<false>(columns)
    }
    fn iter_with_origins<'a, const DECODE_ORIGINS: bool>(
        &'a self,
        columns: &'a [ColumnLayout],
    ) -> impl StreamingIterator<Item = [u8]> + 'a {
        self.ring
            .iter()
            .enumerate()
            .into_streaming_iter()
            .flat_map(move |(i, frame)| {
                let projection = self.layout_for_block(self.offset + i);
                let columns = projection.map_or(columns, |p| p.columns.as_slice());
                super::dataframe::ProjectedIter::new(
                    super::dataframe::ArchivedBlockIter::<M, DECODE_ORIGINS>::new(
                        &frame.data,
                        columns,
                    ),
                    projection,
                )
            })
            .chain(self.active_rows())
    }

    pub fn active_rows(&self) -> impl StreamingIterator<Item = [u8]> + '_ {
        super::dataframe::ProjectedIter::new(
            self.active
                .iter()
                .map(|row| &row[..self.unit])
                .into_streaming_iter_ref(),
            self.layouts.last(),
        )
    }

    pub fn compression_parts(&self) -> (CompressionStats, CompressionStats) {
        let archived = self
            .ring
            .iter()
            .map(Data::<M>::compression_ratio)
            .fold(CompressionStats::default(), |a, b| a + b)
            + CompressionStats {
                overalloc: (self.ring.capacity() - self.ring.len()) * size_of::<Data<M>>(),
                ..Default::default()
            };
        (self.active_compression_ratio(), archived)
    }

    pub fn compression_ratio(&self) -> CompressionStats {
        let (active, archived) = self.compression_parts();
        active + archived
    }
    pub fn active_compression_ratio(&self) -> CompressionStats {
        self.active.compression_ratio()
            + self.active_descriptor.compression_ratio()
            + CompressionStats {
                overhead: size_of::<M>() + self.active_metadata.heap_bytes() + size_of::<Self>()
                    - size_of::<Descriptor>()
                    - size_of::<M>()
                    + self.edit_buffer.len()
                    + self.layouts_heap_bytes(),
                overalloc: self.edit_buffer.capacity() - self.edit_buffer.len(),
                ..Default::default()
            }
    }
}

impl<M: BlockMetadata> BlockStore<M> {
    /// Commit a caller-prepared archive; row identity and ownership are generic.
    pub fn push_archive(&mut self, data: SmallBox<ArchivedBlock<M>>) {
        assert_eq!(data.frame.len(), self.active.length);
        let next = Descriptor {
            index: self.active_descriptor.index + self.active.length,
        };
        self.ring.push_back(Data {
            data,
            descriptor: std::mem::replace(&mut self.active_descriptor, next),
        });
        self.active.clear();
    }
    /// The basic owner can archive ordinary columns without any domain policy.
    pub fn archive_active(&mut self, columns: &[ColumnLayout]) {
        if self.active.length == 0 {
            return;
        }
        let columns = self
            .layouts
            .last()
            .map_or(columns, |layout| layout.columns.as_slice());
        let mut trailer = Vec::new();
        super::column::fit(&mut self.active, &mut trailer, columns);
        let metadata = std::mem::take(&mut self.active_metadata);
        let data = ArchivedBlock::new_with_trailer(&self.active, metadata, &trailer);
        self.push_archive(data);
    }
    pub fn promote(
        &mut self,
        logical: &[u8],
        byte_target: usize,
        min_rows: usize,
        extra: usize,
    ) -> bool {
        let Some(old) = self.layouts.last() else {
            return false;
        };
        let Some(mut next) = old.promoted(
            logical,
            self.active_descriptor.index == 1 && self.active.length == 0,
        ) else {
            return false;
        };
        next.block_index = self.offset + self.ring.len();
        let capacity = self.active.capacity();
        let mut converted = Frame::new(next.unit() + extra, capacity);
        let mut row = smallvec::SmallVec::<[u8; 256]>::from_elem(0, old.logical_unit());
        let mut packed = smallvec::SmallVec::<[u8; 256]>::from_elem(0, next.unit() + extra);
        for previous in self.active.iter() {
            old.unpack(previous, &mut row);
            next.pack(&row, &mut packed[..next.unit()]);
            if extra != 0 {
                packed[next.unit()..].copy_from_slice(&previous[self.unit..]);
            }
            converted.push(&packed);
        }
        // Preserve structural edit locks; payload masks are rebuilt at their new positions.
        converted.buffer[..next.header()].copy_from_slice(&self.active.mask()[..next.header()]);
        self.unit = next.unit();
        self.frame_size_max = (byte_target / (next.unit() + extra)).max(min_rows).max(1);
        self.active = converted;
        if self.layouts.last().unwrap().block_index == next.block_index {
            *self.layouts.last_mut().unwrap() = next;
        } else {
            self.layouts.push(next);
        }
        true
    }
}

#[cfg(test)]
mod lookup_tests {
    use super::*;
    #[test]
    fn indexed_lookup_survives_wrapping_eviction_and_active_tails() {
        let mut store = BlockStore::<()>::new(1, 2, 2);
        for chunk in 0..18u8 {
            for value in [chunk, chunk] {
                store.allocate_row().0[0] = value;
                store.update_mask_after_append();
            }
            store.archive_active(&[]);
            if chunk >= 4 {
                let first = store.ring.front().unwrap().descriptor.index;
                store.drop_chunk(first).unwrap();
            }
        }
        assert!(!store.ring.as_slices().1.is_empty());
        for (ordinal, block) in store.ring.iter().enumerate() {
            for index in block.descriptor.index..block.descriptor.index + block.data.frame.len() {
                let (found, actual) = store.archived_at(index).unwrap();
                assert_eq!(found, store.offset + ordinal);
                assert!(std::ptr::eq(actual, block));
            }
        }
        assert!(store.archived_at(0).is_none());
        assert!(
            store
                .archived_at(store.ring.front().unwrap().descriptor.index - 1)
                .is_none()
        );
        assert!(store.archived_at(store.active_descriptor.index).is_none());
    }
}
