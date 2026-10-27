//! CODEX GENERATED
//! Full row decoding, shared by verification, benchmarks, and future viewers.
use super::{
    InputValue, PerCallsiteData, RecordingLayer, SpanFieldsRef, SpanId, SpanRegistry, StringStorage,
};
use crate::data::column::{AnyEncoding, Context, Encoding, Layout as ColumnLayout, OriginEncoding};
use crate::data::{CompressionStats, blockbuffer::ArchivedFrameIter, dyntuple::ValueKind};
use smallvec::SmallVec;
// One transient decoder model, not expanded row timestamps or retained parent data.
type OriginCache = Option<(u128, OriginEncoding)>;
use streaming_iterator::StreamingIterator;

/// Storage categories retain active tails instead of silently dropping partial blocks.
#[derive(Debug, Clone, Copy, Default)]
pub struct StorageStats {
    pub active_messages: CompressionStats,
    pub archived_messages: CompressionStats,
    pub active_strings: CompressionStats,
    pub archived_strings: CompressionStats,
}
impl StorageStats {
    pub fn total(self) -> CompressionStats {
        self.active_messages + self.archived_messages + self.active_strings + self.archived_strings
    }
}
impl std::ops::AddAssign for StorageStats {
    fn add_assign(&mut self, rhs: Self) {
        self.active_messages = self.active_messages + rhs.active_messages;
        self.archived_messages = self.archived_messages + rhs.archived_messages;
        self.active_strings = self.active_strings + rhs.active_strings;
        self.archived_strings = self.archived_strings + rhs.archived_strings;
    }
}

#[derive(Debug, Clone)]
pub struct DecodedRow {
    pub archetype: u32,
    pub index: u64,
    pub timestamp: Option<u64>,
    /// Event/log run within a shifted tick. For nondecreasing timestamps, sort by
    /// (timestamp, timestamp_index, index). Absent on spans and span updates.
    /// u16::MAX marks lost ordering precision until the timestamp changes.
    pub timestamp_index: Option<u16>,
    pub end_timestamp: Option<u64>,
    /// Resolved from block-level candidates on read; never stored in a start row.
    pub extra_fields: Option<SpanFieldsRef>,
    /// Present on span starts and update rows, absent on events.
    pub span: Option<SpanId>,
    pub parent: Option<SpanId>,
    pub follows_from: Option<SpanId>,
    pub fields: Vec<Option<InputValue>>,
}

// Explicit scalar reads avoid forming references to unaligned row contents.
struct Reader<'a>(&'a [u8]);

#[derive(Clone, Copy)]
enum StartTime {
    Absolute(u64),
    Parent(u64, SpanId),
}

// Copy one node under a short read guard, then release it before following
// another parent. A caller already borrowing its own site must not relock it.
fn resolve_start<const CACHE: bool>(
    registry: &SpanRegistry,
    local: Option<&PerCallsiteData>,
    mut span: SpanId,
    origin_cache: &mut OriginCache,
) -> Option<u64> {
    let mut elapsed = 0u64;
    let mut visited = SmallVec::<[u128; 8]>::new();
    loop {
        // SpanId is a checked, padding-free 16-byte value. Scalar markers make
        // cycle checks cheap without hashing, allocating or retaining timestamps.
        let identity = bytemuck::cast::<SpanId, u128>(span);
        if visited.contains(&identity) {
            return None;
        }
        let node = if let Some(site) = local.filter(|site| site.id == u64::from(span.callsite)) {
            site.start_time::<CACHE>(span, origin_cache)?
        } else {
            let shared = registry.read().get(span.callsite as usize)?.upgrade()?;
            let node = shared.read().start_time::<CACHE>(span, origin_cache)?;
            node
        };
        match node {
            StartTime::Absolute(start) => return start.checked_add(elapsed),
            StartTime::Parent(offset, parent) => {
                // Only followed edges need a cycle marker. Absolute roots end
                // the walk immediately and must not grow its transient scratch.
                visited.push(identity);
                elapsed = elapsed.checked_add(offset)?;
                span = parent;
            }
        }
    }
}

