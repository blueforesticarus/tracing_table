//! Chunk-local string storage. Strings are deduplicated within each writable buffer;
//! FSST tables are learned once per archetype, then shared across active/frozen
//! buffers. Frozen payloads additionally use LZ4. Templates are deferred.
use std::{borrow::Cow, collections::VecDeque, hash::BuildHasher, ops::Deref, sync::Arc};

mod fsst;

/// Select training on the entire first buffer at its first nonempty rotation.
/// Keep this as a sentinel so an experimental policy adds no per-buffer bytes.
pub const FSST_TRAIN_FIRST_BUFFER: usize = usize::MAX;

// Tune warm-up without changing the wire layout. Zero retains the legacy UTF-8
// baseline; "first-buffer" defers training until the first buffer is frozen.
pub fn fsst_training_bytes() -> usize {
    // First-buffer training slightly reduced archived payload but increased total storage versus 16 KiB warm-up by 531,123 B for Bevy and 11,180 B for Actix.
    std::env::var("STRING_FSST_TRAIN_BYTES")
        .ok()
        .and_then(|v| {
            if v == "first-buffer" {
                Some(FSST_TRAIN_FIRST_BUFFER)
            } else {
                v.parse().ok()
            }
        })
        .unwrap_or(16 * 1024)
}

use foldhash::fast::RandomState;
use hashbrown::{HashTable, hash_table::Entry};

use super::{CompressionStats, dyntuple::ValueData, util::unalign::Unalign};

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

    pub(crate) fn read(self, buffer: &ExpandedBuffer, reference: u32) -> Cow<'_, str> {
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

/// An append-only cache with stable insertion indexes. Training may move offsets.
/// Ordinary strings use packed 8-byte lookup entries. Inputs outside those packed
/// bounds use a linear overflow lookup without adding metadata to every buffer.
#[derive(Clone, Debug, Default)]
pub struct Buffer {
    storage: ExpandedBuffer,
    // Equality checks the single stored copy; hashes remain available when the table grows.
    lookup: HashTable<StringEntry>,
    hash_builder: RandomState,
    // Standalone scratch buffers default to raw; archetype collections opt in.
    train_bytes: usize,
    training_attempted: bool,
}

/// Immutable expanded strings, with index and offset reads but no dedup metadata.
#[derive(Clone, Debug, Default)]
pub struct ExpandedBuffer {
    // A u16 length precedes UTF-8. u16::MAX escapes to a following u32 length,
    // allowing large shader diagnostics without widening ordinary string headers.
    data: Vec<u8>,
    index: Vec<u32>,
    // Rotation continues to use original framed UTF-8 sizes, so FSST does not
    // silently change message/string buffer boundaries or retention behavior.
    raw_bytes: usize,
    codec: Option<Arc<fsst::Table>>,
}

// Share the read API and borrow active storage without copying or exposing mutation.
impl Deref for Buffer {
    type Target = ExpandedBuffer;

    fn deref(&self) -> &ExpandedBuffer {
        &self.storage
    }
}

impl Buffer {
    pub fn with_fsst_training(train_bytes: usize) -> Self {
        Self {
            train_bytes,
            ..Self::default()
        }
    }

    pub fn has_fsst(&self) -> bool {
        self.storage.codec.is_some()
    }

