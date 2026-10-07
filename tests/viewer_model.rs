#[path = "../examples/bevy_viewer/src/inspection.rs"]
mod inspection;
// Compile the exact example policy without pulling Bevy into root tests.
#[path = "../examples/bevy_viewer/src/model.rs"]
mod model;

#[test]
fn viewer_reset_and_components() {
    // The indexed switch is process-global: isolate its regression test so other
    // recorder tests cannot change mode halfway through their own emissions.
    const CHILD: &str = "TRACING_VIEWER_RESET_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "viewer_reset_and_components", "--nocapture"])
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    use inspection::{InspectRecording, RecordingControl};
    use std::sync::Arc;
    use tracing_table::{data::dyntuple::ValueKind, subscriber::RecordingLayer};

    // Use the same callsite after every reset to verify schema reinitialization.
    fn emit() {
        for i in 0..1000u64 {
            tracing::info!(
                value = i,
                text = "retained field value",
                message = "prefix fixture"
            );
        }
    }
    let recorder = Arc::new(RecordingLayer::new());
    let control = RecordingControl::new(recorder.clone());
    let dispatch = tracing::Dispatch::new(inspection::subscriber_with_control(control.clone()));
    tracing::dispatcher::with_default(&dispatch, || {
        for indexed in [false, true, false] {
            // An open span must not try to patch its discarded start row later.
            let old = tracing::info_span!("old_span", deferred = tracing::field::Empty);
            control.set_indexed(indexed);
            assert!(recorder.inspect(&Default::default()).is_empty());
            old.record("deferred", "discarded update");
            let fresh = tracing::info_span!("fresh_span");
            fresh.follows_from(&old);
            let entered = fresh.enter();
            emit();
            drop(entered);
            drop(old);
            drop(fresh);
            let mut cache = model::StatsCache::default();
            cache.refresh(&recorder);
            assert!(
                cache
                    .archetypes
                    .values()
                    .all(|arch| arch.snapshot.metadata.name() != "old_span")
            );
            let arch = cache
                .archetypes
                .values()
                .find(|arch| arch.snapshot.rows == 1000)
                .unwrap();
            assert!(
                arch.snapshot
                    .fields
                    .iter()
                    .any(|(name, kind)| name == "text"
                        && *kind
                            == Some(if indexed {
                                ValueKind::StrIndex
                            } else {
                                ValueKind::Str
                            }))
            );
            let components = &arch.snapshot.components;
            assert_eq!(components[0].name, "ts");
            assert_eq!(components[1].name, "parent");
            for pair in components.windows(2) {
                assert_eq!(pair[0].range.end, pair[1].range.start);
            }
            assert!(components.iter().any(|part| part.name == "message.prefix"));
            assert!(components.iter().any(|part| part.name == "message.suffix"));
            let part = components
                .iter()
                .find(|part| part.name == "message.prefix")
                .unwrap();
            assert_eq!(part.kind, if indexed { "StrIndex" } else { "Str" });
            assert_eq!(
                part.range.len(),
                if indexed {
                    ValueKind::StrIndex.unit()
                } else {
                    ValueKind::Str.unit()
                }
            );
            // All row bytes must be attributed once, with header bytes included.
            assert_eq!(
                arch.components.iter().map(|part| part.raw).sum::<usize>(),
                arch.ring.data
            );
            assert!(
                arch.components
                    .iter()
                    .all(|part| part.rows > 0 && part.packed <= part.raw)
            );
            let span = cache
                .archetypes
                .values()
                .find(|arch| arch.snapshot.metadata.name() == "fresh_span")
                .unwrap();
            assert_eq!(
                span.snapshot
                    .components
                    .iter()
                    .map(|part| part.name.as_str())
                    .collect::<Vec<_>>(),
                ["ts", "end_ts", "follows_from"]
            );
            let rows = recorder.rows(arch.snapshot.id, None, 1000);
            assert_eq!(rows.len(), 1000);
            assert!(
                rows.iter()
                    .all(|row| model::message(row) == "prefix fixture")
            );
        }
    });
}
