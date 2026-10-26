//! CODEX GENERATED
//! Full row decoding, shared by verification, benchmarks, and future viewers.
use super::{InputValue, PerCallsiteData, SpanFieldsRef, SpanId, StringStorage};
use crate::data::{CompressionStats, blockbuffer::ArchivedFrameIter, dyntuple::ValueKind};
use smallvec::SmallVec;
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
        dictionary: crate::data::blockbuffer::ParentOffsets<'_>,
        row: usize,
        length: usize,
    ) -> SpanId {
        let residual = self.u64();
        let entry = dictionary.entry(self.u16());
        SpanId {
            index: entry.decode(residual, row, length),
            callsite: entry.identity.callsite(),
            archetype: entry.identity.archetype(),
        }
    }
}
impl PerCallsiteData {
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
                let layout = update.archetype.column_layout();
                let dictionary = frame.data.parent_offsets(&layout);
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
                .map(|b| {
                    (
                        b.data.frame.len(),
                        &b.descriptor,
                        &b.data.metadata,
                        b.data.parent_offsets(&columns),
                    )
                })
                .chain(std::iter::once((
                    arch.data.active.length,
                    &arch.data.active_descriptor,
                    &arch.data.active_metadata,
                    arch.data.active_parent_offsets(),
                )));
            let mut rows = arch.data.iter(&columns);
            let mut strings = None;
            let mut string_id = None;
            for (count, descriptor, metadata, parent_offsets) in frames {
                if count == 0 {
                    continue;
                }
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
                        row.timestamp = Some(metadata.decode_timestamp(
                            descriptor,
                            reader.u64(),
                            local_row,
                            count,
                        ));
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
                            row.end_timestamp = Some(if offset == 0 {
                                0
                            } else {
                                row.timestamp.unwrap() + (offset - 1)
                            });
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
                                index: entry.decode(residual, local_row, count),
                                callsite: entry.identity.callsite(),
                                archetype: entry.identity.archetype(),
                            });
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
                                    StringStorage::Offset.read(strings, reader.u32()).to_owned(),
                                ),
                                ValueKind::BufferIndex => InputValue::Text(
                                    StringStorage::Index
                                        .read(strings, u32::from(reader.u16()))
                                        .to_owned(),
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
                        let captures: SmallVec<[&str; 4]> = (0..count)
                            .map(|_| {
                                // Template captures follow the callsite's packed string mode.
                                let reference = match self.string_storage {
                                    StringStorage::Offset => reader.u32(),
                                    StringStorage::Index => u32::from(reader.u16()),
                                };
                                self.string_storage.read(strings, reference)
                            })
                            .collect();
                        let message = self
                            .template
                            .as_ref()
                            .expect("template archetype")
                            .render(&captures);
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