    // Train on a bounded prefix, then rewrite all entries in insertion order.
    // Rows retain their string indexes; packed dedup entries get new offsets.
    fn maybe_train(&mut self, full_buffer: bool) {
        if self.train_bytes == 0
            || self.training_attempted
            || self.has_fsst()
            || self.storage.is_empty()
            || (!full_buffer && self.storage.raw_bytes < self.train_bytes)
        {
            return;
        }
        self.training_attempted = true;
        let mut samples = Vec::new();
        let mut sampled = 0;
        // Offer every string of the completed buffer in first-buffer mode.
        // Threshold mode retains its bounded prefix; upstream FSST may sample
        // either supplied corpus internally during symbol-table calibration.
        let sample_limit = if full_buffer { usize::MAX } else { 64 * 1024 };
        for &offset in &self.storage.index {
            let range = self
                .storage
                .entry_range(offset as usize)
                .expect("stored entry");
            let bytes = &self.storage.data[range];
            let take = bytes.len().min(sample_limit.saturating_sub(sampled));
            if take > 0 {
                samples.push(&bytes[..take]);
                sampled += take;
            }
            if sampled == sample_limit {
                break;
            }
        }
        let Some(codec) = fsst::Table::train(&samples) else {
            return;
        };
        let mut encoded = ExpandedBuffer {
            codec: Some(codec),
            ..Default::default()
        };
        for index in 0..self.storage.len() {
            encoded.append(&self.storage.get_index(index).expect("stored UTF-8"));
        }
        // Include the complete shared table before accepting a learned model.
        if encoded.data.len() + fsst::Table::allocation_bytes() >= self.storage.data.len() {
            return;
        }
        self.storage = encoded;
        let old: Vec<_> = self.lookup.iter().cloned().collect();
        self.lookup.clear();
        for entry in old {
            let offset = self.storage.index[entry.index()] as usize;
            if offset <= Self::MAX_OFFSET {
                self.lookup.insert_unique(
                    table_hash(entry.hash()),
                    StringEntry::new(entry.hash(), offset, entry.length(), entry.index()),
                    |entry| table_hash(entry.hash()),
                );
            }
        }
    }

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
        // Hash raw text independently of the codec; decode only matching
        // candidates so dedup hits never encode the incoming string.
        let hash = self.hash_builder.hash_one(text) as u16;
        let entry = self.lookup.entry(
            table_hash(hash),
            |entry| {
                entry.hash() == hash
                    && entry.length() == length
                    && self.storage.get_offset(entry.offset()).as_deref() == Some(text)
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
        // Encoding is needed only when inserting a new unique string.
        let address = self.storage.append(text);
        vacant.insert(entry);
        self.maybe_train(false);
        (address.0, self.storage.index[address.0] as usize)
    }

    // add a string too large for the hash table sizes, does not use hash table.
    fn add_overflow(&mut self, text: &str) -> (usize, usize) {
        if let Some((index, &offset)) =
            self.storage.index.iter().enumerate().find(|(_, offset)| {
                self.storage.get_offset(**offset as usize).as_deref() == Some(text)
            })
        {
            return (index, offset as usize);
        }
        let address = self.storage.append(text);
        self.maybe_train(false);
        (address.0, self.storage.index[address.0] as usize)
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
            raw_bytes: self.storage.raw_bytes,
            codec: self.storage.codec.clone(),
        }
    }
}

impl ExpandedBuffer {
    // Keep writing and scanning the escaped length format in this module only.
    fn append(&mut self, text: &str) -> (usize, usize) {
        let encoded = self
            .codec
            .as_ref()
            .map(|codec| codec.encode(text.as_bytes()));
        self.append_bytes(encoded.as_deref().unwrap_or(text.as_bytes()), text.len())
    }

    fn append_bytes(&mut self, bytes: &[u8], raw_length: usize) -> (usize, usize) {
        // Owners must rotate before appending an entry that cannot have a u16 index.
        assert!(
            self.index.len() < Buffer::MAX_ENTRIES,
            "string buffer is full"
        );
        let offset = self.data.len();
        let length = u32::try_from(bytes.len()).expect("string exceeds u32 length");
        let prefix = if length < u16::MAX as u32 { 2 } else { 6 };
        assert!(
            offset
                .checked_add(prefix + bytes.len())
                .is_some_and(|end| end <= u32::MAX as usize),
            "string buffer exceeds u32 offsets"
        );
        if prefix == 2 {
            self.data.extend_from_slice(&(length as u16).to_le_bytes());
        } else {
            self.data.extend_from_slice(&u16::MAX.to_le_bytes());
            self.data.extend_from_slice(&length.to_le_bytes());
        }
        self.data.extend_from_slice(bytes);
        self.raw_bytes += raw_length + if raw_length < u16::MAX as usize { 2 } else { 6 };
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
        self.raw_bytes
    }

    /// Resident entry payload before outer LZ4; FSST codes when trained.
    pub fn stored_byte_len(&self) -> usize {
        self.data.len()
    }

    /// Shared retained table allocation, charged once to the active owner.
    pub fn fsst_table_bytes(&self) -> usize {
        self.codec
            .as_ref()
            .map_or(0, |_| fsst::Table::allocation_bytes())
    }

    /// Physical bytes of one framed entry, without decoding it for accounting.
    pub fn entry_byte_len(&self, index: usize) -> Option<usize> {
        let offset = *self.index.get(index)? as usize;
        self.entry_range(offset).map(|range| range.end - offset)
    }

