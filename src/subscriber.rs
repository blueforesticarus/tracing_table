use std::{
    cell::RefCell,
    collections::HashMap,
    default,
    fs::metadata,
    hash::Hash,
    ops::{Add, Deref, DerefMut},
    sync::{
        Arc, LazyLock,
        atomic::{AtomicU64, AtomicUsize, Ordering},
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

use crate::data::{
    CompressionStats,
    blockbuffer::BlockCache,
    dyntuple::{BytePacker, ValueData, ValueKind},
    stringbuffer::{Buffer, BufferCollection, ExpandedBuffer},
};

// Portable capture/loading is opt-in; live recording does not depend on its state.
pub mod dump;
pub mod read;
mod value;
pub use value::InputValue;
// Templates resolve once per callsite, with a source fallback during development.
pub mod template;

/// Uncompressed bytes per archetype block; the reference comparison favors 4 KiB.
/// Capacity rounds down to whole rows.
pub const MESSAGE_BLOCK_TARGET_SIZE: usize = 4 * 1024;

/// Target uncompressed bytes (32 KiB by default); set via env or `.store(bytes, Ordering::Relaxed)`.
#[rustfmt::skip]
pub static STRING_BUFFER_TARGET_SIZE: LazyLock<AtomicUsize> = LazyLock::new(|| {
    AtomicUsize::new(std::env::var("STRING_BUFFER_TARGET_SIZE").ok().and_then(|s| s.parse().ok()).unwrap_or(1 << 15))
});

/// Choose a string reference once per callsite. Both forms use u32 in raw rows;
/// indices have smaller numeric ranges for the ordinary compaction algorithms.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StringStorage {
    #[default]
    Offset,
    Index,
}
impl StringStorage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Offset => "offset",
            Self::Index => "index",
        }
    }

    fn kind(self, value: &ValueData) -> ValueKind {
        match value {
            ValueData::BufferOffset(_) | ValueData::BufferIndex(_) => match self {
                Self::Offset => ValueKind::BufferOffset,
                Self::Index => ValueKind::BufferIndex,
            },
            _ => ValueKind::from(value),
        }
    }

    fn intern(self, buffer: &mut Buffer, text: &str) -> ValueData {
        let (index, offset) = buffer.add_string(text);
        match self {
            Self::Offset => {
                ValueData::BufferOffset(u32::try_from(offset).expect("string offset exceeds u32"))
            }
            Self::Index => {
                ValueData::BufferIndex(u32::try_from(index).expect("string index exceeds u32"))
            }
        }
    }

    fn read(self, buffer: &ExpandedBuffer, reference: u32) -> &str {
        match self {
            Self::Offset => buffer.get_offset(reference as usize),
            Self::Index => buffer.get_index(reference as usize),
        }
        .expect("stored string reference")
    }
}

// Existing data is exposed for example-owned inspection; recording stays unchanged.
pub struct PerCallsiteData {
    pub id: u64,
    pub metadata: &'static Metadata<'static>,
    /// Resolve once on registration; retain None so failed lookups don't repeat.
    pub template: Option<template::CompiledTemplate>,
    /// Fixed at registration so previously stored rows keep their interpretation.
    pub string_storage: StringStorage,

    // Reuse each field's string capacity across recordings.
    field_value_scratchpad: FieldValues,
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
        let cap = MESSAGE_BLOCK_TARGET_SIZE / unit;

