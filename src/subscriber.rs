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
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::{Instant, SystemTime},
};

use parking_lot::{ArcRwLockWriteGuard, RawRwLock, RwLock};
use tracing::{
    Level, Metadata,
    field::{FieldSet, Visit},
};
use tracing_core::{
    Callsite, Kind,
    callsite::{self, Identifier},
};
use tracing_log::NormalizeEvent;
use tracing_subscriber::{Layer, registry::LookupSpan};

use crate::data::stringcache::{Index, Stats};

use crate::data::{
    blockbuffer::BlockCache,
    dataframe::CompressionStats,
    dyntuple::{Atom, BytePacker, ValueData, ValueKind},
};

mod dump;

/// Controls indexed string storage for subsequently recorded fields in all layers.
/// Defaults to Atom storage; toggle with store(value, Ordering::Relaxed).
pub static USE_INDEXED_ATOM: AtomicBool = AtomicBool::new(false);

// Existing data is exposed for example-owned inspection; recording stays unchanged.
pub struct PerCallsiteData {
    pub id: u64,
    pub metadata: &'static Metadata<'static>,

    // used for prefix
    last_message: Option<Atom>,

    // Stored here to avoid heap allocations
    pub field_value_scratchpad: Vec<Option<ValueData>>,
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

/// the tracing_log crate adapts the log crates message to tracing events.
/// log does not use 'static metadata. this function stores the strings and metadata perminantly so we can use it with the rest of the logger.
/// it dedups by both string and metadata
pub fn log_callsite_adapter(e: &'_ tracing::Event) -> &'static Metadata<'static> {
    const MAX_CALLSITES: usize = 4000;
    const MAX_STRINGS: usize = MAX_CALLSITES * 3 + 1; //should not be hit

    if !e.is_log() {
        return e.metadata();
    }

    // get normalized metadata with tracing_log::NormalizedMetadata. does no allocations
    // NOTE log::Record has file_static() and module_path_static() which return options if the strings are in fact static.
    // we could rewrite tracing_log to more intelligently produce 'static metadata.
    // especially nice since we could skip string equality checks
    let meta = e.normalized_metadata().expect("is_log");

    fn intern(text: &str) -> &'static str {
        use parking_lot::RwLock;
        use std::{collections::HashSet, sync::LazyLock};

        // One permanent copy per distinct string, shared across calls.
        static STRINGS: LazyLock<RwLock<HashSet<&'static str>>> = LazyLock::new(Default::default);

        if let Some(&stored) = STRINGS.read().get(text) {
            return stored;
        }

        let mut strings = STRINGS.write();
        // Another thread may have inserted it before we acquired the write lock.
        if let Some(&stored) = strings.get(text) {
            return stored;
        }

        let stored: &'static str = Box::leak(text.to_owned().into_boxed_str());
        assert!(
            strings.len() < MAX_STRINGS,
            "should never happen, MAX_CALLSITES should be hit first"
        );
        strings.insert(stored);

        stored
    }

    // intern strings that are 'a
    let target = intern(meta.target());
    let file = meta.file().map(intern);
    let module = meta.module_path().map(intern);

    // Interned strings have unique addresses: metadata lookup hashes no string bytes.
    // The adapter callsite identifies the level; Options preserve missing vs empty.
    type Key = (
        Identifier,
        Level,
        usize,
        Option<usize>,
        Option<usize>,
        Option<u32>,
    );
    static METADATA: std::sync::LazyLock<RwLock<HashMap<Key, &'static Metadata<'static>>>> =
        std::sync::LazyLock::new(Default::default);
    let key = (
        meta.callsite(),
        *meta.level(),
        target.as_ptr() as usize,
        module.map(|text| text.as_ptr() as usize),
        file.map(|text| text.as_ptr() as usize),
        meta.line(),
    );
    if let Some(&stored) = METADATA.read().get(&key) {
        return stored;
    }

