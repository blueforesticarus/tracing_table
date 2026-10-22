//! Chunk-local string storage. Strings are deduplicated within each writable buffer;
//! frozen buffers retain only LZ4 data and, optionally, an index. Templates are deferred.
use std::{borrow::Cow, collections::VecDeque, hash::BuildHasher, ops::Deref};

use foldhash::fast::RandomState;
use hashbrown::{HashTable, hash_table::Entry};

use super::{CompressionStats, dyntuple::ValueData};

/// Choose a string reference once per callsite: u32 offsets or u16 indexes in raw rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum StringStorage {
    Offset,
    // Prefer narrower references; byte offsets remain an explicit option.
    #[default]
    Index,
}
impl StringStorage {
    // Share the packed reference width with template row sizing and consumers.
    pub fn unit(self) -> usize {
        match self {
            Self::Offset => size_of::<u32>(),
            Self::Index => size_of::<u16>(),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Offset => "offset",
            Self::Index => "index",
        }
    }

    pub(crate) fn intern(self, buffer: &mut Buffer, text: &str) -> ValueData {
        let (index, offset) = buffer.add_string(text);
        match self {
            Self::Offset => {
                ValueData::BufferOffset(u32::try_from(offset).expect("string offset exceeds u32"))
            }
            Self::Index => {
                ValueData::BufferIndex(u16::try_from(index).expect("string index exceeds u16"))
            }
        }
    }

    pub(crate) fn read(self, buffer: &ExpandedBuffer, reference: u32) -> &str {
        match self {
            Self::Offset => buffer.get_offset(reference as usize),
            Self::Index => buffer.get_index(reference as usize),
        }
        .expect("stored string reference")
    }
}

// Bits 0..16: hash, 16..36: offset, 36..50: length, 50..64: index.
// Keep packing here so all insertions use the same checked layout.
#[derive(Clone, Debug)]
struct StringEntry(u64);

impl StringEntry {
    fn new(hash: u16, offset: usize, length: usize, index: usize) -> Self {
        assert!(offset <= Buffer::MAX_OFFSET, "buffer exceeds 20-bit offset");
        assert!(
            length <= Buffer::MAX_STRING_LENGTH,
            "string exceeds 14-bit length"
        );
        assert!(
            index < Buffer::MAX_STRINGS,
            "buffer exceeds 14-bit string index"
        );
        Self(
            u64::from(hash)
                | ((offset as u64) << 16)
                | ((length as u64) << 36)
                | ((index as u64) << 50),
        )
    }
    fn hash(&self) -> u16 {
        self.0 as u16
    }
    fn offset(&self) -> usize {
        ((self.0 >> 16) & 0xF_FFFF) as usize
    }
    fn length(&self) -> usize {
        ((self.0 >> 36) & 0x3FFF) as usize
    }
    fn index(&self) -> usize {
        (self.0 >> 50) as usize
    }
}