impl RecordingLayer {
    /// Resolve immutable starts through retained parents. Evicted nodes return
    /// None; this optional encoding deliberately does not pin ancestor blocks.
    /// The lookup index is populated only when parent_timestamp_offsets was
    /// enabled before registration; normal visit_rows works in both modes.
    pub fn span_timestamp(&self, span: SpanId) -> Option<u64> {
        resolve_start::<false>(&self.span_registry, None, span, &mut None)
    }

    /// Dense weak index only, not a cache of expanded timestamps. The storage
    /// benchmark charges this optional recorder-wide allocation once.
    pub fn parent_timestamp_registry_bytes(&self) -> usize {
        let registry = self.span_registry.read();
        16 + size_of::<SpanRegistry>()
            + registry.capacity()
                * size_of::<std::sync::Weak<parking_lot::RwLock<PerCallsiteData>>>()
    }
}
impl Reader<'_> {
    fn bytes<const N: usize>(&mut self) -> [u8; N] {
        let (head, tail) = self.0.split_at(N);
        self.0 = tail;
        head.try_into().unwrap()
    }
    fn u32(&mut self) -> u32 {
        u32::from_ne_bytes(self.bytes())
    }
    fn u16(&mut self) -> u16 {
        u16::from_ne_bytes(self.bytes())
    }
    fn u64(&mut self) -> u64 {
        u64::from_ne_bytes(self.bytes())
    }
    // Structural references all use the shared footer dictionary, including
    // active rows whose predictor has zero stride until rollover.
    fn span(
        &mut self,
        dictionary: crate::subscriber::storage::ParentOffsets<'_>,
        row: usize,
        length: usize,
    ) -> SpanId {
        let residual = self.u64();
        let entry = dictionary.entry(self.u16());
        SpanId {
            index: entry.decode_index(residual, row, length),
            callsite: crate::subscriber::storage::CallsiteArchetype::from_bits(entry.identity)
                .callsite(),
            archetype: crate::subscriber::storage::CallsiteArchetype::from_bits(entry.identity)
                .archetype(),
        }
    }
}
impl PerCallsiteData {
    fn start_time<const CACHE: bool>(
        &self,
        span: SpanId,
        origin_cache: &mut OriginCache,
    ) -> Option<StartTime> {
        let arch = self.cache.get(span.archetype as usize)?;
        if arch.archetype.is_span_end || arch.archetype.is_event {
            return None;
        }
        let index = usize::try_from(span.index).ok()?;
        let data = &arch.data;
        if index >= data.active_descriptor.index {
            let row = index - data.active_descriptor.index;
            if row >= data.active.length {
                return None;
            }
            let at = (row + 1) * data.active.unit;
            return Some(StartTime::Absolute(u64::from_ne_bytes(
                data.active.buffer[at..at + 8].try_into().unwrap(),
            )));
        }
        let (ordinal, block) = data.archived_at(index)?;
        let row = index - block.descriptor.index;
        let mut encoded = [0; 8];
        block.data.frame.read_column(row, 0, &mut encoded);
        let value = u64::from_ne_bytes(encoded);
        // Recover the block and schema in the same reverse lookup. Absolute
        // ancestor reads must not rescan the ring from its oldest block.
        let fallback;
        let columns = if let Some(projection) = data.layout_for_block(ordinal) {
            projection.columns.as_slice()
        } else {
            fallback = arch.archetype.column_layout();
            &fallback
        };
        if !block.data.timestamp_parent_relative(columns) {
            // Revalidate retention under this read guard before reusing a model.
            // Immutable footer coefficients survive payload edits and never pin a block.
            let key = bytemuck::cast::<SpanId, u128>(SpanId {
                index: block.descriptor.index as u64,
                ..span
            });
            let encoding = if CACHE
                && origin_cache
                    .as_ref()
                    .is_some_and(|(cached, _)| *cached == key)
            {
                origin_cache.as_ref().unwrap().1
            } else {
                let column = block.data.timestamp_column(columns);
                let AnyEncoding::Origin(model) = column.encoding() else {
                    // Split dictionaries borrow immutable footer tables. Decode
                    // through the current guarded block instead of retaining a
                    // pointer across archive rebuilds or eviction.
                    return Some(StartTime::Absolute(
                        column.decode(u128::from(value), row) as u64,
                    ));
                };
                let model = *model;
                if CACHE {
                    *origin_cache = Some((key, model));
                }
                model
            };
            let context = Context {
                frame: &block.data.frame,
                layout: ColumnLayout::tagged_origin(0),
            };
            return Some(StartTime::Absolute(
                encoding.decode(u128::from(value), row, context) as u64,
            ));
        }
        if !arch.archetype.parent {
            return None;
        }
        let mut bytes = [0; 10];
        block.data.frame.read_column(row, 16, &mut bytes);
        let parent = Reader(&bytes).span(
            block.data.parent_offsets(columns),
            row,
            block.data.frame.len(),
        );
        let elapsed = block.data.timestamp_column(columns).decode(u128::from(value), row) as u64;
        Some(StartTime::Parent(elapsed, parent))
    }

