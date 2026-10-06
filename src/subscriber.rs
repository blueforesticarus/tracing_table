use std::{
    cell::RefCell,
    collections::HashMap,
    default,
    fs::metadata,
    hash::Hash,
    ops::{Add, Deref, DerefMut},
    path::Prefix,
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use parking_lot::{ArcRwLockWriteGuard, RawRwLock, RwLock};
use tracing::{Metadata, field::Visit};
use tracing_core::{Callsite, callsite::Identifier};
use tracing_subscriber::{Layer, registry::LookupSpan};

use crate::data::{blockbuffer::BlockCache, dataframe::CompressionStats, dyntuple::{Atom, BytePacker, ValueData, ValueKind}};

// Existing data is exposed for example-owned inspection; recording stays unchanged.
pub struct PerCallsiteData {
    pub id: u64,
    pub metadata: &'static Metadata<'static>,

    // used for prefix
    last_message: Option<Atom>,

    // Stored here to avoid heap allocations
    pub last_values: Vec<Option<ValueData>>,
    last_archetype: Archetype,

    call_count: usize,
    finished_count: usize,

    // Hopefully will be only one field
    pub cache: Vec<PerArchData>,
    lookup: HashMap<Archetype, usize>,
}

impl PerCallsiteData {
    fn get_arch(&mut self) -> usize {
        let archetype = &self.last_archetype; //aliased mutability issue with structs needs to be fixed. 
        let unit = archetype.unit();
        let cap = 4096 / unit;

        let i = match self.lookup.get(archetype) {
            Some(i) => *i,
            None => {
                let i = self.cache.len();
                self.lookup.insert(archetype.clone(), i);
                self.cache.push(
                    PerArchData {
                        data: BlockCache::new(unit, 1, cap),
                        // id_counter: 1.into(),
                        archetype: archetype.clone(),
                        id: i as u32,
                    }
                    .into(),
                );
                i
            }
        };
        // &mut self.cache[i]
        i // if you are going to design your language around not allowing aliased mutability
        // even in unsafe blocks. Then you need to have a way to deal with partial borrows.
        // better yet, just let be break aliased mutability in unsafe blocks.
        // this will never be a systems language
    }
}

pub struct PerArchData {
    pub data: BlockCache,
    // id_counter: AtomicU64,
    pub archetype: Archetype,
    pub id: u32,
}

// Needed because tracing_log adapter doesn't register it's callsites
fn leak_metadata<'a>(callsite: &'a Metadata<'a>) -> &'static Metadata<'static> {
    todo!()
}

// NOTE: this could include a basis to indicate whether it is system time or monotonic, and to convert between them
// it would get compacted. it might complicate lookup though.
#[derive(Debug, Clone, Copy)]
struct Time {
    value: u64,
    shift: u8,
}

pub struct RecordingLayer {
    // TODO need to seperate storage thread and layer
    pub callsites: RwLock<HashMap<Identifier, Arc<RwLock<PerCallsiteData>>>>,
    /// starts at 1
    span_id_counter: AtomicU64,
    /// starts at u32MAX + 1, to not conflict with span ids
    /// currently not stored since nothing refers to it
    event_id_counter: AtomicU64,

    /// How many bits to shift timestamp.
    /// a value of 0 corresponds to nanoseconds since epoch
    /// a value of 10 (the default) therefore has a resolution of 1.024 microseconds.
    /// You can adjust this to only store as much precision as is meaningfull.
    /// This is needed because the CompactFrame cannot filter out noise. For many callsites, timestamps will be the only recorded data.
    /// low variance (more signifigant) bits get automatically compressed when constant within a frame.
    pub timestamp_bit_shift: u8,

    system_time: SystemTime,
    started: Instant,
}

impl RecordingLayer {
    pub fn new() -> Self {
        RecordingLayer {
            callsites: Default::default(),
            span_id_counter: 1.into(),
            event_id_counter: ((1usize + std::u32::MAX as usize) as u64).into(),
            timestamp_bit_shift: 3,
            started: Instant::now(),
            system_time: SystemTime::now(),
        }
    }