        let i = match self.lookup.get(archetype) {
            Some(i) => *i,
            None => {
                let i = self.cache.len();
                self.lookup.insert(archetype.clone(), i);
                self.cache.push(
                    PerArchData {
                        data: BlockCache::new(unit, 1, cap),
                        strings: BufferCollection::default(),
                        chunk_string_start: 0,
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
        i
    }
}

pub struct PerArchData {
    pub data: BlockCache,
    pub strings: BufferCollection,
    // Uncompressed bytes when the active message chunk began. (used to decide when to replace buffer)
    chunk_string_start: usize,
    // id_counter: AtomicU64,
    pub archetype: Archetype,
    pub id: u32,
}

// TODO this and cycle_if_needed ought to be next to each other.
impl PerArchData {
    /// Stored bytes for all active/compressed chunks and string buffers, counted
    /// once each. Keep sizing here so it can be precomputed later without changing eviction.
    fn storage_size(&self) -> usize {
        let stats = self.data.compression_ratio() + self.strings.compression_ratio();
        stats.compressed + stats.overhead + stats.overalloc
    }

    /// Keep message-chunk policy here; BufferCollection only knows how to rotate.
    fn prepare_chunk(&mut self) -> bool {
        // Check caps only on initial allocation, frame growth, or chunk rollover.
        let allocated = self.data.active.length == self.data.active.capacity()
            || (self.data.count == 1 && self.data.active.length == 0);
        self.data.cycle_if_needed();
        // Only an empty chunk can change buffers, regardless of who cycled it.
        if self.data.active.length == 0 {
            let size = self.strings.active.byte_len();
            let delta = size - self.chunk_string_start;
            // Rotate if another equal-growth chunk would approach (or pass) the
            // target within half a delta. Oversized chunks remain indivisible.
            let projected = size.saturating_add(delta).saturating_add(delta / 2);
            if size > 0 && projected >= STRING_BUFFER_TARGET_SIZE.load(Ordering::Relaxed) {
                self.strings.rotate(false); // Expansion rebuilds the index from entry lengths.
            }
            self.chunk_string_start = self.strings.active.byte_len();
            self.data.string_buffer = self.strings.active_index();
        }
        allocated
    }
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

    /// Used for newly registered callsites; configure before recording.
    pub string_storage: StringStorage,

    /// Retained chunk/string storage limits in bytes; active storage is protected.
    pub global_memory_cap: usize,
    pub callsite_memory_cap: usize,

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
            string_storage: StringStorage::Offset,
            global_memory_cap: 20 * 1024 * 1024,
            callsite_memory_cap: 256 * 1024,
            started: Instant::now(),
            system_time: SystemTime::now(),
        }
    }

    fn register(
        &self,
        metadata: &'static Metadata<'static>,
    ) -> ArcRwLockWriteGuard<RawRwLock, PerCallsiteData> {
        self.register_with_template(metadata, || {
            if metadata.is_event() && metadata.fields().field("message").is_some() {
                template::lookup(
                    metadata.file().unwrap_or("<unknown>"),
                    metadata.line().unwrap_or(0),
                    metadata.name() == "compat:log",
                    metadata.module_path(),
                )
            } else {
                None
            }
        })
    }

    // seperated out so benchmark log replay can provide stored template.
    fn register_with_template(
        &self,
        metadata: &'static Metadata<'static>,
        resolve: impl FnOnce() -> Option<template::CompiledTemplate>,
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
                let template = resolve();
                let acount = template.as_ref().map_or(0, |t| t.field_names.len());
                let fields = FieldValues::new(vcount, acount);
                Arc::new(
                    PerCallsiteData {
                        id,
                        metadata,
                        template,
                        string_storage: self.string_storage,
                        field_value_scratchpad: fields,
                        cache: Default::default(),
                        lookup: Default::default(),
                        call_count: 0,
                        finished_count: 0,
                        last_archetype: Archetype {
                            callsite: id,
                            parent: false,
                            follows_from: false,
                            field_types: vec![None; vcount],
                            template: None,
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
                    .map(|v| v.data.compression_ratio() + v.strings.compression_ratio())
                    .collect::<Vec<_>>()
            })
            .fold(
                CompressionStats::default(), /*TODO better estimate overhead */
                Add::add,
            )
    }

    /// After a frame allocation, enforce the allocating callsite's cap first,
    /// then the global cap. Active chunks and active string buffers are protected.
    /// CODEX GENERATED
    /// TODO cleanup
    fn drop_old_chunks(&self, callsite: u64) {
        // Unlimited retention needs no accounting or eviction candidates. Replay
        // uses this mode so growing archives do not trigger repeated full scans.
        if self.global_memory_cap == usize::MAX && self.callsite_memory_cap == usize::MAX {
            return;
        }
        // Copy handles, not lock guards. Each scan holds just one callsite read lock;
        // deletion takes only the selected callsite's write lock.
        let sites: Vec<_> = self.callsites.read().values().cloned().collect();
        let bytes = |s: CompressionStats| s.compressed + s.overhead + s.overalloc;
        loop {
            let mut total = 0;
            let mut local = None;
            let mut global = None;
            let mut oldest = usize::MAX;
            let mut best_score = -1.0;
            for (site_index, shared) in sites.iter().enumerate() {
                let site = shared.read();
                let size: usize = site.cache.iter().map(PerArchData::storage_size).sum();
                total += size;
                for (arch_index, arch) in site.cache.iter().enumerate() {
                    let Some(chunk) = arch.data.ring.first() else {
                        continue;
                    };
                    let candidate = (site_index, arch_index, chunk.index);
                    // The per-callsite cap chooses the oldest chunk across its archetypes.
                    if site.id == callsite
                        && size > self.callsite_memory_cap
                        && (local.is_none() || chunk.timestamp.start < oldest)
                    {
                        oldest = chunk.timestamp.start;
                        local = Some(candidate);
                    }
                    // Global candidates are each archetype's oldest chunk. Buffer IDs
                    // increase with chunk age, so users of this buffer form a prefix.
                    let buffer_id = chunk.string_buffer;
                    let users = arch
                        .data
                        .ring
                        .partition_point(|c| c.string_buffer == buffer_id)
                        + usize::from(arch.data.string_buffer == buffer_id);
                    let buffer_bytes = if buffer_id == arch.strings.active_index() {
                        bytes(arch.strings.active.compression_ratio())
                    } else {
                        bytes(
                            arch.strings.compressed[buffer_id - arch.strings.offset()]
                                .compression_ratio(),
                        )
                    };
                    let duration = chunk
                        .timestamp
                        .end
                        .saturating_sub(chunk.timestamp.start)
                        .max(1);
                    // Prefer more stored bytes per timestamp tick, charging only this
                    // chunk's share of its buffer. Zero-duration chunks use one tick.
                    let score = (size_of_val(chunk.data.as_ref()) as f64
                        + size_of_val(chunk) as f64
                        + buffer_bytes as f64 / users as f64)
                        / duration as f64;
                    if score > best_score {
                        best_score = score;
                        global = Some(candidate);
                    }
                }
            }
            let candidate = local.or_else(|| {
                if total > self.global_memory_cap {
                    global
                } else {
                    None
                }
            });
            // Active storage alone may exceed a cap; stop when no completed chunks remain.
            let Some((site_index, arch_index, index)) = candidate else {
                break;
            };

            // Aquire write lock so we can delete it.
            let mut site = sites[site_index].write();
            let arch = &mut site.cache[arch_index];
            // Another sweep may have removed the candidate; the cache validates it.
            if arch.data.drop_chunk(index).is_err() {
                continue;
            }
            // Release frozen buffers only after their last chunk is gone. With no
            // completed chunks left, discard_before(active_index) still keeps the active buffer.
            let new_oldest = arch.data.ring.first();
            let first_buffer = new_oldest
                .map(|chunk| chunk.string_buffer)
                .unwrap_or(arch.strings.active_index());
            arch.strings.discard_before(first_buffer);
        }
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

    /// Matched template argument count; Some(0) represents a matched literal message.
    pub template: Option<usize>,
    extra_hash: usize,
}

impl Archetype {
    /// Full packed row width: header, metadata fields, then template argument offsets.
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
        // Template references use u32 for either offset or index addressing.
        bytes + self.template.unwrap_or(0) * size_of::<u32>()
    }
}

// Fix field order and verify that raw header writes cannot include padding.
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy, bytemuck::NoUninit)]
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, bytemuck::NoUninit)]
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

