//! Collect storage chart evidence into one CSV on stdout.
//! Defaults to the Bevy + Actix captures; use FIXTURE [SHIFT] for a single run.
//! See doc/storage_chart.md; the CSV is the only interface to the renderer.

#[path = "support/string_characters.rs"]
mod string_characters;

#[cfg(not(feature = "bit-compression"))]
fn main() {
    panic!("enable --features bit-compression");
}

#[cfg(feature = "bit-compression")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    collect::run()
}

#[cfg(feature = "bit-compression")]
pub mod collect {
    use std::{
        collections::{BTreeMap, BTreeSet},
        fs::File,
        io::{self, BufReader, Write},
        ops::Range,
        sync::atomic::Ordering,
    };
    use streaming_iterator::StreamingIterator;
    use tracing_table::{
        data::{
            CompressionStats,
            dyntuple::ValueKind,
            stringbuffer::{Buffer, BufferCollection, CompressedBuffer},
        },
        subscriber::{
            Archetype, MESSAGE_BLOCK_MIN_ROWS, MESSAGE_BLOCK_TARGET_SIZE, PackedTimestamp,
            PerArchData, RecordingLayer, STRING_BUFFER_TARGET_SIZE, SpanFieldsRef, SpanId,
            dump::{Fixture, ReplayOptions},
            storage::{BlockCache, Data},
        },
    };

    struct Component {
        group: &'static str,
        name: String,
        range: Range<usize>,
        string_reference: bool,
    }

    fn components(arch: &Archetype) -> Vec<Component> {
        let mut result = Vec::new();
        let mut offset = 0;
        let mut entry = |group, name: &str, size, string_reference| {
            result.push(Component {
                group,
                name: name.into(),
                range: offset..offset + size,
                string_reference,
            });
            offset += size;
        };
        // Keep order identical to recorder row layout; the assertion below
        // catches schema drift. Deferred updates belong to their owning spans.
        assert_eq!(size_of::<SpanId>(), 16);
        assert_eq!(size_of::<SpanFieldsRef>(), 8);
        if !arch.is_span_end {
            entry("metadata", "timestamp", size_of::<PackedTimestamp>(), false);
            if arch.is_event {
                entry("metadata", "timestamp_index", 2, false);
            } else {
                entry(
                    "metadata",
                    "end_timestamp",
                    size_of::<PackedTimestamp>(),
                    false,
                );
            }
        }
        for (present, prefix) in [
            (arch.is_span_end, "update_span"),
            (arch.follows_from, "follows_from"),
        ] {
            if present {
                // All span references use residual + shared identity-table key.
                for (member, size) in [("index", 8), ("table_index", 2)] {
                    entry("metadata", &format!("{prefix}.{member}"), size, false);
                }
            }
        }
        if arch.parent {
            // Full identities/predictors live in trailers; rows store indices.
            entry("metadata", "parent.index", 8, false);
            entry("metadata", "parent.table_index", 2, false);
        }
        let argument_start = arch
            .formats
            .iter()
            .map(|(_, _, start)| *start)
            .min()
            .unwrap_or(usize::MAX);
        for (index, kind) in arch
            .field_types
            .iter()
            .enumerate()
            .filter_map(|(index, kind)| kind.as_ref().map(|kind| (index, kind)))
        {
            match kind {
                ValueKind::BufferIndex => {
                    let group = if index >= argument_start {
                        "format_args_index"
                    } else {
                        "string_field_index"
                    };
                    entry(group, group, 2, true)
                }
                ValueKind::BufferOffset => panic!("indexed replay required"),
                _ => entry(
                    "non_string_fields",
                    &format!("{kind:?}"),
                    kind.unit(),
                    false,
                ),
            }
        }
        for _ in 0..arch.template.unwrap_or(0) {
            entry("format_args_index", "format_args_index", 2, true);
        }
        assert_eq!(
            offset,
            arch.unit(),
            "components must cover every dyntuple byte: {arch:?}"
        );
        result
    }

    fn referenced_string_bytes(source: &Buffer, indices: &BTreeSet<usize>) -> usize {
        // Indices are deduplicated, and encoded length prefixes count as payload.
        // Shared strings intentionally also occur in the compressed archive set.
        indices
            .iter()
            .map(|&index| source.entry_byte_len(index).expect("stored active string"))
            .sum()
    }

    #[derive(Default)]
    struct LifetimeAudit {
        active_messages: usize,
        frozen_strings: usize,
        writable_archived_compressed: usize,
        writable_active_raw: usize,
        archived_message_overhead: usize,
        active_message_overhead: usize,
        archived_string_overhead: usize,
        writable_string_overhead: usize,
        resident_container_bytes: usize,
        writable_source_raw: usize,
    }

