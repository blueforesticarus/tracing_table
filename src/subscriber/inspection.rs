//! Owned snapshots for test tools. Filtering and aggregate caches live outside
//! the recorder, which only knows frame identity, revisions, and packed schemas.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArchetypeId {
    pub callsite: u64,
    pub archetype: u32,
}

/// Key a caller's completed-frame revision map with (archetype, first row index).
pub type FrameRevisions = HashMap<(ArchetypeId, usize), u64>;

#[derive(Debug, Clone)]
pub struct CompletedFrameUpdate {
    pub id: usize,
    pub revision: u64,
    pub stats: CompressionStats,
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
    pub rows: usize,
    /// Includes the active allocation, even before its next rollover.
    pub blocks: usize,
    /// Recorded fields' byte positions within a full row, after its header.
    pub field_ranges: Vec<Option<std::ops::Range<usize>>>,
    pub active: CompressionStats,
    /// Only new/revised completed frames have their masks inspected.
    pub completed_updates: Vec<CompletedFrameUpdate>,
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

impl RecordingLayer {
    /// Snapshot under short per-callsite read locks. No caller code runs under
    /// these locks and unchanged completed frames never calculate stats again.
    pub fn inspect(&self, known: &FrameRevisions) -> Vec<ArchetypeSnapshot> {
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
                    .filter(|(frame, revision)| known.get(&(id, *frame)) != Some(revision))
                    .map(|(frame, revision)| {
                        let (rows, mask) = arch.data.completed_mask(frame);
                        CompletedFrameUpdate {
                            id: frame,
                            revision,
                            stats: arch.data.completed_stats(frame),
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
                let field_ranges = arch
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
    pub fn rows(&self, id: ArchetypeId, before: Option<usize>, limit: usize) -> Vec<RecordedRow> {
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
                .map(|(index, mut bytes)| {
                    let mut packer = BytePacker::new(&mut bytes);
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
                    // references while the storage lock keeps original owners alive.
                    unsafe {
                        if arch.archetype.is_span_end {
                            row.span = Some(packer.read_any_owned(false));
                        } else {
                            let timestamp: u64 = packer.read_any_owned(false);
                            row.timestamp_ns = Some(timestamp << self.timestamp_bit_shift);
                            if !arch.archetype.is_event {
                                let timestamp: u64 = packer.read_any_owned(false);
                                row.end_timestamp_ns = (timestamp != 0)
                                    .then_some(timestamp << self.timestamp_bit_shift);
                            }
                        }
                        if arch.archetype.follows_from {
                            let span: SpanId = packer.read_any_owned(false);
                            row.follows_from = (span.index != 0).then_some(span);
                        }
                        if arch.archetype.parent {
                            row.parent = Some(packer.read_any_owned(false));
                        }
                        for (field, kind) in data
                            .metadata
                            .fields()
                            .iter()
                            .zip(&arch.archetype.field_types)
                        {
                            if let Some(kind) = kind {
                                row.fields.push((
                                    field.name().to_owned(),
                                    packer.read_value(kind.clone(), false),
                                ));
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