// Layer callbacks resolve tracing context and sample the clock. The storage
// methods below accept these inputs explicitly, also making deterministic import possible.
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
        id: &tracing_core::span::Id,
        values: &tracing_core::span::Record<'_>,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let span = ctx.span(id).unwrap();
        let mut ext = span.extensions_mut();
        if ext.get_mut::<FieldValues>().is_none() {
            ext.insert(FieldValues::new(span.fields().len(), 0));
        }
        ext.get_mut::<FieldValues>()
            .unwrap()
            .record(|visitor| values.record(visitor));
    }

    fn on_close(
        &self,
        tracing_id: tracing_core::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let time = self.timestamp();
        let span = ctx.span(&tracing_id).unwrap();
        let mut ext = span.extensions_mut();
        let follows = ext.get_mut::<FollowFrom>().map(|value| value.0);
        let id = *ext.get_mut::<SpanId>().unwrap();
        self.close_span(
            span.metadata(),
            id,
            time,
            follows,
            ext.get_mut::<FieldValues>(),
        );
    }

    fn on_new_span(
        &self,
        attrs: &tracing_core::span::Attributes<'_>,
        id: &tracing_core::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let time = self.timestamp();
        let parent = if attrs.is_contextual() {
            ctx.lookup_current()
        } else {
            attrs.parent().and_then(|id| ctx.span(id))
        };
        let parent = parent.map(|span| *span.extensions().get::<SpanId>().unwrap_or(&PLACE_HOLDER));
        let stored = self.record_start(attrs.metadata(), time, parent, false, |visitor| {
            attrs.record(visitor)
        });
        ctx.span(id).unwrap().extensions_mut().insert(stored);
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let time = self.timestamp();
        let parent = if event.is_contextual() {
            ctx.lookup_current()
        } else {
            event.parent().and_then(|id| ctx.span(id))
        };
        let parent = parent.map(|span| *span.extensions().get::<SpanId>().unwrap_or(&PLACE_HOLDER));
        self.record_start(
            log_callsite_adapter(event),
            time,
            parent,
            event.is_log(),
            |visitor| event.record(visitor),
        );
    }
}