    /// Pruned archive snapshots must reuse the owning archetype's table rather
    /// than silently rebuilding a UTF-8 representation for the storage report.
    pub fn empty_with_same_encoding(&self) -> Buffer {
        Buffer {
            storage: Self {
                codec: self.codec.clone(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    pub fn compression_ratio(&self) -> CompressionStats {
        CompressionStats {
            data: self.raw_bytes,
            compressed: self.data.len(),
            overhead: size_of::<Self>()
                + self.index.len() * size_of::<u32>()
                + self
                    .codec
                    .as_ref()
                    .map_or(0, |_| fsst::Table::allocation_bytes()),
            overalloc: self.data.capacity() - self.data.len()
                + (self.index.capacity() - self.index.len()) * size_of::<u32>(),
        }
    }

    pub fn get_index(&self, index: usize) -> Option<Cow<'_, str>> {
        self.get_offset(*self.index.get(index)? as usize)
    }

    /// Offsets are valid until FSST training rewrites this buffer. Use indexes
    /// for retained references; decoding never expands unrelated strings.
    pub fn get_offset(&self, offset: usize) -> Option<Cow<'_, str>> {
        let bytes = self.data.get(self.entry_range(offset)?)?;
        match &self.codec {
            Some(codec) => String::from_utf8(codec.decode(bytes)?).ok().map(Cow::Owned),
            None => std::str::from_utf8(bytes).ok().map(Cow::Borrowed),
        }
    }
}

/// LZ4-compressed storage. The optional index is compressed separately.
#[derive(Debug)]
pub struct CompressedBuffer {
    data: Box<[u8]>,
    index: Option<Box<[u8]>>,
    raw_bytes: usize,
    codec: Option<Arc<fsst::Table>>,
}

impl CompressedBuffer {
    pub fn has_index(&self) -> bool {
        self.index.is_some()
    }

    pub fn byte_len(&self) -> usize {
        self.data.len() + self.index.as_ref().map_or(0, |index| index.len())
    }

    /// Count the LZ4 size prefix and optional compressed index as overhead.
    /// The shared FSST allocation is charged once to the owning collection's
    /// active buffer, including when that buffer is empty after rotation.
    pub fn compression_ratio(&self) -> CompressionStats {
        let (_, compressed) =
            lz4_flex::block::uncompressed_size(&self.data).expect("stored LZ4 size prefix");
        CompressionStats {
            data: self.raw_bytes,
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
            raw_bytes: self.raw_bytes,
            codec: self.codec.clone(),
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
pub struct StringBufferId(Unalign<u32, 2>);

impl StringBufferId {
    /// The stored ID needs only two-byte alignment; callers receive an ordinary u32.
    pub const fn new(value: u32) -> Self {
        Self(Unalign::new(value))
    }

    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

// Keep IDs compact without exposing an unaligned u32 reference to consumers.
const _: () = {
    assert!(size_of::<StringBufferId>() == 4);
    assert!(align_of::<StringBufferId>() == 2);
};

/// Buffer IDs are absolute indexes: removing old buffers does not renumber survivors.
/// The owner decides when to rotate and when no retained data refers to old buffers.
#[derive(Debug)]
pub struct BufferCollection {
    pub active: Buffer,
    // Eviction inspects stored sizes without expanding the buffers.
    pub(crate) compressed: VecDeque<CompressedBuffer>,
    offset: usize,
}

impl Default for BufferCollection {
    fn default() -> Self {
        Self::with_fsst_training(fsst_training_bytes())
    }
}

impl BufferCollection {
    pub fn with_fsst_training(train_bytes: usize) -> Self {
        Self {
            active: Buffer::with_fsst_training(train_bytes),
            compressed: VecDeque::new(),
            offset: 0,
        }
    }
    pub fn active_index(&self) -> StringBufferId {
        StringBufferId::new(
            u32::try_from(self.offset + self.compressed.len())
                .expect("string buffer ID exceeds u32"),
        )
    }

    pub fn offset(&self) -> StringBufferId {
        StringBufferId::new(self.offset as u32)
    }

    /// Iterate retained frozen IDs without exposing deque-relative indexes.
    pub fn archived_ids(&self) -> impl ExactSizeIterator<Item = StringBufferId> {
        (self.offset as u32..self.active_index().get()).map(StringBufferId::new)
    }

    /// Inspect retained compressed bytes without decoding or recompressing.
    /// Evicted IDs and the writable active ID have no compressed buffer.
    pub fn get_compressed(&self, index: StringBufferId) -> Option<&CompressedBuffer> {
        self.compressed
            .get((index.get() as usize).checked_sub(self.offset)?)
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
        if self.active.train_bytes == FSST_TRAIN_FIRST_BUFFER {
            // Train before cloning the model into the replacement active buffer;
            // this also re-encodes the complete first buffer before outer LZ4.
            self.active.maybe_train(true);
        }
        let mut next = Buffer::with_fsst_training(self.active.train_bytes);
        next.training_attempted = self.active.training_attempted;
        next.storage.codec = self.active.storage.codec.clone();
        self.compressed.push_back(
            std::mem::replace(&mut self.active, next).compress_with_level(keep_index, level),
        );
        self.active_index()
    }

    /// Decompress once per read batch; the active cache can be borrowed directly.
    pub fn get(&self, index: StringBufferId) -> Option<Cow<'_, ExpandedBuffer>> {
        if index == self.active_index() {
            return Some(Cow::Borrowed(&self.active));
        }
        let buffer = self.get_compressed(index)?;
        let decompressed = buffer
            .decompress()
            .expect("compressed data should not be corrupted");
        Some(Cow::Owned(decompressed))
    }

    /// Drop completed buffers before this ID. The active buffer is always retained.
    pub fn discard_before(&mut self, index: StringBufferId) {
        let count = (index.get() as usize)
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
    fn first_buffer_training_waits_for_rotation_and_rewrites_all_indexes() {
        let mut buffers = BufferCollection::with_fsst_training(FSST_TRAIN_FIRST_BUFFER);
        let texts: Vec<_> = (0..900).map(|index| format!(
            "render pipeline shader module {index:04}: validating resource binding and texture descriptor"
        )).collect();
        for text in &texts {
            buffers.active.add_string(text);
        }
        // Exceed both the normal threshold and its 64 KiB corpus cap without
        // learning early. The first completed buffer is the offered corpus.
        assert!(buffers.active.byte_len() > 64 * 1024);
        assert!(!buffers.active.has_fsst());
        let first = buffers.active_index();
        buffers.rotate_with_level(false, Some(3));
        assert!(buffers.active.has_fsst());
        let saved = buffers.get(first).unwrap();
        for (index, text) in texts.iter().enumerate() {
            assert_eq!(saved.get_index(index).as_deref(), Some(text.as_str()));
        }
        drop(saved);
        let model = buffers.active.storage.codec.clone().unwrap();
        buffers.active.add_string("unseen Unicode é🦀");
        buffers.rotate(true);
        assert!(Arc::ptr_eq(
            &model,
            buffers.active.storage.codec.as_ref().unwrap()
        ));
        assert_eq!(
            buffers
                .get(StringBufferId::new(1))
                .unwrap()
                .get_index(0)
                .as_deref(),
            Some("unseen Unicode é🦀")
        );
    }

    #[test]
    fn learned_fsst_keeps_indexes_and_shares_one_table_across_rotation() {
        let mut buffers = BufferCollection::with_fsst_training(16 * 1024);
        let texts: Vec<_> = (0..600).map(|index| format!(
            "render pipeline shader module {index:04}: validating resource binding and texture descriptor"
        )).collect();
        for (index, text) in texts.iter().enumerate() {
            assert_eq!(buffers.active.add_string(text).0, index);
        }
        assert!(buffers.active.has_fsst());
        assert!(buffers.active.storage.data.len() < buffers.active.byte_len());
        for (index, text) in texts.iter().enumerate() {
            assert_eq!(
                buffers.active.get_index(index).as_deref(),
                Some(text.as_str())
            );
            assert_eq!(buffers.active.add_string(text).0, index);
        }
        let model = buffers.active.storage.codec.clone().unwrap();
        let initial_size = buffers.active.byte_len();
        let first = buffers.active_index();
        buffers.rotate_with_level(false, Some(3));
        assert!(buffers.active.has_fsst());
        assert_eq!(buffers.active.byte_len(), 0);
        let archived = buffers.get_compressed(first).unwrap();
        assert!(Arc::ptr_eq(archived.codec.as_ref().unwrap(), &model));
        assert!(Arc::ptr_eq(
            buffers.active.storage.codec.as_ref().unwrap(),
            &model
        ));
        assert_eq!(archived.compression_ratio().data, initial_size);
        let restored = buffers.get(first).unwrap();
        for (index, text) in texts.iter().enumerate() {
            assert_eq!(restored.get_index(index).as_deref(), Some(text.as_str()));
        }
        drop(restored);
        // New active strings can contain bytes absent from the training sample.
        for text in ["", "a\0b", "Unicode é🦀", "unseen data 123"] {
            let (index, _) = buffers.active.add_string(text);
            assert_eq!(buffers.active.get_index(index).as_deref(), Some(text));
            assert_eq!(buffers.active.add_string(text).0, index);
        }
        let (active, archived_stats) = buffers.compression_parts();
        assert!(active.overhead >= fsst::Table::allocation_bytes());
        assert_eq!(archived_stats.overhead, size_of::<CompressedBuffer>() + 4);
        // Exercise the extended length prefix after compression too: an unseen
        // UTF-8 value can expand past 64 KiB under the frozen symbol table.
        let large = "🦀".repeat(18_000);
        let (large_index, _) = buffers.active.add_string(&large);
        assert_eq!(
            buffers.active.get_index(large_index).as_deref(),
            Some(large.as_str())
        );
        assert_eq!(buffers.active.add_string(&large).0, large_index);
        let second = buffers.active_index();
        buffers.rotate_with_level(true, None);
        assert_eq!(
            buffers
                .get(second)
                .unwrap()
                .get_index(large_index)
                .as_deref(),
            Some(large.as_str())
        );
        buffers.discard_before(buffers.active_index());
        assert!(buffers.active.has_fsst());
        assert_eq!(buffers.archived_ids().len(), 0);
    }

    #[test]
    fn learned_fsst_rejects_tables_that_cost_more_than_they_save() {
        let mut buffer = Buffer::with_fsst_training(1);
        // Very small strings cannot amortize even a tiny learned dictionary.
        buffer.add_string("tiny");
        assert!(!buffer.has_fsst());
        assert_eq!(buffer.get_index(0).as_deref(), Some("tiny"));
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
                assert_eq!(restored.get_index(address.0).as_deref(), Some(text));
                assert_eq!(restored.get_offset(address.1).as_deref(), Some(text));
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
                assert_eq!(
                    restored.get_index(address.0).as_deref(),
                    Some(text.as_str())
                );
                assert_eq!(
                    restored.get_offset(address.1).as_deref(),
                    Some(text.as_str())
                );
            }
            assert_eq!(
                restored.get_offset(short.1).as_deref(),
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
            vec![StringBufferId::new(0), StringBufferId::new(1)]
        );
        let before = buffers.compression_ratio();
        let removed = buffers.compressed[0].compression_ratio();
        let freed_bytes = buffers.compressed[0].byte_len();
        // Absolute IDs work across eviction; reading sizes never expands LZ4.
        assert_eq!(
            buffers
                .get_compressed(StringBufferId::new(0))
                .unwrap()
                .byte_len(),
            freed_bytes
        );
        assert!(buffers.get_compressed(buffers.active_index()).is_none());
        buffers.discard_before(StringBufferId::new(1));
        let after = buffers.compression_ratio();
        assert_eq!(before.data - after.data, removed.data);
        // The removed deque slot becomes spare; only its boxed payload is freed.
        let allocated = |s: CompressionStats| s.compressed + s.overhead + s.overalloc;
        assert_eq!(allocated(before) - allocated(after), freed_bytes);
        assert_eq!(buffers.offset(), StringBufferId::new(1));
        assert_eq!(
            buffers.archived_ids().collect::<Vec<_>>(),
            vec![StringBufferId::new(1)]
        );
        assert!(buffers.get(StringBufferId::new(0)).is_none());
        assert!(buffers.get_compressed(StringBufferId::new(0)).is_none());
        assert!(buffers.get_compressed(StringBufferId::new(1)).is_some());
        let retained = buffers.get(StringBufferId::new(1)).unwrap();
        assert_eq!(retained.get_index(0).as_deref(), Some("second"));
        drop(retained);
        // Even deleting all completed buffers must preserve the active ID.
        buffers.discard_before(StringBufferId::new(u32::MAX));
        assert_eq!(buffers.archived_ids().len(), 0);
        assert_eq!(buffers.active_index(), StringBufferId::new(2));
        assert_eq!(buffers.rotate(true), StringBufferId::new(3));
        assert!(buffers.get(StringBufferId::new(2)).unwrap().is_empty());
        assert!(matches!(
            buffers.get(StringBufferId::new(3)),
            Some(Cow::Borrowed(_))
        ));
    }
}

#[cfg(test)]
impl Buffer {
    /// Boundary fixtures need distinct row slots without exercising the dedup
    /// overflow scan quadratically. This hook is absent from recording builds.
    pub(crate) fn fill_unused_entries(&mut self, count: usize) {
        while self.len() < count {
            self.storage.append("unused");
        }
    }
}
