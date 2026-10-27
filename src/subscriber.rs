use std::{
    cell::RefCell,
    collections::HashMap,
    default,
    fs::metadata,
    hash::Hash,
    ops::{Add, Deref, DerefMut},
    sync::{
        Arc, LazyLock, Weak,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Instant, SystemTime},
};

use parking_lot::{ArcRwLockWriteGuard, Mutex, RawRwLock, RwLock};
use smallvec::SmallVec;
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
    blockbuffer::{BlockCache, CallsiteArchetype},
    column::Layout as ColumnLayout,
    dyntuple::{BytePacker, ValueData, ValueKind},
    stringbuffer::{Buffer, BufferCollection},
};

// Portable capture/loading is opt-in; live recording does not depend on its state.
pub mod dump;
pub mod format;
pub mod read;
mod value;
pub use value::InputValue;
// Rendered-message fallback templates resolve once; inspectable formats capture directly.
pub mod template;

/// Uncompressed bytes per archetype block; the reference comparison favors 4 KiB.
/// Capacity rounds down to whole rows, subject to RecordingLayer::message_block_min_rows.
pub const MESSAGE_BLOCK_TARGET_SIZE: usize = 4 * 1024;

/// Default row floor for new recording layers; zero disables the optional floor.
/// Kept for existing experiments; prefer RecordingLayer::message_block_min_rows.
pub static MESSAGE_BLOCK_MIN_ROWS: AtomicUsize = AtomicUsize::new(0);

/// Target uncompressed bytes (32 KiB by default); set via env or `.store(bytes, Ordering::Relaxed)`.
#[rustfmt::skip]
pub static STRING_BUFFER_TARGET_SIZE: LazyLock<AtomicUsize> = LazyLock::new(|| {
    AtomicUsize::new(std::env::var("STRING_BUFFER_TARGET_SIZE").ok().and_then(|s| s.parse().ok()).unwrap_or(1 << 15))
});

// Preserve the existing subscriber-facing import while buffer policy lives with storage.
pub use crate::data::stringbuffer::StringStorage;
// Keep the public subscriber import while the block owns reference storage.
pub use crate::data::blockbuffer::SpanFieldsRef;

// Existing data is exposed for example-owned inspection; recording stays unchanged.
pub struct PerCallsiteData {
    pub id: u64,
    pub metadata: &'static Metadata<'static>,
    /// Legacy rendered-message matcher; native inspectable formats use `formats`.
    pub template: Option<template::CompiledTemplate>,
    /// Directly captured formatting schemas, shared by every row at this callsite.
    pub formats: Vec<Arc<format::Format>>,
    /// Fixed at registration so previously stored rows keep their interpretation.
    pub string_storage: StringStorage,
    // Weak entries resolve archived parent-relative starts without retaining
    // expanded timestamps or creating ownership cycles between callsites.
    span_registry: Weak<SpanRegistry>,
    parent_timestamp_offsets: bool,

    // Reuse each field's string capacity across recordings.
    field_value_scratchpad: FieldValues,
    // Native Format callbacks bypass lookup. A rendered-only message resolves
    // the fallback once, including a cached miss; replay controls its own templates.
    template_pending: bool,
    last_archetype: Archetype,

    call_count: usize,
    finished_count: usize,

    // Hopefully will be only one field
    pub cache: Vec<PerArchData>,
    lookup: HashMap<Archetype, usize>,
}