    /// Site-level visitors can resolve same-site parents without recursively
    /// acquiring their own RwLock, including when a writer is waiting.
    /// Cross-callsite lookup uses the optional parent-offset registry.
    pub fn span_timestamp(&self, span: SpanId) -> Option<u64> {
        let registry = self.span_registry.upgrade()?;
        resolve_start::<false>(&registry, Some(self), span, &mut None)
    }
    /// Locate deferred data using only this start block's deduplicated candidates.
    /// Missing/evicted candidates are harmless; other rows in a candidate block
    /// must not be mistaken for the requested span.
    pub fn span_fields_ref(&self, span: SpanId) -> Option<SpanFieldsRef> {
        if self.id != u64::from(span.callsite) {
            return None;
        }
        let start = self.cache.get(span.archetype as usize)?;
        self.span_fields_ref_from_candidates(span, start.data.span_fields_refs(span.index as usize))
    }

    // Sequential decoding already knows the start block; reuse its candidates
    // instead of searching the archive again for every span.
    fn span_fields_ref_from_candidates(
        &self,
        span: SpanId,
        candidates: &[SpanFieldsRef],
    ) -> Option<SpanFieldsRef> {
        for &reference in candidates {
            let Some(update) = self.cache.get(reference.archetype as usize) else {
                continue;
            };
            if !update.archetype.is_span_end {
                continue;
            }
            let Some(block) = (reference.block as usize)
                .checked_sub(1)
                .and_then(|block| block.checked_sub(update.data.offset))
            else {
                continue;
            };
            let matches = |bytes: &[u8], dictionary, row, length| {
                let id = Reader(bytes).span(dictionary, row, length);
                (id.callsite, id.archetype, id.index) == (span.callsite, span.archetype, span.index)
            };
            if let Some(frame) = update.data.ring.get(block) {
                let fallback = update.archetype.column_layout();
                let layout = update
                    .data
                    .columns_for_block(update.data.offset + block, &fallback);
                let dictionary = frame.data.parent_offsets(layout);
                let mut rows = ArchivedFrameIter::new(&frame.data.frame);
                let mut index = 0;
                while let Some(row) = rows.next() {
                    if matches(row, dictionary, index, frame.data.frame.len()) {
                        return Some(reference);
                    }
                    index += 1;
                }
            } else if block == update.data.ring.len() {
                let dictionary = update.data.active_parent_offsets();
                for (index, row) in update.data.active.iter().enumerate() {
                    if matches(row, dictionary, index, update.data.active.length) {
                        return Some(reference);
                    }
                }
            }
        }
        None
    }

