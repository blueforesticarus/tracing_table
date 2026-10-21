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
    stringbuffer::BufferCollection,
};

mod dump;
// Optional embedded templates are loaded only when first queried.
pub mod template;

/// Target uncompressed bytes (32 KiB by default); set via env or `.store(bytes, Ordering::Relaxed)`.
#[rustfmt::skip]
pub static STRING_BUFFER_TARGET_SIZE: LazyLock<AtomicUsize> = LazyLock::new(|| {
    AtomicUsize::new(std::env::var("STRING_BUFFER_TARGET_SIZE").ok().and_then(|s| s.parse().ok()).unwrap_or(1 << 15))
});

// Existing data is exposed for example-owned inspection; recording stays unchanged.
pub struct PerCallsiteData {
    pub id: u64,
    pub metadata: &'static Metadata<'static>,

    // Reuse each field's string capacity across recordings.
    field_value_scratchpad: FieldValues,
    last_archetype: Archetype,

    call_count: usize,
    finished_count: usize,

    // Hopefully will be only one field
    pub cache: Vec<PerArchData>,
    lookup: HashMap<Archetype, usize>,
    // added to executable symbols by post compile tool, or checked against filesystem in development
    // compile_template:
    // local_template:
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
                self.strings.rotate(false); // Rows address offsets, so omit the frozen index.
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
                        field_value_scratchpad: FieldValues::new(vcount),
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

    /// After a frame allocation, enforce the allocating callsite's cap first,
    /// then the global cap. Active chunks and active string buffers are protected.
    fn drop_old_chunks(&self, callsite: u64) {
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
        let v = ext.get_mut::<FieldValues>();

        let mut v = match v {
            Some(v) => v,
            None => {
                let v = FieldValues::new(ctx.fields().len());
                ext.insert(v);
                ext.get_mut::<FieldValues>().unwrap()
            }
        };

        let mut visitor = ValueRecorder {
            _metadata: ctx.metadata(),
            data: &mut v,
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
        let fields: Option<&mut FieldValues> = ext.get_mut();

        let mut callsite_guard = self.register(ctx.metadata());
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
                    .map(|opt| opt.as_ref().map(ValueKind::from)),
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
            for (index, field) in fields.values.iter_mut().enumerate() {
                if let Some(mut field) = field.take() {
                    if let ValueData::BufferOffset(offset) = &mut field {
                        // Buffer checks its narrower 20-bit offset limit on insertion.
                        *offset = arch.strings.active.add_string(&fields.strings[index]).1 as u32;
                    }
                    data.write_any_value(field, false);
                }
            }
            arch.data.update_mask_after_append();
            arch.data.compute_timestamp(time.value as usize, None);
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
                data.compute_timestamp(time.value as usize, Some(id.index as usize));
            }
        }
        drop(callsite_guard);
        if allocated {
            self.drop_old_chunks(callsite);
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

        let mut callsite_guard = self.register(attrs.metadata());
        let callsite_data = callsite_guard.deref_mut();
        callsite_data.call_count += 1;

        callsite_data.field_value_scratchpad.values.fill(None);
        let mut visitor = ValueRecorder {
            _metadata: attrs.metadata(),
            data: &mut callsite_data.field_value_scratchpad,
            is_log_compat: false,
        };

        // records fields directly into PerCallsiteData temp buffer.
        attrs.record(&mut visitor as &mut dyn Visit);

        // we use callsite data's copy so we can avoid allocating a vec
        callsite_data.last_archetype.field_types.clear();
        callsite_data.last_archetype.field_types.extend(
            callsite_data
                .field_value_scratchpad
                .values
                .iter()
                .map(|opt| opt.as_ref().map(ValueKind::from)),
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
        let allocated = arch.prepare_chunk();
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
        data.write_any_value(time.value, false);
        data.write_any_value(PackedTimestamp::default(), false); // space for end_timestamp
        data.write_any_value(SpanFieldsRef::default(), false); // deferred fields are unknown until close
        data.write_any_value(PLACE_HOLDER, false); // space for follows_from
        if arch.archetype.parent {
            data.write_any_value(span_id.unwrap(), false);
        }
        let fields = &mut callsite_data.field_value_scratchpad;
        for (index, field) in fields.values.iter_mut().enumerate() {
            if let Some(mut field) = field.take() {
                if let ValueData::BufferOffset(offset) = &mut field {
                    *offset = arch.strings.active.add_string(&fields.strings[index]).1 as u32;
                }
                data.write_any_value(field, false);
            }
        }
        arch.data.update_mask_after_append();
        arch.data.compute_timestamp(time.value as usize, None);
        drop(callsite_guard);
        if allocated {
            self.drop_old_chunks(callsite);
        }
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

        let mut callsite_guard = self.register(metadata);
        let callsite_data = callsite_guard.deref_mut();
        callsite_data.call_count += 1;

        // event id's currently not stored. since nothing refers to them
        callsite_data.field_value_scratchpad.values.fill(None);
        let mut visitor = ValueRecorder {
            _metadata: metadata,
            data: &mut callsite_data.field_value_scratchpad,
            is_log_compat,
        };

        // records fields directly into PerCallsiteData buffer.
        event.record(&mut visitor as &mut dyn Visit);

        // we use callsite data's copy so we can avoid allocating a vec
        callsite_data.last_archetype.field_types.clear();
        callsite_data.last_archetype.field_types.extend(
            callsite_data
                .field_value_scratchpad
                .values
                .iter()
                .map(|opt| opt.as_ref().map(ValueKind::from)),
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
        let allocated = arch.prepare_chunk();
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
        data.write_any_value(time.value, false);
        if arch.archetype.parent {
            data.write_any_value(span_id.unwrap(), false);
        }
        let fields = &mut callsite_data.field_value_scratchpad;
        for (index, field) in fields.values.iter_mut().enumerate() {
            if let Some(mut field) = field.take() {
                if let ValueData::BufferOffset(offset) = &mut field {
                    *offset = arch.strings.active.add_string(&fields.strings[index]).1 as u32;
                }
                data.write_any_value(field, false);
            }
        }
        arch.data.update_mask_after_append();
        arch.data.compute_timestamp(time.value as usize, None);
        drop(callsite_guard);
        if allocated {
            self.drop_old_chunks(callsite);
        }
    }
}

// ----------------  CRUFT ------------------------

// Text belongs to the subscriber until its archetype is known (or a span closes).
// Strings retain capacity; BufferOffset(0) refers to the start of that field's text
// and is replaced with its deduplicated chunk offset before packing.
struct FieldValues {
    values: Vec<Option<ValueData>>,
    strings: Vec<String>,
}
impl FieldValues {
    fn new(count: usize) -> Self {
        Self {
            values: vec![None; count],
            strings: vec![String::new(); count],
        }
    }
}

struct ValueRecorder<'a> {
    _metadata: &'a Metadata<'a>,
    data: &'a mut FieldValues,
    is_log_compat: bool,
}

impl ValueRecorder<'_> {
    fn record_text(&mut self, field: &tracing_core::Field, value: impl std::fmt::Display) {
        use std::fmt::Write;
        if self.is_log_compat && field.name() != "message" {
            return; // tracing_log metadata is fields, but we already extracted it
        }

        // we record the text to a temporary buffer, and put a dummy BufferOffset.
        // after archetype is known, these strings are copied into the archetypes string buffer.
        let text = &mut self.data.strings[field.index()];
        text.clear();
        write!(text, "{value}").expect("writing to String");
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