impl PerCallsiteData {
    fn get_arch(
        &mut self,
        min_rows: usize,
        policy: crate::data::column::physical::LayoutPolicy,
    ) -> usize {
        let archetype = &self.last_archetype; //aliased mutability issue with structs needs to be fixed. 

        let i = match self.lookup.get(archetype) {
            Some(i) => *i,
            None => {
                let physical = archetype.physical_layout_with_policy(policy);
                let unit = physical.unit();
                // The schema is fixed: reserve its worst-case string entries once.
                let strings_per_row = archetype
                    .field_types
                    .iter()
                    .flatten()
                    .filter(|kind| matches!(kind, ValueKind::BufferOffset | ValueKind::BufferIndex))
                    .count()
                    + archetype.template.unwrap_or(0);
                assert!(
                    strings_per_row <= Buffer::MAX_ENTRIES,
                    "row exceeds string buffer capacity"
                );
                // A row floor takes precedence over the target for wide schemas.
                let cache_parent =
                    self.parent_timestamp_offsets && archetype.parent && !archetype.is_span_end;
                let cap = (MESSAGE_BLOCK_TARGET_SIZE / (unit + usize::from(cache_parent) * 8))
                    .max(min_rows)
                    .max(1);
                let i = self.cache.len();
                self.lookup.insert(archetype.clone(), i);
                let mut data = BlockCache::new(unit, 1, cap);
                data.configure_layout(physical, cap);
                data.cache_parent_timestamps(cache_parent);
                self.cache.push(
                    PerArchData {
                        data,
                        // Production rows use stable indexes. Retain the legacy
                        // explicit offset mode without moving its byte addresses.
                        strings: if archetype.string_storage == StringStorage::Offset {
                            BufferCollection::with_fsst_training(0)
                        } else {
                            BufferCollection::default()
                        },
                        chunk_string_start: 0,
                        strings_per_row,
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
    // Includes ordinary string fields and extracted template arguments.
    strings_per_row: usize,
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
    fn prepare_chunk(&mut self, string_compression_level: Option<u32>) -> bool {
        let strings = self.strings_per_row;
        let strings_full = self.strings.active.len() + strings > Buffer::MAX_ENTRIES
            || self.strings.active.len() == Buffer::MAX_ENTRIES;
        // Check caps only on initial allocation, frame growth, or chunk rollover.
        let allocated = self.data.active.length == self.data.active.capacity()
            || (self.data.active_descriptor.index == 1 && self.data.active.length == 0)
            || strings_full;
        if strings_full {
            eprintln!(
                "WARNING: string buffer entry limit (65536) forces string buffer and chunk rotation: callsite={}, archetype={}, entries={}, row_strings={}",
                self.archetype.callsite,
                self.id,
                self.strings.active.len(),
                strings,
            );
            // Temporary: chunks require a constant string buffer, so a forced
            // string-buffer rotation must also rotate the current chunk.
            self.data
                .cycle_with_columns(&self.archetype.column_layout());
        }
        if self.data.active.length == self.data.active.capacity() {
            self.data
                .cycle_if_needed_with_columns(&self.archetype.column_layout());
        }
        // Only an empty chunk can change buffers, regardless of who cycled it.
        if self.data.active.length == 0 {
            let size = self.strings.active.byte_len();
            let delta = size - self.chunk_string_start;
            // Rotate if another equal-growth chunk would approach (or pass) the
            // target within half a delta. Oversized chunks remain indivisible.
            let projected = size.saturating_add(delta).saturating_add(delta / 2);
            if strings_full
                || (size > 0 && projected >= STRING_BUFFER_TARGET_SIZE.load(Ordering::Relaxed))
            {
                // Expansion rebuilds the index from entry lengths for either encoder.
                self.strings
                    .rotate_with_level(false, string_compression_level);
            }
            self.chunk_string_start = self.strings.active.byte_len();
            self.data.active_metadata.string_buffer = self.strings.active_index();
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

/// Row-header time: unsigned offset or archived ZigZag stride residual,
/// in ticks of 1 << RecordingLayer::timestamp_bit_shift nanoseconds.
/// Span end slots store duration + 1 instead; zero marks a still-open span.
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
type SpanRegistry = RwLock<Vec<Weak<RwLock<PerCallsiteData>>>>;

/// Optional parent identity partitioning; row headers and dictionaries stay unchanged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ParentArchetyping {
    #[default]
    Off,
    Callsite,
    CallsiteArch,
}
impl ParentArchetyping {
    fn from_env() -> Self {
        match std::env::var("PARENT_ARCHETYPING").as_deref() {
            Ok("callsite") => Self::Callsite,
            Ok("callsite+arch") => Self::CallsiteArch,
            _ => Self::Off,
        }
    }
    fn key(self, parent: Option<SpanId>) -> u64 {
        // Exact concatenation, not a hash: distinct parent identities cannot collide.
        parent.map_or(0, |p| match self {
            Self::Off => 0,
            Self::Callsite => u64::from(p.callsite),
            Self::CallsiteArch => u64::from(p.callsite) | (u64::from(p.archetype) << 32),
        })
    }
}

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
    /// A value of 0 preserves nanoseconds since the recorder started.
    /// The default of 10 gives a resolution of 1,024 nanoseconds per tick.
    /// You can adjust this to only store as much precision as is meaningfull.
    /// This is needed because the CompactFrame cannot filter out noise. For many callsites, timestamps will be the only recorded data.
    /// low variance (more signifigant) bits get automatically compressed when constant within a frame.
    /// TODO could be automatic based on time between messages.
    pub timestamp_bit_shift: u8,

    /// Used for newly registered callsites; configure before recording.
    pub string_storage: StringStorage,

    /// Some(3) defaults to native LZ4 HC3; None selects lz4_flex.
    /// Initialized from STRING_COMPRESSION_LEVEL ("flex" or 1..=12).
    pub string_compression_level: Option<u32>,

    /// Optional row floor over the byte target; zero disables it (default).
    /// Initialized from MESSAGE_BLOCK_MIN_ROWS; configure before recording.
    pub message_block_min_rows: usize,
    /// Configure before recording; independent switches support replay ablations.
    pub layout_policy: crate::data::column::physical::LayoutPolicy,
    /// Initialized from PARENT_ARCHETYPING: off (default), callsite, callsite+arch.
    /// Configure before recording; this changes schema selection only.
    pub parent_archetyping: ParentArchetyping,
    /// Cache parent starts in active rows and choose smaller parent-relative
    /// archives. Configure before recording; PARENT_TIMESTAMP_OFFSETS=on enables it.
    pub parent_timestamp_offsets: bool,
    span_registry: Arc<SpanRegistry>,

    /// Retained chunk/string storage limits in bytes; active storage is protected.
    pub global_memory_cap: usize,
    pub callsite_memory_cap: usize,

    // Shared across event/log callsites; spans never advance this ordering state.
    // Last shifted timestamp, (callsite, archetype), and run index.
    last_event: Mutex<Option<(PackedTimestamp, (u64, u32), u16)>>,
    system_time: SystemTime,
    started: Instant,
}

impl RecordingLayer {
    pub fn new() -> Self {
        RecordingLayer {
            callsites: Default::default(),
            span_id_counter: 1.into(),
            event_id_counter: ((1usize + std::u32::MAX as usize) as u64).into(),
            last_event: Mutex::new(None),
            timestamp_bit_shift: 10,
            string_storage: StringStorage::default(),
            // Preserve the explicit flex override while defaulting to HC3.
            string_compression_level: match std::env::var("STRING_COMPRESSION_LEVEL").as_deref() {
                Ok("flex") => None,
                Ok(value) => Some(
                    value
                        .parse()
                        .ok()
                        .filter(|level| (1..=12).contains(level))
                        .unwrap_or(3),
                ),
                Err(_) => Some(3),
            },
            message_block_min_rows: std::env::var("MESSAGE_BLOCK_MIN_ROWS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| MESSAGE_BLOCK_MIN_ROWS.load(Ordering::Relaxed)),
            layout_policy: Default::default(),
            parent_archetyping: ParentArchetyping::from_env(),
            parent_timestamp_offsets: matches!(
                std::env::var("PARENT_TIMESTAMP_OFFSETS").as_deref(),
                Ok("on") | Ok("true") | Ok("1")
            ),
            span_registry: Arc::default(),
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
        let mut pending = false;
        let mut site = self.register_with_template(metadata, || {
            if metadata.is_event() && metadata.fields().field("message").is_some() {
                if cfg!(feature = "tracing_patched") && metadata.name() != "compat:log" {
                    pending = true;
                    None
                } else {
                    template::lookup_metadata(metadata, metadata.name() == "compat:log")
                }
            } else {
                None
            }
        });
        // The resolver only runs for a newly registered callsite, so explicit
        // dump replay templates (including disabled extraction) retain control.
        if pending {
            site.template_pending = true;
        }
        site
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
                let fields = FieldValues::new(vcount, acount, self.string_storage);
                let shared = Arc::new(
                    PerCallsiteData {
                        id,
                        metadata,
                        template,
                        formats: Vec::new(),
                        string_storage: self.string_storage,
                        span_registry: Arc::downgrade(&self.span_registry),
                        parent_timestamp_offsets: self.parent_timestamp_offsets,
                        field_value_scratchpad: fields,
                        template_pending: false,
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
                            formats: Vec::new(),
                            string_storage: self.string_storage,
                            parent_identity: 0,
                            is_span_end: false,
                            is_event: false,
                            // unit: 0,
                        },
                    }
                    .into(),
                );
                if metadata.is_span() && self.parent_timestamp_offsets {
                    let mut spans = self.span_registry.write();
                    let length = (id as usize + 1).max(spans.len());
                    spans.resize_with(length, Weak::new);
                    spans[id as usize] = Arc::downgrade(&shared);
                }
                shared
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
                    let Some(chunk) = arch.data.ring.front() else {
                        continue;
                    };
                    let candidate = (site_index, arch_index, chunk.descriptor.index);
                    // The per-callsite cap chooses the oldest chunk across its archetypes.
                    if site.id == callsite
                        && size > self.callsite_memory_cap
                        && (local.is_none() || chunk.descriptor.timestamp.start < oldest)
                    {
                        oldest = chunk.descriptor.timestamp.start;
                        local = Some(candidate);
                    }
                    // Global candidates are each archetype's oldest chunk. Buffer IDs
                    // increase with chunk age, so users of this buffer form a prefix.
                    let buffer_id = chunk.data.metadata.string_buffer;
                    let users = arch
                        .data
                        .ring
                        .partition_point(|c| c.data.metadata.string_buffer == buffer_id)
                        + usize::from(arch.data.active_metadata.string_buffer == buffer_id);
                    let buffer_bytes = if buffer_id == arch.strings.active_index() {
                        bytes(arch.strings.active.compression_ratio())
                    } else {
                        bytes(
                            arch.strings.compressed
                                [(buffer_id.get() - arch.strings.offset().get()) as usize]
                                .compression_ratio(),
                        )
                    };
                    let duration = chunk
                        .descriptor
                        .timestamp
                        .end
                        .saturating_sub(chunk.descriptor.timestamp.start)
                        .max(1);
                    // Prefer more stored bytes per timestamp tick, charging only this
                    // chunk's share of its buffer. Zero-duration chunks use one tick.
                    let score = (chunk.data.allocation_size() as f64
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
            let new_oldest = arch.data.ring.front();
            let first_buffer = new_oldest
                .map(|chunk| chunk.data.metadata.string_buffer)
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
    /// (metadata field, callsite recipe, first argument column). Argument columns
    /// follow metadata fields in field_types and use the ordinary packed encoding.
    pub formats: Vec<(usize, usize, usize)>,
    /// Template captures use the same reference width as ordinary string fields.
    pub string_storage: StringStorage,
    // Partition-only identity; never replaces the parent stored in each row.
    parent_identity: u64,
}

impl Archetype {
    /// Physical history belongs to this archetype's BlockCache, outside its hash
    /// identity. Logical kinds continue to drive public values and Format recipes.
    pub fn physical_layout(&self) -> crate::data::column::physical::PhysicalLayout {
        self.physical_layout_with_policy(Default::default())
    }
    fn physical_layout_with_policy(
        &self,
        policy: crate::data::column::physical::LayoutPolicy,
    ) -> crate::data::column::physical::PhysicalLayout {
        use crate::data::column::physical::{Field, PhysicalLayout};
        let payload: usize = self
            .field_types
            .iter()
            .flatten()
            .map(ValueKind::unit)
            .sum::<usize>()
            + self.template.unwrap_or(0) * self.string_storage.unit();
        let header = self.unit() - payload;
        let mut offset = header;
        let mut fields = SmallVec::new();
        let kinds =
            self.field_types
                .iter()
                .flatten()
                .cloned()
                .chain(
                    (0..self.template.unwrap_or(0)).map(|_| match self.string_storage {
                        StringStorage::Index => ValueKind::BufferIndex,
                        StringStorage::Offset => ValueKind::BufferOffset,
                    }),
                );
        for kind in kinds {
            let logical = ColumnLayout::new(offset, kind.unit(), kind == ValueKind::BufferIndex);
            let signed = matches!(
                kind,
                ValueKind::I8
                    | ValueKind::I16
                    | ValueKind::I32
                    | ValueKind::I64
                    | ValueKind::I128
                    | ValueKind::Isize
            );
            let integer = signed
                || matches!(
                    kind,
                    ValueKind::U8
                        | ValueKind::U16
                        | ValueKind::U32
                        | ValueKind::U64
                        | ValueKind::U128
                        | ValueKind::Usize
                );
            fields.push(Field {
                logical,
                stored: logical,
                source: fields.len().try_into().expect("too many columns"),
                family: kind.clone() as u8,
                signed,
                integer,
                float64: kind == ValueKind::F64,
            });
            offset += kind.unit();
        }
        PhysicalLayout::new(
            header,
            self.unit(),
            !self.is_span_end && !self.is_event,
            fields,
            policy,
        )
    }
    /// Caller-owned layout is rebuilt at block boundaries/traversal, never stored per block.
    pub fn column_layout(&self) -> SmallVec<[ColumnLayout; 8]> {
        let args = self.template.unwrap_or(0);
        let fields: usize = self.field_types.iter().flatten().map(ValueKind::unit).sum();
        let mut offset = self.unit() - fields - args * self.string_storage.unit();
        let mut columns = SmallVec::new();
        // Biased durations (including zero for open spans) are logical u64 fields.
        // Reuse exact origins/residuals and the normal archived-edit rebuild path.
        if !self.is_span_end && !self.is_event {
            columns.push(ColumnLayout::new(8, 8, false));
        }
        for kind in self.field_types.iter().flatten() {
            columns.push(ColumnLayout::new(
                offset,
                kind.unit(),
                *kind == ValueKind::BufferIndex,
            ));
            offset += kind.unit();
        }
        for _ in 0..args {
            columns.push(ColumnLayout::new(
                offset,
                self.string_storage.unit(),
                self.string_storage == StringStorage::Index,
            ));
            offset += self.string_storage.unit();
        }
        columns
    }

    /// Full packed row width: header, metadata fields, then template string references.
    pub fn unit(&self) -> usize {
        let mut bytes = 0;
        if !self.is_span_end {
            bytes += std::mem::size_of::<PackedTimestamp>();
            if self.is_event {
                bytes += size_of::<u16>(); // timestamp index, before parent/fields
            } else {
                bytes += std::mem::size_of::<PackedTimestamp>(); //end timestamp
            }
        } else {
            bytes += crate::data::blockbuffer::ParentOffsets::ROW_BYTES;
        }
        if self.parent {
            bytes += crate::data::blockbuffer::ParentOffsets::ROW_BYTES;
        }
        if self.follows_from {
            bytes += crate::data::blockbuffer::ParentOffsets::ROW_BYTES;
        }
        for field in self.field_types.iter() {
            if let Some(k) = field {
                bytes += k.unit()
            }
        }
        bytes + self.template.unwrap_or(0) * self.string_storage.unit()
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

pub const PLACE_HOLDER: SpanId = SpanId {
    index: 0,
    callsite: 0,
    archetype: 0,
};

// TODO: Support multiple follows-from predecessors per span; storage currently
// retains only the latest predecessor, including during dump replay.
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
        // Match replay's single-predecessor storage: the latest callback wins.
        // insert() panics if a span receives a second follows-from callback.
        ctx.span(span)
            .unwrap()
            .extensions_mut()
            .replace(FollowFrom(id));
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
            let storage = self.register(span.metadata()).string_storage;
            ext.insert(FieldValues::new(span.fields().len(), 0, storage));
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
        record: impl FnOnce(&mut ValueRecorder<'_>),
    ) -> SpanId {
        // Resolve before taking the child callsite's write lock: recursive
        // spans can have a parent at the same callsite.
        let parent_start = if self.parent_timestamp_offsets {
            parent.and_then(|p| self.span_timestamp(p))
        } else {
            None
        };
        let is_event = metadata.is_event();
        let mut guard = self.register(metadata);
        let site = guard.deref_mut();
        site.call_count += 1;
        // Discard the previous row's argument columns while retaining capacities.
        site.field_value_scratchpad
            .values
            .truncate(metadata.fields().len());
        site.field_value_scratchpad.values.fill(None);
        site.field_value_scratchpad.formats.clear();
        let mut visitor = ValueRecorder {
            data: &mut site.field_value_scratchpad,
            is_log_compat,
            template: if is_event {
                site.template.as_ref()
            } else {
                None
            },
            template_arg_count: None,
            formats: Some(&mut site.formats),
        };
        record(&mut visitor);
        let mut template_count = visitor.template_arg_count;
        drop(visitor);
        if site.template_pending
            && site.template.is_none()
            && let Some(field) = metadata.fields().field("message")
            && matches!(
                site.field_value_scratchpad.values[field.index()],
                Some(ValueData::BufferIndex(_) | ValueData::BufferOffset(_))
            )
        {
            site.template_pending = false;
            site.template = template::lookup_metadata(metadata, is_log_compat);
            if let Some(template) = &site.template {
                let fields = &mut site.field_value_scratchpad;
                fields
                    .template_args
                    .resize_with(template.field_names.len(), String::new);
                if let Some(captures) = template.captures_inline(&fields.strings[field.index()]) {
                    for (capture, argument) in captures.into_iter().zip(&mut fields.template_args) {
                        argument.clear();
                        argument.push_str(capture);
                    }
                    fields.values[field.index()] = None;
                    template_count = Some(template.field_names.len());
                }
            }
        }
        site.last_archetype.template = template_count;
        site.last_archetype.formats = site.field_value_scratchpad.format_schema(&mut site.formats);
        site.last_archetype.field_types.clear();
        site.last_archetype.field_types.extend(
            site.field_value_scratchpad
                .values
                .iter()
                .map(|value| value.as_ref().map(ValueKind::from)),
        );
        site.last_archetype.parent_identity = self.parent_archetyping.key(parent);
        // Follows-from is known at close and lives only in deferred rows.
        site.last_archetype.follows_from = false;
        site.last_archetype.parent = parent.is_some();
        site.last_archetype.is_span_end = false;
        site.last_archetype.is_event = is_event;
        let callsite = site.id;
        let index = site.get_arch(self.message_block_min_rows, self.layout_policy);
        let arch = &mut site.cache[index];
        let allocated = arch.prepare_chunk(self.string_compression_level);
        // Rollover has finished; intern into the dictionary for this exact row.
        let parent = parent.map(|p| {
            // Parent follows the timestamp and event ordinal or span-end slot.
            let offset = size_of::<PackedTimestamp>()
                + if is_event {
                    size_of::<u16>()
                } else {
                    size_of::<PackedTimestamp>()
                };
            arch.data.encode_parent(
                p.index,
                CallsiteArchetype::new(p.callsite, p.archetype),
                offset,
            )
        });
        // Build one logical row, then refine sharing/widths before allocation.
        let mut logical = SmallVec::<[u8; 256]>::from_elem(0, arch.archetype.unit());
        let mut packer = BytePacker::new(&mut logical);
        packer.write_any_value(0u64, false);
        if is_event {
            // Same-archetype runs already have row order; count only transitions
            // within a shifted tick. Keep the full callsite ID (event IDs exceed u32).
            let mut last = self.last_event.lock();
            let origin = (callsite, arch.id);
            let count = match *last {
                // MAX marks lost precision and stays saturated until the tick changes.
                Some((ts, previous, count)) if ts == time.value => {
                    count.saturating_add(u16::from(previous != origin))
                }
                _ => 0,
            };
            *last = Some((time.value, origin, count));
            packer.write_any_value(count, false);
        } else {
            packer.write_any_value(PackedTimestamp::default(), false);
        }
        if let Some((residual, table_index)) = parent {
            packer.write_any_value(residual, false);
            packer.write_any_value(table_index, false);
        }
        let fields = &mut site.field_value_scratchpad;
        fields.write(
            &mut packer,
            &mut arch.strings.active,
            arch.archetype.template.unwrap_or(0),
        );
        arch.data.promote(&logical, self.message_block_min_rows);
        let mut physical = if let Some(projection) = arch.data.layouts.last() {
            let mut physical = SmallVec::<[u8; 256]>::from_elem(0, projection.unit());
            projection.pack(&logical, &mut physical);
            physical
        } else {
            logical.clone()
        };
        let (bytes, index, timestamp_offset) = arch
            .data
            .allocate_timestamp_with_parent(time.value, parent_start);
        physical[..8].copy_from_slice(&timestamp_offset.to_ne_bytes());
        bytes.copy_from_slice(&physical);
        let id = SpanId {
            index: index as u64,
            callsite: callsite as u32,
            archetype: arch.id,
        };
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
        // Archived relative starts need a parent lookup; perform it before
        // acquiring the mutable child site. Missing ancestors are allowed.
        let resolved_start = if self.parent_timestamp_offsets {
            self.span_timestamp(id)
        } else {
            None
        };
        let mut callsite_guard = self.register(metadata);
        let callsite_data = callsite_guard.deref_mut();
        let callsite = callsite_data.id;
        callsite_data.finished_count += 1;

        let mut allocated = false;
        // A follows-only span needs an update row too; absent fields consume no bytes.
        let has_fields = fields
            .as_ref()
            .is_some_and(|f| !f.formats.is_empty() || f.values.iter().any(Option::is_some));
        let extra_fields = if has_fields || follow_from.is_some() {
            callsite_data.last_archetype.field_types = if let Some(fields) = fields.as_ref() {
                fields
                    .values
                    .iter()
                    .map(|v| v.as_ref().map(ValueKind::from))
                    .collect()
            } else {
                vec![None; metadata.fields().len()]
            };
            callsite_data.last_archetype.formats =
                fields.as_ref().map_or_else(Vec::new, |fields| {
                    fields.format_schema(&mut callsite_data.formats)
                });
            // Deferred rows have no parent; clear the reused start-row partition.
            callsite_data.last_archetype.parent_identity = 0;
            callsite_data.last_archetype.follows_from = follow_from.is_some();
            callsite_data.last_archetype.template = None;
            callsite_data.last_archetype.is_span_end = true;
            callsite_data.last_archetype.is_event = false;
            callsite_data.last_archetype.parent = false;

            let arch = callsite_data.get_arch(self.message_block_min_rows, self.layout_policy);
            let arch = &mut callsite_data.cache[arch];

            allocated = arch.prepare_chunk(self.string_compression_level);
            // Both references share the block's identity dictionary and use the
            // same residual/key representation as parent references.
            let target = arch.data.encode_parent(
                id.index,
                CallsiteArchetype::new(id.callsite, id.archetype),
                0,
            );
            let follows = follow_from.map(|id| {
                arch.data.encode_parent(
                    id.index,
                    CallsiteArchetype::new(id.callsite, id.archetype),
                    crate::data::blockbuffer::ParentOffsets::ROW_BYTES,
                )
            });
            let mut logical = SmallVec::<[u8; 256]>::from_elem(0, arch.archetype.unit());
            let mut data = BytePacker::new(&mut logical);
            data.write_any_value(target.0, false);
            data.write_any_value(target.1, false);
            if let Some((residual, key)) = follows {
                data.write_any_value(residual, false);
                data.write_any_value(key, false);
            }
            // Transfer values into the row, interning strings in this chunk's buffer.
            if let Some(fields) = fields {
                fields.write(&mut data, &mut arch.strings.active, 0);
            }
            arch.data.promote(&logical, self.message_block_min_rows);
            let physical = if let Some(projection) = arch.data.layouts.last() {
                let mut physical = SmallVec::<[u8; 256]>::from_elem(0, projection.unit());
                projection.pack(&logical, &mut physical);
                physical
            } else {
                logical.clone()
            };
            arch.data.allocate(time.value).0.copy_from_slice(&physical);
            arch.data.update_mask_after_append();
            arch.data.update_ts_range(time.value as usize, None);
            // allocate() can roll over the active block, so capture its ordinal afterward.
            Some(SpanFieldsRef {
                archetype: arch.id,
                block: u32::try_from(arch.data.offset + arch.data.ring.len() + 1)
                    .expect("span update block exceeds u32"),
            })
        } else {
            None
        };
        {
            let arch = &mut callsite_data.cache[id.archetype as usize];
            let columns = arch.archetype.column_layout();
            let data = &mut arch.data;
            // A long-lived span may close after its start chunk has been evicted.
            let retained = if let Some((descriptor, mut row)) =
                data.get_mut_physical(id.index as usize, &columns)
            {
                let start = if self.parent_timestamp_offsets {
                    resolved_start
                } else {
                    Some(row.decode_timestamp(descriptor))
                };
                let Some(start) = start else {
                    return;
                };
                let mut packer = BytePacker::new(row.get_mut());
                packer.skip::<PackedTimestamp>(false);
                // Bias the duration so a zero-tick closed span differs from an open span.
                packer.write_any_value(time.value - start + 1, false);
                true
            } else {
                false
            };
            if retained {
                if let Some(reference) = extra_fields {
                    data.add_span_fields_ref(id.index as usize, reference);
                }
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
// Strings retain capacity. Text placeholders already have their final reference
// kind; packing fills in the buffer address without changing that kind.
struct FieldValues {
    // Capture the callsite's mode once, including for deferred span updates.
    string_storage: StringStorage,
    values: Vec<Option<ValueData>>,
    strings: Vec<String>,
    // Rendered captures are packed only when the selected archetype uses a template.
    template_args: Vec<String>,
    // Deferred updates retain recipes until close; live rows share callsite Arcs.
    formats: Vec<RecordedFormat>,
}
struct RecordedFormat {
    field: usize,
    recipe: Arc<format::Format>,
    start: usize,
}
impl FieldValues {
    // Drop stale argument columns when a span update overwrites a formatted field.
    fn clear_format(&mut self, field: usize) {
        if let Some(index) = self.formats.iter().position(|format| format.field == field) {
            let format = self.formats.remove(index);
            let end = format.start + format.recipe.template.value_count();
            // Compact deferred updates instead of retaining dead argument columns
            // after every overwrite of a long-lived span field.
            self.values.drain(format.start..end);
            self.strings.drain(format.start..end);
            for remaining in &mut self.formats {
                if remaining.start >= end {
                    remaining.start -= end - format.start;
                }
            }
        }
    }
    fn format_schema(&self, recipes: &mut Vec<Arc<format::Format>>) -> Vec<(usize, usize, usize)> {
        self.formats
            .iter()
            .map(|format| {
                let recipe = recipes
                    .iter()
                    .position(|r| Arc::ptr_eq(r, &format.recipe) || **r == *format.recipe)
                    .unwrap_or_else(|| {
                        recipes.push(format.recipe.clone());
                        recipes.len() - 1
                    });
                (format.field, recipe, format.start)
            })
            .collect()
    }
    /// Events, span starts and deferred updates share reference encoding.
    /// Scratch strings stay textual until the destination buffer is selected.
    fn write(&mut self, packer: &mut BytePacker<'_>, buffer: &mut Buffer, args: usize) {
        // Even strings above the buffer's dedup cutoff can share within one row.
        let mut strings = SmallVec::<[(&str, ValueData); 8]>::new();
        let mut intern = |text: &str| self.string_storage.intern(buffer, text);
        for (index, field) in self.values.iter_mut().enumerate() {
            if let Some(value) = field.take() {
                let value = match value {
                    ValueData::BufferOffset(_) | ValueData::BufferIndex(_) => {
                        let text = self.strings[index].as_str();
                        if let Some((_, value)) = strings.iter().find(|(s, _)| *s == text) {
                            *value
                        } else {
                            let value = intern(text);
                            strings.push((text, value));
                            value
                        }
                    }
                    value => value,
                };
                packer.write_any_value(value, false);
            }
        }
        for argument in &self.template_args[..args] {
            let value = if let Some((_, value)) = strings.iter().find(|(s, _)| *s == argument) {
                *value
            } else {
                let value = intern(argument);
                strings.push((argument.as_str(), value));
                value
            };
            packer.write_any_value(value, false);
        }
    }
    fn record(&mut self, record: impl FnOnce(&mut ValueRecorder<'_>)) {
        record(&mut ValueRecorder {
            data: self,
            is_log_compat: false,
            template: None,
            template_arg_count: None,
            formats: None,
        });
    }

    // Size both kinds of reusable scratch storage when the callsite is registered.
    fn new(vcount: usize, arg_count: usize, string_storage: StringStorage) -> Self {
        Self {
            string_storage,
            values: vec![None; vcount],
            strings: vec![String::new(); vcount],
            template_args: vec![String::new(); arg_count],
            formats: Vec::new(),
        }
    }
}

struct ValueRecorder<'a> {
    data: &'a mut FieldValues,
    is_log_compat: bool,
    template: Option<&'a template::CompiledTemplate>,
    // Preserve a successful zero-argument match without storing the message field index.
    template_arg_count: Option<usize>,
    formats: Option<&'a mut Vec<Arc<format::Format>>>,
}

impl ValueRecorder<'_> {
    // The comparison capture snapshots user formatting once, then shares the
    // owned arguments with this same packing path used by visit_format.
    fn record_format(
        &mut self,
        field: &tracing_core::Field,
        recipe: Arc<format::Format>,
        values: Vec<InputValue>,
    ) {
        if let Some(recipes) = &mut self.formats {
            if !recipes
                .iter()
                .any(|r| Arc::ptr_eq(r, &recipe) || **r == *recipe)
            {
                recipes.push(recipe.clone());
            }
        }
        self.data.clear_format(field.index());
        self.data.values[field.index()] = None;
        let start = self.data.values.len();
        // Arguments share ordinary typed columns; strings are interned only after
        // selecting the destination archetype and its current buffer.
        for value in values {
            let (value, text) = match value {
                // Capture the actual primitive layout before choosing block capacity.
                InputValue::I8(v) => (ValueData::I8(v), String::new()),
                InputValue::I16(v) => (ValueData::I16(v), String::new()),
                InputValue::I32(v) => (ValueData::I32(v), String::new()),
                InputValue::Isize(v) => (ValueData::Isize(v), String::new()),
                InputValue::U8(v) => (ValueData::U8(v), String::new()),
                InputValue::U16(v) => (ValueData::U16(v), String::new()),
                InputValue::U32(v) => (ValueData::U32(v), String::new()),
                InputValue::Usize(v) => (ValueData::Usize(v), String::new()),
                InputValue::F32(v) => (ValueData::F32(f32::from_bits(v)), String::new()),
                InputValue::Char(v) => (ValueData::Char(v), String::new()),
                InputValue::F64(v) => (ValueData::F64(f64::from_bits(v)), String::new()),
                InputValue::I64(v) => (ValueData::I64(v), String::new()),
                InputValue::U64(v) => (ValueData::U64(v), String::new()),
                InputValue::I128(v) => (ValueData::I128(v), String::new()),
                InputValue::U128(v) => (ValueData::U128(v), String::new()),
                InputValue::Bool(v) => (ValueData::Bool(v), String::new()),
                InputValue::Text(v) => (
                    match self.data.string_storage {
                        StringStorage::Offset => ValueData::BufferOffset(0),
                        StringStorage::Index => ValueData::BufferIndex(0),
                    },
                    v,
                ),
                InputValue::Bytes(_) => unreachable!("format arguments are primitive or textual"),
            };
            let index = self.data.values.len();
            self.data.values.push(Some(value));
            if let Some(scratch) = self.data.strings.get_mut(index) {
                scratch.clear();
                scratch.push_str(&text);
            } else {
                self.data.strings.push(text);
            }
        }
        self.data.formats.push(RecordedFormat {
            field: field.index(),
            recipe,
            start,
        });
        // Direct capture takes precedence over reverse matching of message text.
        if field.name() == "message" {
            self.template_arg_count = None;
        }
    }

    fn record_text(&mut self, field: &tracing_core::Field, value: impl std::fmt::Display) {
        use std::fmt::Write;
        if self.is_log_compat && field.name() != "message" {
            return; // tracing_log metadata is fields, but we already extracted it
        }

        self.data.clear_format(field.index());
        // Format once into scratch; physical offset/index references are assigned on write.
        let text = &mut self.data.strings[field.index()];
        text.clear();
        write!(text, "{value}").expect("writing to String");
        if field.name() == "message"
            && let Some(template) = self.template
            && let Some(captures) = template.captures_inline(text)
        {
            for (capture, argument) in captures.into_iter().zip(&mut self.data.template_args) {
                argument.clear();
                argument.push_str(capture);
            }
            self.data.values[field.index()] = None;
            self.template_arg_count = Some(template.field_names.len());
            return;
        }
        // Keep the placeholder's type identical to the eventual stored reference.
        self.data.values[field.index()] = Some(match self.data.string_storage {
            StringStorage::Offset => ValueData::BufferOffset(0),
            StringStorage::Index => ValueData::BufferIndex(0),
        });
    }
}

impl Visit for ValueRecorder<'_> {
    fn visit_format(
        &mut self,
        field: &tracing_core::Field,
        value: &tracing_core::field::Format<'_>,
    ) {
        if self.is_log_compat && field.name() != "message" {
            return;
        }
        let cached = self
            .formats
            .as_ref()
            .and_then(|recipes| recipes.iter().find(|recipe| recipe.template.matches(value)))
            .cloned();
        let (recipe, values) = if let Some(recipe) = cached {
            let values = recipe.capture(value).expect("capturing format arguments");
            (recipe, values)
        } else {
            let (recipe, values) =
                format::Format::capture_new(value).expect("compiling format arguments");
            let recipe = Arc::new(recipe);
            (recipe, values)
        };
        self.record_format(field, recipe, values);
    }

    fn record_str(&mut self, field: &tracing_core::Field, value: &str) {
        self.record_text(field, value);
    }

    fn record_debug(&mut self, field: &tracing_core::Field, value: &dyn std::fmt::Debug) {
        // Format once, directly into reusable text for events and span starts.
        self.record_text(field, format_args!("{value:?}"));
    }

    fn record_f64(&mut self, field: &tracing_core::Field, value: f64) {
        self.data.clear_format(field.index());
        self.data.values[field.index()] = Some(ValueData::F64(value));
    }

    fn record_i64(&mut self, field: &tracing_core::Field, value: i64) {
        self.data.clear_format(field.index());
        self.data.values[field.index()] = Some(ValueData::I64(value));
    }

    fn record_u64(&mut self, field: &tracing_core::Field, value: u64) {
        // log.line lives in normalized metadata, outside the message-only row buffer.
        if self.is_log_compat {
            return;
        }
        self.data.clear_format(field.index());
        self.data.values[field.index()] = Some(ValueData::U64(value));
    }

    fn record_i128(&mut self, field: &tracing_core::Field, value: i128) {
        self.data.clear_format(field.index());
        self.data.values[field.index()] = Some(ValueData::I128(value));
    }

    fn record_u128(&mut self, field: &tracing_core::Field, value: u128) {
        self.data.clear_format(field.index());
        self.data.values[field.index()] = Some(ValueData::U128(value));
    }

    fn record_bool(&mut self, field: &tracing_core::Field, value: bool) {
        self.data.clear_format(field.index());
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
    use tracing::{Level, Metadata, debug, info, span};
    use tracing_subscriber::{Registry, layer::SubscriberExt};

    use crate::data::stringbuffer::StringBufferId;
    use crate::subscriber::{LocalRecordingLayer, RecordingLayer};

    // Shared static metadata exercises both row headers without a seed subscriber.
    fn timestamp_metadata() -> [&'static Metadata<'static>; 2] {
        use tracing_core::Callsite;
        [
            tracing::callsite!(name: "timestamp event", kind: tracing_core::Kind::EVENT, fields:)
                .metadata(),
            tracing::callsite!(name: "timestamp span", kind: tracing_core::Kind::SPAN, fields:)
                .metadata(),
        ]
    }

    #[test]
    fn parent_timestamp_offsets_decode_active_archived_late_closes_and_evicted_parents() {
        let [event, span] = timestamp_metadata();
        let mut recorder = RecordingLayer::new();
        recorder.parent_timestamp_offsets = true;
        recorder.parent_archetyping = super::ParentArchetyping::CallsiteArch;
        recorder.global_memory_cap = usize::MAX;
        recorder.callsite_memory_cap = usize::MAX;
        let mut children = Vec::new();
        for i in 0..40u64 {
            let start = 1_000_000 + i * i * 10_000;
            let parent = recorder.record_start(span, timestamp(start), None, false, |_| {});
            let child =
                recorder.record_start(span, timestamp(start + 7), Some(parent), false, |_| {});
            recorder.record_start(event, timestamp(start + 11), Some(child), false, |_| {});
            children.push((child, start));
            if i % 2 == 0 {
                recorder.close_span(span, child, timestamp(start + 100), None, None);
            }
        }
        // Force both parent and child archives without depending on byte targets.
        for site in recorder.callsites.read().values() {
            let mut site = site.write();
            for arch in &mut site.cache {
                let columns = arch.archetype.column_layout();
                arch.data.cycle_with_columns(&columns);
            }
        }
        for &(child, start) in &children {
            assert_eq!(recorder.span_timestamp(child), Some(start + 7));
            recorder.close_span(span, child, timestamp(start + 100), None, None);
        }
        let sites = recorder.callsites.read();
        let shared = sites[&(span as *const _ as usize)].clone();
        drop(sites);
        {
            let site = shared.read();
            let mut count = 0;
            site.visit_rows(|r| {
                if r.parent.is_some() {
                    count += 1;
                    let start = children[(r.index - 1) as usize].1;
                    assert_eq!(r.timestamp, Some(start + 7));
                    assert_eq!(r.end_timestamp, Some(start + 100));
                }
            });
            assert_eq!(count, 40);
            assert!(site.cache.iter().any(|a| {
                a.data
                    .ring
                    .iter()
                    .any(|b| b.data.metadata.timestamp_parent_relative)
            }));
        }
        // Losing ancestors is explicitly allowed; report missing time instead
        // of interpreting elapsed ticks as an absolute timestamp.
        {
            let mut site = shared.write();
            let roots = site.cache.iter_mut().find(|a| !a.archetype.parent).unwrap();
            while let Some(block) = roots.data.ring.front() {
                let index = block.descriptor.index;
                roots.data.drop_chunk(index).unwrap();
            }
        }
        assert_eq!(recorder.span_timestamp(children[0].0), None);
        shared.read().visit_rows(|r| {
            if r.parent.is_some() {
                assert_eq!(r.timestamp, None);
                assert_eq!(r.end_timestamp, None);
            }
        });
    }

    #[test]
    fn parent_timestamp_cache_survives_field_width_promotion_and_projected_edits() {
        use tracing_core::Callsite;
        let metadata=tracing::callsite!(name:"parent timestamp promotions",kind:tracing_core::Kind::SPAN,fields:value).metadata();
        let field = metadata.fields().field("value").unwrap();
        let mut recorder = RecordingLayer::new();
        recorder.parent_timestamp_offsets = true;
        recorder.global_memory_cap = usize::MAX;
        recorder.callsite_memory_cap = usize::MAX;
        let values = [1, 70_000, u64::MAX, 19];
        let mut children = Vec::new();
        for (i, &value) in values.iter().enumerate() {
            let start = 1_000_000 + (i * i) as u64 * 100_000;
            let parent = recorder.record_start(metadata, timestamp(start), None, false, |_| {});
            children.push(recorder.record_start(
                metadata,
                timestamp(start + 3),
                Some(parent),
                false,
                |v| tracing::field::Visit::record_u64(v, &field, value),
            ));
        }
        let shared = recorder.callsites.read().values().next().unwrap().clone();
        {
            let mut site = shared.write();
            let arch = site.cache.iter_mut().find(|a| a.archetype.parent).unwrap();
            assert_eq!(
                arch.data.active.unit,
                arch.data.layouts.last().unwrap().unit() + 8
            );
            let columns = arch.archetype.column_layout();
            // Logical edits repack only the payload prefix and preserve scratch.
            {
                let (_, mut row) = arch
                    .data
                    .get_mut(children[0].index as usize, &columns)
                    .unwrap();
                row.get_mut()[8..16].copy_from_slice(&2u64.to_ne_bytes());
            }
            arch.data.cycle_with_columns(&columns);
        }
        shared.read().visit_rows(|r| {
            if r.parent.is_some() {
                let i = (r.index - 1) as usize;
                assert_eq!(r.timestamp, Some(1_000_003 + (i * i) as u64 * 100_000));
                assert_eq!(r.fields, vec![Some(super::InputValue::U64(values[i]))]);
            }
        });
    }

    fn timestamp(value: u64) -> super::Time {
        super::Time { value, shift: 0 }
    }

    #[test]
    fn promoted_span_payloads_preserve_ids_strings_and_late_closes() {
        use super::{InputValue, StringStorage};
        use tracing_core::{Callsite, field::Visit};
        let metadata = tracing::callsite!(
            name: "promoted payload", kind: tracing_core::Kind::SPAN,
            fields: left, right, text, copy
        )
        .metadata();
        let fields: Vec<_> = metadata.fields().iter().collect();
        // Above the buffer dedup cutoff, equality still needs one reference per
        // row. Exercise both reference formats and archived/active span closes.
        let text = "x".repeat(crate::data::stringbuffer::Buffer::MAX_STRING_LENGTH + 1);
        for storage in [StringStorage::Index, StringStorage::Offset] {
            let mut recorder = RecordingLayer::new();
            recorder.string_storage = storage;
            recorder.global_memory_cap = usize::MAX;
            recorder.callsite_memory_cap = usize::MAX;
            let values = [
                (1, 1, text.as_str()),
                (2, 2, text.as_str()),
                (1u64 << 40, 3, "different"),
            ];
            let mut ids = Vec::new();
            for (index, &(left, right, copy)) in values.iter().enumerate() {
                ids.push(recorder.record_start(
                    metadata,
                    timestamp(100 + index as u64),
                    None,
                    false,
                    |v| {
                        v.record_u64(&fields[0], left);
                        v.record_u64(&fields[1], right);
                        v.record_str(&fields[2], &text);
                        v.record_str(&fields[3], copy);
                    },
                ));
                if index == 0 {
                    let shared = recorder.callsites.read().values().next().unwrap().clone();
                    let mut site = shared.write();
                    let arch = &mut site.cache[0];
                    assert_eq!(arch.data.layouts[0].fields[1].source, 0);
                    assert_eq!(arch.data.layouts[0].fields[3].source, 2);
                    assert_eq!(arch.strings.active.len(), 1);
                    let columns = arch.archetype.column_layout();
                    arch.data.cycle_with_columns(&columns);
                }
            }
            for &id in &ids {
                recorder.close_span(metadata, id, timestamp(200), None, None);
            }
            let shared = recorder.callsites.read().values().next().unwrap().clone();
            let site = shared.read();
            assert_eq!(site.cache.len(), 1);
            assert_eq!(site.cache[0].data.layouts.len(), 2);
            site.visit_rows(|row| {
                let index = (row.index - 1) as usize;
                assert_eq!(row.span.unwrap().index, ids[index].index);
                assert_eq!(row.end_timestamp, Some(200));
                assert_eq!(
                    row.fields,
                    vec![
                        Some(InputValue::U64(values[index].0)),
                        Some(InputValue::U64(values[index].1)),
                        Some(InputValue::Text(text.clone())),
                        Some(InputValue::Text(values[index].2.into())),
                    ]
                );
            });
        }
    }

    #[test]
    fn parent_partitioning_preserves_parent_rows() {
        use super::{ParentArchetyping, SpanId};
        // Vary identity independently of the parent index; roots remain separate.
        let parents = [
            None,
            Some(SpanId {
                callsite: 1,
                archetype: 0,
                index: 10,
            }),
            Some(SpanId {
                callsite: 1,
                archetype: 1,
                index: 20,
            }),
            Some(SpanId {
                callsite: 2,
                archetype: 0,
                index: 30,
            }),
            Some(SpanId {
                callsite: 1,
                archetype: 0,
                index: 40,
            }),
        ];
        for (mode, expected) in [
            (ParentArchetyping::Off, 2),
            (ParentArchetyping::Callsite, 3),
            (ParentArchetyping::CallsiteArch, 4),
        ] {
            for metadata in timestamp_metadata() {
                let mut recorder = RecordingLayer::new();
                recorder.parent_archetyping = mode;
                let mut ids = Vec::new();
                for (i, parent) in parents.into_iter().enumerate() {
                    ids.push(recorder.record_start(
                        metadata,
                        timestamp(100 + i as u64),
                        parent,
                        metadata.is_event(),
                        |_| {},
                    ));
                }
                let sites = recorder.callsites.read();
                let mut site = sites.values().next().unwrap().write();
                assert_eq!(site.cache.len(), expected);
                assert_eq!(ids[1].archetype, ids[4].archetype);
                // Force archive decoding as well as the normal active-tail path.
                for arch in &mut site.cache {
                    let layout = arch.archetype.column_layout();
                    arch.data.cycle_with_columns(&layout);
                }
                let mut decoded = Vec::new();
                site.visit_rows(|r| decoded.push((r.timestamp.unwrap(), r.parent)));
                decoded.sort_by_key(|r| r.0);
                let identity = |p: Option<SpanId>| p.map(|p| (p.callsite, p.archetype, p.index));
                assert_eq!(
                    decoded.iter().map(|r| identity(r.1)).collect::<Vec<_>>(),
                    parents.map(identity)
                );
            }
        }
    }

    #[test]
    fn duration_origins_preserve_open_spans_and_late_closes() {
        let metadata = timestamp_metadata()[1];
        let mut recorder = RecordingLayer::new();
        recorder.global_memory_cap = usize::MAX;
        recorder.callsite_memory_cap = usize::MAX;
        let mut spans = Vec::new();
        let mut duration_state = 7u64;
        for row in 0..512u64 {
            let start = 1000 + row * 100;
            let id = recorder.record_start(metadata, timestamp(start), None, false, |_| {});
            // Values straddle a binary boundary; an exact origin removes the
            // otherwise varying high bits without relying on a duration stride.
            duration_state = duration_state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1);
            let duration = (1u64 << 40) - 128 + ((duration_state >> 32) % 251);
            recorder.close_span(metadata, id, timestamp(start + duration), None, None);
            spans.push((id, start, duration));
        }
        let open = recorder.record_start(metadata, timestamp(100_000), None, false, |_| {});
        let shared = recorder.callsites.read().values().next().unwrap().clone();
        {
            let mut site = shared.write();
            let arch = &mut site.cache[0];
            let layout = arch.archetype.column_layout();
            // Archive the open sentinel as well as the closed-duration blocks.
            arch.data.cycle_with_columns(&layout);
            assert!(arch.data.ring.iter().any(|block| {
                block
                    .data
                    .column_metadata(&layout)
                    .entries()
                    .any(|(column, record)| column.offset == 8 && record.first() == Some(&1))
            }));
        }
        // Exercise editing an origin-encoded duration and an archived open slot.
        spans[7].2 += 12345;
        recorder.close_span(
            metadata,
            spans[7].0,
            timestamp(spans[7].1 + spans[7].2),
            None,
            None,
        );
        shared.read().visit_rows(|row| {
            if row.index == open.index {
                assert_eq!(row.end_timestamp, Some(0));
            } else {
                let (_, start, duration) = spans[(row.index - 1) as usize];
                assert_eq!(row.timestamp, Some(start));
                assert_eq!(row.end_timestamp, Some(start + duration));
            }
        });
        recorder.close_span(metadata, open, timestamp(100_999), None, None);
        shared.read().visit_rows(|row| {
            if row.index == open.index {
                assert_eq!(row.end_timestamp, Some(100_999));
            }
        });
    }

    #[test]
    fn update_and_follows_references_share_footer_and_survive_eviction() {
        let metadata = timestamp_metadata()[1];
        let mut recorder = RecordingLayer::new();
        recorder.global_memory_cap = usize::MAX;
        recorder.callsite_memory_cap = usize::MAX;
        let mut spans = Vec::new();
        for row in 0..700u64 {
            let id = recorder.record_start(metadata, timestamp(1000 + row), None, false, |_| {});
            // Alternate shared and different callarch identities. The same
            // identity in both columns must not duplicate dictionary entries.
            let follows = super::SpanId {
                index: u64::MAX - row * 19,
                callsite: if row % 2 == 0 { id.callsite } else { 99 },
                archetype: if row % 2 == 0 { id.archetype } else { 7 },
            };
            recorder.close_span(metadata, id, timestamp(2000 + row), Some(follows), None);
            spans.push((id, follows));
            if row == 0 {
                let shared = recorder.callsites.read().values().next().unwrap().clone();
                let mut site = shared.write();
                for arch in &mut site.cache {
                    // Isolate the first row, then exercise normal rollover and a tail.
                    let layout = arch.archetype.column_layout();
                    arch.data.cycle_with_columns(&layout);
                }
            }
        }
        let shared = recorder.callsites.read().values().next().unwrap().clone();
        let check = |site: &super::PerCallsiteData| {
            let mut updates = 0;
            site.visit_rows(|row| {
                if row.timestamp.is_none() {
                    let id = row.span.unwrap();
                    let (expected, follows) = spans[(id.index - 1) as usize];
                    assert_eq!(
                        (id.callsite, id.archetype, id.index),
                        (expected.callsite, expected.archetype, expected.index)
                    );
                    let actual = row.follows_from.unwrap();
                    assert_eq!(
                        (actual.callsite, actual.archetype, actual.index),
                        (follows.callsite, follows.archetype, follows.index)
                    );
                    assert!(site.span_fields_ref(id).is_some());
                    updates += 1;
                }
            });
            updates
        };
        assert_eq!(check(&shared.read()), spans.len());
        let mut site = shared.write();
        let update = site
            .cache
            .iter_mut()
            .find(|arch| arch.archetype.is_span_end)
            .unwrap();
        assert_eq!(update.archetype.unit(), 20);
        let layout = update.archetype.column_layout();
        assert_eq!(update.data.ring[1].data.parent_offsets(&layout).len(), 2);
        let first = update.data.ring[0].descriptor.index;
        update.data.drop_chunk(first).unwrap();
        assert_eq!(check(&site), spans.len() - 1);
    }

    #[test]
    fn block_timestamp_offsets_preserve_event_and_span_starts_forever() {
        use crate::data::blockbuffer::BlockCache;
        for metadata in timestamp_metadata() {
            let mut recorder = RecordingLayer::new();
            recorder.global_memory_cap = usize::MAX;
            recorder.callsite_memory_cap = usize::MAX;
            recorder.record_start(metadata, timestamp(0), None, false, |_| {});
            let shared = recorder.callsites.read().values().next().unwrap().clone();
            {
                let mut site = shared.write();
                let arch = &mut site.cache[0];
                // Tiny fixed-capacity blocks force two archives and an active tail.
                arch.data = BlockCache::new(arch.archetype.unit(), 2, 2);
            }
            let times = [1_000, 1_001, 1_005, 5_000, 4_990];
            let spans: Vec<_> = times
                .into_iter()
                .map(|ts| recorder.record_start(metadata, timestamp(ts), None, false, |_| {}))
                .collect();
            if metadata.is_span() {
                // Patch archived and active rows; decoded starts must stay exact.
                for &span in &spans[..4] {
                    recorder.close_span(metadata, span, timestamp(6_000), None, None);
                }
            }
            let mut site = shared.write();
            let mut decoded = Vec::new();
            site.visit_rows(|row| {
                decoded.push(row.timestamp.unwrap());
                if metadata.is_span() {
                    assert_eq!(
                        row.end_timestamp,
                        Some(if row.index == 5 { 0 } else { 6_000 })
                    );
                }
            });
            assert_eq!(decoded, times);
            let data = &mut site.cache[0].data;
            let mut raw = data.iter(&[]);
            let mut offsets = Vec::new();
            while let Some(row) = raw.next() {
                offsets.push(u64::from_ne_bytes(row[..8].try_into().unwrap()));
            }
            drop(raw);
            assert_eq!(offsets, [0, 0, 0, 0, 0]); // Archived starts lie on their strides.
            assert_eq!(data.ring[0].descriptor.timestamp.start, 1_000);
            assert_eq!(data.ring[1].descriptor.timestamp.start, 1_005);
            assert_eq!(data.active_descriptor.timestamp.start, 4_990);
            data.drop_chunk(1).unwrap();
            assert!(data.descriptor(1).is_none());
            drop(site);
            let mut retained = Vec::new();
            shared
                .read()
                .visit_rows(|row| retained.push(row.timestamp.unwrap()));
            assert_eq!(retained, times[2..]);
        }
    }

    #[test]
    fn earlier_block_timestamp_panics_without_allocating_a_row() {
        let metadata = timestamp_metadata()[0];
        let recorder = RecordingLayer::new();
        recorder.record_start(metadata, timestamp(1_000), None, false, |_| {});
        let shared = recorder.callsites.read().values().next().unwrap().clone();
        // Rejection must leave the existing origin and row count intact.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            recorder.record_start(metadata, timestamp(999), None, false, |_| {});
        }));
        assert!(result.is_err());
        assert_eq!(shared.read().cache[0].data.active.length, 1);
    }

    #[test]
    fn timestamp_index_saturates_until_the_tick_changes() {
        fn emit() {
            tracing::info!(parent: None, "same origin");
        }
        let mut layer = RecordingLayer::new();
        layer.timestamp_bit_shift = 63; // All live calls in this test occupy tick zero.
        let recorder = Arc::new(layer);
        let dispatch =
            tracing::Dispatch::new(Registry::default().with(LocalRecordingLayer(recorder.clone())));
        tracing::dispatcher::with_default(&dispatch, || {
            emit();
            let (ts, origin, _) = recorder.last_event.lock().unwrap();
            // Seed just below the limit instead of emitting 65K transitions.
            *recorder.last_event.lock() = Some((ts, origin, u16::MAX - 1));
            emit(); // Same origin retains MAX - 1.
            tracing::info!(parent: None, "different origin"); // Reaches MAX.
            emit(); // Further transitions saturate, including returns to an old origin.
            emit();
        });
        let mut indices = Vec::new();
        for site in recorder.callsites.read().values() {
            site.read()
                .visit_rows(|row| indices.push(row.timestamp_index.unwrap()));
        }
        indices.sort_unstable();
        assert_eq!(indices, [0, u16::MAX - 1, u16::MAX, u16::MAX, u16::MAX]);

        // A new shifted tick restores precise indexing even after saturation.
        let metadata = recorder
            .callsites
            .read()
            .values()
            .next()
            .unwrap()
            .read()
            .metadata;
        recorder.record_start(
            metadata,
            super::Time {
                value: 1,
                shift: 63,
            },
            None,
            false,
            |_| {},
        );
        assert_eq!(recorder.last_event.lock().unwrap().2, 0);
    }

    #[test]
    fn matched_messages_store_only_buffer_offset_args() {
        fn emit(count: u64, message: &str) {
            tracing::info!(count, message, tail = "tail");
        }
        // Fill an archived block even when the configured target grows. The
        // fixed template has the smallest row: timestamp/index, count and tail offset.
        let pairs = super::MESSAGE_BLOCK_TARGET_SIZE
            / (size_of::<super::PackedTimestamp>() + size_of::<u16>() + 2 + 4)
            + 1;
        for format in ["hello {name} {}!", "fixed"] {
            // Keep this offset-specific contract explicit as the default is indexed.
            let mut layer = RecordingLayer::new();
            layer.string_storage = super::StringStorage::Offset;
            let recorder = Arc::new(layer);
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
                    super::FieldValues::new(3, template.field_names.len(), site.string_storage);
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
                    size_of::<super::PackedTimestamp>()
                        + size_of::<u16>()
                        + 8
                        + 4
                        + arg_count * size_of::<u32>()
                );
                assert_eq!(
                    matched.data.active.unit,
                    matched.data.layouts.last().unwrap().unit()
                );
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
                    arch.data.active_metadata.string_buffer = arch.strings.active_index();
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
                assert!(site.cache[0].strings.get(StringBufferId::new(0)).is_some());
                assert_eq!(
                    site.cache[1].strings.get(StringBufferId::new(0)).is_none(),
                    global
                );
            }
            recorder.callsite_memory_cap = 0;
            recorder.global_memory_cap = 0;
            recorder.drop_old_chunks(id);
            for arch in &shared.read().cache {
                assert!(arch.data.ring.is_empty());
                assert!(arch.strings.get(StringBufferId::new(0)).is_none());
                assert_eq!(arch.data.active.length, 1);
                assert_eq!(
                    arch.strings.active.get_index(0).as_deref(),
                    Some("protected")
                );
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
                assert_eq!(
                    arch.data.iter(&arch.archetype.column_layout()).count(),
                    1000
                );
            }
        }
    }
    #[test]
    fn only_empty_chunks_can_change_string_buffers() {
        use super::*;
        let mut layer = RecordingLayer::new();
        layer.string_compression_level = Some(9); // Exercise native HC through recording.
        let recorder = Arc::new(layer);
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
        arch.prepare_chunk(recorder.string_compression_level);
        assert_eq!(
            arch.data.active_metadata.string_buffer,
            StringBufferId::new(0)
        );
        arch.data.push(&vec![0; arch.archetype.unit()], 0);
        arch.data.cycle_if_needed();
        assert_eq!(arch.data.active.length, 0);
        arch.prepare_chunk(recorder.string_compression_level);
        arch.prepare_chunk(recorder.string_compression_level); // Repeated preparation must not rotate again.
        assert_eq!(
            arch.data.ring[0].data.metadata.string_buffer,
            StringBufferId::new(0)
        );
        assert_eq!(
            arch.data.active_metadata.string_buffer,
            StringBufferId::new(1)
        );
        let old = arch.strings.get(StringBufferId::new(0)).unwrap();
        assert_eq!(old.get_index(0).as_deref(), Some(text.as_str()));
    }
}
