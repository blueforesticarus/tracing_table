use std::{
    collections::HashSet,
    fmt,
    io::Write,
    sync::{LazyLock, Mutex},
    time::UNIX_EPOCH,
};

use rmpv::Value as MpValue;
use streaming_iterator::StreamingIterator;
use tracing::Metadata;

use super::RecordingLayer;
use crate::data::{dyntuple::ValueKind, stringbuffer::ExpandedBuffer};

/// Print everything exposed by an erased debug message the first time a
/// callsite supplies a value with a particular trait-object vtable.
///
/// This is intentionally diagnostic rather than part of the MessagePack dump:
/// a vtable address is useful for grouping values in one process, but is not a
/// stable type identifier and must not become part of the storage format.
pub(super) fn dump_debug_message(
    metadata: &Metadata<'_>,
    value: &dyn fmt::Debug,
    is_log_compat: bool,
) {
    type SeenKey = (usize, String);
    static SEEN: LazyLock<Mutex<HashSet<SeenKey>>> = LazyLock::new(Default::default);

    // Vtable equality is only a useful observation within this process: codegen
    // may duplicate a type's vtable or merge identical vtables from two types.
    let value_metadata = std::ptr::metadata(value);
    // Store the diagnostic representation rather than DynMetadata itself so
    // the erased value's non-static object lifetime cannot escape this call.
    let value_metadata_identity = format!("{value_metadata:?}");
    let data_address = value as *const dyn fmt::Debug as *const ();
    let callsite_address = metadata as *const Metadata<'_> as usize;
    if !SEEN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert((callsite_address, value_metadata_identity))
    {
        return;
    }

    let fields: Vec<_> = metadata.fields().iter().map(|field| field.name()).collect();
    let debug = format!("{value:?}");
    let alternate_debug = format!("{value:#?}");
    let lower_hex_debug = format!("{value:x?}");
    let upper_hex_debug = format!("{value:X?}");

    // tracing-log 0.2's LogTracer constructs its `message` Value directly
    // from `log::Record::args()`. Therefore, for this explicitly identified
    // path, the erased concrete value is guaranteed to be fmt::Arguments.
    // `as_str` exposes a literal/compile-time-folded message, but returns None
    // for the usual case containing runtime formatting arguments.
    let log_arguments_as_str = is_log_compat.then(|| {
        let arguments = unsafe { &*(data_address as *const fmt::Arguments<'_>) };
        arguments.as_str()
    });

    eprintln!(
        "tracing_table debug-message dump:\n  callsite_metadata: {{ address: {callsite_address:#x}, name: {:?}, target: {:?}, level: {:?}, kind: {}, file: {:?}, line: {:?}, module: {:?}, fields: {:?} }}\n  value_metadata: {{ data_address: {:#x}, vtable: {:?}, dynamic_size: {}, dynamic_alignment: {}, erased_type_name: {:?}, source: {:?} }}\n  first_value: {{ debug: {:?}, alternate_debug: {:?}, lower_hex_debug: {:?}, upper_hex_debug: {:?}, tracing_log_arguments_as_str: {:?} }}",
        metadata.name(),
        metadata.target(),
        metadata.level(),
        if metadata.is_event() { "event" } else { "span" },
        metadata.file(),
        metadata.line(),
        metadata.module_path(),
        fields,
        data_address as usize,
        value_metadata,
        value_metadata.size_of(),
        value_metadata.align_of(),
        std::any::type_name_of_val(value),
        if is_log_compat {
            "tracing_log::LogTracer (known fmt::Arguments)"
        } else {
            "tracing (concrete Debug type erased)"
        },
        debug,
        alternate_debug,
        lower_hex_debug,
        upper_hex_debug,
        log_arguments_as_str,
    );
}

impl RecordingLayer {
    /// Write consecutive MessagePack maps for format, callsite, archetype, and row records.
    /// Headers and numeric fields are binary; string fields are text resolved through each chunk's buffer.
    pub fn dump_all_data(&self, mut out: impl Write) -> Result<(), Box<dyn std::error::Error>> {
        fn record(
            out: &mut impl Write,
            pairs: impl IntoIterator<Item = (&'static str, MpValue)>,
        ) -> Result<(), Box<dyn std::error::Error>> {
            let value = MpValue::Map(
                pairs
                    .into_iter()
                    .map(|(key, value)| (key.into(), value))
                    .collect(),
            );
            rmpv::encode::write_value(out, &value)?;
            Ok(())
        }
        record(
            &mut out,
            [
                ("type", "format".into()),
                ("version", 1u8.into()),
                (
                    "endian",
                    if cfg!(target_endian = "little") {
                        "little"
                    } else {
                        "big"
                    }
                    .into(),
                ),
                ("timestamp_bit_shift", self.timestamp_bit_shift.into()),
                (
                    "started_unix_ns",
                    self.system_time
                        .duration_since(UNIX_EPOCH)?
                        .as_nanos()
                        .to_string()
                        .into(),
                ),
            ],
        )?;
        let mut callsites: Vec<_> = self.callsites.read().values().cloned().collect();
        callsites.sort_by_key(|shared| shared.read().id);
        for shared in callsites {
            let callsite = shared.read();
            let m = callsite.metadata;
            record(
                &mut out,
                [
                    ("type", "callsite".into()),
                    ("id", callsite.id.into()),
                    ("name", m.name().into()),
                    ("target", m.target().into()),
                    ("level", m.level().to_string().into()),
                    ("kind", if m.is_event() { "event" } else { "span" }.into()),
                    ("file", m.file().map(MpValue::from).unwrap_or(MpValue::Nil)),
                    ("line", m.line().map(MpValue::from).unwrap_or(MpValue::Nil)),
                    (
                        "module",
                        m.module_path().map(MpValue::from).unwrap_or(MpValue::Nil),
                    ),
                    (
                        "fields",
                        MpValue::Array(m.fields().iter().map(|f| f.name().into()).collect()),
                    ),
                ],
            )?;
            for arch in &callsite.cache {
                let a = &arch.archetype;
                record(
                    &mut out,
                    [
                        ("type", "archetype".into()),
                        ("callsite", callsite.id.into()),
                        ("id", arch.id.into()),
                        ("event", a.is_event.into()),
                        ("span_update", a.is_span_end.into()),
                        ("parent", a.parent.into()),
                        ("follows_from", a.follows_from.into()),
                        (
                            "fields",
                            MpValue::Array(
                                a.field_types
                                    .iter()
                                    .map(|k| {
                                        k.as_ref()
                                            .map(|k| format!("{k:?}").into())
                                            .unwrap_or(MpValue::Nil)
                                    })
                                    .collect(),
                            ),
                        ),
                    ],
                )?;
                let header_len = a.unit()
                    - a.field_types
                        .iter()
                        .flatten()
                        .map(ValueKind::unit)
                        .sum::<usize>();
                let ends: Vec<usize> = arch
                    .data
                    .ring
                    .iter()
                    .scan(0, |n, b| {
                        *n += b.data.length;
                        Some(*n)
                    })
                    .collect();
                // Reuse a decompressed buffer across consecutive chunks sharing its ID.
                let mut strings = None;
                let mut string_id = None;
                let mut rows = arch.data.iter();
                // Eviction removes prefixes; exported IDs must still match span references.
                let first_index = arch.data.ring.first().map_or(arch.data.count, |b| b.index);
                let mut index = 0;
                while let Some(bytes) = rows.next() {
                    let block = ends.partition_point(|&end| end <= index);
                    let id = arch
                        .data
                        .ring
                        .get(block)
                        .map_or(arch.data.string_buffer, |b| b.string_buffer);
                    if string_id != Some(id) {
                        strings = arch.strings.get(id);
                        string_id = Some(id);
                    }
                    let strings = strings.as_deref().expect("chunk string buffer retained");
                    let mut offset = header_len;
                    let fields: Vec<_> = a
                        .field_types
                        .iter()
                        .map(|kind| match kind {
                            None => MpValue::Nil,
                            Some(kind) => {
                                let part = &bytes[offset..offset + kind.unit()];
                                offset += kind.unit();
                                dump_field(kind, part, strings)
                            }
                        })
                        .collect();
                    let block = block + arch.data.offset + 1;
                    record(
                        &mut out,
                        [
                            ("type", "row".into()),
                            ("callsite", callsite.id.into()),
                            ("archetype", arch.id.into()),
                            ("index", (index + first_index).into()),
                            ("block", block.into()),
                            ("header", bytes[..header_len].to_vec().into()),
                            ("fields", MpValue::Array(fields)),
                        ],
                    )?;
                    index += 1;
                }
            }
        }
        Ok(())
    }
}

// String fields contain plain offsets; no row owns heap-allocated string handles.
fn dump_field(kind: &ValueKind, bytes: &[u8], strings: &ExpandedBuffer) -> MpValue {
    match kind {
        ValueKind::BufferOffset => strings
            .get_offset(u32::from_ne_bytes(bytes.try_into().unwrap()) as usize)
            .expect("valid string offset")
            .into(),
        _ => bytes.to_vec().into(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rmpv::Value as MpValue;
    use tracing_subscriber::{Registry, layer::SubscriberExt};

    use crate::subscriber::{LocalRecordingLayer, RecordingLayer};

    #[test]
    fn dump_all_data_preserves_strings_and_span_references() {
        let recorder = Arc::new(RecordingLayer::new());
        let dispatch =
            tracing::Dispatch::new(Registry::default().with(LocalRecordingLayer(recorder.clone())));
        tracing::dispatcher::with_default(&dispatch, || {
            let span = tracing::info_span!("dump_span", deferred = tracing::field::Empty);
            span.record("deferred", "superseded text");
            span.record("deferred", "deferred dynamic text");
            let _entered = span.enter();
            for n in 0..150u64 {
                let message = format!("dump message {n}");
                tracing::info!(message = message.as_str(), count = n, detail = ?("é", n), empty = "");
            }
        });
        let mut output = Vec::new();
        recorder.dump_all_data(&mut output).unwrap();
        // MessagePack records are self-delimiting, so decode until the stream ends.
        let mut cursor = std::io::Cursor::new(output);
        let mut records = Vec::new();
        while cursor.position() < cursor.get_ref().len() as u64 {
            records.push(rmpv::decode::read_value(&mut cursor).unwrap());
        }
        assert_eq!(records[0]["version"].as_u64(), Some(1));
        let event = records
            .iter()
            .find(|v| {
                v["type"].as_str() == Some("callsite")
                    && v["fields"]
                        .as_array()
                        .unwrap()
                        .contains(&MpValue::from("count"))
            })
            .unwrap();
        let span = records
            .iter()
            .find(|v| {
                v["type"].as_str() == Some("callsite") && v["name"].as_str() == Some("dump_span")
            })
            .unwrap();
        let event_rows: Vec<_> = records
            .iter()
            .filter(|v| v["type"].as_str() == Some("row") && v["callsite"] == event["id"])
            .collect();
        assert_eq!(event_rows.len(), 150);
        assert!(event_rows.iter().any(|v| v["block"].as_u64().unwrap() > 1));
        assert_eq!(event_rows[0]["fields"][0], MpValue::from("dump message 0"));
        // Multiple scratch ranges, including formatted UTF-8 and empty text, stay distinct.
        for (n, row) in event_rows.iter().enumerate() {
            assert_eq!(row["fields"][0], MpValue::from(format!("dump message {n}")));
            assert_eq!(row["fields"][2], MpValue::from(format!("{:?}", ("é", n))));
            assert_eq!(row["fields"][3].as_str(), Some(""));
        }
        assert_eq!(
            event_rows[149]["fields"][1].as_slice().unwrap(),
            149u64.to_ne_bytes()
        );
        // The span-start header contains a one-based reference to its update block.
        let start = records
            .iter()
            .find(|v| {
                v["type"].as_str() == Some("row")
                    && v["callsite"] == span["id"]
                    && v["header"].as_slice().unwrap().len() > 16
            })
            .unwrap();
        let header = start["header"].as_slice().unwrap();
        assert_eq!(u32::from_ne_bytes(header[20..24].try_into().unwrap()), 1);
        assert!(records.iter().any(|v| {
            v["type"].as_str() == Some("row")
                && v["callsite"] == span["id"]
                && v["fields"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|f| f.as_str() == Some("deferred dynamic text"))
        }));
    }
}