    fn register(
        &self,
        metadata: &'static Metadata<'static>,
    ) -> ArcRwLockWriteGuard<RawRwLock, PerCallsiteData> {
        if let Some(v) = self.callsites.read().get(&metadata.callsite()) {
            // metadata will almost always be registered
            return v.write_arc();
        }

        let id_counter = match (metadata.is_event(), metadata.is_span()) {
            (true, false) => &self.event_id_counter,
            (false, true) => &self.span_id_counter,
            (true, true) => unreachable!(),
            (false, false) => panic!(),
        };
        let id = id_counter.fetch_add(1u64, std::sync::atomic::Ordering::Relaxed);
        let vcount = metadata.fields().len();

        self.callsites
            .write()
            .entry(metadata.callsite())
            .or_insert_with(|| {
                Arc::new(
                    PerCallsiteData {
                        id,
                        metadata,
                        last_message: None,
                        last_values: vec![None; vcount],
                        cache: Default::default(),
                        lookup: Default::default(),
                        call_count: 0,
                        finished_count: 0,
                        last_archetype: Archetype {
                            callsite: id,
                            parent: false,
                            follows_from: false,
                            field_types: vec![None; vcount],
                            extra_hash: 0,
                            is_span_end: false,
                            is_event: false,
                            // unit: 0,
                        },
                    }
                    .into(),
                )
            })
            .write_arc()
    }

    fn timestamp(&self) -> Time {
        let nanos = self.started.elapsed().as_nanos() as u64;
        Time {
            value: nanos >> self.timestamp_bit_shift,
            shift: self.timestamp_bit_shift as u8,
        }
    }

    pub fn compression_ratio(&self) -> CompressionStats {
        self.callsites
            .read()
            .values()
            .flat_map(|v| {
                v.read()
                    .cache
                    .iter()
                    .map(|v| v.data.compression_ratio())
                    .collect::<Vec<_>>()
            })
            .fold(
                CompressionStats::default(), /*TODO better estimate overhead */
                Add::add,
            )
    }
}

impl RecordingLayer {}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Archetype {
    callsite: u64, // implies metadata
    pub parent: bool,
    pub follows_from: bool,
    pub is_span_end: bool,
    pub is_event: bool,
    pub field_types: Vec<Option<ValueKind>>,
    extra_hash: usize,
}

impl Archetype {
    /// bytes needed to store this archetype, argueably should be derived elsewhere
    pub fn unit(&self) -> usize {
        let mut bytes = 0;
        if !self.is_span_end {
            bytes += std::mem::size_of::<u64>();
            if !self.is_event {
                bytes += std::mem::size_of::<u64>();
            }
        } else {
            bytes += std::mem::size_of::<SpanId>();
        }
        if self.parent {
            bytes += std::mem::size_of::<SpanId>();
        }
        if self.follows_from {
            bytes += std::mem::size_of::<SpanId>();
        }
        for field in self.field_types.iter() {
            if let Some(k) = field {
                bytes += k.unit()
            }
        }
        bytes
    }
}

#[repr(align(8))]
#[derive(Debug, Clone, Copy)]
pub struct SpanId {
    // starts at 1
    pub index: u64,

    // starts at 1
    pub callsite: u32,

    // starts at 0
    pub archetype: u32,
}

pub const PLACE_HOLDER: SpanId = SpanId {
    index: 0,
    callsite: 0,
    archetype: 0,
};

// pub struct EventData {
//     pub timestamp: u64,
//     pub parent: Option<SpanId>,
//     pub fields: Vec<ValueData>,
// }

// pub struct SpanData {
//     pub id: SpanId, //stored as index
//     pub timestamp: u64,
//     pub end_ts: u64,
//     pub parent: Option<SpanId>,
//     pub follows_from: Option<SpanId>,
//     pub fields: Vec<ValueData>,
// }

// pub struct SpanEndData {
//     pub id: SpanId,
//     pub fields: Vec<ValueData>,
// }

struct FollowFrom(SpanId);

#[derive(Clone)]
// Public tuple constructor keeps subscriber setup in the example.
pub struct LocalRecordingLayer(pub Arc<RecordingLayer>);

impl Deref for LocalRecordingLayer {
    type Target = RecordingLayer;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl<S> Layer<S> for LocalRecordingLayer
where
    S: tracing::Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_follows_from(
        &self,
        span: &tracing_core::span::Id,
        follows: &tracing_core::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let id: SpanId = *ctx.span(follows).unwrap().extensions().get().unwrap();
        ctx.span(span)
            .unwrap()
            .extensions_mut()
            .insert(FollowFrom(id));
    }