    pub fn storage_stats(&self) -> StorageStats {
        let mut total = StorageStats::default();
        for arch in &self.cache {
            let (active_messages, archived_messages) = arch.data.compression_parts();
            let (active_strings, archived_strings) = arch.strings.compression_parts();
            total += StorageStats {
                active_messages,
                archived_messages,
                active_strings,
                archived_strings,
            };
        }
        total
    }

    /// Visit every stored row, including active rows. Strings expand once per
    /// consecutive buffer within an archetype; no expansion survives this call.
    pub fn visit_rows(&self, mut consume: impl FnMut(DecodedRow)) {
        // Keep only the weak lookup index alive for this traversal. Parents are
        // still upgraded and retention-checked under fresh short read guards.
        let registry = self.span_registry.upgrade();
        let mut origin_cache = None;
        for arch in &self.cache {
            let a = &arch.archetype;
            let mut index = arch
                .data
                .ring
                .front()
                .map_or(arch.data.active_descriptor.index, |b| b.descriptor.index)
                as u64;
            let columns = a.column_layout();
            let frames = arch
                .data
                .ring
                .iter()
                .enumerate()
                .map(|(ordinal, b)| {
                    let layout = arch
                        .data
                        .columns_for_block(arch.data.offset + ordinal, &columns);
                    let origin = if !a.is_span_end {
                        Some(b.data.timestamp_column(layout))
                    } else {
                        None
                    };
                    (
                        b.data.frame.len(),
                        &b.descriptor,
                        &b.data.metadata,
                        b.data.parent_offsets(layout),
                        origin,
                    )
                })
                .chain(std::iter::once((
                    arch.data.active.length,
                    &arch.data.active_descriptor,
                    &arch.data.active_metadata,
                    arch.data.active_parent_offsets(),
                    None,
                )));
            let mut rows = arch.data.iter_encoded_origins(&columns);
            let mut strings = None;
            let mut string_id = None;
            for (count, _descriptor, metadata, parent_offsets, origin) in frames {
                if count == 0 {
                    continue;
                }
                let relative = origin.as_ref().is_some_and(|c| c.encoding().is_relative());
                let buffer_id = metadata.string_buffer;
                if string_id != Some(buffer_id) {
                    strings = arch.strings.get(buffer_id);
                    string_id = Some(buffer_id);
                }
                let strings = strings.as_deref().expect("retained string buffer");
                for local_row in 0..count {
                    let mut reader = Reader(rows.next().expect("stored row"));
                    let mut row = DecodedRow {
                        archetype: arch.id,
                        index,
                        timestamp: None,
                        timestamp_index: None,
                        end_timestamp: None,
                        extra_fields: None,
                        span: None,
                        parent: None,
                        follows_from: None,
                        fields: Vec::with_capacity(a.field_types.len()),
                    };
                    index += 1;
                    if a.is_span_end {
                        row.span = Some(reader.span(parent_offsets, local_row, count));
                    } else {
                        // Span end edits never move the block's decoding origin.
                        let encoded_start = reader.u64();
                        // Both bases may now have a split dictionary. Resolve
                        // the stored scalar before adding any parent start.
                        let decoded_start = origin.as_ref().map_or(encoded_start, |column| {
                            column.decode(encoded_start as u128, local_row) as u64
                        });
                        row.timestamp = if relative {
                            None
                        } else {
                            Some(decoded_start)
                        };
                        let mut duration = None;
                        if a.is_event {
                            row.timestamp_index = Some(reader.u16());
                        } else {
                            row.span = Some(SpanId {
                                index: row.index,
                                callsite: self.id as u32,
                                archetype: arch.id,
                            });
                            // Stored ends are duration + 1; preserve the public
                            // nominal timestamp and existing zero-for-open API.
                            let offset = reader.u64();
                            duration = Some(offset);
                            row.extra_fields = self.span_fields_ref_from_candidates(
                                row.span.unwrap(),
                                metadata.span_fields().unwrap_or_default(),
                            );
                        }
                        if a.parent {
                            let residual = reader.u64();
                            let entry = parent_offsets.entry(reader.u16());
                            row.parent = Some(SpanId {
                                // Prediction uses this block's zero-based row,
                                // independent of absolute indices and eviction.
                                index: entry.decode_index(residual, local_row, count),
                                callsite: crate::subscriber::storage::CallsiteArchetype::from_bits(
                                    entry.identity,
                                )
                                .callsite(),
                                archetype:
                                    crate::subscriber::storage::CallsiteArchetype::from_bits(
                                        entry.identity,
                                    )
                                    .archetype(),
                            });
                        }
                        if relative {
                            row.timestamp = row
                                .parent
                                .and_then(|parent| {
                                    registry.as_deref().and_then(|registry| {
                                        resolve_start::<true>(
                                            registry,
                                            Some(self),
                                            parent,
                                            &mut origin_cache,
                                        )
                                    })
                                })
                                .and_then(|start| start.checked_add(decoded_start));
                        }
                        if let Some(duration) = duration {
                            row.end_timestamp = if duration == 0 {
                                Some(0)
                            } else {
                                row.timestamp
                                    .and_then(|start| start.checked_add(duration - 1))
                            };
                        }
                    }
                    if a.follows_from {
                        row.follows_from = Some(reader.span(parent_offsets, local_row, count));
                    }
                    for kind in &a.field_types {
                        row.fields.push(kind.as_ref().map(|kind| {
                            match kind {
                                // Decode precisely the width selected by the format schema.
                                ValueKind::I8 => InputValue::I8(i8::from_ne_bytes(reader.bytes())),
                                ValueKind::I16 => {
                                    InputValue::I16(i16::from_ne_bytes(reader.bytes()))
                                }
                                ValueKind::I32 => {
                                    InputValue::I32(i32::from_ne_bytes(reader.bytes()))
                                }
                                ValueKind::Isize => {
                                    InputValue::Isize(isize::from_ne_bytes(reader.bytes()))
                                }
                                ValueKind::U8 => InputValue::U8(u8::from_ne_bytes(reader.bytes())),
                                ValueKind::U16 => {
                                    InputValue::U16(u16::from_ne_bytes(reader.bytes()))
                                }
                                ValueKind::U32 => {
                                    InputValue::U32(u32::from_ne_bytes(reader.bytes()))
                                }
                                ValueKind::Usize => {
                                    InputValue::Usize(usize::from_ne_bytes(reader.bytes()))
                                }
                                ValueKind::F32 => {
                                    InputValue::F32(u32::from_ne_bytes(reader.bytes()))
                                }
                                ValueKind::Char => InputValue::Char(
                                    char::from_u32(u32::from_ne_bytes(reader.bytes()))
                                        .expect("stored char"),
                                ),
                                ValueKind::F64 => InputValue::F64(reader.u64()),
                                ValueKind::I64 => {
                                    InputValue::I64(i64::from_ne_bytes(reader.bytes()))
                                }
                                ValueKind::U64 => InputValue::U64(reader.u64()),
                                ValueKind::I128 => {
                                    InputValue::I128(i128::from_ne_bytes(reader.bytes()))
                                }
                                ValueKind::U128 => {
                                    InputValue::U128(u128::from_ne_bytes(reader.bytes()))
                                }
                                ValueKind::Bool => InputValue::Bool(reader.bytes::<1>()[0] != 0),
                                ValueKind::BufferOffset => InputValue::Text(
                                    StringStorage::Offset
                                        .read(strings, reader.u32())
                                        .into_owned(),
                                ),
                                ValueKind::BufferIndex => InputValue::Text(
                                    StringStorage::Index
                                        .read(strings, u32::from(reader.u16()))
                                        .into_owned(),
                                ),
                            }
                        }));
                    }
                    // Reconstruct every formatted field from its callsite recipe
                    // before hiding the appended physical argument columns.
                    for &(field, recipe, start) in &a.formats {
                        let recipe = &self.formats[recipe];
                        let count = recipe.template.value_count();
                        let text = recipe
                            .render(&row.fields[start..start + count])
                            .expect("stored format arguments");
                        row.fields[field] = Some(InputValue::Text(text));
                    }
                    row.fields.truncate(self.metadata.fields().len());
                    if let Some(count) = a.template {
                        // Common templates fit inline, matching the ingestion path.
                        let captures: SmallVec<[std::borrow::Cow<'_, str>; 4]> = (0..count)
                            .map(|_| {
                                // Template captures follow the callsite's packed string mode.
                                let reference = match self.string_storage {
                                    StringStorage::Offset => reader.u32(),
                                    StringStorage::Index => u32::from(reader.u16()),
                                };
                                self.string_storage.read(strings, reference)
                            })
                            .collect();
                        // Keep individually decoded captures alive during render;
                        // raw warm-up strings continue to borrow their buffer.
                        let capture_refs: SmallVec<[&str; 4]> =
                            captures.iter().map(|s| s.as_ref()).collect();
                        let message = self
                            .template
                            .as_ref()
                            .expect("template archetype")
                            .render(&capture_refs);
                        let field = self.metadata.fields().field("message").unwrap().index();
                        row.fields[field] = Some(InputValue::Text(message));
                    }
                    debug_assert!(reader.0.is_empty());
                    consume(row);
                }
            }
        }
    }
}

#[cfg(test)]
mod model_cache_tests {
    use super::*;
    use crate::subscriber::{RecordingLayer, Time};
    use tracing_core::Callsite;