impl RecordingLayer {
    // Events and span starts differ only in their headers and message matching.
    // Both use this one path for archetyping, strings, rollover, and retention.
    fn record_start(
        &self,
        metadata: &'static Metadata<'static>,
        time: Time,
        parent: Option<SpanId>,
        is_log_compat: bool,
        record: impl FnOnce(&mut dyn Visit),
    ) -> SpanId {
        let is_event = metadata.is_event();
        let mut guard = self.register(metadata);
        let site = guard.deref_mut();
        site.call_count += 1;
        site.field_value_scratchpad.values.fill(None);
        let mut visitor = ValueRecorder {
            data: &mut site.field_value_scratchpad,
            is_log_compat,
            template: if is_event {
                site.template.as_ref()
            } else {
                None
            },
            template_arg_count: None,
        };
        record(&mut visitor);
        site.last_archetype.template = visitor.template_arg_count;
        site.last_archetype.field_types.clear();
        site.last_archetype.field_types.extend(
            site.field_value_scratchpad
                .values
                .iter()
                .map(|value| value.as_ref().map(|v| site.string_storage.kind(v))),
        );
        site.last_archetype.extra_hash = 0;
        site.last_archetype.follows_from = !is_event;
        site.last_archetype.parent = parent.is_some();
        site.last_archetype.is_span_end = false;
        site.last_archetype.is_event = is_event;
        let callsite = site.id;
        let index = site.get_arch();
        let arch = &mut site.cache[index];
        let allocated = arch.prepare_chunk();
        let (bytes, index) = arch.data.allocate();
        let id = SpanId {
            index: index as u64,
            callsite: callsite as u32,
            archetype: arch.id,
        };
        let mut packer = BytePacker::new(bytes);
        packer.write_any_value(time.value, false);
        if !is_event {
            packer.write_any_value(PackedTimestamp::default(), false);
            packer.write_any_value(SpanFieldsRef::default(), false);
            packer.write_any_value(PLACE_HOLDER, false);
        }
        if let Some(parent) = parent {
            packer.write_any_value(parent, false);
        }
        let fields = &mut site.field_value_scratchpad;
        fields.write(&mut packer, &mut arch.strings.active, site.string_storage);
        if let Some(count) = arch.archetype.template {
            for argument in &fields.template_args[..count] {
                packer.write_any_value(
                    site.string_storage.intern(&mut arch.strings.active, argument),
                    false,
                );
            }
        }
        arch.data.update_mask_after_append();
        arch.data.update_ts_range(time.value as usize, None);
        drop(guard);
        if allocated {
            self.drop_old_chunks(callsite);
        }
        id
    }

