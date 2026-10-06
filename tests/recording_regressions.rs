//! Storage regressions needed by the external viewer and concurrent benchmark.
use std::sync::Arc;
use streaming_iterator::StreamingIterator;
use tracing_subscriber::{Registry, layer::SubscriberExt};
use tracing_table::{
    data::{
        blockbuffer::BlockCache,
        dataframe::{CompactFrame, CompactFrameIter, Frame},
        dyntuple::ValueData,
    },
    subscriber::{RecordKind, RecordingLayer},
};

#[test]
fn constant_frames_terminate_and_keep_their_row_count() {
    // Odd widths exercise padded allocation/deallocation and wide rows the heap path.
    for width in [1, 7, 17, 257] {
        let mut frame = Frame::new(width, 9);
        for _ in 0..9 {
            frame.push(&vec![42; width]);
        }
        let compact = CompactFrame::new_from(&frame);
        let mut iter = CompactFrameIter::new(&compact);
        for _ in 0..9 {
            assert_eq!(iter.next().unwrap(), vec![42; width]);
        }
        assert!(iter.next().is_none());
        assert_eq!(compact.compression_ratio().compressed, width);
    }
}

#[test]
fn edits_invalidate_completed_frames_and_preserve_other_rows() {
    let mut cache = BlockCache::new(3, 1, 2);
    for _ in 0..5 {
        cache.push(&[1, 2, 3], 0);
    }
    // Editing a constant byte in the first compacted row must rebuild its mask.
    cache.get_mut(1).unwrap().get_mut()[0] = 9;
    let mut iter = cache.iter();
    assert_eq!(iter.next().unwrap(), &[9, 2, 3]);
    for _ in 0..4 {
        assert_eq!(iter.next().unwrap(), &[1, 2, 3]);
    }
    assert!(iter.next().is_none());
    drop(iter);
    // Editing row zero of an active frame must compare against its next row.
    let mut active = BlockCache::new(3, 1, 4);
    active.push(&[1, 2, 3], 0);
    active.push(&[1, 2, 3], 0);
    active.get_mut(1).unwrap().get_mut()[2] = 8;
    for _ in 0..3 {
        active.push(&[1, 2, 3], 0);
    }
    let mut rows = active.iter();
    assert_eq!(rows.next().unwrap(), &[1, 2, 8]);
    for _ in 0..4 {
        assert_eq!(rows.next().unwrap(), &[1, 2, 3]);
    }
    assert!(rows.next().is_none());
    assert!(cache.get_mut(0).is_none());
    assert!(cache.get_mut(99).is_none());
}

#[test]
fn atoms_and_unaligned_values_survive_rollover_and_recorder_drop() {
    let recorder = Arc::new(RecordingLayer::new());
    let subscriber = Registry::default().with(recorder.layer());
    let messages = [
        "a long unique dynamic message éé",
        "a long",
        "a",
        "",
        "é",
        "ê",
        "😀suffix",
        "😀",
    ];
    {
        let _guard = tracing::subscriber::set_default(subscriber);
        for i in 0..1000u64 {
            // bool before u128/atoms intentionally creates unaligned field offsets.
            tracing::info!(flag = true, wide = i as u128, entity = i, text = %format!("owned-string-number-{i}"), message = messages[i as usize % messages.len()]);
        }
    }
    let snapshot = recorder.inspect(&Default::default()).remove(0);
    assert_eq!(snapshot.rows, 1000);
    let rows = recorder.rows(snapshot.id, None, 1000);
    assert_eq!(rows.len(), 1000);
    drop(recorder);
    // Inspection owns atom clones, not borrowed pointers into destroyed storage.
    for row in rows {
        let i = row.index - 1;
        for (name, value) in row.fields {
            match (name.as_str(), value) {
                ("wide", ValueData::U128(v)) => assert_eq!(v, i as u128),
                ("entity", ValueData::U64(v)) => assert_eq!(v, i as u64),
                ("text", ValueData::Str(v)) => assert_eq!(&*v, format!("owned-string-number-{i}")),
                ("message", ValueData::PrefixStr(a, b)) => {
                    assert_eq!(format!("{a}{b}"), messages[i % messages.len()])
                }
                ("flag", ValueData::Bool(v)) => assert!(v),
                _ => panic!("unexpected field"),
            }
        }
    }
}

#[test]
fn closed_compacted_spans_and_updated_fields_decode() {
    let recorder = Arc::new(RecordingLayer::new());
    let subscriber = Registry::default().with(recorder.layer());
    let _guard = tracing::subscriber::set_default(subscriber);
    let make_span = || {
        tracing::info_span!(
            "retained_span",
            entity = 123u64,
            updated = tracing::field::Empty
        )
    };
    let retained = make_span();
    for _ in 0..300 {
        drop(make_span());
    }
    let snapshots = recorder.inspect(&Default::default());
    let span = snapshots
        .iter()
        .find(|s| s.kind == RecordKind::Span)
        .unwrap();
    assert!(!span.completed_updates.is_empty());
    let known = snapshots
        .iter()
        .flat_map(|s| {
            s.completed_updates
                .iter()
                .map(move |f| ((s.id, f.id), f.revision))
        })
        .collect();
    retained.record("updated", "late owned string");
    drop(retained);
    let changed = recorder.inspect(&known);
    assert_eq!(
        changed
            .iter()
            .find(|s| s.id == span.id)
            .unwrap()
            .completed_updates
            .len(),
        1
    );
    let rows = recorder.rows(span.id, Some(2), 1);
    assert!(rows[0].end_timestamp_ns.is_some());
    let update = changed
        .iter()
        .find(|s| s.kind == RecordKind::SpanUpdate)
        .unwrap();
    let rows = recorder.rows(update.id, None, 1);
    assert_eq!(rows[0].span.unwrap().index, 1);
    assert!(rows[0].fields.iter().any(|(name, value)| name == "updated"
        && matches!(value, ValueData::Str(v) if &**v == "late owned string")));
}

#[test]
fn repeated_dependencies_decode_the_latest_edge() {
    let recorder = Arc::new(RecordingLayer::new());
    let _guard = tracing::subscriber::set_default(Registry::default().with(recorder.layer()));
    let first = tracing::info_span!("first_dependency");
    let second = tracing::info_span!("second_dependency");
    let dependent = tracing::info_span!("dependent_span");
    dependent.follows_from(first.id());
    dependent.follows_from(second.id());
    drop(dependent);
    let snapshots = recorder.inspect(&Default::default());
    let dependency = snapshots
        .iter()
        .find(|s| s.metadata.name() == "second_dependency")
        .unwrap();
    let dependent = snapshots
        .iter()
        .find(|s| s.metadata.name() == "dependent_span")
        .unwrap();
    let row = recorder.rows(dependent.id, None, 1).remove(0);
    assert_eq!(
        row.follows_from.unwrap().callsite as u64,
        dependency.id.callsite
    );
    assert!(row.end_timestamp_ns.is_some());
    // The aligned allocation helper keeps a byte-compatible deallocation layout.
    let bytes = tracing_table::data::dataframe::aligned_boxed_slice(37, 1);
    assert_eq!(&*bytes, &[0; 37]);
}
