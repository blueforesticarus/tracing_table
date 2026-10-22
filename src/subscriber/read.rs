//! CODEX GENERATED
//! Full row decoding, shared by verification, benchmarks, and future viewers.
use super::{InputValue, PerCallsiteData, SpanFieldsRef, SpanId, StringStorage};
use crate::data::{CompressionStats, dyntuple::ValueKind};
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
    fn span(&mut self) -> SpanId {
        SpanId {
            index: self.u64(),
            callsite: self.u32(),
            archetype: self.u32(),
        }
    }
}
impl PerCallsiteData {
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
            let mut index = arch.data.ring.first().map_or(arch.data.count, |b| b.index) as u64;
            let frames = arch
                .data
                .ring
                .iter()
                .map(|b| (b.data.len(), b.string_buffer))
                .chain(std::iter::once((
                    arch.data.active.length,
                    arch.data.string_buffer,
                )));
            let mut rows = arch.data.iter();
            let mut strings = None;
            let mut string_id = None;
            for (count, buffer_id) in frames {
                if count == 0 {
                    continue;
                }
                if string_id != Some(buffer_id) {
                    strings = arch.strings.get(buffer_id);
                    string_id = Some(buffer_id);
                }
                let strings = strings.as_deref().expect("retained string buffer");
                for _ in 0..count {
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
                        row.span = Some(reader.span());
                    } else {
                        row.timestamp = Some(reader.u64());
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
                            row.extra_fields = Some(SpanFieldsRef {
                                archetype: reader.u32(),
                                block: reader.u32(),
                            });
                            let follows = reader.span();
                            row.follows_from = (follows.index != 0).then_some(follows);
                        }
                        if a.parent {
                            row.parent = Some(reader.span());
                        }
                    }
                    for kind in &a.field_types {
                        row.fields.push(kind.as_ref().map(|kind| match kind {
                            ValueKind::F64 => InputValue::F64(reader.u64()),
                            ValueKind::I64 => InputValue::I64(i64::from_ne_bytes(reader.bytes())),
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
                                StringStorage::Index.read(strings, u32::from(reader.u16())).to_owned(),
                            ),
                        }));
                    }
                    if let Some(count) = a.template {
                        let captures: Vec<_> = (0..count)
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
