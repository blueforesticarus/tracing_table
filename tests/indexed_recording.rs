// Exercise the same row decoding used by the viewer in both storage modes.
#[path = "../examples/bevy_viewer/src/inspection.rs"]
mod inspection;

use inspection::{FrameSizes, InspectRecording};
use parking_lot::RwLock;
use std::sync::Arc;
use tracing_table::{
    data::{
        dyntuple::{ValueData, ValueKind},
        stringcache::Index,
    },
    subscriber::RecordingLayer,
};

#[test]
fn strings_round_trip_in_both_modes() {
    for indexed in [false, true] {
        let mut layer = RecordingLayer::new();
        layer.string_index = indexed.then(|| RwLock::new(Index::default()));
        let layer = Arc::new(layer);
        // The example retains historical Atom references for its decoder.
        let subscriber = tracing::Dispatch::new(inspection::subscriber(layer.clone()));
        tracing::dispatcher::with_default(&subscriber, || {
            // Deferred fields exercise the span-close packing path too.
            let span = tracing::info_span!("indexed_span", deferred = tracing::field::Empty);
            span.record("deferred", "long deferred string");
            for message in ["long message", "long", "é", ""] {
                tracing::info!(message = message, short = "abc", long = "a longer string");
            }
            drop(span);
        });
        let snapshots = layer.inspect(&FrameSizes::default());
        let mut events = 0;
        let mut deferred = 0;
        for snapshot in snapshots {
            let rows = layer.rows(snapshot.id, None, usize::MAX);
            for row in rows {
                for (name, value) in row.fields {
                    match (name.as_str(), value) {
                        ("short", ValueData::Str(value)) => {
                            assert_eq!(&*value, "abc");
                            events += 1;
                        }
                        ("long", ValueData::Str(value)) => assert_eq!(&*value, "a longer string"),
                        ("deferred", ValueData::Str(value)) => {
                            assert_eq!(&*value, "long deferred string");
                            deferred += 1;
                        }
                        ("message", ValueData::PrefixStr(a, b)) => assert!(
                            ["long message", "long", "é", ""].contains(&format!("{a}{b}").as_str())
                        ),
                        other => panic!("unexpected field: {other:?}"),
                    }
                }
            }
        }
        assert_eq!(events, 4);
        assert_eq!(deferred, 1);
        assert!(
            layer
                .callsites
                .read()
                .values()
                .any(|data| data.read().cache.iter().any(|arch| arch
                    .archetype
                    .field_types
                    .contains(&Some(if indexed {
                        ValueKind::StrIndex
                    } else {
                        ValueKind::Str
                    }))))
        );
    }
}

#[test]
fn indexed_values_pack_and_read() {
    use tracing_table::data::dyntuple::BytePacker;
    use tracing_table::data::stringcache::{Atom, AtomIndexed};
    let mut index = Index::default();
    let values = [
        ValueData::StrIndex(index.insert(Atom::from("a longer string"))),
        ValueData::PrefixStrIndex(
            AtomIndexed::from_small_str("é"),
            AtomIndexed::from_small_str("abc"),
        ),
    ];
    // Word storage keeps the packer's borrowed reads naturally aligned.
    let mut storage = [0usize; 3];
    let mut packer = BytePacker::new(bytemuck::cast_slice_mut(&mut storage));
    for value in &values {
        packer.write_any_value(value, false);
    }
    let mut packer = BytePacker::new(bytemuck::cast_slice_mut(&mut storage));
    for value in values {
        let kind = ValueKind::from(&value);
        match unsafe { packer.read_value(kind, false) } {
            ValueData::StrIndex(v) => assert_eq!(&*index.get(v).unwrap(), "a longer string"),
            ValueData::PrefixStrIndex(a, b) => {
                assert_eq!(a.as_small_str(), "é");
                assert_eq!(b.as_small_str(), "abc");
            }
            other => panic!("unexpected value: {other:?}"),
        }
    }
}