    fn on_record(
        &self,
        span: &tracing_core::span::Id,
        values: &tracing_core::span::Record<'_>,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let ctx = ctx.span(span).unwrap();
        let mut ext = ctx.extensions_mut();
        let v = ext.get_mut::<Vec<Option<ValueData>>>();

        let mut v = match v {
            Some(v) => v,
            None => {
                let v: Vec<Option<ValueData>> = vec![None; ctx.fields().len()];
                ext.insert(v);
                ext.get_mut::<Vec<Option<ValueData>>>().unwrap()
            }
        };

        let mut callsite_data = self.register(ctx.metadata());
        let callsite_data = callsite_data.deref_mut();

        let mut visitor = ValueRecorder {
            _metadata: ctx.metadata(),
            data: &mut v,
            prefix: &mut callsite_data.last_message,
        };

        // records fields directly into PerCallsiteData temp buffer.
        values.record(&mut visitor as &mut dyn Visit);
    }

    fn on_close(
        &self,
        tracing_id: tracing_core::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let time = self.timestamp();
        let ctx = ctx.span(&tracing_id).unwrap();
        let ext = ctx.extensions();
        let fields: Option<&Vec<Option<ValueData>>> = ext.get();
        let follow_from: Option<&FollowFrom> = ext.get();
        let id: SpanId = *ext.get().unwrap();

        let mut callsite_data = self.register(ctx.metadata());
        let callsite_data = callsite_data.deref_mut();
        callsite_data.finished_count += 1;
        {
            let data = &mut callsite_data.cache[id.archetype as usize].data;
            {
                let mut row = data.get_mut(id.index as usize).unwrap();
                let row = row.get_mut();
                let mut packer = BytePacker::new(row);
                packer.skip::<u64>(false);
                packer.write_any(&time.value, false); // end timestamp;
                if let Some(FollowFrom(f)) = follow_from {
                    packer.write_any(f, false)
                }
            }
            data.compute_timestamp(time.value as usize, Some(id.index as usize));
        }

        if let Some(fields) = fields {
            callsite_data.last_archetype.field_types.clear();
            callsite_data
                .last_archetype
                .field_types
                .extend(fields.iter().map(|opt| opt.as_ref().map(|v| v.into())));
            callsite_data.last_archetype.extra_hash = 0;
            callsite_data.last_archetype.follows_from = false;
            callsite_data.last_archetype.is_span_end = true;
            callsite_data.last_archetype.is_event = false;
            callsite_data.last_archetype.parent = false;

            // let callsite = callsite_data.id;
            let arch = callsite_data.get_arch();
            let arch = &mut callsite_data.cache[arch];

            let (data, _index) = arch.data.allocate();
            let mut data = BytePacker::new(data);
            // data.write_any(&time.value, false);
            data.write_any(&id, false); // end data points to start data
            // most won't have end data, so this is good
            for field in fields.iter() {
                if let Some(field) = field {
                    data.write_any(field, false);
                }
            }
            arch.data.compute_mask();
            arch.data.compute_timestamp(time.value as usize, None);
        }
    }