// Spread the cached 16 bits across the table's bucket and fingerprint bits.
// Lookups and table growth must use the same conversion.
fn table_hash(hash: u16) -> u64 {
    u64::from(hash).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

// TODO: template storage
// 1. we would like to support template storage in this file. as an option (opposed to archetyping on template+args)
//    this gives nice flexibility since readers will just see a string, and not care that it is stored as template + args
// 2. there is no point in doing this if the template is stored in the buffer. since LZ4 compression will do a good-enough job compressing in that case.
//    however if the template is a static string we can avoid storing it here at all. meaning we don't have to store it per block
//
// Design:
//  - the buffer should store the template by setting the MSB of the length to 1 to flag a template, the next 3 bytes are flags and then 4bits identify the number of args.
//  - the first flag indicates whether the template is stored as a usize pointer to a static string, or a provided u16 index in an external template buffer
//  - the second flag indicates whether the values are offsets (u32) or indices (u16)
//  - the third flag is unused
//  - this make that first byte sufficient to identify the length of the descriptor.
//  - the index stores the offset of the first byte same as a non-template string. This means that index addressed references could survive reprocessing the whole block for templates.
//  - template strings are dedupped same as regular strings. hash the bytes making up the descriptor described above.. hash the bytes making up the descriptor described above.
//
//  FUTURE WORK:
//  - store template descriptors together in index and buffer to improve compaction of references in message chunk.
//    could use negative offset/indices for templates.

/// An append-only cache with stable insertion indexes and byte offsets.
/// Ordinary strings use packed 8-byte lookup entries. Inputs outside those packed
/// bounds use a linear overflow lookup without adding metadata to every buffer.
#[derive(Clone, Debug, Default)]
pub struct Buffer {
    storage: ExpandedBuffer,
    // Equality checks the single stored copy; hashes remain available when the table grows.
    lookup: HashTable<StringEntry>,
    hash_builder: RandomState,
}

/// Immutable expanded strings, with index and offset reads but no dedup metadata.
#[derive(Clone, Debug, Default)]
pub struct ExpandedBuffer {
    // A u16 length precedes UTF-8. u16::MAX escapes to a following u32 length,
    // allowing large shader diagnostics without widening ordinary string headers.
    data: Vec<u8>,
    index: Vec<u32>,
}

// Share the read API and borrow active storage without copying or exposing mutation.
impl Deref for Buffer {
    type Target = ExpandedBuffer;

    fn deref(&self) -> &ExpandedBuffer {
        &self.storage
    }
}

impl Buffer {
    /// Hard entry limit, independent of the smaller packed lookup bounds below.
    pub const MAX_ENTRIES: usize = 1 << 16;

    /// Fast lookup bounds; oversized entries remain readable through their u32 offsets.
    pub const MAX_STRING_LENGTH: usize = (1 << 14) - 1;
    pub const MAX_STRINGS: usize = 1 << 14;
    pub const MAX_OFFSET: usize = (1 << 20) - 1;

    /// Add a string (deduplicated by default), returning (index, byte offset).
    pub fn add_string(&mut self, text: &str) -> (usize, usize) {
        let length = text.len();
        if length > Self::MAX_STRING_LENGTH {
            return self.add_overflow(text);
        }
        let hash = self.hash_builder.hash_one(text) as u16;
        let entry = self.lookup.entry(
            table_hash(hash),
            |entry| {
                entry.hash() == hash
                    && entry.length() == length
                    && &self.storage.data[entry.offset() + 2..entry.offset() + 2 + length]
                        == text.as_bytes()
            },
            |entry| table_hash(entry.hash()),
        );
        let vacant = match entry {
            Entry::Occupied(entry) => {
                return (entry.get().index(), entry.get().offset());
            }
            Entry::Vacant(entry) => entry,
        };
        let index = self.storage.index.len();
        let offset = self.storage.data.len();
        if offset > Self::MAX_OFFSET || index >= Self::MAX_STRINGS {
            // Release the vacant table entry before borrowing the buffer again.
            drop(vacant);
            return self.add_overflow(text);
        }
        let entry = StringEntry::new(hash, offset, length, index);
        let address = self.storage.append(text);
        vacant.insert(entry);
        address
    }

    // add a string too large for the hash table sizes, does not use hash table.
    fn add_overflow(&mut self, text: &str) -> (usize, usize) {
        if let Some((index, &offset)) = self
            .storage
            .index
            .iter()
            .enumerate()
            .find(|(_, offset)| self.storage.get_offset(**offset as usize) == Some(text))
        {
            return (index, offset as usize);
        }
        self.storage.append(text)
    }

    /// Encoded unique strings (including encoded lengths) are data; the index and
    /// dedup table are overhead. Empty allocated slots are over-allocation.
    pub fn compression_ratio(&self) -> CompressionStats {
        let mut stats = self.storage.compression_ratio();
        let lookup_bytes = self.lookup.allocation_size();
        let lookup_spare = if lookup_bytes == 0 {
            0 // An empty table has a virtual bucket but no allocation.
        } else {
            (self.lookup.num_buckets() - self.lookup.len()) * size_of::<StringEntry>()
        };
        // Storage already counts its inline size; add only the writable buffer's extra bytes.
        stats.overhead +=
            size_of::<Self>() - size_of::<ExpandedBuffer>() + lookup_bytes - lookup_spare;
        stats.overalloc += lookup_spare;
        stats
    }

    /// Consume the writable cache, discarding its dedup lookup.
    pub fn compress(self, keep_index: bool) -> CompressedBuffer {
        self.compress_with_level(keep_index, None)
    }

    /// Keep the size-prefixed block format identical across flex and native HC.
    pub fn compress_with_level(self, keep_index: bool, level: Option<u32>) -> CompressedBuffer {
        fn compress(bytes: &[u8], level: Option<u32>) -> Box<[u8]> {
            // Native HC expects an addressable source; empty Vecs have a dangling pointer.
            if bytes.is_empty() {
                return lz4_flex::compress_prepend_size(bytes).into_boxed_slice();
            }
            match level {
                None => lz4_flex::compress_prepend_size(bytes),
                Some(level) => {
                    assert!((1..=12).contains(&level), "LZ4 HC level must be 1..=12");
                    lz4::block::compress(
                        bytes,
                        Some(lz4::block::CompressionMode::HIGHCOMPRESSION(level as i32)),
                        true,
                    )
                    .expect("string buffer LZ4 compression")
                }
            }
            .into_boxed_slice()
        }
        // TODO compressing index is probably pointless
        let index = keep_index.then(|| {
            let bytes: Vec<u8> = self.index.iter().flat_map(|v| v.to_le_bytes()).collect();
            compress(&bytes, level)
        });
        CompressedBuffer {
            data: compress(&self.data, level),
            index,
        }
    }
}

impl ExpandedBuffer {
    // Keep writing and scanning the escaped length format in this module only.
    fn append(&mut self, text: &str) -> (usize, usize) {
        // Owners must rotate before appending an entry that cannot have a u16 index.
        assert!(
            self.index.len() < Buffer::MAX_ENTRIES,
            "string buffer is full"
        );
        let offset = self.data.len();
        let length = u32::try_from(text.len()).expect("string exceeds u32 length");
        let prefix = if length < u16::MAX as u32 { 2 } else { 6 };
        assert!(
            offset
                .checked_add(prefix + text.len())
                .is_some_and(|end| end <= u32::MAX as usize),
            "string buffer exceeds u32 offsets"
        );
        if prefix == 2 {
            self.data.extend_from_slice(&(length as u16).to_le_bytes());
        } else {
            self.data.extend_from_slice(&u16::MAX.to_le_bytes());
            self.data.extend_from_slice(&length.to_le_bytes());
        }
        self.data.extend_from_slice(text.as_bytes());
        let index = self.index.len();
        self.index.push(offset as u32);
        (index, offset)
    }

    fn entry_range(&self, offset: usize) -> Option<std::ops::Range<usize>> {
        let mut start = offset.checked_add(2)?;
        let short = u16::from_le_bytes(self.data.get(offset..start)?.try_into().ok()?);
        let length = if short == u16::MAX {
            let end = start.checked_add(4)?;
            let length = u32::from_le_bytes(self.data.get(start..end)?.try_into().ok()?) as usize;
            start = end;
            length
        } else {
            usize::from(short)
        };
        Some(start..start.checked_add(length)?)
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Uncompressed storage bytes, including entry lengths (used for size targets).
    pub fn byte_len(&self) -> usize {
        self.data.len()
    }

    pub fn compression_ratio(&self) -> CompressionStats {
        CompressionStats {
            data: self.data.len(),
            compressed: self.data.len(),
            overhead: size_of::<Self>() + self.index.len() * size_of::<u32>(),
            overalloc: self.data.capacity() - self.data.len()
                + (self.index.capacity() - self.index.len()) * size_of::<u32>(),
        }
    }

    pub fn get_index(&self, index: usize) -> Option<&str> {
        self.get_offset(*self.index.get(index)? as usize)
    }

    /// Resolve an entry offset previously returned by `Buffer::add_string`.
    pub fn get_offset(&self, offset: usize) -> Option<&str> {
        std::str::from_utf8(self.data.get(self.entry_range(offset)?)?).ok()
    }
}

/// LZ4-compressed storage. The optional index is compressed separately.
#[derive(Debug)]
pub struct CompressedBuffer {
    data: Box<[u8]>,
    index: Option<Box<[u8]>>,
}

impl CompressedBuffer {
    pub fn has_index(&self) -> bool {
        self.index.is_some()
    }

    pub fn byte_len(&self) -> usize {
        self.data.len() + self.index.as_ref().map_or(0, |index| index.len())
    }

    /// Count the LZ4 size prefix and optional compressed index as overhead.
    /// The original encoded data size is available without decompressing.
    pub fn compression_ratio(&self) -> CompressionStats {
        let (data, compressed) =
            lz4_flex::block::uncompressed_size(&self.data).expect("stored LZ4 size prefix");
        CompressionStats {
            data,
            compressed: compressed.len(),
            overhead: size_of::<Self>()
                + size_of::<u32>()
                + self.index.as_ref().map_or(0, |index| index.len()),
            overalloc: 0,
        }
    }

    /// Expand immutable storage without rebuilding hashes or the dedup table.
    /// Omitted indexes are rebuilt by scanning entry lengths alone.
    pub fn decompress(&self) -> Result<ExpandedBuffer, lz4_flex::block::DecompressError> {
        let mut buffer = ExpandedBuffer {
            data: lz4_flex::decompress_size_prepended(&self.data)?,
            index: Vec::new(),
        };
        if let Some(index) = &self.index {
            buffer.index = lz4_flex::decompress_size_prepended(index)?
                .chunks_exact(size_of::<u32>())
                .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
                .collect();
        } else {
            let mut offset = 0;
            while offset < buffer.data.len() {
                buffer
                    .index
                    .push(u32::try_from(offset).expect("stored u32 offset"));
                offset = buffer
                    .entry_range(offset)
                    .expect("stored string length")
                    .end;
            }
        }
        Ok(buffer)
    }
}

/// Absolute, zero-based ID within one BufferCollection; stable across eviction.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StringBufferId(pub u32);

/// Buffer IDs are absolute indexes: removing old buffers does not renumber survivors.
/// The owner decides when to rotate and when no retained data refers to old buffers.
#[derive(Debug, Default)]
pub struct BufferCollection {
    pub active: Buffer,
    // Eviction inspects stored sizes without expanding the buffers.
    pub(crate) compressed: VecDeque<CompressedBuffer>,
    offset: usize,
}

impl BufferCollection {
    pub fn active_index(&self) -> StringBufferId {
        StringBufferId(
            u32::try_from(self.offset + self.compressed.len())
                .expect("string buffer ID exceeds u32"),
        )
    }

    pub fn offset(&self) -> StringBufferId {
        StringBufferId(self.offset as u32)
    }

    /// Iterate retained frozen IDs without exposing deque-relative indexes.
    pub fn archived_ids(&self) -> impl ExactSizeIterator<Item = StringBufferId> {
        (self.offset as u32..self.active_index().0).map(StringBufferId)
    }

    /// Aggregate all retained buffers and collection storage, counting the
    /// inline active buffer once and unused deque slots as over-allocation.
    /// Collection state and spare deque slots are charged to active storage once.
    pub fn compression_parts(&self) -> (CompressionStats, CompressionStats) {
        let active = self.active.compression_ratio()
            + CompressionStats {
                overhead: size_of::<Self>() - size_of::<Buffer>(),
                overalloc: (self.compressed.capacity() - self.compressed.len())
                    * size_of::<CompressedBuffer>(),
                ..Default::default()
            };
        let archived = self
            .compressed
            .iter()
            .map(CompressedBuffer::compression_ratio)
            .fold(CompressionStats::default(), |a, b| a + b);
        (active, archived)
    }

    pub fn compression_ratio(&self) -> CompressionStats {
        let (active, archived) = self.compression_parts();
        active + archived
    }

    /// Freeze the active buffer and start an empty one, returning its ID.
    pub fn rotate(&mut self, keep_index: bool) -> StringBufferId {
        self.rotate_with_level(keep_index, None)
    }

    /// The recording layer chooses the encoder; buffer IDs and readers stay the same.
    pub fn rotate_with_level(&mut self, keep_index: bool, level: Option<u32>) -> StringBufferId {
        self.compressed
            .push_back(std::mem::take(&mut self.active).compress_with_level(keep_index, level));
        self.active_index()
    }

    /// Decompress once per read batch; the active cache can be borrowed directly.
    pub fn get(&self, index: StringBufferId) -> Option<Cow<'_, ExpandedBuffer>> {
        if index == self.active_index() {
            return Some(Cow::Borrowed(&self.active));
        }
        let Some(buffer) = (index.0 as usize)
            .checked_sub(self.offset)
            .and_then(|i| self.compressed.get(i))
        else {
            return None;
        };
        let decompressed = buffer
            .decompress()
            .expect("compressed data should not be corrupted");
        Some(Cow::Owned(decompressed))
    }

    /// Drop completed buffers before this ID. The active buffer is always retained.
    pub fn discard_before(&mut self, index: StringBufferId) {
        let count = (index.0 as usize)
            .saturating_sub(self.offset)
            .min(self.compressed.len());
        self.compressed.drain(..count);
        self.offset += count;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                while buffer.len() < Buffer::MAX_ENTRIES - 2 {
                    buffer.storage.append("unused");
                }
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
                StringBufferId(0)
            );
            assert_eq!(arch.data.active_metadata.string_buffer, StringBufferId(1));
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

    #[test]
    #[should_panic(expected = "string buffer is full")]
    fn string_buffers_reject_entries_beyond_u16_indexes() {
        let mut buffer = Buffer::default();
        // Exercise the hard append guard independently of recording-layer rotation.
        for _ in 0..=Buffer::MAX_ENTRIES {
            buffer.storage.append("entry");
        }
    }

    #[test]
    fn strings_survive_dedup_and_compression() {
        let mut buffer = Buffer::default();
        // Enough entries to check retained addresses through repeated table growth.
        let texts: Vec<_> = ["", "a\0b", "é🦀", "shared string"]
            .map(str::to_owned)
            .into_iter()
            .chain((0..300).map(|i| format!("entry {i}")))
            .collect();
        let entries: Vec<_> = texts
            .iter()
            .map(|s| (s.as_str(), buffer.add_string(s)))
            .collect();
        let stats = buffer.compression_ratio();
        assert_eq!(
            (stats.data, stats.compressed),
            (buffer.byte_len(), buffer.byte_len())
        );
        // Accounting must match the full allocations, including spare capacity.
        assert_eq!(
            stats.compressed + stats.overhead + stats.overalloc,
            size_of::<Buffer>()
                + buffer.data.capacity()
                + buffer.index.capacity() * size_of::<u32>()
                + buffer.lookup.allocation_size()
        );
        for &(text, address) in &entries {
            assert_eq!(buffer.add_string(text), address);
        }
        assert_eq!(buffer.byte_len(), stats.data);
        // Native HC blocks must remain readable by flex, with and without an index.
        for (keep_index, level) in [None, Some(1), Some(3), Some(9), Some(12)]
            .into_iter()
            .flat_map(|level| [false, true].map(|keep_index| (keep_index, level)))
        {
            let empty = Buffer::default().compress_with_level(keep_index, level);
            assert!(empty.decompress().unwrap().is_empty());
            let compressed = buffer.clone().compress_with_level(keep_index, level);
            assert_eq!(compressed.has_index(), keep_index);
            let stats = compressed.compression_ratio();
            assert_eq!(stats.data, buffer.byte_len());
            assert_eq!(
                stats.compressed + stats.overhead + stats.overalloc,
                size_of::<CompressedBuffer>() + compressed.byte_len()
            );
            let restored = compressed.decompress().unwrap();
            // Immutable expansion preserves both addresses, including empty and UTF-8 strings.
            assert_eq!(restored.len(), entries.len());
            assert_eq!(restored.byte_len(), buffer.byte_len());
            for &(text, address) in &entries {
                assert_eq!(restored.get_index(address.0), Some(text));
                assert_eq!(restored.get_offset(address.1), Some(text));
            }
            assert_eq!(restored.get_index(entries.len()), None);
            assert_eq!(restored.get_offset(restored.byte_len()), None);
            let stats = restored.compression_ratio();
            assert_eq!(
                stats.compressed + stats.overhead + stats.overalloc,
                size_of::<ExpandedBuffer>()
                    + restored.data.capacity()
                    + restored.index.capacity() * size_of::<u32>()
            );
        }
        assert_eq!(buffer.add_string("new").0, entries.len());
    }

    #[test]
    fn packed_lookup_and_large_entries_share_stable_addresses() {
        assert_eq!(size_of::<StringEntry>(), 8);
        let mut buffer = Buffer::default();
        let mut expected = Vec::new();
        // Exercise the packed length bound, the extended prefix boundary, and
        // enough payload to put subsequent entries beyond the packed offset.
        for length in [
            Buffer::MAX_STRING_LENGTH,
            Buffer::MAX_STRING_LENGTH + 1,
            u16::MAX as usize - 1,
            u16::MAX as usize,
            u16::MAX as usize + 1,
            1_100_000,
        ] {
            let text = "x".repeat(length);
            let address = buffer.add_string(&text);
            assert_eq!(buffer.add_string(&text), address);
            expected.push((text, address));
        }
        let short = buffer.add_string("short after large shader");
        assert!(short.1 > Buffer::MAX_OFFSET);
        assert_eq!(buffer.add_string("short after large shader"), short);
        for keep_index in [false, true] {
            let restored = buffer.clone().compress(keep_index).decompress().unwrap();
            for (text, address) in &expected {
                assert_eq!(restored.get_index(address.0), Some(text.as_str()));
                assert_eq!(restored.get_offset(address.1), Some(text.as_str()));
            }
            assert_eq!(
                restored.get_offset(short.1),
                Some("short after large shader")
            );
        }
        let mut many = Buffer::default();
        for i in 0..=Buffer::MAX_STRINGS {
            assert_eq!(many.add_string(&i.to_string()).0, i);
        }
        assert_eq!(many.add_string("16384").0, Buffer::MAX_STRINGS);
    }

    #[test]
    fn deletion_preserves_buffer_ids() {
        let mut buffers = BufferCollection::default();
        assert_eq!(
            buffers.compression_ratio().overhead,
            size_of::<BufferCollection>()
        );
        for text in ["first", "second"] {
            buffers.active.add_string(text);
            buffers.rotate(false);
        }
        // Typed iteration exposes absolute IDs, including after prefix eviction.
        assert_eq!(
            buffers.archived_ids().collect::<Vec<_>>(),
            vec![StringBufferId(0), StringBufferId(1)]
        );
        let before = buffers.compression_ratio();
        let removed = buffers.compressed[0].compression_ratio();
        let freed_bytes = buffers.compressed[0].byte_len();
        buffers.discard_before(StringBufferId(1));
        let after = buffers.compression_ratio();
        assert_eq!(before.data - after.data, removed.data);
        // The removed deque slot becomes spare; only its boxed payload is freed.
        let allocated = |s: CompressionStats| s.compressed + s.overhead + s.overalloc;
        assert_eq!(allocated(before) - allocated(after), freed_bytes);
        assert_eq!(buffers.offset(), StringBufferId(1));
        assert_eq!(
            buffers.archived_ids().collect::<Vec<_>>(),
            vec![StringBufferId(1)]
        );
        assert!(buffers.get(StringBufferId(0)).is_none());
        let retained = buffers.get(StringBufferId(1)).unwrap();
        assert_eq!(retained.get_index(0), Some("second"));
        drop(retained);
        // Even deleting all completed buffers must preserve the active ID.
        buffers.discard_before(StringBufferId(u32::MAX));
        assert_eq!(buffers.archived_ids().len(), 0);
        assert_eq!(buffers.active_index(), StringBufferId(2));
        assert_eq!(buffers.rotate(true), StringBufferId(3));
        assert!(buffers.get(StringBufferId(2)).unwrap().is_empty());
        assert!(matches!(
            buffers.get(StringBufferId(3)),
            Some(Cow::Borrowed(_))
        ));
    }
}
