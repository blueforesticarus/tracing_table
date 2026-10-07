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
    subscriber::{PackedTimestamp, RecordingLayer, SpanFieldsRef, SpanId},
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
    pub extra_fields: Option<SpanFieldsRef>,
    pub span: Option<SpanId>,
    pub parent: Option<SpanId>,
    pub follows_from: Option<SpanId>,
    pub fields: Vec<(String, ValueData)>,
}

// Keep all inspection, copying, and presentation APIs in the example package.
pub trait InspectRecording {
    fn inspect(&self, known: &FrameSizes) -> Vec<ArchetypeSnapshot>;
    fn rows(&self, id: ArchetypeId, before: Option<usize>, limit: usize) -> Vec<RecordedRow>;
    fn span_fields(
        &self,
        span: SpanId,
        reference: SpanFieldsRef,
    ) -> Option<Vec<(String, ValueData)>>;
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
                    header("ts", "PackedTimestamp", size_of::<PackedTimestamp>());
                    if !arch.archetype.is_event {
                        header("end_ts", "PackedTimestamp", size_of::<PackedTimestamp>());
                        header("extra_archetype", "U32", size_of::<u32>());
                        header("extra_block", "U32", size_of::<u32>());
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
                        extra_fields: None,
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
                            let timestamp: PackedTimestamp = packer.read();
                            row.timestamp_ns = Some(timestamp << self.timestamp_bit_shift);
                            if !arch.archetype.is_event {
                                let timestamp: PackedTimestamp = packer.read();
                                row.end_timestamp_ns = (timestamp != 0)
                                    .then_some(timestamp << self.timestamp_bit_shift);
                                let reference: SpanFieldsRef = packer.read();
                                row.extra_fields = (reference.block != 0).then_some(reference);
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
    fn span_fields(
        &self,
        span: SpanId,
        reference: SpanFieldsRef,
    ) -> Option<Vec<(String, ValueData)>> {
        let block = reference.block.checked_sub(1)? as usize;
        let callsite = self
            .callsites
            .read()
            .values()
            .find(|data| data.read().id == span.callsite as u64)?
            .clone();
        let data = callsite.read();
        let arch = data.cache.get(reference.archetype as usize)?;
        if !arch.archetype.is_span_end {
            return None;
        }
        let decode = |bytes: &[u8]| {
            let mut reader = RowReader { bytes, offset: 0 };
            // Scan only the referenced block and match the original span identity.
            let id: SpanId = unsafe { reader.read() };
            if (id.callsite, id.archetype, id.index) != (span.callsite, span.archetype, span.index)
            {
                return None;
            }
            let fields = data
                .metadata
                .fields()
                .iter()
                .zip(&arch.archetype.field_types)
                .filter_map(|(field, kind)| {
                    kind.as_ref()
                        .map(|kind| (field.name().to_owned(), unsafe { reader.value(kind) }))
                })
                .collect();
            Some(fields)
        };
        if let Some(frame) = arch.data.ring.get(block) {
            let mut rows = CompactFrameIter::new(&frame.data);
            for _ in 0..frame.data.length {
                if let Some(fields) = rows.next().and_then(&decode) {
                    return Some(fields);
                }
            }
        } else if block == arch.data.ring.len() {
            return arch.data.active.iter().find_map(decode);
        }
        None
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
            iter.next().map(|row| {
                PackedTimestamp::from_ne_bytes(
                    row[..size_of::<PackedTimestamp>()].try_into().unwrap(),
                )
            })
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

#[cfg(test)]
mod span_fields_tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn span_links_resolve_after_start_and_update_blocks_roll_over() {
        let recorder = Arc::new(RecordingLayer::new());
        let _guard = tracing::subscriber::set_default(subscriber(recorder.clone()));
        // Keep starts open until several blocks have compressed, then close in
        // creation order to exercise row zero and interior edits in old blocks.
        let spans: Vec<_> = (0..600u64)
            .map(|initial| {
                tracing::info_span!(
                    "linked_fields",
                    initial,
                    extra = tracing::field::Empty,
                    optional = tracing::field::Empty
                )
            })
            .collect();
        let start = recorder
            .inspect(&Default::default())
            .into_iter()
            .find(|snapshot| snapshot.kind == RecordKind::Span)
            .unwrap();
        assert!(start.blocks > 2);
        assert!(
            recorder
                .rows(start.id, None, 600)
                .iter()
                .all(|row| row.extra_fields.is_none())
        );
        for (initial, span) in spans.into_iter().enumerate() {
            if initial % 3 != 0 {
                span.record("extra", initial as u64 + 1000);
                if initial % 3 == 2 {
                    span.record("optional", initial as u64 + 2000);
                }
            }
            drop(span);
        }
        let rows = recorder.rows(start.id, None, 600);
        assert_eq!(rows.len(), 600);
        let mut references = Vec::new();
        for row in rows {
            let initial = row
                .fields
                .iter()
                .find_map(|(name, value)| match value {
                    ValueData::U64(value) if name == "initial" => Some(*value),
                    _ => None,
                })
                .unwrap();
            assert_eq!(row.index as u64, initial + 1);
            assert!(row.end_timestamp_ns.unwrap() >= row.timestamp_ns.unwrap());
            if initial % 3 == 0 {
                assert!(row.extra_fields.is_none());
                continue;
            }
            let reference = row.extra_fields.unwrap();
            let span = SpanId {
                callsite: start.id.callsite as u32,
                archetype: start.id.archetype,
                index: row.index as u64,
            };
            let fields = recorder.span_fields(span, reference).unwrap();
            assert!(fields.iter().any(|(name, value)| name == "extra"
                && matches!(value, ValueData::U64(value) if *value == initial + 1000)));
            assert_eq!(fields.len(), if initial % 3 == 2 { 2 } else { 1 });
            if initial % 3 == 2 {
                assert!(fields.iter().any(|(name, value)| name == "optional"
                    && matches!(value, ValueData::U64(value) if *value == initial + 2000)));
            }
            references.push(reference);
        }
        assert!(references.iter().any(|reference| reference.block > 1));
        assert!(
            references
                .iter()
                .any(|reference| reference.archetype != references[0].archetype)
        );
        assert_eq!(size_of::<SpanFieldsRef>(), 8);
    }

    #[test]
    fn active_span_reference_and_follows_from_keep_their_header_offsets() {
        let recorder = Arc::new(RecordingLayer::new());
        let _guard = tracing::subscriber::set_default(subscriber(recorder.clone()));
        let other = tracing::info_span!("other");
        let span = tracing::info_span!("active_link", extra = tracing::field::Empty);
        span.follows_from(&other);
        span.record("extra", 42u64);
        drop(span);
        let start = recorder
            .inspect(&Default::default())
            .into_iter()
            .find(|snapshot| {
                snapshot.metadata.name() == "active_link" && snapshot.kind == RecordKind::Span
            })
            .unwrap();
        let row = recorder.rows(start.id, None, 1).remove(0);
        assert!(row.follows_from.is_some());
        let reference = row.extra_fields.unwrap();
        assert_eq!(reference.block, 1);
        let span = SpanId {
            callsite: start.id.callsite as u32,
            archetype: start.id.archetype,
            index: row.index as u64,
        };
        assert!(matches!(
            &recorder.span_fields(span, reference).unwrap()[0].1,
            ValueData::U64(42)
        ));
        assert!(
            recorder
                .span_fields(span, SpanFieldsRef::default())
                .is_none()
        );
        assert!(
            recorder
                .span_fields(
                    span,
                    SpanFieldsRef {
                        block: u32::MAX,
                        ..reference
                    }
                )
                .is_none()
        );
        drop(other);
    }
}
