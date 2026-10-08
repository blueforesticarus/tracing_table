//! Keep historical string fields alive without changing the recorder's storage.
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, RwLock},
};
use tracing::{
    Event, Metadata, Subscriber,
    span::{Attributes, Id, Record},
};
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};
use tracing_table::{
    data::dyntuple::{Atom, ValueData},
    subscriber::{LocalRecordingLayer, RecordingLayer, log_callsite_adapter},
};

// A generation marks spans whose backing rows were discarded by a UI reset.
// Holding the gate across each callback makes mode changes atomic to workers.
#[derive(Clone, Copy)]
struct RecordingGeneration(u64);
pub struct RecordingControl {
    recorder: Arc<RecordingLayer>,
    generation: RwLock<u64>,
    pools: Mutex<HashMap<usize, Arc<Mutex<HashSet<Atom>>>>>,
}
impl RecordingControl {
    pub fn new(recorder: Arc<RecordingLayer>) -> Arc<Self> {
        Arc::new(Self {
            recorder,
            generation: RwLock::new(0),
            pools: Mutex::new(HashMap::new()),
        })
    }
    pub fn set_indexed(&self, indexed: bool) {
        let mut generation = self.generation.write().unwrap();
        *generation += 1;
        self.recorder.callsites.write().clear();
        self.pools.lock().unwrap().clear();
        tracing_table::subscriber::USE_INDEXED_ATOM
            .store(indexed, std::sync::atomic::Ordering::Relaxed);
    }
}
pub(super) struct RetainedLayer {
    inner: LocalRecordingLayer,
    control: Arc<RecordingControl>,
}
impl RetainedLayer {
    pub(super) fn new(control: Arc<RecordingControl>) -> Self {
        Self {
            inner: LocalRecordingLayer(control.recorder.clone()),
            control,
        }
    }
    fn record(&self, metadata: &'static Metadata<'static>, write: impl FnOnce() -> Vec<ValueData>) {
        // Use the recorder's canonical metadata identity for retention too.
        let key = metadata as *const Metadata<'static> as usize;
        let pool = self
            .control
            .pools
            .lock()
            .unwrap()
            .entry(key)
            .or_default()
            .clone();
        let mut pool = pool.lock().unwrap();
        let extra = write();
        let callsites = self.inner.callsites.read();
        let data = callsites.get(&key).unwrap().read();
        for value in data.field_value_scratchpad.iter().flatten().chain(&extra) {
            match value {
                ValueData::Str(atom) => {
                    pool.insert(atom.clone());
                }
                ValueData::PrefixStr(prefix, suffix) => {
                    pool.insert(prefix.clone());
                    pool.insert(suffix.clone());
                }
                _ => {}
            }
        }
    }
}
impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for RetainedLayer {
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let _generation = self.control.generation.read().unwrap();
        self.record(log_callsite_adapter(event), || {
            self.inner.on_event(event, ctx);
            Vec::new()
        });
    }
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let generation = self.control.generation.read().unwrap();
        self.record(attrs.metadata(), || {
            self.inner.on_new_span(attrs, id, ctx.clone());
            ctx.span(id)
                .unwrap()
                .extensions_mut()
                .insert(RecordingGeneration(*generation));
            Vec::new()
        });
    }
    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let generation = self.control.generation.read().unwrap();
        let span = ctx.span(id).unwrap();
        if span.extensions().get::<RecordingGeneration>().map(|g| g.0) != Some(*generation) {
            return;
        }
        self.record(span.metadata(), || {
            self.inner.on_record(id, values, ctx.clone());
            // Updated span values live in tracing extensions, rather than the
            // callsite buffer. Retain them before closing drops those owners.
            span.extensions()
                .get::<Vec<Option<ValueData>>>()
                .into_iter()
                .flatten()
                .flatten()
                .cloned()
                .collect()
        });
    }
    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let generation = self.control.generation.read().unwrap();
        if ctx
            .span(&id)
            .unwrap()
            .extensions()
            .get::<RecordingGeneration>()
            .map(|g| g.0)
            == Some(*generation)
        {
            self.inner.on_close(id, ctx);
        }
    }
    fn on_follows_from(&self, id: &Id, follows: &Id, ctx: Context<'_, S>) {
        let generation = self.control.generation.read().unwrap();
        if [id, follows].iter().all(|id| {
            ctx.span(id)
                .unwrap()
                .extensions()
                .get::<RecordingGeneration>()
                .map(|g| g.0)
                == Some(*generation)
        }) {
            self.inner.on_follows_from(id, follows, ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inspection::{InspectRecording, subscriber};

    #[test]
    fn log_origins_have_distinct_storage() {
        use streaming_iterator::StreamingIterator;
        let recorder = Arc::new(RecordingLayer::new());
        let dispatch = tracing::Dispatch::new(subscriber(recorder.clone()));
        tracing::dispatcher::with_default(&dispatch, || {
            // Same adapter callsite, different origins; repeating one must reuse storage.
            for target in ["compat_origin_a", "compat_origin_b", "compat_origin_a"] {
                let record = tracing_log::log::Record::builder()
                    .level(tracing_log::log::Level::Info)
                    .target(target)
                    // Exercise log.line's numeric visitor as well as string metadata.
                    .file(Some("compat_fixture.rs"))
                    .line(Some(42))
                    .args(format_args!("compat fixture"))
                    .build();
                tracing_log::format_trace(&record).unwrap();
            }
        });
        let callsites = recorder.callsites.read();
        assert_eq!(callsites.len(), 2);
        let mut origins: Vec<_> = callsites
            .values()
            .map(|data| {
                let data = data.read();
                (data.metadata.target(), data.cache[0].data.iter().count())
            })
            .collect();
        origins.sort();
        assert_eq!(origins, [("compat_origin_a", 2), ("compat_origin_b", 1)]);
    }

    #[test]
    fn changing_strings_survive_threaded_recording_and_rollover() {
        let recorder = Arc::new(RecordingLayer::new());
        let dispatch = tracing::Dispatch::new(subscriber(recorder.clone()));
        std::thread::scope(|scope| {
            for worker in 0..4 {
                let dispatch = dispatch.clone();
                scope.spawn(move || tracing::dispatcher::with_default(&dispatch, || {
                    for i in 0..300 {
                        // Temporary strings disappear after emission; the example
                        // must keep their interned owners alive for old-row reads.
                        tracing::info!(worker, i, text = %format!("historic rotation {worker}:{i}"), message = "rotation fixture");
                    }
                }));
            }
        });
        let snapshot = recorder.inspect(&Default::default()).remove(0);
        assert!(!snapshot.completed_updates.is_empty());
        let rows = recorder.rows(snapshot.id, None, 1200);
        assert_eq!(rows.len(), 1200);
        for row in rows {
            let number = |name: &str| {
                row.fields
                    .iter()
                    .find_map(|(field, value)| match value {
                        ValueData::I64(value) if field == name => Some(*value),
                        _ => None,
                    })
                    .unwrap()
            };
            let expected = format!("historic rotation {}:{}", number("worker"), number("i"));
            assert!(row.fields.iter().any(|(name, value)| name == "text"
                && matches!(value, ValueData::Str(atom) if &**atom == expected)));
        }
    }
}