    // Recheck under the write lock before allocating, so concurrent misses leak once.
    let mut metadata = METADATA.write();
    assert!(
        metadata.len() < MAX_CALLSITES,
        "most likely log targets are being dynamically generated"
    );
    metadata.entry(key).or_insert_with(|| {
        let static_meta = Metadata::new(
            "compat:log",
            target,
            *meta.level(),
            file,
            meta.line(),
            module,
            FieldSet::new(&["message"], meta.callsite()),
            Kind::EVENT,
        );

        Box::leak(Box::new(static_meta))
    })
}

/// Row-header time: nanoseconds since the recorder started, shifted right by
/// RecordingLayer::timestamp_bit_shift. Shared by row sizing, writing, and decoding.
pub type PackedTimestamp = u64;

// NOTE: this could include a basis to indicate whether it is system time or monotonic, and to convert between them
// it would get compacted. it might complicate lookup though.
#[derive(Debug, Clone, Copy)]
struct Time {
    value: PackedTimestamp,
    shift: u8,
}

// literally a &'static Metadata
// cannot use tracing callsite id because tracing_log uses one callsite for all logs
type CallsiteId = usize;

pub struct RecordingLayer {
    // TODO need to seperate storage thread and layer
    // Canonical metadata addresses distinguish log origins sharing tracing-log's Identifier.
    pub callsites: RwLock<HashMap<CallsiteId, Arc<RwLock<PerCallsiteData>>>>,

    /// these identify callsites
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
    /// TODO could be automatic based on time between messages.
    pub timestamp_bit_shift: u8,