    fn close_span(
        &self,
        metadata: &'static Metadata<'static>,
        id: SpanId,
        time: Time,
        follow_from: Option<SpanId>,
        fields: Option<&mut FieldValues>,
    ) {
        let mut callsite_guard = self.register(metadata);
        let callsite_data = callsite_guard.deref_mut();
        let callsite = callsite_data.id;
        callsite_data.finished_count += 1;

        let mut allocated = false;
        let extra_fields = if let Some(fields) = fields {
            callsite_data.last_archetype.field_types.clear();
            callsite_data.last_archetype.field_types.extend(
                fields
                    .values
                    .iter()
                    .map(|opt| opt.as_ref().map(|v| callsite_data.string_storage.kind(v))),
            );
            callsite_data.last_archetype.extra_hash = 0;
            callsite_data.last_archetype.follows_from = false;
            callsite_data.last_archetype.is_span_end = true;
            callsite_data.last_archetype.is_event = false;
            callsite_data.last_archetype.parent = false;

            // let callsite = callsite_data.id;
            let arch = callsite_data.get_arch();
            let arch = &mut callsite_data.cache[arch];

            allocated = arch.prepare_chunk();
            let (data, _index) = arch.data.allocate();
            let mut data = BytePacker::new(data);
            // Write the header by value through its verified NoUninit implementation.
            data.write_any_value(id, false); // end data points to start data
            // most won't have end data, so this is good
            // Transfer values into the row, interning strings in this chunk's buffer.
            fields.write(
                &mut data,
                &mut arch.strings.active,
                callsite_data.string_storage,
            );
            arch.data.update_mask_after_append();
            arch.data.update_ts_range(time.value as usize, None);
            // allocate() can roll over the active block, so capture its ordinal afterward.
            SpanFieldsRef {
                archetype: arch.id,
                block: u32::try_from(arch.data.offset + arch.data.ring.len() + 1)
                    .expect("span update block exceeds u32"),
            }
        } else {
            SpanFieldsRef::default()
        };
        {
            let data = &mut callsite_data.cache[id.archetype as usize].data;
            // A long-lived span may close after its start chunk has been evicted.
            let retained = if let Some(mut row) = data.get_mut(id.index as usize) {
                let row = row.get_mut();
                let mut packer = BytePacker::new(row);
                packer.skip::<PackedTimestamp>(false);
                packer.write_any_value(time.value, false); // end timestamp;
                packer.write_any_value(extra_fields, false);
                if let Some(f) = follow_from {
                    packer.write_any_value(f, false)
                }
                true
            } else {
                false
            };
            if retained {
                data.update_ts_range(time.value as usize, Some(id.index as usize));
            }
        }
        drop(callsite_guard);
        if allocated {
            self.drop_old_chunks(callsite);
        }
    }
}

// ----------------  CRUFT ------------------------

// Text belongs to the subscriber until its archetype is known (or a span closes).
// Strings retain capacity; BufferOffset(0) is only a scratch placeholder for text
// and is replaced with its deduplicated buffer offset/index before packing.
struct FieldValues {
    values: Vec<Option<ValueData>>,
    strings: Vec<String>,
    // Rendered captures are packed only when the selected archetype uses a template.
    template_args: Vec<String>,
}
impl FieldValues {
    /// Events, span starts and deferred updates share reference encoding.
    /// Scratch strings stay textual until the destination buffer is selected.
    fn write(&mut self, packer: &mut BytePacker<'_>, buffer: &mut Buffer, storage: StringStorage) {
        for (index, field) in self.values.iter_mut().enumerate() {
            if let Some(value) = field.take() {
                let value = match value {
                    ValueData::BufferOffset(_) | ValueData::BufferIndex(_) => {
                        storage.intern(buffer, &self.strings[index])
                    }
                    value => value,
                };
                packer.write_any_value(value, false);
            }
        }
    }
    fn record(&mut self, record: impl FnOnce(&mut dyn Visit)) {
        record(&mut ValueRecorder {
            data: self,
            is_log_compat: false,
            template: None,
            template_arg_count: None,
        });
    }