    fn on_new_span(
        &self,
        attrs: &tracing_core::span::Attributes<'_>,
        tracing_id: &tracing_core::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let time = self.timestamp();

        let mut span_id = None;
        if attrs.is_contextual() {
            if let Some(span) = ctx.lookup_current() {
                span_id = Some(*span.extensions().get::<SpanId>().unwrap_or(&PLACE_HOLDER));
            }
        } else if let Some(span) = attrs.parent() {
            if let Some(span) = ctx.span(span) {
                span_id = Some(*span.extensions().get::<SpanId>().unwrap_or(&PLACE_HOLDER));
            }
        }

        let mut callsite_data = self.register(attrs.metadata());
        let callsite_data = callsite_data.deref_mut();
        callsite_data.call_count += 1;

        callsite_data.last_values.fill(None);
        let mut visitor = ValueRecorder {
            _metadata: attrs.metadata(),
            data: &mut callsite_data.last_values,
            prefix: &mut callsite_data.last_message,
        };

        // records fields directly into PerCallsiteData temp buffer.
        attrs.record(&mut visitor as &mut dyn Visit);

        // we use callsite data's copy so we can avoid allocating a vec
        callsite_data.last_archetype.field_types.clear();
        callsite_data.last_archetype.field_types.extend(
            callsite_data
                .last_values
                .iter()
                .map(|opt| opt.as_ref().map(|v| v.into())),
        );
        callsite_data.last_archetype.extra_hash = 0;
        callsite_data.last_archetype.follows_from = true; //only known later
        callsite_data.last_archetype.is_span_end = false;
        callsite_data.last_archetype.is_event = false;
        callsite_data.last_archetype.parent = span_id.is_some();

        let callsite = callsite_data.id;
        let arch = callsite_data.get_arch();
        let arch = &mut callsite_data.cache[arch];

        // Id not used for events

        // all that is left is to write to the store.
        let (data, index) = arch.data.allocate();

        let id = SpanId {
            index: index as u64,
            callsite: callsite as u32,
            archetype: arch.id as u32,
        };

        ctx.span(tracing_id).unwrap().extensions_mut().insert(id);

        // NOTE for now I'm choosing to reduce overhead by writing the fields unaligned.
        // probably should change this in the future.
        // NOTE: with compaction, probably could use serialization.
        let mut data = BytePacker::new(data);
        data.write_any(&time.value, false);
        data.write_any(&0u64, false); //space for end_timestamp
        data.write_any(&PLACE_HOLDER, false); // space for follows_from
        if arch.archetype.parent {
            data.write_any(&span_id.unwrap(), false);
        }
        for field in callsite_data.last_values.iter() {
            if let Some(field) = field {
                data.write_any_value(field, false);
            }
        }
        arch.data.compute_mask();
        arch.data.compute_timestamp(time.value as usize, None);
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let time = self.timestamp();

        let mut span_id = None;

        if event.is_contextual() {
            if let Some(span) = ctx.lookup_current() {
                span_id = Some(*span.extensions().get::<SpanId>().unwrap_or(&PLACE_HOLDER));
            }
        } else if let Some(span) = event.parent() {
            if let Some(span) = ctx.span(span) {
                span_id = Some(*span.extensions().get::<SpanId>().unwrap_or(&PLACE_HOLDER));
            }
        }

        let mut callsite_data = self.register(event.metadata());
        let callsite_data = callsite_data.deref_mut();
        callsite_data.call_count += 1;

        // event id's currently not stored. since nothing refers to them
        callsite_data.last_values.fill(None);
        let mut visitor = ValueRecorder {
            _metadata: event.metadata(),
            data: &mut callsite_data.last_values,
            prefix: &mut callsite_data.last_message,
        };

        // records fields directly into PerCallsiteData buffer.
        event.record(&mut visitor as &mut dyn Visit);

        // we use callsite data's copy so we can avoid allocating a vec
        callsite_data.last_archetype.field_types.clear();
        callsite_data.last_archetype.field_types.extend(
            callsite_data
                .last_values
                .iter()
                .map(|opt| opt.as_ref().map(|v| v.into())),
        );
        callsite_data.last_archetype.extra_hash = 0;
        callsite_data.last_archetype.follows_from = false;
        callsite_data.last_archetype.parent = span_id.is_some();
        callsite_data.last_archetype.is_span_end = false;
        callsite_data.last_archetype.is_event = true;

        let callsite = callsite_data.id;
        let arch = callsite_data.get_arch();
        let arch = &mut callsite_data.cache[arch];

        // Id not used for events

        // all that is left is to write to the store.
        let (data, index) = arch.data.allocate();

        let _id = SpanId {
            index: index as u64,
            callsite: callsite as u32,
            archetype: arch.id as u32,
        };

        // NOTE for now I'm choosing to reduce overhead by writing the fields unaligned.
        // probably should change this in the future.
        // NOTE: with compaction, probably could use serialization.
        let mut data = BytePacker::new(data);
        data.write_any(&time.value, false);
        if arch.archetype.parent {
            data.write_any(&span_id.unwrap(), false);
        }
        for field in callsite_data.last_values.iter() {
            if let Some(field) = field {
                data.write_any_value(field, false);
            }
        }
        arch.data.compute_mask();
        arch.data.compute_timestamp(time.value as usize, None);
    }
}

// ----------------  CRUFT ------------------------