    #[test]
    fn transient_models_do_not_revive_parents_evicted_during_a_visit() {
        let parent_meta = tracing::callsite!(name: "decoder model parent", kind: tracing_core::Kind::SPAN, fields:).metadata();
        let child_meta = tracing::callsite!(name: "decoder model child", kind: tracing_core::Kind::EVENT, fields:).metadata();
        let mut recorder = RecordingLayer::new();
        recorder.parent_timestamp_offsets = true;
        recorder.callsite_memory_cap = usize::MAX;
        recorder.global_memory_cap = usize::MAX;
        let time = |value| Time { value, shift: 0 };
        let mut parents = Vec::new();
        for row in 0..32u64 {
            let start = 1_000_000 + row * row * 100_000;
            let parent = recorder.record_start(parent_meta, time(start), None, false, |_| {});
            parents.push(parent);
            recorder.record_start(child_meta, time(start + 7), Some(parent), false, |_| {});
        }
        let sites = recorder.callsites.read();
        let parent_site = sites
            .get(&(parent_meta as *const _ as usize))
            .unwrap()
            .clone();
        let child_site = sites
            .get(&(child_meta as *const _ as usize))
            .unwrap()
            .clone();
        drop(sites);
        // Force both source blocks into archives. One parent model serves the
        // first child, then that source allocation disappears before the next row.
        for shared in [&parent_site, &child_site] {
            let mut site = shared.write();
            let arch = &mut site.cache[0];
            let columns = arch.archetype.column_layout();
            arch.data.cycle_with_columns(&columns);
        }
        {
            let site = child_site.read();
            assert!(
                site.cache[0].data.ring[0]
                    .data
                    .timestamp_parent_relative(&site.cache[0].archetype.column_layout())
            );
        }
        let mut starts = Vec::new();
        child_site.read().visit_rows(|row| {
            starts.push(row.timestamp);
            if starts.len() == 1 {
                parent_site.write().cache[0].data.drop_chunk(1).unwrap();
            }
        });
        assert_eq!(starts[0], Some(1_000_007));
        assert_eq!(starts.len(), parents.len());
        assert!(starts[1..].iter().all(Option::is_none));
    }
}