    impl LifetimeAudit {
        fn observe_active(&mut self, arch: &PerArchData, ranges: &[Range<usize>]) {
            // Keep payload independently usable; the snapshot now adds allocation
            // headers, capacities and lookup heaps to the corresponding slices.
            let frame = &arch.data.active;
            let used = (frame.length + 1) * frame.unit;
            self.active_messages += used;
            let mut active = BTreeSet::new();
            if !ranges.is_empty() && frame.length > 0 {
                assert_eq!(
                    arch.data.active_metadata.string_buffer,
                    arch.strings.active_index()
                );
                let mut rows = arch.data.active_rows();
                while let Some(row) = rows.next() {
                    for range in ranges {
                        active.insert(
                            u16::from_ne_bytes(row[range.clone()].try_into().unwrap()) as usize
                        );
                    }
                }
            }
            self.writable_active_raw += referenced_string_bytes(&arch.strings.active, &active);

            let block_total = allocated(arch.data.compression_ratio());
            let string_total = allocated(arch.strings.compression_ratio());
            let writable = allocated(arch.strings.active.compression_ratio());
            // "Raw" here means before outer LZ4; active payload can be FSST codes.
            let raw = arch.strings.active.stored_byte_len();
            let schema =
                size_of::<PerArchData>() - size_of::<BlockCache>() - size_of::<BufferCollection>()
                    + arch.archetype.field_types.capacity() * size_of::<Option<ValueKind>>()
                    + arch.archetype.formats.capacity() * size_of::<(usize, usize, usize)>();
            self.resident_container_bytes += block_total + string_total + schema;
            self.writable_source_raw += raw;
            if arch.data.ring.is_empty() {
                // With no archived blocks, all inline state and spare capacity
                // belong to the active overview instead of the archive overview.
                assert_eq!(arch.strings.offset(), arch.strings.active_index());
                self.active_message_overhead += block_total - used + schema;
                self.writable_string_overhead += string_total - raw;
            } else {
                // Preserve the existing archive overview's ownership of shared
                // containers and scratch. Only the active frame/string heaps
                // are additional active overhead for these archetypes.
                let archived_payload: usize = arch
                    .data
                    .ring
                    .iter()
                    .map(|block| block.data.frame.buffer().len())
                    .sum();
                let active_frame = allocated(frame.compression_ratio());
                let active_dictionary = arch.data.active_parent_offsets_heap_bytes();
                self.archived_message_overhead +=
                    block_total - active_frame - active_dictionary - archived_payload + schema;
                self.active_message_overhead += active_frame - used + active_dictionary;
                let frozen = arch.strings.compression_parts().1;
                let frozen_payload = frozen.compressed + 4 * arch.strings.archived_ids().len();
                // The shared codec belongs to archive metadata once archived
                // rows exist, even though its Arc owner is the writable buffer.
                let writable_heap =
                    writable - size_of::<Buffer>() - arch.strings.active.fsst_table_bytes();
                self.archived_string_overhead += string_total - writable_heap - frozen_payload;
                self.writable_string_overhead += writable_heap - raw;
            }
        }
    }

    fn allocated(stats: CompressionStats) -> usize {
        stats.compressed + stats.overhead + stats.overalloc
    }

    fn metadata_overhead(
        arch: &PerArchData,
        block_payload: usize,
        frozen_payload: usize,
        snapshot_payload: usize,
    ) -> BTreeMap<&'static str, usize> {
        // Scope: only archetypes with archived frames. Keep shared containers
        // once, excluding active frame/string heaps and allocator bookkeeping.
        let block_total = allocated(arch.data.compression_ratio())
            - allocated(arch.data.active.compression_ratio())
            - arch.data.active_parent_offsets_heap_bytes()
            - block_payload;
        let frame_overhead: usize = arch
            .data
            .ring
            .iter()
            .map(|block| block.data.allocation_size() - block.data.frame.buffer().len())
            .sum();
        // Count fixed metadata at either end of the frame, including tracing
        // retention starts. Only actual alignment gaps belong to frame_padding.
        let frame_headers: usize = arch
            .data
            .ring
            .iter()
            .enumerate()
            .map(|(ordinal, block)| {
                size_of::<u16>() + block.data.frame.buffer().as_ptr() as usize
                    - block.data.as_ref() as *const _ as *const u8 as usize
                    + block.data.retention_footer_bytes(arch.data.columns_for_block(
                        arch.data.offset + ordinal,
                        &arch.archetype.column_layout(),
                    ))
            })
            .sum();
        let parent_tables: usize = arch
            .data
            .ring
            .iter()
            .enumerate()
            .map(|(ordinal, block)| {
                block
                    .data
                    .parent_offsets(arch.data.columns_for_block(
                        arch.data.offset + ordinal,
                        &arch.archetype.column_layout(),
                    ))
                    .len()
                    * tracing_table::subscriber::storage::ParentOffsets::ENTRY_BYTES
            })
            .sum();
        // Preserve the historical CSV key; its payload now includes all field codecs.
        let string_starts: usize =
            arch.data
                .ring
                .iter()
                .enumerate()
                .map(|(ordinal, block)| {
                    block.data.column_footer_bytes(arch.data.columns_for_block(
                        arch.data.offset + ordinal,
                        &arch.archetype.column_layout(),
                    ))
                })
                .sum();
        let frame_padding = frame_overhead - frame_headers - parent_tables - string_starts;
        let block_entries = arch.data.ring.len() * size_of::<Data>();
        let block_spare = (arch.data.ring.capacity() - arch.data.ring.len()) * size_of::<Data>();
        // Thin span-list handles are already in shared active/archive metadata.
        // Charge their heap length prefixes and exact-size entries separately.
        let block_span_refs = arch.data.span_fields_heap_bytes();
        let block_scratch = block_total
            - size_of::<BlockCache>()
            - frame_headers
            - frame_padding
            - parent_tables
            - string_starts
            - block_entries
            - block_spare
            - block_span_refs;