    // Size both kinds of reusable scratch storage when the callsite is registered.
    fn new(vcount: usize, arg_count: usize) -> Self {
        Self {
            values: vec![None; vcount],
            strings: vec![String::new(); vcount],
            template_args: vec![String::new(); arg_count],
        }
    }
}

struct ValueRecorder<'a> {
    data: &'a mut FieldValues,
    is_log_compat: bool,
    template: Option<&'a template::CompiledTemplate>,
    // Preserve a successful zero-argument match without storing the message field index.
    template_arg_count: Option<usize>,
}

impl ValueRecorder<'_> {
    fn record_text(&mut self, field: &tracing_core::Field, value: impl std::fmt::Display) {
        use std::fmt::Write;
        if self.is_log_compat && field.name() != "message" {
            return; // tracing_log metadata is fields, but we already extracted it
        }

        // Format once into scratch; physical offset/index references are assigned on write.
        let text = &mut self.data.strings[field.index()];
        text.clear();
        write!(text, "{value}").expect("writing to String");
        if field.name() == "message"
            && let Some(template) = self.template
            && let Some(captures) = template.captures(text)
        {
            for (capture, argument) in captures.into_iter().zip(&mut self.data.template_args) {
                argument.clear();
                argument.push_str(capture);
            }
            self.data.values[field.index()] = None;
            self.template_arg_count = Some(template.field_names.len());
            return;
        }
        self.data.values[field.index()] = Some(ValueData::BufferOffset(0));
    }
}

