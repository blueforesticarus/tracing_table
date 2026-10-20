//! Chunk-local string storage. Strings are deduplicated within each writable buffer;
//! frozen buffers retain only LZ4 data and, optionally, an index. Templates are deferred.
use std::{borrow::Cow, collections::VecDeque, hash::BuildHasher, ops::Deref};

use foldhash::fast::RandomState;
use hashbrown::{HashTable, hash_table::Entry};

use super::CompressionStats;

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

/// An append-only cache with stable insertion indexes and byte offsets.
/// Strings are at most 16,383 UTF-8 bytes; buffers hold at most 16,384 strings.
/// Entry offsets fit 20 bits. Limits are checked before appending.
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
    // Each entry is a u16 byte length followed by UTF-8, so offsets need no index.
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
    /// Limits imposed by the lookup entry's packed fields.
    pub const MAX_STRING_LENGTH: usize = (1 << 14) - 1;
    pub const MAX_STRINGS: usize = 1 << 14;
    pub const MAX_OFFSET: usize = (1 << 20) - 1;

    /// Add a string (deduplicated by default), returning (index, byte offset).
    pub fn add_string(&mut self, text: &str) -> (usize, usize) {
        let length = text.len();
        assert!(
            length <= Self::MAX_STRING_LENGTH,
            "string exceeds 14-bit length"
        );
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
        let entry = StringEntry::new(hash, offset, length, index);
        self.storage
            .data
            .extend_from_slice(&(length as u16).to_le_bytes());
        self.storage.data.extend_from_slice(text.as_bytes());
        self.storage.index.push(offset as u32); // Construction checked the narrower packed offset.
        vacant.insert(entry);
        (index, offset)
    }

    /// Encoded unique strings (including u16 lengths) are data; the index and
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
        // TODO compressing index is probably pointless
        let index = keep_index.then(|| {
            let bytes: Vec<u8> = self.index.iter().flat_map(|v| v.to_le_bytes()).collect();
            lz4_flex::compress_prepend_size(&bytes).into_boxed_slice()
        });
        CompressedBuffer {
            data: lz4_flex::compress_prepend_size(&self.data).into_boxed_slice(),
            index,
        }
    }
}

impl ExpandedBuffer {
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
        let start = offset.checked_add(2)?;
        let length = u16::from_le_bytes(self.data.get(offset..start)?.try_into().ok()?) as usize;
        std::str::from_utf8(self.data.get(start..start.checked_add(length)?)?).ok()
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
                let length =
                    u16::from_le_bytes(buffer.data[offset..offset + 2].try_into().unwrap());
                offset += 2 + usize::from(length);
            }
        }
        Ok(buffer)
    }
}

/// Buffer IDs are absolute indexes: removing old buffers does not renumber survivors.
/// The owner decides when to rotate and when no retained data refers to old buffers.
#[derive(Debug, Default)]
pub struct BufferCollection {
    pub active: Buffer,
    compressed: VecDeque<CompressedBuffer>,
    offset: usize,
}

impl BufferCollection {
    pub fn active_index(&self) -> usize {
        self.offset + self.compressed.len()
    }

    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Aggregate all retained buffers and collection storage, counting the
    /// inline active buffer once and unused deque slots as over-allocation.
    pub fn compression_ratio(&self) -> CompressionStats {
        self.compressed
            .iter()
            .map(CompressedBuffer::compression_ratio)
            .fold(self.active.compression_ratio(), |total, stats| {
                total + stats
            })
            + CompressionStats {
                overhead: size_of::<Self>() - size_of::<Buffer>(),
                overalloc: (self.compressed.capacity() - self.compressed.len())
                    * size_of::<CompressedBuffer>(),
                ..Default::default()
            }
    }

    /// Freeze the active buffer and start an empty one, returning its ID.
    pub fn rotate(&mut self, keep_index: bool) -> usize {
        self.compressed
            .push_back(std::mem::take(&mut self.active).compress(keep_index));
        self.active_index()
    }

    /// Decompress once per read batch; the active cache can be borrowed directly.
    pub fn get(&self, index: usize) -> Option<Cow<'_, ExpandedBuffer>> {
        if index == self.active_index() {
            return Some(Cow::Borrowed(&self.active));
        }
        let Some(buffer) = index
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
    pub fn discard_before(&mut self, index: usize) {
        let count = index.saturating_sub(self.offset).min(self.compressed.len());
        self.compressed.drain(..count);
        self.offset += count;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        for keep_index in [false, true] {
            let compressed = buffer.clone().compress(keep_index);
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
    fn compact_metadata_limits() {
        use std::panic::{AssertUnwindSafe, catch_unwind};
        assert_eq!(size_of::<StringEntry>(), 8);
        let mut buffer = Buffer::default();
        let longest = "x".repeat(Buffer::MAX_STRING_LENGTH);
        assert_eq!(buffer.add_string(&longest), (0, 0));
        assert_eq!(buffer.byte_len(), longest.len() + 2);
        assert!(catch_unwind(AssertUnwindSafe(|| buffer.add_string(&(longest + "x")))).is_err());
        let mut buffer = Buffer::default();
        for i in 0..Buffer::MAX_STRINGS {
            buffer.add_string(&i.to_string());
        }
        // A full buffer still accepts duplicates; only another unique entry is rejected.
        assert_eq!(buffer.add_string("16383").0, Buffer::MAX_STRINGS - 1);
        assert!(catch_unwind(AssertUnwindSafe(|| buffer.add_string("new"))).is_err());
        assert_eq!(buffer.len(), Buffer::MAX_STRINGS);
        let mut buffer = Buffer::default();
        // Large unique strings reach the offset limit before the index limit.
        for i in 0..64 {
            buffer.add_string(&format!(
                "{i:02}{}",
                "x".repeat(Buffer::MAX_STRING_LENGTH - 2)
            ));
        }
        assert!(buffer.byte_len() > Buffer::MAX_OFFSET);
        assert!(catch_unwind(AssertUnwindSafe(|| buffer.add_string("new"))).is_err());
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
        let before = buffers.compression_ratio();
        let removed = buffers.compressed[0].compression_ratio();
        let freed_bytes = buffers.compressed[0].byte_len();
        buffers.discard_before(1);
        let after = buffers.compression_ratio();
        assert_eq!(before.data - after.data, removed.data);
        // The removed deque slot becomes spare; only its boxed payload is freed.
        let allocated = |s: CompressionStats| s.compressed + s.overhead + s.overalloc;
        assert_eq!(allocated(before) - allocated(after), freed_bytes);
        assert_eq!(buffers.offset(), 1);
        assert!(buffers.get(0).is_none());
        let retained = buffers.get(1).unwrap();
        assert_eq!(retained.get_index(0), Some("second"));
        drop(retained);
        // Even deleting all completed buffers must preserve the active ID.
        buffers.discard_before(usize::MAX);
        assert_eq!(buffers.active_index(), 2);
        assert_eq!(buffers.rotate(true), 3);
        assert!(buffers.get(2).unwrap().is_empty());
        assert!(matches!(buffers.get(3), Some(Cow::Borrowed(_))));
    }
}
