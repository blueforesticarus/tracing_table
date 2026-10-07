//! Owned snapshots for test tools. Filtering and aggregate caches live outside
//! the recorder, which only knows frame identity, packed lengths, and schemas.
#[path = "retained.rs"]
mod retained;
use std::collections::HashMap;
use streaming_iterator::StreamingIterator;
use tracing::Metadata;
use tracing_table::{
    data::{
        blockbuffer::BlockCache,
        dataframe::{CompactFrameIter, CompressionStats},
        dyntuple::{Atom, ValueData, ValueKind},
    },
    subscriber::{RecordingLayer, SpanId},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArchetypeId {
    pub callsite: u64,
    pub archetype: u32,
}

/// Example-owned cache keys: (archetype, first row index) to packed buffer length.
/// No mutation bookkeeping is added to storage; retained masks only grow.
pub type FrameSizes = HashMap<(ArchetypeId, usize), usize>;

#[derive(Debug, Clone)]
pub struct CompletedFrameUpdate {
    pub id: usize,
    pub packed_len: usize,
    pub stats: CompressionStats,
    /// Range of row-start timestamps; span-update rows contain no timestamp.
    pub duration_ns: Option<u64>,
    /// Logical row count and byte mask, excluding active/uncompressed storage.
    pub rows: usize,
    pub mask: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind {
    Event,
    Span,
    SpanUpdate,
}

#[derive(Debug, Clone)]
pub struct ArchetypeSnapshot {
    pub id: ArchetypeId,
    pub metadata: &'static Metadata<'static>,
    pub kind: RecordKind,
    pub fields: Vec<(String, Option<ValueKind>)>,
    /// Every stored row component, including headers and split string parts.
    pub components: Vec<RowComponent>,
    pub rows: usize,
    /// Includes the active allocation, even before its next rollover.
    pub blocks: usize,
    /// Recorded fields' byte positions within a full row, after its header.
    pub field_ranges: Vec<Option<std::ops::Range<usize>>>,
    pub active: CompressionStats,
    /// Only new/resized completed frames have their masks inspected.
    pub completed_updates: Vec<CompletedFrameUpdate>,
}

/// Schema for the detailed byte table; ranges exactly partition the packed row.
#[derive(Debug, Clone)]
pub struct RowComponent {
    pub name: String,
    pub kind: String,
    pub range: std::ops::Range<usize>,
}

#[derive(Debug, Clone)]
pub struct RecordedRow {
    pub index: usize,
    pub timestamp_ns: Option<u64>,
    pub end_timestamp_ns: Option<u64>,
    pub span: Option<SpanId>,
    pub parent: Option<SpanId>,
    pub follows_from: Option<SpanId>,
    pub fields: Vec<(String, ValueData)>,
}

// Keep all inspection, copying, and presentation APIs in the example package.
pub trait InspectRecording {
    fn inspect(&self, known: &FrameSizes) -> Vec<ArchetypeSnapshot>;
    fn rows(&self, id: ArchetypeId, before: Option<usize>, limit: usize) -> Vec<RecordedRow>;
}
impl InspectRecording for RecordingLayer {
    /// Snapshot under short per-callsite read locks. No caller code runs under
    /// these locks and unchanged completed frames never calculate stats again.
    fn inspect(&self, known: &FrameSizes) -> Vec<ArchetypeSnapshot> {
        let callsites: Vec<_> = self.callsites.read().values().cloned().collect();
        let mut snapshots = Vec::new();
        for callsite in callsites {
            let data = callsite.read();
            for arch in &data.cache {
                let id = ArchetypeId {
                    callsite: data.id,
                    archetype: arch.id,
                };
                let completed_updates = arch
                    .data
                    .completed_frames()
                    .filter(|(frame, packed_len)| known.get(&(id, *frame)) != Some(packed_len))
                    .map(|(frame, packed_len)| {
                        let (rows, mask) = arch.data.completed_mask(frame);
                        CompletedFrameUpdate {
                            id: frame,
                            packed_len,
                            stats: arch.data.completed_stats(frame),
                            duration_ns: if arch.archetype.is_span_end {
                                None
                            } else {
                                arch.data
                                    .completed_duration_ns(frame, self.timestamp_bit_shift)
                            },
                            rows,
                            mask,
                        }
                    })
                    .collect();
                // Packing is unaligned. Derive field ranges from the same row
                // schema used by writing/decoding, rather than guessing offsets.
                let fields_size: usize = arch
                    .archetype
                    .field_types
                    .iter()
                    .flatten()
                    .map(ValueKind::unit)
                    .sum();
                let mut offset = arch.archetype.unit() - fields_size;
                let field_ranges: Vec<_> = arch
                    .archetype
                    .field_types
                    .iter()
                    .map(|kind| {
                        kind.as_ref().map(|kind| {
                            let start = offset;
                            offset += kind.unit();
                            start..offset
                        })
                    })
                    .collect();
                // Mirror writer order so headers and string halves use their own masks.
                let mut components = Vec::new();
                let mut header_offset = 0;
                let mut header = |name: &str, kind: &str, width: usize| {
                    components.push(RowComponent {
                        name: name.into(),
                        kind: kind.into(),
                        range: header_offset..header_offset + width,
                    });
                    header_offset += width;
                };
                if arch.archetype.is_span_end {
                    header("span", "SpanId", size_of::<SpanId>());
                } else {
                    header("ts", "U64", size_of::<u64>());
                    if !arch.archetype.is_event {
                        header("end_ts", "U64", size_of::<u64>());
                    }
                }
                if arch.archetype.follows_from {
                    header("follows_from", "SpanId", size_of::<SpanId>());
                }
                if arch.archetype.parent {
                    header("parent", "SpanId", size_of::<SpanId>());
                }
                for ((field, kind), range) in data
                    .metadata
                    .fields()
                    .iter()
                    .zip(&arch.archetype.field_types)
                    .zip(&field_ranges)
                {
                    let (Some(kind), Some(range)) = (kind, range) else {
                        continue;
                    };
                    let part_kind = match kind {
                        ValueKind::PrefixStr => Some(ValueKind::Str),
                        ValueKind::PrefixStrIndex => Some(ValueKind::StrIndex),
                        _ => None,
                    };
                    if let Some(part_kind) = part_kind {
                        let middle = range.start + part_kind.unit();
                        for (part, range) in [
                            ("prefix", range.start..middle),
                            ("suffix", middle..range.end),
                        ] {
                            components.push(RowComponent {
                                name: format!("{}.{part}", field.name()),
                                kind: format!("{part_kind:?}"),
                                range,
                            });
                        }
                    } else {
                        components.push(RowComponent {
                            name: field.name().into(),
                            kind: format!("{kind:?}"),
                            range: range.clone(),
                        });
                    }
                }
                snapshots.push(ArchetypeSnapshot {
                    id,
                    metadata: data.metadata,
                    kind: if arch.archetype.is_span_end {
                        RecordKind::SpanUpdate
                    } else if arch.archetype.is_event {
                        RecordKind::Event
                    } else {
                        RecordKind::Span
                    },
                    fields: data
                        .metadata
                        .fields()
                        .iter()
                        .zip(&arch.archetype.field_types)
                        .map(|(field, kind)| (field.name().to_owned(), kind.clone()))
                        .collect(),
                    rows: arch.data.len(),
                    blocks: arch.data.block_count(),
                    field_ranges,
                    components,
                    active: arch.data.active_stats(),
                    completed_updates,
                });
            }
        }
        snapshots.sort_by_key(|snapshot| snapshot.id);
        snapshots
    }

    /// Read a bounded newest-first page. `before` is an exclusive row index;
    /// passing the last returned index fetches the next older page.
    fn rows(&self, id: ArchetypeId, before: Option<usize>, limit: usize) -> Vec<RecordedRow> {
        let callsites: Vec<_> = self.callsites.read().values().cloned().collect();
        for callsite in callsites {
            let data = callsite.read();
            if data.id != id.callsite {
                continue;
            }
            let Some(arch) = data.cache.get(id.archetype as usize) else {
                return Vec::new();
            };
            return arch
                .data
                .rows_before(before, limit)
                .into_iter()
                .map(|(index, bytes)| {
                    let mut packer = RowReader {
                        bytes: &bytes,
                        offset: 0,
                    };
                    let mut row = RecordedRow {
                        index,
                        timestamp_ns: None,
                        end_timestamp_ns: None,
                        span: None,
                        parent: None,
                        follows_from: None,
                        fields: Vec::new(),
                    };
                    // Types come from the recorder's own schema. Atom reads clone
                    // references without transferring ownership from the stored row.
                    unsafe {
                        if arch.archetype.is_span_end {
                            row.span = Some(packer.read());
                        } else {
                            let timestamp: u64 = packer.read();
                            row.timestamp_ns = Some(timestamp << self.timestamp_bit_shift);
                            if !arch.archetype.is_event {
                                let timestamp: u64 = packer.read();
                                row.end_timestamp_ns = (timestamp != 0)
                                    .then_some(timestamp << self.timestamp_bit_shift);
                            }
                        }
                        if arch.archetype.follows_from {
                            let span: SpanId = packer.read();
                            row.follows_from = (span.index != 0).then_some(span);
                        }
                        if arch.archetype.parent {
                            row.parent = Some(packer.read());
                        }
                        for (field, kind) in data
                            .metadata
                            .fields()
                            .iter()
                            .zip(&arch.archetype.field_types)
                        {
                            if let Some(kind) = kind {
                                row.fields
                                    .push((field.name().to_owned(), packer.value(kind)));
                            }
                        }
                    }
                    row
                })
                .collect();
        }
        Vec::new()
    }
}

trait InspectBlocks {
    fn completed_frames(&self) -> impl Iterator<Item = (usize, usize)> + '_;
    fn completed_stats(&self, id: usize) -> CompressionStats;
    fn completed_duration_ns(&self, id: usize, shift: u8) -> Option<u64>;
    fn completed_mask(&self, id: usize) -> (usize, Vec<u8>);
    fn block_count(&self) -> usize;
    fn active_stats(&self) -> CompressionStats;
    fn len(&self) -> usize;
    fn rows_before(&self, before: Option<usize>, limit: usize) -> Vec<(usize, Vec<u8>)>;
}
impl InspectBlocks for BlockCache {
    fn completed_frames(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.ring
            .iter()
            .map(|frame| (frame.index, frame.data.buffer.len()))
    }

    fn completed_stats(&self, id: usize) -> CompressionStats {
        let frame = self.ring.iter().find(|frame| frame.index == id).unwrap();
        frame.data.compression_ratio()
            + CompressionStats {
                overhead: size_of_val(frame),
                ..Default::default()
            }
    }

    fn completed_duration_ns(&self, id: usize, shift: u8) -> Option<u64> {
        let frame = self.ring.iter().find(|frame| frame.index == id).unwrap();
        let mut iter = CompactFrameIter::new(&frame.data);
        // Only read the timestamp header; copying/decoding atoms would be costly.
        let timestamps = (0..frame.data.length).filter_map(|_| {
            iter.next()
                .map(|row| u64::from_ne_bytes(row[..8].try_into().unwrap()))
        });
        timestamp_duration_ns(timestamps, shift)
    }

    /// Copy a completed block's mask when requested by inspection.
    fn completed_mask(&self, id: usize) -> (usize, Vec<u8>) {
        let frame = self.ring.iter().find(|frame| frame.index == id).unwrap();
        (frame.data.length, frame.data.mask().to_vec())
    }

    fn block_count(&self) -> usize {
        self.ring.len() + 1
    }

    fn active_stats(&self) -> CompressionStats {
        self.active.compression_ratio()
            + CompressionStats {
                overhead: size_of::<Self>(),
                ..Default::default()
            }
    }

    fn len(&self) -> usize {
        self.count - 1 + self.active.length
    }

    /// Copy a bounded page in newest-first order. No query or entity policy is
    /// imposed here; callers can page backwards until enough matches are found.
    fn rows_before(&self, before: Option<usize>, limit: usize) -> Vec<(usize, Vec<u8>)> {
        let end = before.unwrap_or(self.len() + 1).min(self.len() + 1);
        let start = end.saturating_sub(limit).max(1);
        let mut rows = Vec::new();
        for frame in &self.ring {
            if frame.index >= end || frame.index + frame.data.length <= start {
                continue;
            }
            let mut iter = CompactFrameIter::new(&frame.data);
            let mut index = frame.index;
            // Bound iteration by logical rows, including zero-stride blocks.
            for _ in 0..frame.data.length {
                let Some(row) = iter.next() else {
                    break;
                };
                if index >= start && index < end {
                    rows.push((index, row.to_vec()));
                }
                index += 1;
                if index >= end {
                    break;
                }
            }
        }
        for (offset, row) in self.active.iter().enumerate() {
            let index = self.count + offset;
            if index >= start && index < end {
                rows.push((index, row.to_vec()));
            }
        }
        rows.reverse();
        rows
    }
}

// Keep this subscriber alive while inspecting: it owns historical string atoms.
// Construction uses the existing public tuple wrapper, without library helpers.
pub fn subscriber(recorder: std::sync::Arc<RecordingLayer>) -> impl tracing::Subscriber {
    subscriber_with_control(RecordingControl::new(recorder))
}

// Packed inspection stays in the example. Copy scalars with unaligned reads;
// borrowed atom bytes must never drop their original stored reference.
struct RowReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl RowReader<'_> {
    unsafe fn read<T: Copy>(&mut self) -> T {
        let end = self.offset + size_of::<T>();
        let bytes = &self.bytes[self.offset..end];
        self.offset = end;
        unsafe { bytes.as_ptr().cast::<T>().read_unaligned() }
    }
    unsafe fn atom(&mut self) -> Atom {
        let end = self.offset + size_of::<Atom>();
        let bytes = &self.bytes[self.offset..end];
        self.offset = end;
        let atom =
            std::mem::ManuallyDrop::new(unsafe { bytes.as_ptr().cast::<Atom>().read_unaligned() });
        Atom::clone(&atom)
    }
    unsafe fn value(&mut self, kind: &ValueKind) -> ValueData {
        unsafe {
            match kind {
                ValueKind::F64 => ValueData::F64(self.read()),
                ValueKind::I64 => ValueData::I64(self.read()),
                ValueKind::U64 => ValueData::U64(self.read()),
                ValueKind::I128 => ValueData::I128(self.read()),
                ValueKind::U128 => ValueData::U128(self.read()),
                ValueKind::Bool => ValueData::Bool(self.read()),
                // Preserve indexed handles; presentation dereferences their global Atoms.
                ValueKind::StrIndex => ValueData::StrIndex(self.read()),
                ValueKind::PrefixStrIndex => ValueData::PrefixStrIndex(self.read(), self.read()),
                ValueKind::Str => ValueData::Str(self.atom()),
                ValueKind::PrefixStr => ValueData::PrefixStr(self.atom(), self.atom()),
            }
        }
    }
}

// Shared by the overlay and subscriber so resets cannot race recording callbacks.
pub use retained::RecordingControl;
pub fn subscriber_with_control(
    control: std::sync::Arc<RecordingControl>,
) -> impl tracing::Subscriber {
    use tracing_subscriber::layer::SubscriberExt;
    tracing_subscriber::Registry::default().with(retained::RetainedLayer::new(control))
}

/// Compute a nonnegative envelope, even when concurrent appends reorder times.
/// Subtract before restoring quantized nanoseconds so rounding follows storage.
fn timestamp_duration_ns(timestamps: impl IntoIterator<Item = u64>, shift: u8) -> Option<u64> {
    let mut timestamps = timestamps.into_iter();
    let first = timestamps.next()?;
    let (min, max) = timestamps.fold((first, first), |(min, max), time| {
        (min.min(time), max.max(time))
    });
    Some((max - min) << shift)
}

#[cfg(test)]
mod timing_tests {
    use super::*;

    #[test]
    fn duration_uses_envelope_not_append_order_and_preserves_quantization() {
        assert_eq!(timestamp_duration_ns([], 3), None);
        assert_eq!(timestamp_duration_ns([0], 3), Some(0));
        assert_eq!(timestamp_duration_ns([12, 12], 3), Some(0));
        assert_eq!(timestamp_duration_ns([30, 10, 20], 3), Some(160));
    }

    #[test]
    fn completed_duration_reads_packed_timestamp_headers() {
        let mut blocks = BlockCache::new(8, 2, 2);
        for time in [30u64, 10, 50] {
            // A third row rolls the first two into a completed block.
            blocks.push(&time.to_ne_bytes(), time as usize);
        }
        assert_eq!(blocks.completed_duration_ns(1, 3), Some(160));
    }
}