impl Visit for ValueRecorder<'_> {
    fn record_str(&mut self, field: &tracing_core::Field, value: &str) {
        self.record_text(field, value);
    }

    fn record_debug(&mut self, field: &tracing_core::Field, value: &dyn std::fmt::Debug) {
        // Format once, directly into reusable text for events and span starts.
        self.record_text(field, format_args!("{value:?}"));
    }

    fn record_f64(&mut self, field: &tracing_core::Field, value: f64) {
        self.data.values[field.index()] = Some(ValueData::F64(value));
    }

    fn record_i64(&mut self, field: &tracing_core::Field, value: i64) {
        self.data.values[field.index()] = Some(ValueData::I64(value));
    }

    fn record_u64(&mut self, field: &tracing_core::Field, value: u64) {
        // log.line lives in normalized metadata, outside the message-only row buffer.
        if self.is_log_compat {
            return;
        }
        self.data.values[field.index()] = Some(ValueData::U64(value));
    }

    fn record_i128(&mut self, field: &tracing_core::Field, value: i128) {
        self.data.values[field.index()] = Some(ValueData::I128(value));
    }

    fn record_u128(&mut self, field: &tracing_core::Field, value: u128) {
        self.data.values[field.index()] = Some(ValueData::U128(value));
    }

    fn record_bool(&mut self, field: &tracing_core::Field, value: bool) {
        self.data.values[field.index()] = Some(ValueData::Bool(value));
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
    fn matched_messages_store_only_buffer_offset_args() {
        fn emit(count: u64, message: &str) {
            tracing::info!(count, message, tail = "tail");
        }
        // Fill an archived block even when the configured target grows. The
        // fixed template has the smallest row: timestamp, count and tail offset.
        let pairs = super::MESSAGE_BLOCK_TARGET_SIZE
            / (size_of::<super::PackedTimestamp>() + 8 + 4)
            + 1;
        for format in ["hello {name} {}!", "fixed"] {
            let recorder = Arc::new(RecordingLayer::new());
            let dispatch = tracing::Dispatch::new(
                Registry::default().with(LocalRecordingLayer(recorder.clone())),
            );
            tracing::dispatcher::with_default(&dispatch, || emit(u64::MAX, "warmup"));
            let site = recorder.callsites.read().values().next().unwrap().clone();
            // A dynamic-message callsite lets this test alternate matches and raw rows.
            {
                let mut site = site.write();
                let template = super::template::CompiledTemplate::compile(format).unwrap();
                site.field_value_scratchpad =
                    super::FieldValues::new(3, template.field_names.len());
                site.template = Some(template);
            }
            tracing::dispatcher::with_default(&dispatch, || {
                for n in 0..(2 * pairs) as u64 {
                    let message = if n % 2 != 0 {
                        format!("raw {n}")
                    } else if format == "fixed" {
                        "fixed".to_owned()
                    } else if n == 0 {
                        "hello  !".to_owned() // Empty captures are still stored strings.
                    } else {
                        format!("hello é {n}!")
                    };
                    emit(n, &message);
                }
            });
            let (matched, arg_count) = {
                let site = site.read();
                assert_eq!(site.cache.len(), 2);
                let arg_count = site.template.as_ref().unwrap().field_names.len();
                let matched = site
                    .cache
                    .iter()
                    .find(|a| a.archetype.template == Some(arg_count))
                    .unwrap();
                assert_eq!(site.field_value_scratchpad.values.len(), 3);
                assert_eq!(site.field_value_scratchpad.strings.len(), 3);
                assert_eq!(site.field_value_scratchpad.template_args.len(), arg_count);
                let types = &matched.archetype.field_types;
                assert_eq!(types.len(), 3);
                assert_eq!(types[1], None); // No original message bytes or string offset.
                assert_eq!(
                    matched.archetype.unit(),
                    size_of::<super::PackedTimestamp>() + 8 + 4 + arg_count * size_of::<u32>()
                );
                assert_eq!(matched.data.active.unit, matched.archetype.unit());
                assert!(!matched.data.ring.is_empty()); // Exercise compressed chunks too.
                (matched.id, arg_count)
            };
            // Decode the actual offsets and reconstructed message through the production reader.
            let (mut matched_rows, mut raw_rows) = (0, 0);
            site.read().visit_rows(|row| {
                use super::InputValue;
                let Some(InputValue::U64(n)) = row.fields[0] else {
                    panic!("count type");
                };
                assert_eq!(row.fields[2], Some(InputValue::Text("tail".into())));
                let expected = if row.archetype == matched {
                    matched_rows += 1;
                    assert_eq!(n % 2, 0);
                    if format == "fixed" {
                        "fixed".to_owned()
                    } else if n == 0 {
                        "hello  !".to_owned()
                    } else {
                        format!("hello é {n}!")
                    }
                } else {
                    raw_rows += 1;
                    if n == u64::MAX {
                        "warmup".to_owned()
                    } else {
                        format!("raw {n}")
                    }
                };
                assert_eq!(row.fields[1], Some(InputValue::Text(expected)));
                assert!(row.timestamp.is_some());
            });
            assert_eq!((matched_rows, raw_rows), (pairs, pairs + 1));
        }
    }

    #[test]
    fn chunk_eviction_respects_caps_and_buffer_ownership() {
        use crate::data::blockbuffer::{BlockCache, DropChunkError};

        // Exercise rejection paths alongside the eviction coverage below.
        let mut cache = BlockCache::new(8, 2, 2);
        for ts in 1..=5 {
            cache.push(&[0; 8], ts);
        }
        assert_eq!(cache.drop_chunk(3), Err(DropChunkError::NotOldest));
        assert_eq!(cache.drop_chunk(1), Ok(()));
        assert_eq!(cache.drop_chunk(1), Err(DropChunkError::AlreadyDropped));
        assert_eq!(cache.offset, 1);
        assert_eq!(cache.drop_chunk(5), Err(DropChunkError::NotOldest));

        for global in [false, true] {
            let recorder = Arc::new(RecordingLayer::new());
            tracing::subscriber::with_default(
                Registry::default().with(LocalRecordingLayer(recorder.clone())),
                || {
                    // Two archetypes belonging to the same callsite.
                    for value in [Some(1u64), None] {
                        tracing::info!(value);
                    }
                },
            );
            let mut recorder = Arc::try_unwrap(recorder).ok().unwrap();
            let shared = recorder.callsites.read().values().next().unwrap().clone();
            let id = shared.read().id;
            {
                let mut site = shared.write();
                for (i, arch) in site.cache.iter_mut().enumerate() {
                    let unit = arch.archetype.unit();
                    arch.data = BlockCache::new(unit, 2, 2);
                    arch.strings
                        .active
                        .add_string(&"shared payload".repeat(100));
                    // The older archetype shares one buffer across two longer chunks.
                    for chunk in 0..if i == 0 { 2 } else { 1 } {
                        let start = 1 + i * 2000 + chunk * 100;
                        arch.data.push(&vec![0; unit], start);
                        arch.data
                            .push(&vec![255; unit], start + if i == 0 { 100 } else { 1 });
                        arch.data.cycle_if_needed();
                    }
                    arch.strings.rotate(false);
                    arch.data.string_buffer = arch.strings.active_index();
                    arch.strings.active.add_string("protected");
                    arch.data.push(&vec![0; unit], 4000);
                }
            }
            let total: usize = shared
                .read()
                .cache
                .iter()
                .map(super::PerArchData::storage_size)
                .sum();
            if global {
                recorder.global_memory_cap = total - 1;
            } else {
                recorder.callsite_memory_cap = total - 1;
            }
            recorder.drop_old_chunks(id);
            {
                let site = shared.read();
                let lengths: Vec<_> = site.cache.iter().map(|arch| arch.data.ring.len()).collect();
                // Local eviction chooses oldest; global eviction chooses size / duration.
                assert_eq!(lengths, if global { vec![2, 0] } else { vec![1, 1] });
                assert!(site.cache[0].strings.get(0).is_some());
                assert_eq!(site.cache[1].strings.get(0).is_none(), global);
            }
            recorder.callsite_memory_cap = 0;
            recorder.global_memory_cap = 0;
            recorder.drop_old_chunks(id);
            for arch in &shared.read().cache {
                assert!(arch.data.ring.is_empty());
                assert!(arch.strings.get(0).is_none());
                assert_eq!(arch.data.active.length, 1);
                assert_eq!(arch.strings.active.get_index(0), Some("protected"));
            }
        }
    }

    #[test]
    fn log_and_query() {
        // Set up the tracing registry with the RecordingLayer
        let recording_layer = LocalRecordingLayer(Arc::new(RecordingLayer::new()));
        let subscriber = Registry::default().with(recording_layer.clone());
        let _guard = tracing::subscriber::set_default(subscriber);

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
    #[test]
    fn only_empty_chunks_can_change_string_buffers() {
        use super::*;
        let recorder = Arc::new(RecordingLayer::new());
        let _guard = tracing::subscriber::set_default(
            Registry::default().with(LocalRecordingLayer(recorder.clone())),
        );
        let text = "x".repeat(
            STRING_BUFFER_TARGET_SIZE
                .load(Ordering::Relaxed)
                .min(crate::data::stringbuffer::Buffer::MAX_STRING_LENGTH),
        );
        tracing::info!(message = text.as_str());
        let sites = recorder.callsites.read();
        let mut site = sites.values().next().unwrap().write();
        let arch = &mut site.cache[0];
        // Reach the default target with several strings that fit the packed length.
        for value in ["a", "b", "c"] {
            arch.strings.active.add_string(&value.repeat(text.len()));
        }

        // Tiny dummy row chunks exercise populated and already-cycled states.
        arch.data = BlockCache::new(arch.archetype.unit(), 2, 2);
        arch.data.push(&vec![0; arch.archetype.unit()], 0);
        arch.prepare_chunk();
        assert_eq!(arch.data.string_buffer, 0);
        arch.data.push(&vec![0; arch.archetype.unit()], 0);
        arch.data.cycle_if_needed();
        assert_eq!(arch.data.active.length, 0);
        arch.prepare_chunk();
        arch.prepare_chunk(); // Repeated preparation must not rotate again.
        assert_eq!(arch.data.ring[0].string_buffer, 0);
        assert_eq!(arch.data.string_buffer, 1);
        let old = arch.strings.get(0).unwrap();
        assert_eq!(old.get_index(0), Some(text.as_str()));
    }
}
