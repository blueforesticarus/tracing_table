//! Collect storage chart evidence into one CSV on stdout.
//! Defaults to the Bevy + Actix captures; use FIXTURE [SHIFT] for a single run.
//! See doc/storage_chart.md; the CSV is the only interface to the renderer.

#[cfg(not(feature = "bit-compression"))]
fn main() {
    panic!("enable --features bit-compression");
}

#[cfg(feature = "bit-compression")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    collect::run()
}

#[cfg(feature = "bit-compression")]
mod collect {
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
            blockbuffer::{BlockCache, Data},
            dyntuple::ValueKind,
            stringbuffer::{Buffer, BufferCollection, CompressedBuffer},
        },
        subscriber::{
            Archetype, MESSAGE_BLOCK_MIN_ROWS, MESSAGE_BLOCK_TARGET_SIZE, PackedTimestamp,
            PerArchData, RecordingLayer, STRING_BUFFER_TARGET_SIZE, SpanFieldsRef, SpanId,
            StringStorage, dump::Fixture,
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
                for (member, size) in [("index", 8), ("callsite", 4), ("archetype", 4)] {
                    entry("metadata", &format!("{prefix}.{member}"), size, false);
                }
            }
        }
        if arch.parent {
            // Full identities/predictors live in trailers; rows store indices.
            entry("metadata", "parent.index", 8, false);
            entry("metadata", "parent.table_index", 2, false);
        }
        for kind in arch.field_types.iter().flatten() {
            match kind {
                ValueKind::BufferIndex => {
                    entry("string_field_index", "string_field_index", 2, true)
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
            .map(|&index| {
                let text = source.get_index(index).expect("stored active string");
                text.len() + if text.len() < u16::MAX as usize { 2 } else { 6 }
            })
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
                for row in frame.iter() {
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
            let raw = arch.strings.active.byte_len();
            let schema =
                size_of::<PerArchData>() - size_of::<BlockCache>() - size_of::<BufferCollection>()
                    + arch.archetype.field_types.capacity() * size_of::<Option<ValueKind>>();
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
                let writable_heap = writable - size_of::<Buffer>();
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
        // The payload's address measures the value header; prefix alignment gaps
        // are charged to frame_padding alongside final allocation padding.
        let frame_headers: usize = arch
            .data
            .ring
            .iter()
            .map(|block| {
                size_of::<u16>() + block.data.frame.buffer().as_ptr() as usize
                    - block.data.as_ref() as *const _ as *const u8 as usize
            })
            .sum();
        let parent_tables: usize = arch
            .data
            .ring
            .iter()
            .map(|block| {
                block.data.parent_offsets().len()
                    * tracing_table::data::blockbuffer::ParentOffsets::ENTRY_BYTES
            })
            .sum();
        let string_starts: usize = arch
            .data
            .ring
            .iter()
            .map(|block| block.data.string_index_footer_bytes())
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
        let archetype_fields =
            arch.archetype.field_types.capacity() * size_of::<Option<ValueKind>>();
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
        // Run names scope every callsite/archetype/block ID in combined evidence.
        run: String,
    }
    impl<W: Write> Csv<W> {
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
        let mut inputs = Vec::new();
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
            run: String::new(),
        };
        writeln!(output.writer, "{}", COLUMNS.join(","))?;
        for (run, path) in inputs {
            eprintln!("collecting run={run}: {path}");
            output.run = run;
            collect_fixture(&mut output, &path, shift, min_rows)?;
        }
        output.writer.flush()?;
        Ok(())
    }

    // Each fixture owns a fresh recorder and is dropped before the next replay;
    // run identity belongs only to the CSV, never to the captured tracing IDs.
    fn collect_fixture<W: Write>(
        output: &mut Csv<W>,
        path: &str,
        shift: u8,
        min_rows: usize,
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
        for (key, value) in [("schema", 2), ("timestamp_bit_shift", shift as usize)] {
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
                    ("framework", if source.is_log { "log" } else { "tracing" }.into()),
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
        // Chart evidence only measures the supported templates-on configuration.
        {
            let templates = true;
            let recorder = fixture.replay_with_options(templates, StringStorage::Index);
            let sites = recorder.callsites.read();
            for (callsite, source) in fixture.callsites.iter().enumerate() {
                let Some(site) = sites.get(&(source.metadata as *const _ as usize)) else {
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
                    ("framework", if source.is_log { "log" } else { "tracing" }.into()),
                ];
                // Per-callsite counters are the common denominator for both weighted
                // pies. Active-only sites still participate in the row-weighted view.
                let mut lifetime = LifetimeAudit::default();
                let mut metadata = BTreeMap::new();
                let (mut archived_rows, mut active_rows, mut blocks, mut span_starts) =
                    (0, 0, 0, 0);
                let (mut message_bytes, mut string_bytes) = (0, 0);
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
                    let mut buffers = BTreeSet::new();
                    let mut active_indices = BTreeSet::new();
                    let mut arch_block_bytes = 0;
                    for block in &arch.data.ring {
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
                        assert!(
                            rows >= min_rows && rows >= MESSAGE_BLOCK_TARGET_SIZE / frame.unit()
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
                        let mut attributed_bits = 0;
                        for part in &parts {
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
                            attributed_bits += part_bits;
                            output.row(
                                &block_identity,
                                &[
                                    ("group", part.group.into()),
                                    ("component", part.name.clone()),
                                    ("first_bytes", part.range.len().to_string()),
                                    ("mask_bytes", part.range.len().to_string()),
                                    ("varying_bits", (part_bits * (rows - 1)).to_string()),
                                    ("padding_bytes", format!("{share:.12}")),
                                    ("column_values", rows.to_string()),
                                ],
                            )?;
                        }
                        assert_eq!(attributed_bits, bits);
                        let ends = block.data.parent_offsets().len()
                            * tracing_table::data::blockbuffer::ParentOffsets::ENTRY_BYTES
                            + block.data.string_index_footer_bytes();
                        output.row(
                            &block_identity,
                            &[
                                ("group", "end_buffer".into()),
                                ("component", "end_buffer".into()),
                                ("compressed_bytes", ends.to_string()),
                            ],
                        )?;
                        if !ranges.is_empty() {
                            buffers.insert(block.data.metadata.string_buffer);
                            if block.data.metadata.string_buffer == arch.strings.active_index() {
                                let mut iter = block.data.iter();
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
                            let mut filtered = Buffer::default();
                            for &index in &active_indices {
                                filtered.add_string(
                                    arch.strings
                                        .active
                                        .get_index(index)
                                        .expect("stored string index"),
                                );
                            }
                            assert_eq!(filtered.len(), active_indices.len());
                            let packed = filtered
                                .compress_with_level(false, recorder.string_compression_level);
                            assert!(!packed.has_index());
                            lifetime.writable_archived_compressed += packed.byte_len();
                            packed.byte_len()
                        } else {
                            let bytes = arch
                                .strings
                                .get_compressed(id)
                                .expect("retained string buffer")
                                .byte_len();
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
                    for (key, bytes) in
                        metadata_overhead(arch, arch_block_bytes, frozen_bytes, arch_string_bytes)
                    {
                        *metadata.entry(key).or_insert(0usize) += bytes;
                    }
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
                    ("archived_rows", archived_rows),
                    ("active_rows", active_rows),
                    ("archived_blocks", blocks),
                    ("span_starts", span_starts),
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
