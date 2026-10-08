use std::{io::Write, mem::ManuallyDrop, time::UNIX_EPOCH};

use rmpv::Value as MpValue;
use streaming_iterator::StreamingIterator;

use super::RecordingLayer;
use crate::data::{
    dyntuple::{Atom, ValueKind},
    stringcache::AtomIndexed,
};

impl RecordingLayer {
    /// Write consecutive MessagePack maps for format, callsite, archetype, and row records.
    /// Headers and numeric fields are binary; string fields are text (prefixes have two parts).
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
                let mut rows = arch.data.iter();
                let mut index = 0;
                while let Some(bytes) = rows.next() {
                    let mut offset = header_len;
                    let fields: Vec<_> = a
                        .field_types
                        .iter()
                        .map(|kind| match kind {
                            None => MpValue::Nil,
                            Some(kind) => {
                                let part = &bytes[offset..offset + kind.unit()];
                                offset += kind.unit();
                                dump_field(kind, part)
                            }
                        })
                        .collect();
                    let block = ends.partition_point(|&end| end <= index) + 1;
                    record(
                        &mut out,
                        [
                            ("type", "row".into()),
                            ("callsite", callsite.id.into()),
                            ("archetype", arch.id.into()),
                            ("index", (index + 1).into()),
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

// Read packed Atom bits without dropping the row's owner; indexes resolve to text too.
fn dump_field(kind: &ValueKind, bytes: &[u8]) -> MpValue {
    fn atom(bytes: &[u8]) -> String {
        let value = ManuallyDrop::new(unsafe { (bytes.as_ptr() as *const Atom).read_unaligned() });
        value.to_string()
    }
    fn indexed(bytes: &[u8]) -> String {
        let value = unsafe { (bytes.as_ptr() as *const AtomIndexed).read_unaligned() };
        value.to_string()
    }
    match kind {
        ValueKind::Str => atom(bytes).into(),
        ValueKind::StrIndex => indexed(bytes).into(),
        ValueKind::PrefixStr => {
            let n = size_of::<Atom>();
            (atom(&bytes[..n]) + &atom(&bytes[n..])).into()
        }
        ValueKind::PrefixStrIndex => {
            let n = size_of::<AtomIndexed>();
            (indexed(&bytes[..n]) + &indexed(&bytes[n..])).into()
        }
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
            span.record("deferred", "deferred dynamic text");
            let _entered = span.enter();
            for n in 0..150u64 {
                let message = format!("dump message {n}");
                tracing::info!(message = message.as_str(), count = n);
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
        assert_eq!(
            event_rows[0]["fields"][0],
            MpValue::Array(vec!["dump message 0".into(), "".into()])
        );
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
        // Exercise indexed-string conversion without changing the global recording mode.
        let mut bytes = [0u8; size_of::<crate::data::stringcache::AtomIndexed>()];
        let indexed = crate::data::stringcache::AtomIndexed::from("indexed dynamic text");
        crate::data::dyntuple::BytePacker::new(&mut bytes).write_any(indexed, false);
        assert_eq!(
            super::dump_field(&crate::data::dyntuple::ValueKind::StrIndex, &bytes),
            MpValue::from("indexed dynamic text")
        );
    }
}