        let (active, frozen) = arch.strings.compression_parts();
        let writable = arch.strings.active.compression_ratio();
        // Include the shared table once; frozen buffers only hold Arc handles.
        let fsst_tables = arch.strings.active.fsst_table_bytes();
        // Full BufferCollection includes its inline writable Buffer header. Its
        // heap is excluded; the pruned compressed snapshot contributes data only.
        let string_spare = active.overalloc - writable.overalloc;
        let string_descriptors = allocated(frozen) - frozen_payload;
        assert_eq!(
            string_descriptors,
            arch.strings.archived_ids().len() * size_of::<CompressedBuffer>()
        );
        // PerArchData embeds both containers. Subtract them before charging the
        // remaining archetype/schema header, so inline bytes are never repeated.
        let archetype_structs =
            size_of::<PerArchData>() - size_of::<BlockCache>() - size_of::<BufferCollection>();
        let archetype_fields = arch.archetype.field_types.capacity()
            * size_of::<Option<ValueKind>>()
            + arch.archetype.formats.capacity() * size_of::<(usize, usize, usize)>();
        let parts = BTreeMap::from([
            ("block_cache_structs", size_of::<BlockCache>()),
            ("block_scratch", block_scratch),
            ("block_entries", block_entries),
            ("block_spare", block_spare),
            ("block_span_refs", block_span_refs),
            ("frame_headers", frame_headers),
            ("frame_padding", frame_padding),
            ("parent_tables", parent_tables),
            ("string_starts", string_starts),
            ("string_collection_structs", size_of::<BufferCollection>()),
            ("string_descriptors", string_descriptors),
            ("string_spare", string_spare),
            ("fsst_tables", fsst_tables),
            ("archetype_structs", archetype_structs),
            ("archetype_fields", archetype_fields),
        ]);
        // Independently reconcile the snapshot against resident allocations,
        // replacing only the active string heap with its compressed snapshot.
        let expected = allocated(arch.data.compression_ratio())
            + allocated(arch.strings.compression_ratio())
            + archetype_structs
            + archetype_fields
            - allocated(arch.data.active.compression_ratio())
            - arch.data.active_parent_offsets_heap_bytes()
            - (allocated(writable) - size_of::<Buffer>())
            + fsst_tables
            + (snapshot_payload - frozen_payload);
        assert_eq!(
            block_payload + snapshot_payload + parts.values().sum::<usize>(),
            expected
        );
        parts
    }

    // Named cells keep sparse record types readable and avoid hand-counted commas.
    // Every field is quoted: captures can contain commas, quotes and newlines.
    const COLUMNS: &[&str] = &[
        "record",
        "run",
        "templates",
        "callsite",
        "record_kind",
        "key",
        "value",
        "archetype",
        "block_index",
        "rows",
        "group",
        "component",
        "first_bytes",
        "mask_bytes",
        "footer_bytes",
        "varying_bits",
        "padding_bytes",
        "compressed_bytes",
        "column_values",
        "file",
        "line",
        "target",
        "name",
        "format",
        "fields",
        "framework",
    ];
    struct Csv<W: Write> {
        writer: W,
        // Optional offline corpus export shares the exact character-count population.
        entries: Option<io::BufWriter<File>>,
        // Run names scope every callsite/archetype/block ID in combined evidence.
        run: String,
    }
    impl<W: Write> Csv<W> {
        fn string_entry(
            &mut self,
            callsite: usize,
            archetype: usize,
            buffer: u32,
            index: usize,
            text: &str,
        ) -> io::Result<()> {
            if let Some(entries) = &mut self.entries {
                writeln!(
                    entries,
                    "\"{}\",{callsite},{archetype},{buffer},{index},\"{}\"",
                    self.run.replace('"', "\"\""),
                    text.replace('"', "\"\"")
                )?;
            }
            Ok(())
        }

        fn row(&mut self, identity: &[(&str, String)], cells: &[(&str, String)]) -> io::Result<()> {
            for (index, column) in COLUMNS.iter().enumerate() {
                if index != 0 {
                    write!(self.writer, ",")?;
                }
                let value = if *column == "run" {
                    self.run.as_str()
                } else {
                    cells
                        .iter()
                        .chain(identity)
                        .find(|(key, _)| key == column)
                        .map_or("", |(_, value)| value.as_str())
                };
                write!(self.writer, "\"{}\"", value.replace('"', "\"\""))?;
            }
            writeln!(self.writer)
        }
        fn counter(
            &mut self,
            identity: &[(&str, String)],
            key: &str,
            value: usize,
        ) -> io::Result<()> {
            self.row(
                identity,
                &[
                    ("record", "counter".into()),
                    ("key", key.into()),
                    ("value", value.to_string()),
                ],
            )
        }
    }

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        // Cargo adds --bench even for targets with the benchmark harness disabled.
        let mut args = std::env::args().skip(1).filter(|arg| arg != "--bench");
        let settings = RecordingLayer::new();
        let mut shift = settings.timestamp_bit_shift;
        let mut min_rows = settings.message_block_min_rows;
        let mut replay = ReplayOptions::default();
        let mut inputs = Vec::new();
        let mut entries_path = None;
        let mut args = args.by_ref().peekable();
        // Preserve the historical FIXTURE [SHIFT] invocation for investigations.
        if args.peek().is_some_and(|arg| !arg.starts_with("--")) {
            let path = args.next().unwrap();
            let name = std::path::Path::new(&path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .split('.')
                .next()
                .unwrap()
                .trim_end_matches("_reference")
                .to_owned();
            inputs.push((name, path));
            if args.peek().is_some_and(|arg| !arg.starts_with("--")) {
                shift = args.next().unwrap().parse()?;
            }
        }
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--string-entries" => {
                    if entries_path.is_some() {
                        return Err("--string-entries may only be supplied once".into());
                    }
                    entries_path = Some(args.next().ok_or("missing string entries path")?);
                }
                "--format-args"
                | "--templates"
                | "--column-sharing"
                | "--integer-widths"
                | "--float-widths"
                | "--parent-timestamp-offsets" => {
                    let enabled = match args.next().as_deref() {
                        Some("on") => true,
                        Some("off") => false,
                        _ => return Err(format!("{arg} expects on or off").into()),
                    };
                    match arg.as_str() {
                        "--format-args" => replay.format_args = enabled,
                        "--templates" => replay.extract_templates = enabled,
                        "--column-sharing" => replay.layout_policy.sharing = enabled,
                        "--integer-widths" => replay.layout_policy.integers = enabled,
                        "--parent-timestamp-offsets" => {
                            replay.parent_timestamp_offsets = Some(enabled)
                        }
                        _ => replay.layout_policy.floats = enabled,
                    }
                }
                "--min-block-rows" => min_rows = args.next().ok_or("missing row count")?.parse()?,
                "--timestamp-shift" => {
                    shift = args.next().ok_or("missing timestamp shift")?.parse()?
                }
                "--fixture" => {
                    let value = args.next().ok_or("missing RUN=PATH")?;
                    let (run, path) = value.split_once('=').ok_or("expected RUN=PATH")?;
                    if run.is_empty() || path.is_empty() {
                        return Err("expected nonempty RUN=PATH".into());
                    }
                    inputs.push((run.to_owned(), path.to_owned()));
                }
                _ => return Err(format!("unexpected argument: {arg}").into()),
            }
        }
        if inputs.is_empty() {
            let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
            inputs = ["bevy", "actix"]
                .map(|run| {
                    (
                        run.to_owned(),
                        root.join(format!("doc/{run}_reference.msgpack.lz4"))
                            .to_string_lossy()
                            .into_owned(),
                    )
                })
                .to_vec();
        }
        let mut names = BTreeSet::new();
        for (run, path) in &inputs {
            if !names.insert(run) {
                return Err(format!("duplicate run: {run}").into());
            }
            // Fail before writing partial evidence if a default capture is missing.
            File::open(path)?;
        }
        MESSAGE_BLOCK_MIN_ROWS.store(min_rows, Ordering::Relaxed);
        // Preserve the reference experiment's policy independently of environment.
        STRING_BUFFER_TARGET_SIZE.store(32768, Ordering::Relaxed);
        let mut output = Csv {
            writer: io::BufWriter::new(io::stdout().lock()),
            entries: entries_path
                .map(File::create)
                .transpose()?
                .map(io::BufWriter::new),
            run: String::new(),
        };
        if let Some(entries) = &mut output.entries {
            writeln!(entries, "run,callsite,archetype,buffer,index,text")?;
        }
        writeln!(output.writer, "{}", COLUMNS.join(","))?;
        for (run, path) in inputs {
            eprintln!("collecting run={run}: {path}");
            output.run = run;
            collect_fixture(&mut output, &path, shift, min_rows, replay)?;
        }
        output.writer.flush()?;
        if let Some(entries) = &mut output.entries {
            entries.flush()?;
        }
        Ok(())
    }

    // Each fixture owns a fresh recorder and is dropped before the next replay;
    // run identity belongs only to the CSV, never to the captured tracing IDs.
    fn collect_fixture<W: Write>(
        output: &mut Csv<W>,
        path: &str,
        shift: u8,
        min_rows: usize,
        replay: ReplayOptions,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut fixture = Fixture::load(BufReader::new(File::open(path)?))?;
        assert!(
            shift >= fixture.timestamp_bit_shift && shift <= 63,
            "cannot recover finer timestamps"
        );
        for record in &mut fixture.records {
            record.timestamp >>= shift - fixture.timestamp_bit_shift;
        }
        fixture.timestamp_bit_shift = shift;
        let recorder = fixture.replay_with_options(replay);
        collect_recording(output, &fixture, &recorder, min_rows, None, replay)
    }

    /// Carry the actual replay policy into evidence so charts never infer it.
    pub fn write_recording<W: Write>(
        writer: W,
        run: &str,
        fixture: &Fixture,
        recorder: &RecordingLayer,
        ids: Option<&std::collections::HashMap<usize, u64>>,
        replay: ReplayOptions,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut output = Csv {
            writer,
            entries: None,
            run: run.into(),
        };
        writeln!(output.writer, "{}", COLUMNS.join(","))?;
        collect_recording(
            &mut output,
            fixture,
            recorder,
            recorder.message_block_min_rows,
            ids,
            replay,
        )?;
        output.writer.flush()?;
        Ok(())
    }

    fn collect_recording<W: Write>(
        output: &mut Csv<W>,
        fixture: &Fixture,
        recorder: &RecordingLayer,
        min_rows: usize,
        ids: Option<&std::collections::HashMap<usize, u64>>,
        replay: ReplayOptions,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let shift = fixture.timestamp_bit_shift;
        for (key, value) in [
            ("schema", 2),
            ("timestamp_bit_shift", shift as usize),
            ("min_block_rows", min_rows),
            ("format_args", usize::from(replay.format_args)),
            ("extract_templates", usize::from(replay.extract_templates)),
            ("column_sharing", usize::from(replay.layout_policy.sharing)),
            ("integer_widths", usize::from(replay.layout_policy.integers)),
            ("float_widths", usize::from(replay.layout_policy.floats)),
            // Persist the actual partition mode alongside each measurement.
            ("parent_archetyping", recorder.parent_archetyping as usize),
            (
                "parent_timestamp_offsets",
                usize::from(recorder.parent_timestamp_offsets),
            ),
        ] {
            output.row(
                &[],
                &[
                    ("record", "setting".into()),
                    ("key", key.into()),
                    ("value", value.to_string()),
                ],
            )?;
        }
        for (callsite, source) in fixture.callsites.iter().enumerate() {
            let metadata = source.metadata;
            output.row(
                &[
                    ("record", "source".into()),
                    ("callsite", callsite.to_string()),
                ],
                &[
                    // Capture provenance distinguishes bridged log events from tracing.
                    (
                        "framework",
                        if source.is_log { "log" } else { "tracing" }.into(),
                    ),
                    ("file", metadata.file().unwrap_or("").into()),
                    ("line", metadata.line().unwrap_or(0).to_string()),
                    ("target", metadata.target().into()),
                    ("name", metadata.name().into()),
                    (
                        "format",
                        source.format.as_deref().unwrap_or(metadata.name()).into(),
                    ),
                    (
                        "fields",
                        metadata
                            .fields()
                            .iter()
                            .map(|field| field.name())
                            .collect::<Vec<_>>()
                            .join(", "),
                    ),
                ],
            )?;
        }
        // Keep the existing mode dimension, with explicit policy in settings.
        {
            let templates = replay.extract_templates;
            let sites = recorder.callsites.read();
            // Recorder-wide weak lookup storage belongs to metadata once, not
            // once per archetype. It contains no expanded timestamp values.
            let mut timestamp_registry = recorder.parent_timestamp_registry_bytes();
            for (callsite, source) in fixture.callsites.iter().enumerate() {
                let key = if let Some(ids) = ids {
                    *ids.iter()
                        .find(|(_, id)| **id == callsite as u64)
                        .expect("captured callsite identity")
                        .0
                } else {
                    source.metadata as *const _ as usize
                };
                let Some(site) = sites.get(&key) else {
                    continue;
                };
                let site = site.read();
                let kind = if source.metadata.is_span() {
                    "span"
                } else if source.is_log {
                    "log"
                } else {
                    "event"
                };
                let identity = [
                    ("templates", templates.to_string()),
                    ("callsite", callsite.to_string()),
                    ("record_kind", kind.into()),
                    // Shared identity propagates provenance to every measurement row.
                    (
                        "framework",
                        if source.is_log { "log" } else { "tracing" }.into(),
                    ),
                ];
                // Per-callsite counters are the common denominator for both weighted
                // pies. Active-only sites still participate in the row-weighted view.
                let mut lifetime = LifetimeAudit::default();
                let mut metadata = BTreeMap::new();
                let (mut archived_rows, mut active_rows, mut blocks, mut span_starts) =
                    (0, 0, 0, 0);
                let (mut relative_blocks, mut relative_rows) = (0, 0);
                let (mut message_bytes, mut string_bytes) = (0, 0);
                // Count stored entries once per retained buffer, rather than
                // counting repeated row references or globally distinct text.
                let mut archived_string_entries = 0;
                // The collector is also nested inside both capture examples.
                let mut characters = super::string_characters::CharacterCounts::default();
                // Natural metadata units include each retained writable buffer,
                // whose inline header is charged alongside frozen descriptors.
                let (mut archived_archetypes, mut string_buffers) = (0, 0);
                // Natural population for the legend's bytes per trained archetype.
                let mut fsst_archetypes = 0;
                let mut archived_field_columns = BTreeMap::<&str, usize>::new();
                for arch in &site.cache {
                    active_rows += arch.data.active.length;
                    let parts = components(&arch.archetype);
                    let ranges: Vec<_> = parts
                        .iter()
                        .filter(|part| part.string_reference)
                        .map(|part| part.range.clone())
                        .collect();
                    if arch.data.ring.is_empty() {
                        lifetime.observe_active(arch, &ranges);
                        continue;
                    }
                    // Schema column counts are separate from the row-weighted
                    // column_values already exported by each physical leaf.
                    archived_archetypes += 1;
                    string_buffers += arch.strings.archived_ids().len() + 1;
                    for part in &parts {
                        *archived_field_columns.entry(part.group).or_default() += 1;
                    }
                    let mut buffers = BTreeSet::new();
                    let mut active_indices = BTreeSet::new();
                    let mut arch_block_bytes = 0;
                    for (ordinal, block) in arch.data.ring.iter().enumerate() {
                        let layout_fallback = arch.archetype.column_layout();
                        let block_columns = arch
                            .data
                            .columns_for_block(arch.data.offset + ordinal, &layout_fallback);
                        if block.data.timestamp_parent_relative(block_columns) {
                            relative_blocks += 1;
                            relative_rows += block.data.frame.len();
                        }
                        // Costs cover unique stored columns; logical ranges remain
                        // available separately for reconstructed string references.
                        let projection = arch.data.layout_for_block(arch.data.offset + ordinal);
                        let physical_parts: Vec<_> = parts
                            .iter()
                            .filter_map(|part| {
                                let range = projection.map_or_else(
                                    || Some(part.range.clone()),
                                    |p| p.physical_range(part.range.clone()),
                                )?;
                                Some(Component {
                                    group: part.group,
                                    name: part.name.clone(),
                                    range,
                                    string_reference: part.string_reference,
                                })
                            })
                            .collect();
                        let fallback = arch.archetype.column_layout();
                        let columns = arch
                            .data
                            .columns_for_block(arch.data.offset + ordinal, &fallback);
                        let frame = &block.data.frame;
                        let rows = frame.len();
                        let block_identity = [
                            identity.to_vec(),
                            vec![
                                ("record", "leaf".into()),
                                ("archetype", arch.id.to_string()),
                                ("block_index", block.descriptor.index.to_string()),
                                ("rows", rows.to_string()),
                            ],
                        ]
                        .concat();
                        // Rollover includes the active-only parent start column;
                        // the archived unit is narrower after stripping it.
                        let active_unit =
                            frame.unit() + usize::from(arch.data.caches_parent_timestamps()) * 8;
                        assert!(
                            rows >= min_rows && rows >= MESSAGE_BLOCK_TARGET_SIZE / active_unit
                        );
                        let bits: usize = frame
                            .mask()
                            .iter()
                            .map(|byte| byte.count_ones() as usize)
                            .sum();
                        assert_eq!(frame.compact_unit(), bits.div_ceil(8));
                        let packed_bytes = ((rows - 1) * frame.packed_row_bits()).div_ceil(8);
                        let padding = 8 * packed_bytes - bits * (rows - 1);
                        assert_eq!(frame.buffer().len(), 2 * frame.unit() + packed_bytes);
                        arch_block_bytes += frame.buffer().len();
                        archived_rows += rows;
                        blocks += 1;
                        if !arch.archetype.is_event && !arch.archetype.is_span_end {
                            span_starts += rows;
                        }
                        // Charge directory and codec bytes to the owning physical field;
                        // shared parent tables are divided among reference fields.
                        let footers: Vec<_> = block
                            .data
                            .column_metadata(columns)
                            .entries()
                            .map(|(layout, record)| (layout.range(), 1 + record.len()))
                            .collect();
                        let parent_bytes = block.data.parent_offsets(columns).len()
                            * tracing_table::subscriber::storage::ParentOffsets::ENTRY_BYTES;
                        let references = physical_parts
                            .iter()
                            .filter(|part| part.name.ends_with(".table_index"))
                            .count();
                        let mut attributed_footer = 0.0;
                        let mut attributed_bits = 0;
                        for part in &physical_parts {
                            let part_bits: usize = frame.mask()[part.range.clone()]
                                .iter()
                                .map(|byte| byte.count_ones() as usize)
                                .sum();
                            // Pad only varying bits. Zero-width/constant columns keep
                            // their first/mask costs, with no invented padding share.
                            let share = if bits == 0 {
                                assert_eq!(padding, 0);
                                0.0
                            } else {
                                padding as f64 / 8.0 * part_bits as f64 / bits as f64
                            };
                            let mut footer = footers
                                .iter()
                                .filter(|(range, _)| part.range.contains(&range.start))
                                .map(|(_, bytes)| *bytes as f64)
                                .sum::<f64>();
                            if part.name.ends_with(".table_index") {
                                footer += parent_bytes as f64 / references as f64;
                            }
                            attributed_footer += footer;
                            attributed_bits += part_bits;
                            output.row(
                                &block_identity,
                                &[
                                    ("group", part.group.into()),
                                    ("component", part.name.clone()),
                                    ("first_bytes", part.range.len().to_string()),
                                    ("mask_bytes", part.range.len().to_string()),
                                    ("footer_bytes", format!("{footer:.12}")),
                                    ("varying_bits", (part_bits * (rows - 1)).to_string()),
                                    ("padding_bytes", format!("{share:.12}")),
                                    ("column_values", rows.to_string()),
                                ],
                            )?;
                        }
                        assert_eq!(attributed_bits, bits);
                        assert!(
                            (attributed_footer
                                - (parent_bytes + block.data.column_footer_bytes(columns)) as f64)
                                .abs()
                                < 1e-6
                        );
                        if !ranges.is_empty() {
                            buffers.insert(block.data.metadata.string_buffer);
                            if block.data.metadata.string_buffer == arch.strings.active_index() {
                                let mut iter = tracing_table::data::dataframe::ProjectedIter::new(
                                    block.data.iter(columns),
                                    projection,
                                );
                                while let Some(row) = iter.next() {
                                    for range in &ranges {
                                        active_indices.insert(u16::from_ne_bytes(
                                            row[range.clone()].try_into().unwrap(),
                                        )
                                            as usize);
                                    }
                                }
                            }
                        }
                    }
                    message_bytes += arch_block_bytes;
                    let (mut frozen_bytes, mut arch_string_bytes) = (0, 0);
                    for &id in &buffers {
                        let payload = if id == arch.strings.active_index() {
                            // Recompress only archived references, in original order.
                            // Shared writable strings also remain in the active set.
                            let mut filtered = arch.strings.active.empty_with_same_encoding();
                            for &index in &active_indices {
                                let text = arch
                                    .strings
                                    .active
                                    .get_index(index)
                                    .expect("stored string index");
                                characters.add(&text);
                                output.string_entry(
                                    callsite,
                                    arch.id as usize,
                                    id.get(),
                                    index,
                                    &text,
                                )?;
                                filtered.add_string(&text);
                            }
                            assert_eq!(filtered.len(), active_indices.len());
                            archived_string_entries += filtered.len();
                            let packed = filtered
                                .compress_with_level(false, recorder.string_compression_level);
                            assert!(!packed.has_index());
                            lifetime.writable_archived_compressed += packed.byte_len();
                            packed.byte_len()
                        } else {
                            let buffer = arch
                                .strings
                                .get_compressed(id)
                                .expect("retained string buffer");
                            // Expand once and exclude length prefixes/index bytes.
                            let expanded = buffer.decompress()?;
                            archived_string_entries += expanded.len();
                            for index in 0..expanded.len() {
                                let text = expanded.get_index(index).expect("stored string index");
                                characters.add(&text);
                                output.string_entry(
                                    callsite,
                                    arch.id as usize,
                                    id.get(),
                                    index,
                                    &text,
                                )?;
                            }
                            let bytes = buffer.byte_len();
                            frozen_bytes += bytes;
                            bytes
                        };
                        // LZ4 is shared, so assign its bytes once in proportion to
                        // archived block rows. There is no exact per-string split.
                        let sharing: Vec<_> = arch
                            .data
                            .ring
                            .iter()
                            .filter(|block| block.data.metadata.string_buffer == id)
                            .collect();
                        let sharing_rows: usize =
                            sharing.iter().map(|block| block.data.frame.len()).sum();
                        for block in sharing {
                            let rows = block.data.frame.len();
                            let bytes = payload as f64 * rows as f64 / sharing_rows as f64;
                            output.row(
                                &identity,
                                &[
                                    ("record", "leaf".into()),
                                    ("archetype", arch.id.to_string()),
                                    ("block_index", block.descriptor.index.to_string()),
                                    ("rows", rows.to_string()),
                                    ("group", "strings".into()),
                                    ("component", "compressed_payload".into()),
                                    ("compressed_bytes", format!("{bytes:.12}")),
                                ],
                            )?;
                        }
                        arch_string_bytes += payload;
                    }
                    assert_eq!(
                        buffers
                            .iter()
                            .copied()
                            .filter(|&id| id != arch.strings.active_index())
                            .collect::<BTreeSet<_>>(),
                        arch.strings.archived_ids().collect()
                    );
                    assert_eq!(
                        frozen_bytes,
                        arch.strings.compression_parts().1.compressed
                            + 4 * arch.strings.archived_ids().len()
                    );
                    lifetime.frozen_strings += frozen_bytes;
                    string_bytes += arch_string_bytes;
                    lifetime.observe_active(arch, &ranges);
                    fsst_archetypes += usize::from(arch.strings.active.fsst_table_bytes() != 0);
                    for (key, bytes) in
                        metadata_overhead(arch, arch_block_bytes, frozen_bytes, arch_string_bytes)
                    {
                        *metadata.entry(key).or_insert(0usize) += bytes;
                    }
                }
                // Recipes are per-callsite metadata. Charge their actual retained
                // capacities once, rather than once per row or archetype.
                let recipes = site.formats.capacity()
                    * size_of::<tracing_table::subscriber::Recipe>()
                    + site
                        .formats
                        .iter()
                        .map(|recipe| recipe.heap_bytes())
                        .sum::<usize>()
                    + site
                        .template
                        .as_ref()
                        .map_or(0, |recipe| recipe.heap_bytes());
                lifetime.resident_container_bytes += recipes;
                let registry = std::mem::take(&mut timestamp_registry);
                lifetime.resident_container_bytes += registry;
                if blocks > 0 {
                    lifetime.archived_message_overhead += recipes + registry;
                    metadata.insert("format_recipes", recipes);
                    metadata.insert("parent_timestamp_registry", registry);
                } else {
                    lifetime.active_message_overhead += recipes + registry;
                }
                let ends = metadata.get("parent_tables").copied().unwrap_or(0)
                    + metadata.get("string_starts").copied().unwrap_or(0);
                assert_eq!(
                    lifetime.frozen_strings + lifetime.writable_archived_compressed,
                    string_bytes
                );
                assert_eq!(
                    lifetime.archived_message_overhead + lifetime.archived_string_overhead,
                    metadata.values().sum::<usize>()
                );
                // Independent resident-allocation check catches lost/double-counted
                // headers and shared strings, even when pie totals agree internally.
                let overhead = lifetime.archived_message_overhead
                    + lifetime.active_message_overhead
                    + lifetime.archived_string_overhead
                    + lifetime.writable_string_overhead;
                assert_eq!(
                    message_bytes
                        + lifetime.active_messages
                        + string_bytes
                        + lifetime.writable_active_raw
                        + overhead,
                    lifetime.resident_container_bytes - lifetime.writable_source_raw
                        + lifetime.writable_archived_compressed
                        + lifetime.writable_active_raw
                );
                for (key, value) in [
                    ("archived_string_entries", archived_string_entries),
                    ("archived_rows", archived_rows),
                    ("active_rows", active_rows),
                    ("archived_blocks", blocks),
                    // Include active-only partitions when comparing schema growth.
                    ("total_archetypes", site.cache.len()),
                    ("archived_archetypes", archived_archetypes),
                    ("fsst_archetypes", fsst_archetypes),
                    ("string_buffers", string_buffers),
                    ("span_starts", span_starts),
                    ("parent_relative_blocks", relative_blocks),
                    ("parent_relative_rows", relative_rows),
                    (
                        "lifetime.archived_messages",
                        message_bytes + lifetime.archived_message_overhead,
                    ),
                    (
                        "lifetime.active_messages",
                        lifetime.active_messages + lifetime.active_message_overhead,
                    ),
                    (
                        "lifetime.archived_strings",
                        string_bytes + lifetime.archived_string_overhead,
                    ),
                    (
                        "lifetime.writable_active_raw",
                        lifetime.writable_active_raw + lifetime.writable_string_overhead,
                    ),
                ] {
                    output.counter(&identity, key, value)?;
                }
                for (group, count) in archived_field_columns {
                    output.counter(&identity, &format!("archived_columns.{group}"), count)?;
                }
                assert_eq!(characters.strings, archived_string_entries);
                // Emit zero buckets too, distinguishing absence from old evidence
                // that predates character profiling. ASCII IDs are decimal 0..127.
                for bucket in 0..129 {
                    let name = if bucket == 128 {
                        "non_ascii".to_owned()
                    } else {
                        format!("ascii.{bucket}")
                    };
                    output.counter(
                        &identity,
                        &format!("string_characters.occurrences.{name}"),
                        characters.occurrences[bucket],
                    )?;
                    output.counter(
                        &identity,
                        &format!("string_characters.strings_containing.{name}"),
                        characters.strings_containing[bucket],
                    )?;
                }
                // Trailer bytes are payload, never container overhead. Their exact
                // per-block leaves above replace the old stderr reclassification.
                for (key, value) in metadata {
                    if key != "parent_tables" && key != "string_starts" {
                        output.counter(&identity, &format!("metadata.{key}"), value)?;
                    }
                }
                assert!(lifetime.archived_message_overhead >= ends);
            }
        }
        Ok(())
    }
}