    /// XXX an actual problem with our system is that accurate log ordering cannot be recreated at the moment. Timestamps can be equal and we dont have ordering info across across archetypes
    /// we probably also want thread number in the log somewhere
    // pub last_ts: usize,
    // pub ts_count: usize,
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
        let key = metadata as *const Metadata<'static> as usize;
        if let Some(v) = self.callsites.read().get(&key) {
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
            .entry(key)
            .or_insert_with(|| {
                Arc::new(
                    PerCallsiteData {
                        id,
                        metadata,
                        last_message: None,
                        field_value_scratchpad: vec![None; vcount],
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
            bytes += std::mem::size_of::<PackedTimestamp>();
            if !self.is_event {
                bytes += std::mem::size_of::<PackedTimestamp>(); //end timestamp
                bytes += std::mem::size_of::<SpanFieldsRef>();
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

/// Location of deferred span fields in the same callsite. Blocks are one-based;
/// block zero means no update row. Completed block positions never move.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpanFieldsRef {
    pub archetype: u32,
    pub block: u32,
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
            is_log_compat: false,
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
        let mut ext = ctx.extensions_mut();
        let follow_from = ext.get_mut::<FollowFrom>().map(|value| value.0);
        let id: SpanId = *ext.get_mut().unwrap();
        let fields: Option<&mut Vec<Option<ValueData>>> = ext.get_mut();

        let mut callsite_data = self.register(ctx.metadata());
        let callsite_data = callsite_data.deref_mut();
        callsite_data.finished_count += 1;

        let extra_fields = if let Some(fields) = fields {
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
            // Fix the packed type here so a reference cannot silently become the row value.
            data.write_any::<SpanId>(id, false); // end data points to start data
            // most won't have end data, so this is good
            // Transfer each value to the row; the packer keeps its Atom bits alive.
            for field in fields.iter_mut() {
                if let Some(field) = field.take() {
                    data.write_any_value(field, false);
                }
            }
            arch.data.compute_mask();
            arch.data.compute_timestamp(time.value as usize, None);
            // allocate() can roll over the active block, so capture its ordinal afterward.
            SpanFieldsRef {
                archetype: arch.id,
                block: u32::try_from(arch.data.ring.len() + 1)
                    .expect("span update block exceeds u32"),
            }
        } else {
            SpanFieldsRef::default()
        };
        {
            let data = &mut callsite_data.cache[id.archetype as usize].data;
            {
                let mut row = data.get_mut(id.index as usize).unwrap();
                let row = row.get_mut();
                let mut packer = BytePacker::new(row);
                packer.skip::<PackedTimestamp>(false);
                packer.write_any::<PackedTimestamp>(time.value, false); // end timestamp;
                packer.write_any::<SpanFieldsRef>(extra_fields, false);
                if let Some(f) = follow_from {
                    packer.write_any::<SpanId>(f, false)
                }
            }
            data.compute_timestamp(time.value as usize, Some(id.index as usize));
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

        callsite_data.field_value_scratchpad.fill(None);
        let mut visitor = ValueRecorder {
            _metadata: attrs.metadata(),
            data: &mut callsite_data.field_value_scratchpad,
            prefix: &mut callsite_data.last_message,
            is_log_compat: false,
        };

        // records fields directly into PerCallsiteData temp buffer.
        attrs.record(&mut visitor as &mut dyn Visit);

        // we use callsite data's copy so we can avoid allocating a vec
        callsite_data.last_archetype.field_types.clear();
        callsite_data.last_archetype.field_types.extend(
            callsite_data
                .field_value_scratchpad
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
        data.write_any::<PackedTimestamp>(time.value, false);
        data.write_any::<PackedTimestamp>(PackedTimestamp::default(), false); // space for end_timestamp
        data.write_any::<SpanFieldsRef>(SpanFieldsRef::default(), false); // deferred fields are unknown until close
        data.write_any::<SpanId>(PLACE_HOLDER, false); // space for follows_from
        if arch.archetype.parent {
            data.write_any::<SpanId>(span_id.unwrap(), false);
        }
        for field in callsite_data.field_value_scratchpad.iter_mut() {
            if let Some(field) = field.take() {
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

        let is_log_compat = event.is_log();
        let metadata = log_callsite_adapter(event);

        let mut callsite_data = self.register(metadata);
        let callsite_data = callsite_data.deref_mut();
        callsite_data.call_count += 1;

        // event id's currently not stored. since nothing refers to them
        callsite_data.field_value_scratchpad.fill(None);
        let mut visitor = ValueRecorder {
            _metadata: metadata,
            data: &mut callsite_data.field_value_scratchpad,
            prefix: &mut callsite_data.last_message,
            is_log_compat,
        };

        // records fields directly into PerCallsiteData buffer.
        event.record(&mut visitor as &mut dyn Visit);

        // we use callsite data's copy so we can avoid allocating a vec
        callsite_data.last_archetype.field_types.clear();
        callsite_data.last_archetype.field_types.extend(
            callsite_data
                .field_value_scratchpad
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
        data.write_any::<PackedTimestamp>(time.value, false);
        if arch.archetype.parent {
            data.write_any::<SpanId>(span_id.unwrap(), false);
        }
        for field in callsite_data.field_value_scratchpad.iter_mut() {
            if let Some(field) = field.take() {
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

    is_log_compat: bool,
}

impl<'a> Visit for ValueRecorder<'a> {
    fn record_str(&mut self, field: &tracing_core::Field, value: &str) {
        if self.is_log_compat && field.name() != "message" {
            // do not store log.target and other metadata, we move that into the archetype with adapter.
            // also we remove them from the FieldSet, so this would panic since there is no space allocated for them
            return;
        }

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
                            // A shorter message can exhaust the character comparison.
                            let (prefix, rem) = value.split_at(p.len().min(value.len()));
                            *p = Atom::from(prefix);
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

        // Convert after prefix splitting so both string modes share that behavior.
        if USE_INDEXED_ATOM.load(Ordering::Relaxed) {
            let slot = &mut self.data[field.index()];
            *slot = slot.take().map(|value| match value {
                ValueData::Str(atom) => ValueData::StrIndex(atom.into()),
                ValueData::PrefixStr(prefix, remainder) => {
                    ValueData::PrefixStrIndex(prefix.into(), remainder.into())
                }
                value => value,
            });
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
        // log.line lives in normalized metadata, outside the message-only row buffer.
        if self.is_log_compat {
            return;
        }
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