struct ValueRecorder<'a> {
    _metadata: &'a Metadata<'a>,

    // Note this can be a buffer so we aren't allocing on every log.
    data: &'a mut [Option<ValueData>],
    prefix: &'a mut Option<Atom>,
}

impl<'a> Visit for ValueRecorder<'a> {
    fn record_str(&mut self, field: &tracing_core::Field, value: &str) {
        if field.name() == "message" {
            match self.prefix {
                Some(p) => {
                    let mut new_prefix_index = None; // in bytes
                    for (p_char, value_char) in p.char_indices().zip(value.chars()) {
                        if p_char.1 != value_char {
                            new_prefix_index = Some(p_char.0);
                            break;
                        }
                    }

                    match new_prefix_index {
                        Some(0) => {
                            *p = Atom::from(value);
                            self.data[field.index()] =
                                Some(ValueData::PrefixStr(p.clone(), Atom::from("")))
                        }
                        Some(n) => {
                            let (prefix, rem) = value.split_at(n);
                            *p = Atom::from(prefix);
                            self.data[field.index()] =
                                Some(ValueData::PrefixStr(p.clone(), Atom::from(rem)))
                        }
                        None => {
                            let (prefix, rem) = value.split_at(p.len());
                            self.data[field.index()] =
                                Some(ValueData::PrefixStr(p.clone(), Atom::from(rem)))
                        }
                    }
                }
                None => {
                    *self.prefix = Some(Atom::from(value));
                    self.data[field.index()] =
                        Some(ValueData::PrefixStr(Atom::from(value), Atom::from("")))
                }
            }
        } else {
            self.data[field.index()] = Some(ValueData::Str(Atom::from(value)));
        }
    }

    fn record_debug(&mut self, field: &tracing_core::Field, value: &dyn std::fmt::Debug) {
        self.record_str(field, &format!("{:?}", value));
    }

    fn record_f64(&mut self, field: &tracing_core::Field, value: f64) {
        self.data[field.index()] = Some(ValueData::F64(value));
    }

    fn record_i64(&mut self, field: &tracing_core::Field, value: i64) {
        self.data[field.index()] = Some(ValueData::I64(value));
    }

    fn record_u64(&mut self, field: &tracing_core::Field, value: u64) {
        self.data[field.index()] = Some(ValueData::U64(value));
    }

    fn record_i128(&mut self, field: &tracing_core::Field, value: i128) {
        self.data[field.index()] = Some(ValueData::I128(value));
    }

    fn record_u128(&mut self, field: &tracing_core::Field, value: u128) {
        self.data[field.index()] = Some(ValueData::U128(value));
    }

    fn record_bool(&mut self, field: &tracing_core::Field, value: bool) {
        self.data[field.index()] = Some(ValueData::Bool(value));
    }

    fn record_bytes(&mut self, field: &tracing_core::Field, value: &[u8]) {
        // XXX unimplemented
        //self.record_debug(field, &HexBytes(value))
    }

    fn record_error(
        &mut self,
        field: &tracing_core::Field,
        value: &(dyn std::error::Error + 'static),
    ) {
        self.record_debug(field, &value.to_string());
    }
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use streaming_iterator::StreamingIterator;
    use tracing::{Level, debug, info, span};
    use tracing_subscriber::{Registry, layer::SubscriberExt};

    use crate::subscriber::{LocalRecordingLayer, RecordingLayer};

    #[test]
    fn log_and_query() {
        // Set up the tracing registry with the RecordingLayer
        let recording_layer = LocalRecordingLayer(Arc::new(RecordingLayer::new()));
        let subscriber = Registry::default().with(recording_layer.clone());
        tracing::subscriber::set_global_default(subscriber)
            .expect("Failed to set global subscriber");

        // Generate logs in a loop
        for i in 0..1000 {
            let span = span!(Level::INFO, "test_span", iteration = i);
            let _enter = span.enter();
            info!(message = "Logging some information", value = i);
            debug!(message = "test stupid log message {}", i);
        }

        // Query logs from the RecordingLayer
        let compression_stats = recording_layer.compression_ratio();
        println!("Compression stats: {:?}", compression_stats);

        let callsite_data = recording_layer.callsites.read();
        for (identifier, data) in callsite_data.iter() {
            for arch in data.read().cache.iter() {
                assert_eq!(arch.data.iter().count(), 1000);
            }
        }
    }
}
