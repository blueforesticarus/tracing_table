//! Compare string compression layouts while reading a MessagePack dump record by record.
//!
//! Run with `cargo run --release --example dump_message_compression -- --help`.
//! Each comma-separated dimension expands into aggregate experiment rows.

#[path = "compression_cli.rs"]
mod cli;
mod zstd_dict;

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    env,
    error::Error,
    fs::File,
    io::{self, BufRead, BufReader, Write},
    time::{Duration, Instant},
};

use cli::{Compression, CompressionMode, Options, Setting, target_matches};
use lz4_flex::block::{
    compress as lz4_compress, compress_with_dict as lz4_compress_with_dict,
    decompress as lz4_decompress,
};
use rmpv::Value;
use tracing_table::data::dyntuple::Atom;
use zstd_dict::Zstd;

type FieldRows = HashMap<String, Vec<(u64, Atom)>>;
type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FieldGroup {
    Message,
    Other,
}

impl FieldGroup {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "message" => Ok(Self::Message),
            "other" => Ok(Self::Other),
            _ => Err(invalid("--fields must be message or other").into()),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Other => "other",
        }
    }

    fn includes(self, name: &str) -> bool {
        (name == "message") == (self == Self::Message)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DedupMode {
    Plain,
    Block,
    Archetype,
    Global,
}

impl DedupMode {
    const ALL: [Self; 4] = [Self::Plain, Self::Block, Self::Archetype, Self::Global];

    fn name(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Block => "block",
            Self::Archetype => "archetype",
            Self::Global => "global",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "plain" => Ok(Self::Plain),
            "block" => Ok(Self::Block),
            "archetype" => Ok(Self::Archetype),
            "global" => Ok(Self::Global),
            _ => Err(invalid("--dedup must contain plain, block, archetype, or global").into()),
        }
    }

    fn supports_blocks(self) -> bool {
        matches!(self, Self::Plain | Self::Block)
    }

    fn supports_whole(self) -> bool {
        self != Self::Global
    }

    fn uses_message_dictionary(self) -> bool {
        matches!(self, Self::Archetype | Self::Global)
    }
}

#[derive(Default)]
struct BlockTiming {
    total: Duration,
    ns_bytes: f64,
    bytes: usize,
}

impl BlockTiming {
    fn record(&mut self, elapsed: Duration, bytes: usize) {
        self.total += elapsed;
        self.ns_bytes += elapsed.as_nanos() as f64 * bytes as f64;
        self.bytes += bytes;
    }

    fn add(&mut self, other: &Self) {
        // Combine numerators and weights, rather than averaging archetype means.
        self.total += other.total;
        self.ns_bytes += other.ns_bytes;
        self.bytes += other.bytes;
    }

    fn byte_weighted_us(&self) -> f64 {
        if self.bytes == 0 {
            return 0.0;
        }
        // Average block-size-normalized times: sum(time * bytes / mean_bytes)
        // / N simplifies to sum(time * bytes) / sum(bytes). Equal-size blocks
        // therefore produce the ordinary mean time per block.
        self.ns_bytes / self.bytes as f64 / 1000.0
    }
}

#[derive(Default)]
struct TimingPair {
    compress: BlockTiming,
    decompress: BlockTiming,
}

impl TimingPair {
    fn add(&mut self, other: &Self) {
        self.compress.add(&other.compress);
        self.decompress.add(&other.decompress);
    }
}

// Dictionary units are blocks for plain/block dedup, and individual messages
// for archetype/global dedup. Raw fallback applies only to the message case.
fn measure_dictionary_units(
    units: &[&[u8]],
    compression: Compression,
    zstd: &mut Zstd,
    dictionary: &[u8],
    level: i32,
    prefer_raw: bool,
    decode: bool,
) -> Result<(usize, usize, TimingPair)> {
    let mut payload = 0;
    let mut raw_units = 0;
    let mut timing = TimingPair::default();
    for &original in units {
        let start = Instant::now();
        let encoded = match compression {
            Compression::Lz4 => lz4_compress_with_dict(original, dictionary),
            Compression::Zstd => zstd.compress_bytes(original, Some(dictionary), level)?,
            Compression::None => unreachable!(),
        };
        timing.compress.record(start.elapsed(), original.len());
        if prefer_raw && encoded.len() >= original.len() {
            payload += original.len();
            raw_units += 1;
            // A raw message needs no decoder call. Include its zero codec
            // time in the byte-weighted denominator for the full message set.
            if decode {
                timing.decompress.record(Duration::ZERO, original.len());
            }
        } else {
            payload += encoded.len();
            if decode {
                let start = Instant::now();
                let restored = match compression {
                    Compression::Lz4 => {
                        lz4_flex::block::decompress_with_dict(&encoded, original.len(), dictionary)?
                    }
                    Compression::Zstd => {
                        zstd.decompress_with_dict(&encoded, original.len(), dictionary)?
                    }
                    Compression::None => unreachable!(),
                };
                timing.decompress.record(start.elapsed(), original.len());
                if restored != original {
                    return Err(invalid("dictionary unit roundtrip failed").into());
                }
            }
        }
    }
    Ok((payload, raw_units, timing))
}

fn time_compression(
    blocks: &[Vec<u8>],
    mut encode: impl FnMut(&[u8]) -> Result<Vec<u8>>,
) -> Result<(Vec<Vec<u8>>, BlockTiming)> {
    let mut compressed = Vec::with_capacity(blocks.len());
    let mut timing = BlockTiming::default();
    for block in blocks {
        // As with decoding, time the codec's allocation and work per block;
        // weighting and retaining its result happen outside the timer.
        let start = Instant::now();
        let encoded = encode(block)?;
        let elapsed = start.elapsed();
        timing.record(elapsed, block.len());
        compressed.push(encoded);
    }
    Ok((compressed, timing))
}

fn time_decompression(
    blocks: &[Vec<u8>],
    compressed: &[Vec<u8>],
    mut decode: impl FnMut(&[u8], usize) -> Result<Vec<u8>>,
) -> Result<BlockTiming> {
    let mut timing = BlockTiming::default();
    for (original, compressed) in blocks.iter().zip(compressed) {
        // Include output allocation in the codec call; weighting, validation,
        // and dropping the restored block all happen outside its timer.
        let start = Instant::now();
        let restored = decode(compressed, original.len())?;
        let elapsed = start.elapsed();
        timing.record(elapsed, original.len());
        if restored != *original {
            return Err(invalid("block decompression roundtrip failed").into());
        }
    }
    Ok(timing)
}

#[derive(Default)]
struct ModeStats {
    block_applicable: bool,
    dict_attempted: bool,
    message_dictionary: bool,
    raw_bytes: usize,
    block_count: usize,
    whole_units: usize,
    dict_units: usize,
    lz4_raw_units: usize,
    zstd_raw_units: usize,
    lz4_whole_timing: TimingPair,
    zstd_whole_timing: TimingPair,
    lz4_dict_timing: TimingPair,
    zstd_dict_timing: TimingPair,
    lz4_compress: BlockTiming,
    lz4_decompress: BlockTiming,
    zstd_compress: BlockTiming,
    zstd_decompress: BlockTiming,
    lz4_block_bytes: usize,
    lz4_whole_bytes: usize,
    zstd_block_bytes: usize,
    zstd_whole_bytes: usize,
    lz4_dict_payload_bytes: Option<usize>,
    dict_payload_bytes: Option<usize>,
    dict_bytes: usize,
    dict_error: Option<String>,
}

#[derive(Default)]
struct ModeTotals {
    raw_bytes: usize,
    block_count: usize,
    whole_units: usize,
    dict_units: usize,
    lz4_raw_units: usize,
    zstd_raw_units: usize,
    lz4_whole_timing: TimingPair,
    zstd_whole_timing: TimingPair,
    lz4_dict_timing: TimingPair,
    zstd_dict_timing: TimingPair,
    lz4_compress: BlockTiming,
    lz4_decompress: BlockTiming,
    zstd_compress: BlockTiming,
    zstd_decompress: BlockTiming,
    lz4_block_bytes: usize,
    lz4_whole_bytes: usize,
    zstd_block_bytes: usize,
    zstd_whole_bytes: usize,
    lz4_dict_payload_bytes: usize,
    dict_payload_bytes: usize,
    dict_bytes: usize,
    trained_archetypes: usize,
    empty_archetypes: usize,
    untrained_archetypes: usize,
    trained_raw_bytes: usize,
    trained_lz4_block_bytes: usize,
    trained_zstd_block_bytes: usize,
    lz4_dict_effective_bytes: usize,
    zstd_dict_effective_bytes: usize,
}

impl ModeTotals {
    fn add(&mut self, stats: &ModeStats) {
        self.raw_bytes += stats.raw_bytes;
        self.block_count += stats.block_count;
        self.whole_units += stats.whole_units;
        self.dict_units += stats.dict_units;
        self.lz4_raw_units += stats.lz4_raw_units;
        self.zstd_raw_units += stats.zstd_raw_units;
        self.lz4_whole_timing.add(&stats.lz4_whole_timing);
        self.zstd_whole_timing.add(&stats.zstd_whole_timing);
        self.lz4_dict_timing.add(&stats.lz4_dict_timing);
        self.zstd_dict_timing.add(&stats.zstd_dict_timing);
        self.lz4_compress.add(&stats.lz4_compress);
        self.lz4_decompress.add(&stats.lz4_decompress);
        self.zstd_compress.add(&stats.zstd_compress);
        self.zstd_decompress.add(&stats.zstd_decompress);
        self.lz4_block_bytes += stats.lz4_block_bytes;
        self.lz4_whole_bytes += stats.lz4_whole_bytes;
        self.zstd_block_bytes += stats.zstd_block_bytes;
        self.zstd_whole_bytes += stats.zstd_whole_bytes;
        if !stats.dict_attempted {
            return;
        }
        // Failed training preserves ordinary compressed blocks or raw messages,
        // so every dictionary result still covers the complete selection.
        self.lz4_dict_effective_bytes += stats
            .lz4_dict_payload_bytes
            .map(|payload| payload + stats.dict_bytes)
            .unwrap_or(if stats.message_dictionary {
                stats.raw_bytes
            } else {
                stats.lz4_block_bytes
            });
        self.zstd_dict_effective_bytes += stats
            .dict_payload_bytes
            .map(|payload| payload + stats.dict_bytes)
            .unwrap_or(if stats.message_dictionary {
                stats.raw_bytes
            } else {
                stats.zstd_block_bytes
            });
        if let Some(payload) = stats.dict_payload_bytes {
            self.trained_archetypes += 1;
            self.trained_raw_bytes += stats.raw_bytes;
            self.trained_lz4_block_bytes += if stats.message_dictionary {
                stats.raw_bytes
            } else {
                stats.lz4_block_bytes
            };
            self.trained_zstd_block_bytes += if stats.message_dictionary {
                stats.raw_bytes
            } else {
                stats.zstd_block_bytes
            };
            self.lz4_dict_payload_bytes += stats.lz4_dict_payload_bytes.unwrap_or(0);
            self.dict_payload_bytes += payload;
            self.dict_bytes += stats.dict_bytes;
        } else if stats.raw_bytes == 0 {
            self.empty_archetypes += 1;
        } else {
            self.untrained_archetypes += 1;
        }
    }
}

#[derive(Default)]
struct SummaryTotals {
    archetypes: usize,
    rows: usize,
    strings: usize,
    raw_bytes: usize,
    unique_strings: usize,
    duplicate_bytes: usize,
    field_names: BTreeSet<String>,
    cross_archetype_uses: HashMap<Atom, usize>,
}

struct SelectionSummary {
    group: FieldGroup,
    target: String,
    totals: SummaryTotals,
    global_pools: Vec<GlobalPool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum StringPart {
    Prefix,
    Suffix,
}

struct GlobalPool {
    prefix: bool,
    block_rows: u64,
    // Keep prefix and suffix interning separate even if their bytes match.
    seen: HashSet<(StringPart, Atom)>,
    strings: Vec<(StringPart, Atom)>,
}

impl GlobalPool {
    fn matches(&self, setting: &Setting) -> bool {
        self.prefix == setting.prefix && (!self.prefix || self.block_rows == setting.block_rows)
    }
}

struct Experiment {
    setting: Setting,
    totals: ModeTotals,
}

struct Results {
    summaries: Vec<SelectionSummary>,
    experiments: Vec<Experiment>,
}

impl Results {
    fn new(options: &Options) -> Self {
        Self {
            summaries: options
                .groups
                .iter()
                .flat_map(|group| {
                    options.targets.iter().map(move |target| {
                        let mut global_pools = Vec::<GlobalPool>::new();
                        // Codec and dictionary settings share the same retained
                        // corpus. Prefix mode needs a distinct corpus per block size.
                        for setting in options.settings.iter().filter(|s| {
                            s.mode == DedupMode::Global && s.group == *group && s.target == *target
                        }) {
                            if !global_pools.iter().any(|pool| pool.matches(setting)) {
                                global_pools.push(GlobalPool {
                                    prefix: setting.prefix,
                                    block_rows: setting.block_rows,
                                    seen: HashSet::new(),
                                    strings: Vec::new(),
                                });
                            }
                        }
                        SelectionSummary {
                            group: *group,
                            target: target.clone(),
                            totals: SummaryTotals::default(),
                            global_pools,
                        }
                    })
                })
                .collect(),
            experiments: options
                .settings
                .iter()
                .cloned()
                .map(|setting| Experiment {
                    setting,
                    totals: ModeTotals::default(),
                })
                .collect(),
        }
    }

    fn rows_for(&self, setting: &Setting) -> usize {
        // Use original matched rows for every mode, including global dedup.
        // Neither missing string fields nor deduplication change this weight.
        self.summaries
            .iter()
            .find(|s| s.group == setting.group && s.target == setting.target)
            .expect("each experiment has a field/target selection")
            .totals
            .rows
    }
}

struct ActiveArchetype {
    info: Value,
    fields: FieldRows,
    rows: usize,
    last_index: u64,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn number(record: &Value, key: &str) -> Result<u64> {
    record[key]
        .as_u64()
        .ok_or_else(|| invalid(format!("missing or invalid {key}")))
        .map_err(Into::into)
}

fn text<'a>(record: &'a Value, key: &str) -> Result<&'a str> {
    record[key]
        .as_str()
        .ok_or_else(|| invalid(format!("missing or invalid {key}")))
        .map_err(Into::into)
}

fn is_string_kind(kind: &Value) -> bool {
    matches!(
        kind.as_str(),
        Some("Str" | "PrefixStr" | "StrIndex" | "PrefixStrIndex")
    )
}

// The callsite provides field names and the archetype provides their types.
// Discard the unselected group as rows arrive to keep the active map smaller.
fn add_row(
    active: &mut ActiveArchetype,
    callsite: &Value,
    row: &Value,
    groups: &[FieldGroup],
) -> Result<()> {
    if number(row, "callsite")? != number(callsite, "id")?
        || number(row, "archetype")? != number(&active.info, "id")?
    {
        return Err(invalid("row does not belong to the current archetype").into());
    }
    let index = number(row, "index")?;
    if index == 0 || index <= active.last_index {
        return Err(invalid("row indices must be positive and increasing").into());
    }
    let names = callsite["fields"]
        .as_array()
        .ok_or_else(|| invalid("callsite fields must be an array"))?;
    let kinds = active.info["fields"]
        .as_array()
        .ok_or_else(|| invalid("archetype fields must be an array"))?;
    let values = row["fields"]
        .as_array()
        .ok_or_else(|| invalid("row fields must be an array"))?;
    if names.len() != kinds.len() || kinds.len() != values.len() {
        return Err(invalid("field counts differ between callsite, archetype, and row").into());
    }
    for ((name, kind), value) in names.iter().zip(kinds).zip(values) {
        let name = name.as_str().ok_or_else(|| invalid("invalid field name"))?;
        if groups.iter().any(|group| group.includes(name)) && is_string_kind(kind) {
            let value = value
                .as_str()
                .ok_or_else(|| invalid(format!("string field {name} is not text")))?;
            active
                .fields
                .entry(name.to_owned())
                .or_default()
                .push((index, Atom::from(value)));
        }
    }
    active.rows += 1;
    active.last_index = index;
    Ok(())
}

// For other fields, row order comes first and callsite field order breaks ties.
fn selected_values<'a>(
    callsite: &Value,
    archetype: &Value,
    fields: &'a FieldRows,
    group: FieldGroup,
) -> Result<(Vec<(u64, &'a str)>, Vec<String>)> {
    let names = callsite["fields"]
        .as_array()
        .ok_or_else(|| invalid("callsite fields must be an array"))?;
    let kinds = archetype["fields"]
        .as_array()
        .ok_or_else(|| invalid("archetype fields must be an array"))?;
    if names.len() != kinds.len() {
        return Err(invalid("field counts differ between callsite and archetype").into());
    }
    let mut values = Vec::new();
    let mut selected_names = Vec::new();
    for (position, (name, kind)) in names.iter().zip(kinds).enumerate() {
        let name = name.as_str().ok_or_else(|| invalid("invalid field name"))?;
        if group.includes(name) && is_string_kind(kind) {
            selected_names.push(name.to_owned());
            if let Some(rows) = fields.get(name) {
                values.extend(
                    rows.iter()
                        .map(|(index, value)| (*index, position, value.as_ref())),
                );
            }
        }
    }
    values.sort_unstable_by_key(|(index, position, _)| (*index, *position));
    Ok((
        values
            .into_iter()
            .map(|(index, _, value)| (index, value))
            .collect(),
        selected_names,
    ))
}

struct ModeData<'a> {
    blocks: Vec<Vec<u8>>,
    samples: Vec<&'a [u8]>,
}

// Inspect every message in each original row block before deduplication. A
// single-message block uses the whole message as its prefix and an empty suffix.
fn string_parts<'a>(
    values: &[(u64, &'a str)],
    block_rows: u64,
    prefix: bool,
) -> Vec<(u64, StringPart, &'a str)> {
    if !prefix {
        return values
            .iter()
            .map(|&(row, value)| (row, StringPart::Suffix, value))
            .collect();
    }
    let mut parts = Vec::new();
    let mut start = 0;
    while start < values.len() {
        let block = (values[start].0 - 1) / block_rows;
        let mut end = start + 1;
        while end < values.len() && (values[end].0 - 1) / block_rows == block {
            end += 1;
        }
        let first = values[start].1;
        let mut prefix_len = first.len();
        for &(_, value) in &values[start + 1..end] {
            prefix_len = first.as_bytes()[..prefix_len]
                .iter()
                .zip(value.as_bytes())
                .take_while(|(a, b)| a == b)
                .count();
            // Byte equality can end inside a UTF-8 code point. Keep both
            // prefix and suffix valid strings for interning and reconstruction.
            while !first.is_char_boundary(prefix_len) {
                prefix_len -= 1;
            }
            if prefix_len == 0 {
                break;
            }
        }
        if prefix_len > 0 {
            parts.push((values[start].0, StringPart::Prefix, &first[..prefix_len]));
        }
        for &(row, value) in &values[start..end] {
            parts.push((row, StringPart::Suffix, &value[prefix_len..]));
        }
        start = end;
    }
    parts
}

// Deduplication decides both which strings enter each block and which strings
// enter grouped Zstd training samples. Training sees all selected strings
// from the archetype before either codec's dictionary pass begins.
fn build_mode<'a>(
    values: &[(u64, &'a str)],
    block_rows: u64,
    mode: DedupMode,
    prefix: bool,
) -> ModeData<'a> {
    let mut blocks = Vec::new();
    let mut samples = Vec::new();
    let mut current_block = None;
    let mut bytes = Vec::new();
    let mut seen = HashSet::new();
    for (index, kind, value) in string_parts(values, block_rows, prefix) {
        let block = (index - 1) / block_rows;
        if current_block.is_some_and(|previous| previous != block) {
            if !bytes.is_empty() {
                blocks.push(std::mem::take(&mut bytes));
            }
            if mode == DedupMode::Block {
                seen.clear();
            }
        }
        current_block = Some(block);
        if mode == DedupMode::Plain || seen.insert((kind, value)) {
            bytes.extend_from_slice(value.as_bytes());
            samples.push(value.as_bytes());
        }
    }
    if !bytes.is_empty() {
        blocks.push(bytes);
    }
    ModeData { blocks, samples }
}

// Group selected messages in their original order, reaching the byte target
// before flushing. Messages larger than the target stay intact in one sample.
fn training_samples(strings: &[&[u8]], target_bytes: usize) -> Vec<Vec<u8>> {
    let mut samples = Vec::new();
    let mut sample = Vec::new();
    for string in strings {
        sample.extend_from_slice(string);
        if sample.len() >= target_bytes {
            samples.push(std::mem::take(&mut sample));
        }
    }
    if !sample.is_empty() {
        samples.push(sample);
    }
    samples
}

fn compare(
    data: &ModeData<'_>,
    zstd: &mut Option<Zstd>,
    setting: &Setting,
    options: &Options,
) -> Result<ModeStats> {
    let blocks = &data.blocks;
    let mut stats = ModeStats::default();
    stats.block_applicable = setting.mode.supports_blocks();
    stats.message_dictionary = setting.mode.uses_message_dictionary();
    // Global input has individual message samples only, without making a
    // second concatenated copy of its entire distinct-string corpus.
    stats.raw_bytes = data.samples.iter().map(|s| s.len()).sum();
    if stats.block_applicable {
        stats.block_count = blocks.len();
    }
    let compressed_mode = options
        .compression_modes
        .iter()
        .any(|m| *m != CompressionMode::None);
    let lz4 = options.compression.contains(&Compression::Lz4) && compressed_mode;
    let use_zstd = options.compression.contains(&Compression::Zstd) && compressed_mode;
    let want_dict = options.compression_modes.contains(&CompressionMode::Dict) && (lz4 || use_zstd);
    stats.dict_attempted = want_dict;
    if want_dict {
        stats.dict_units = if stats.message_dictionary {
            data.samples.len()
        } else {
            blocks.len()
        };
        // If training is unavailable, individual-message storage falls back
        // to raw strings; blocks retain their ordinary codec fallback.
        if stats.message_dictionary {
            stats.lz4_raw_units = stats.dict_units;
            stats.zstd_raw_units = stats.dict_units;
        }
    }
    if stats.raw_bytes == 0 {
        stats.dict_error = Some("no text".to_owned());
        return Ok(stats);
    }
    if !lz4 && !use_zstd {
        return Ok(stats);
    }
    if setting.mode.supports_whole() && options.compression_modes.contains(&CompressionMode::Whole)
    {
        // Reference point only: whole-archetype compression is not viable for
        // the implementation. Keep it here to measure the size tradeoff of
        // the independently decodable blocks/messages we can actually store.
        let whole = blocks.concat();
        let units = std::slice::from_ref(&whole);
        stats.whole_units = 1;
        if lz4 {
            let (encoded, timing) = time_compression(units, |s| Ok(lz4_compress(s)))?;
            stats.lz4_whole_bytes = encoded[0].len();
            stats.lz4_whole_timing.compress = timing;
            if options.tables.timing {
                stats.lz4_whole_timing.decompress =
                    time_decompression(units, &encoded, |s, len| Ok(lz4_decompress(s, len)?))?;
            }
        }
        if use_zstd {
            let zstd = zstd.as_mut().expect("Zstd selected");
            let (encoded, timing) = time_compression(units, |s| {
                Ok(zstd.compress_bytes(s, None, setting.zstd_level)?)
            })?;
            stats.zstd_whole_bytes = encoded[0].len();
            stats.zstd_whole_timing.compress = timing;
            if options.tables.timing {
                stats.zstd_whole_timing.decompress =
                    time_decompression(units, &encoded, |s, len| {
                        Ok(zstd.decompress_bytes(s, len)?)
                    })?;
            }
        }
    }
    let want_blocks = options.compression_modes.contains(&CompressionMode::Blocks);
    if stats.block_applicable && (want_blocks || want_dict) {
        if lz4 {
            let (encoded, timing) = time_compression(blocks, |s| Ok(lz4_compress(s)))?;
            stats.lz4_block_bytes = encoded.iter().map(Vec::len).sum();
            stats.lz4_compress = timing;
            if options.tables.timing {
                stats.lz4_decompress =
                    time_decompression(blocks, &encoded, |s, len| Ok(lz4_decompress(s, len)?))?;
            }
        }
        if use_zstd {
            let zstd = zstd.as_mut().expect("Zstd selected");
            let (encoded, timing) = time_compression(blocks, |s| {
                Ok(zstd.compress_bytes(s, None, setting.zstd_level)?)
            })?;
            stats.zstd_block_bytes = encoded.iter().map(Vec::len).sum();
            stats.zstd_compress = timing;
            if options.tables.timing {
                stats.zstd_decompress = time_decompression(blocks, &encoded, |s, len| {
                    Ok(zstd.decompress_bytes(s, len)?)
                })?;
            }
        }
    }
    if want_dict {
        // Message dictionaries use each already-deduplicated message as one
        // training sample, or each retained prefix/suffix part after splitting.
        // Prefix + dictionary compression is discouraged but supported here.
        // Ordinary block dictionaries retain grouped samples.
        let grouped;
        let samples: Vec<&[u8]> = if stats.message_dictionary {
            data.samples.clone()
        } else {
            grouped = training_samples(&data.samples, setting.sample_bytes);
            grouped.iter().map(Vec::as_slice).collect()
        };
        let units: Vec<&[u8]> = if stats.message_dictionary {
            data.samples.clone()
        } else {
            blocks.iter().map(Vec::as_slice).collect()
        };
        let zstd = zstd.as_mut().expect("Zstd loaded for dictionary training");
        match zstd.train(&samples, setting.dict_max_bytes) {
            Ok(dictionary) => {
                let mut payload = 0;
                let mut lz4_payload = 0;
                if lz4 {
                    let (bytes, raw_units, timing) = measure_dictionary_units(
                        &units,
                        Compression::Lz4,
                        zstd,
                        &dictionary,
                        setting.zstd_level,
                        stats.message_dictionary,
                        options.tables.timing,
                    )?;
                    lz4_payload = bytes;
                    stats.lz4_raw_units = raw_units;
                    stats.lz4_dict_timing = timing;
                }
                if use_zstd {
                    let (bytes, raw_units, timing) = measure_dictionary_units(
                        &units,
                        Compression::Zstd,
                        zstd,
                        &dictionary,
                        setting.zstd_level,
                        stats.message_dictionary,
                        options.tables.timing,
                    )?;
                    payload = bytes;
                    stats.zstd_raw_units = raw_units;
                    stats.zstd_dict_timing = timing;
                }
                stats.dict_payload_bytes = Some(payload);
                stats.lz4_dict_payload_bytes = Some(lz4_payload);
                stats.dict_bytes = dictionary.len();
            }
            Err(reason) => {
                stats.dict_error = Some(reason);
                // Untrained block dictionaries use ordinary blocks and their
                // timings; untrained message dictionaries require no codec.
                if stats.block_applicable {
                    stats.lz4_dict_timing.compress.add(&stats.lz4_compress);
                    stats.lz4_dict_timing.decompress.add(&stats.lz4_decompress);
                    stats.zstd_dict_timing.compress.add(&stats.zstd_compress);
                    stats
                        .zstd_dict_timing
                        .decompress
                        .add(&stats.zstd_decompress);
                } else if options.tables.timing {
                    stats
                        .lz4_dict_timing
                        .compress
                        .record(Duration::ZERO, stats.raw_bytes);
                    stats
                        .lz4_dict_timing
                        .decompress
                        .record(Duration::ZERO, stats.raw_bytes);
                    stats
                        .zstd_dict_timing
                        .compress
                        .record(Duration::ZERO, stats.raw_bytes);
                    stats
                        .zstd_dict_timing
                        .decompress
                        .record(Duration::ZERO, stats.raw_bytes);
                }
            }
        }
    }
    Ok(stats)
}

fn process_archetype(
    global: &Value,
    callsite: &Value,
    archetype: &Value,
    fields: FieldRows,
    row_count: usize,
    options: &Options,
    zstd: &mut Option<Zstd>,
    results: &mut Results,
) -> Result<()> {
    // Preserve the callback's full dump context for later format-specific experiments.
    number(global, "version")?;
    let target = text(callsite, "target")?;
    for summary in &mut results.summaries {
        if !target_matches(&summary.target, target) {
            continue;
        }
        // Size-only reports also need the original row count. Count all
        // matching rows even when this field group has no strings in them.
        summary.totals.rows += row_count;
        let (values, names) = selected_values(callsite, archetype, &fields, summary.group)?;
        if options.tables.timing || options.tables.sizes {
            for pool in &mut summary.global_pools {
                for (_, kind, value) in string_parts(&values, pool.block_rows, pool.prefix) {
                    let atom = Atom::from(value);
                    if pool.seen.insert((kind, atom.clone())) {
                        pool.strings.push((kind, atom));
                    }
                }
            }
        }
        // Retain cross-archetype strings only when their summary is requested.
        // Timing/size-only runs without global dedup release all strings here.
        if options.tables.summary {
            let totals = &mut summary.totals;
            let mut unique = HashSet::new();
            for &(_, value) in &values {
                totals.raw_bytes += value.len();
                if !unique.insert(Atom::from(value)) {
                    totals.duplicate_bytes += value.len();
                }
            }
            totals.archetypes += 1;
            totals.strings += values.len();
            totals.unique_strings += unique.len();
            totals.field_names.extend(names);
            for value in unique {
                *totals.cross_archetype_uses.entry(value).or_default() += 1;
            }
        }
        // Each selection shares its summary across experiments.
        // Process every permutation while this archetype is in memory.
        for experiment in &mut results.experiments {
            let setting = &experiment.setting;
            if setting.mode == DedupMode::Global
                || setting.group != summary.group
                || setting.target != summary.target
            {
                continue;
            }
            if options.tables.timing || options.tables.sizes {
                let data = build_mode(&values, setting.block_rows, setting.mode, setting.prefix);
                let stats = compare(&data, zstd, setting, options)?;
                experiment.totals.add(&stats);
            }
        }
    }
    Ok(())
}

fn finish_global(options: &Options, zstd: &mut Option<Zstd>, results: &mut Results) -> Result<()> {
    if !options.tables.timing && !options.tables.sizes {
        return Ok(());
    }
    for summary in &results.summaries {
        for pool in &summary.global_pools {
            let data = ModeData {
                blocks: Vec::new(),
                samples: pool.strings.iter().map(|(_, s)| s.as_bytes()).collect(),
            };
            // The complete distinct corpus is available before training starts.
            // Each global experiment has one dictionary pool, across archetypes.
            for experiment in &mut results.experiments {
                if experiment.setting.mode == DedupMode::Global
                    && experiment.setting.group == summary.group
                    && experiment.setting.target == summary.target
                    && pool.matches(&experiment.setting)
                {
                    let stats = compare(&data, zstd, &experiment.setting, options)?;
                    experiment.totals.add(&stats);
                }
            }
        }
    }
    Ok(())
}

fn finish_active(
    global: &Value,
    callsite: &Value,
    active: &mut Option<ActiveArchetype>,
    options: &Options,
    zstd: &mut Option<Zstd>,
    results: &mut Results,
) -> Result<()> {
    if let Some(active) = active.take() {
        process_archetype(
            global,
            callsite,
            &active.info,
            active.fields,
            active.rows,
            options,
            zstd,
            results,
        )?;
    }
    Ok(())
}

// MessagePack records are self-delimiting. Only one record and one archetype's
// selected strings are retained, plus distinct strings for cross-arch stats
// and requested global pools (separate prefix-on corpora for each block size).
fn read_dump(
    mut input: impl BufRead,
    options: &Options,
    zstd: &mut Option<Zstd>,
) -> Result<Results> {
    let mut global = None;
    let mut callsite = None;
    let mut selected_callsite = false;
    let mut active = None;
    let mut results = Results::new(options);
    while !input.fill_buf()?.is_empty() {
        let record = rmpv::decode::read_value(&mut input)?;
        match text(&record, "type")? {
            "format" => {
                if global.replace(record).is_some() || callsite.is_some() {
                    return Err(invalid("format record must occur once, first").into());
                }
            }
            "callsite" => {
                let global_info = global.as_ref().ok_or_else(|| invalid("missing format"))?;
                if let Some(previous) = callsite.as_ref() {
                    finish_active(
                        global_info,
                        previous,
                        &mut active,
                        options,
                        zstd,
                        &mut results,
                    )?;
                }
                // Match once per callsite instead of allocating glob state
                // for every row in a potentially large archetype.
                let target = text(&record, "target")?;
                selected_callsite = options.targets.iter().any(|p| target_matches(p, target));
                callsite = Some(record);
            }
            "archetype" => {
                let global_info = global.as_ref().ok_or_else(|| invalid("missing format"))?;
                let callsite_info = callsite
                    .as_ref()
                    .ok_or_else(|| invalid("missing callsite"))?;
                finish_active(
                    global_info,
                    callsite_info,
                    &mut active,
                    options,
                    zstd,
                    &mut results,
                )?;
                if number(&record, "callsite")? != number(callsite_info, "id")? {
                    return Err(invalid("archetype has the wrong callsite").into());
                }
                active = Some(ActiveArchetype {
                    info: record,
                    fields: FieldRows::new(),
                    rows: 0,
                    last_index: 0,
                });
            }
            "row" => {
                let callsite_info = callsite
                    .as_ref()
                    .ok_or_else(|| invalid("missing callsite"))?;
                let current = active
                    .as_mut()
                    .ok_or_else(|| invalid("missing archetype"))?;
                // Validate excluded rows but do not retain their strings.
                let groups = if selected_callsite {
                    options.groups.as_slice()
                } else {
                    &[]
                };
                add_row(current, callsite_info, &record, groups)?;
            }
            other => return Err(invalid(format!("unknown record type: {other}")).into()),
        }
    }
    let global_info = global.as_ref().ok_or_else(|| invalid("empty dump"))?;
    if let Some(callsite_info) = callsite.as_ref() {
        finish_active(
            global_info,
            callsite_info,
            &mut active,
            options,
            zstd,
            &mut results,
        )?;
    }
    finish_global(options, zstd, &mut results)?;
    Ok(results)
}

fn list_targets(mut input: impl BufRead, options: &Options, output: &mut impl Write) -> Result<()> {
    let mut targets = BTreeSet::new();
    // Listing needs no string interning, archetype buffering, or codec library.
    while !input.fill_buf()?.is_empty() {
        let record = rmpv::decode::read_value(&mut input)?;
        if text(&record, "type")? == "callsite" {
            let target = text(&record, "target")?;
            if options.targets.iter().any(|p| target_matches(p, target)) {
                targets.insert(target.to_owned());
            }
        }
    }
    for target in targets {
        writeln!(output, "{target}")?;
    }
    Ok(())
}

// Keep one message per physical line without losing tabs/newlines in its text.
// Consumers must decode these four backslash escapes before analyzing prefixes.
fn write_dump_field(output: &mut impl Write, message: &str) -> Result<()> {
    let mut start = 0;
    for (offset, character) in message.char_indices() {
        let escaped = match character {
            '\\' => b"\\\\".as_slice(),
            '\t' => b"\\t".as_slice(),
            '\n' => b"\\n".as_slice(),
            '\r' => b"\\r".as_slice(),
            _ => continue,
        };
        output.write_all(&message.as_bytes()[start..offset])?;
        output.write_all(escaped)?;
        start = offset + character.len_utf8();
    }
    output.write_all(&message.as_bytes()[start..])?;
    Ok(())
}

fn dump_strings(
    mut input: impl BufRead,
    options: &Options,
    output: &mut impl Write,
) -> Result<usize> {
    let mut format_seen = false;
    let mut callsite = None;
    let mut active = None;
    let mut selected_callsite = false;
    let mut message_column = None;
    let mut count = 0;
    // Only retain current metadata and the current record. In particular, do
    // not intern strings or buffer an archetype's rows for this export mode.
    while !input.fill_buf()?.is_empty() {
        let record = rmpv::decode::read_value(&mut input)?;
        match text(&record, "type")? {
            "format" => {
                if format_seen || callsite.is_some() {
                    return Err(invalid("format record must occur once, first").into());
                }
                number(&record, "version")?;
                format_seen = true;
            }
            "callsite" => {
                if !format_seen {
                    return Err(invalid("missing format").into());
                }
                let target = text(&record, "target")?;
                selected_callsite = options.targets.iter().any(|p| target_matches(p, target));
                callsite = Some(record);
                active = None;
                message_column = None;
            }
            "archetype" => {
                let callsite_info = callsite
                    .as_ref()
                    .ok_or_else(|| invalid("missing callsite"))?;
                if number(&record, "callsite")? != number(callsite_info, "id")? {
                    return Err(invalid("archetype has the wrong callsite").into());
                }
                let names = callsite_info["fields"]
                    .as_array()
                    .ok_or_else(|| invalid("callsite fields must be an array"))?;
                let kinds = record["fields"]
                    .as_array()
                    .ok_or_else(|| invalid("archetype fields must be an array"))?;
                if names.len() != kinds.len() {
                    return Err(
                        invalid("field counts differ between callsite and archetype").into(),
                    );
                }
                message_column = names.iter().zip(kinds).position(|(name, kind)| {
                    name.as_str() == Some("message") && is_string_kind(kind)
                });
                active = Some(ActiveArchetype {
                    info: record,
                    fields: FieldRows::new(),
                    rows: 0,
                    last_index: 0,
                });
            }
            "row" => {
                let callsite_info = callsite
                    .as_ref()
                    .ok_or_else(|| invalid("missing callsite"))?;
                let current = active
                    .as_mut()
                    .ok_or_else(|| invalid("missing archetype"))?;
                // Reuse row ownership/schema/order checks with no retained fields.
                add_row(current, callsite_info, &record, &[])?;
                if selected_callsite && let Some(position) = message_column {
                    let message = record["fields"][position]
                        .as_str()
                        .ok_or_else(|| invalid("string message field is not text"))?;
                    // Archetype IDs are local to a callsite in the dump format.
                    // Their numeric pair is a stable dump-wide unique identifier.
                    write!(
                        output,
                        "{}:{}\t",
                        number(callsite_info, "id")?,
                        number(&current.info, "id")?
                    )?;
                    write_dump_field(output, text(callsite_info, "target")?)?;
                    write!(output, "\t")?;
                    write_dump_field(output, message)?;
                    writeln!(output)?;
                    count += 1;
                }
            }
            other => return Err(invalid(format!("unknown record type: {other}")).into()),
        }
    }
    if !format_seen {
        return Err(invalid("empty dump").into());
    }
    Ok(count)
}

// CSV quoting handles user-supplied target globs and aggregated field names.
fn csv_row(output: &mut impl Write, cells: &[String]) -> Result<()> {
    for (index, cell) in cells.iter().enumerate() {
        if index != 0 {
            write!(output, ",")?;
        }
        if cell.contains([',', '"', '\n', '\r']) {
            write!(output, "\"{}\"", cell.replace('"', "\"\""))?;
        } else {
            write!(output, "{cell}")?;
        }
    }
    writeln!(output)?;
    Ok(())
}

fn setting_cells(setting: &Setting) -> Vec<String> {
    vec![
        setting.group.name().to_owned(),
        setting.target.clone(),
        setting.mode.name().to_owned(),
        if setting.mode.uses_message_dictionary() && !setting.prefix {
            "n/a".to_owned()
        } else {
            setting.block_rows.to_string()
        },
        setting.zstd_level.to_string(),
        setting.dict_max_bytes.to_string(),
        if setting.mode.uses_message_dictionary() {
            "n/a".to_owned()
        } else {
            setting.sample_bytes.to_string()
        },
        if setting.prefix { "on" } else { "off" }.to_owned(),
    ]
}

fn compression_ratio(raw: usize, compressed: usize) -> String {
    if compressed == 0 {
        return "n/a".to_owned();
    }
    format!("{:.3}", raw as f64 / compressed as f64)
}

fn bytes_per_row(bytes: usize, rows: usize) -> String {
    // Keep byte totals exact internally; normalize only the report. A target
    // with no rows has no per-row measure, rather than zero storage per row.
    if rows == 0 {
        "n/a".to_owned()
    } else {
        format!("{:.6}", bytes as f64 / rows as f64)
    }
}

// No-compression rows collapse settings that cannot affect the raw payload.
// Block size remains meaningful for block dedup and for prefix splitting,
// including archetype/global pools whose parts depend on the original blocks.
fn uncompressed_cells(setting: &Setting) -> Vec<String> {
    let mut cells = setting_cells(setting);
    if setting.mode != DedupMode::Block && !setting.prefix {
        cells[3] = "n/a".to_owned();
    }
    cells[4..7].fill("n/a".to_owned());
    cells.extend(["none", "none", "none", "n/a"].map(str::to_owned));
    cells
}

fn compression_cells(
    setting: &Setting,
    compression: Compression,
    mode: CompressionMode,
) -> Vec<String> {
    let mut cells = setting_cells(setting);
    let unit = match mode {
        CompressionMode::None => "none",
        CompressionMode::Blocks => "block",
        CompressionMode::Whole => "archetype",
        CompressionMode::Dict if setting.mode.uses_message_dictionary() && setting.prefix => {
            "string_part"
        }
        CompressionMode::Dict if setting.mode.uses_message_dictionary() => "message",
        CompressionMode::Dict => "block",
    };
    let scope = if mode != CompressionMode::Dict {
        "n/a"
    } else if setting.mode == DedupMode::Global {
        "global"
    } else {
        "archetype"
    };
    cells.extend([compression.name(), mode.name(), unit, scope].map(str::to_owned));
    cells
}

fn coherent_layout(dedup: DedupMode, mode: CompressionMode) -> bool {
    match mode {
        CompressionMode::None => true,
        CompressionMode::Blocks => dedup.supports_blocks(),
        CompressionMode::Whole => dedup.supports_whole(),
        CompressionMode::Dict => true,
    }
}

fn layout_timings(
    experiment: &Experiment,
    compression: Compression,
    mode: CompressionMode,
) -> (&BlockTiming, &BlockTiming, usize) {
    let s = &experiment.totals;
    match (compression, mode) {
        (Compression::Lz4, CompressionMode::Blocks) => {
            (&s.lz4_compress, &s.lz4_decompress, s.block_count)
        }
        (Compression::Zstd, CompressionMode::Blocks) => {
            (&s.zstd_compress, &s.zstd_decompress, s.block_count)
        }
        (Compression::Lz4, CompressionMode::Whole) => (
            &s.lz4_whole_timing.compress,
            &s.lz4_whole_timing.decompress,
            s.whole_units,
        ),
        (Compression::Zstd, CompressionMode::Whole) => (
            &s.zstd_whole_timing.compress,
            &s.zstd_whole_timing.decompress,
            s.whole_units,
        ),
        (Compression::Lz4, CompressionMode::Dict) => (
            &s.lz4_dict_timing.compress,
            &s.lz4_dict_timing.decompress,
            s.dict_units,
        ),
        (Compression::Zstd, CompressionMode::Dict) => (
            &s.zstd_dict_timing.compress,
            &s.zstd_dict_timing.decompress,
            s.dict_units,
        ),
        _ => unreachable!(),
    }
}

fn print_tables(results: &Results, options: &Options, output: &mut impl Write) -> Result<()> {
    let tables = options.tables;
    const DIMENSIONS: &str =
        "fields,target,dedup,block_size,zstd_level,dict_size,sample_size,prefix";
    if tables.timing {
        writeln!(output, "# timing")?;
        writeln!(
            output,
            "{DIMENSIONS},compression,compression_mode,unit,dictionary_scope,units,compress_total_ms,decompress_total_ms,compress_unit_avg_us,decompress_unit_avg_us"
        )?;
        let mut seen = HashSet::new();
        for experiment in &results.experiments {
            for &compression in &options.compression {
                if compression == Compression::None {
                    let mut cells = uncompressed_cells(&experiment.setting);
                    if !seen.insert(cells.clone()) {
                        continue;
                    }
                    cells.extend(["n/a", "0.000", "0.000", "n/a", "n/a"].map(str::to_owned));
                    csv_row(output, &cells)?;
                    continue;
                }
                for &mode in &options.compression_modes {
                    if mode == CompressionMode::None {
                        continue;
                    }
                    let mut cells = compression_cells(&experiment.setting, compression, mode);
                    if !seen.insert(cells.clone()) {
                        continue;
                    }
                    if coherent_layout(experiment.setting.mode, mode) {
                        let (encode, decode, count) = layout_timings(experiment, compression, mode);
                        cells.extend([
                            count.to_string(),
                            format!("{:.3}", encode.total.as_secs_f64() * 1000.0),
                            format!("{:.3}", decode.total.as_secs_f64() * 1000.0),
                            format!("{:.6}", encode.byte_weighted_us()),
                            format!("{:.6}", decode.byte_weighted_us()),
                        ]);
                    } else {
                        cells.extend(std::iter::repeat_n("n/a".to_owned(), 5));
                    }
                    csv_row(output, &cells)?;
                }
            }
        }
        writeln!(output)?;
    }
    if tables.sizes {
        writeln!(output, "# sizes")?;
        writeln!(
            output,
            "{DIMENSIONS},compression,compression_mode,unit,dictionary_scope,rows,raw_bytes_per_row,payload_bytes_per_row,stored_bytes_per_row,ratio,units,dict_bytes_per_row,raw_units,compressed_units,trained_pools,untrained_pools,empty_pools"
        )?;
        let mut seen = HashSet::new();
        for experiment in &results.experiments {
            let s = &experiment.totals;
            let rows = results.rows_for(&experiment.setting);
            for &compression in &options.compression {
                if compression == Compression::None {
                    let mut cells = uncompressed_cells(&experiment.setting);
                    if !seen.insert(cells.clone()) {
                        continue;
                    }
                    cells.extend([
                        rows.to_string(),
                        bytes_per_row(s.raw_bytes, rows),
                        bytes_per_row(s.raw_bytes, rows),
                        bytes_per_row(s.raw_bytes, rows),
                        "1.000".to_owned(),
                        "n/a".to_owned(),
                        bytes_per_row(0, rows),
                        "n/a".to_owned(),
                        "n/a".to_owned(),
                        "n/a".to_owned(),
                        "n/a".to_owned(),
                        "n/a".to_owned(),
                    ]);
                    csv_row(output, &cells)?;
                    continue;
                }
                for &mode in &options.compression_modes {
                    if mode == CompressionMode::None {
                        continue;
                    }
                    let mut cells = compression_cells(&experiment.setting, compression, mode);
                    if !seen.insert(cells.clone()) {
                        continue;
                    }
                    cells.push(rows.to_string());
                    cells.push(bytes_per_row(s.raw_bytes, rows));
                    if !coherent_layout(experiment.setting.mode, mode) {
                        cells.extend(std::iter::repeat_n("n/a".to_owned(), 10));
                        csv_row(output, &cells)?;
                        continue;
                    }
                    let stored = match (compression, mode) {
                        (Compression::Lz4, CompressionMode::Blocks) => s.lz4_block_bytes,
                        (Compression::Zstd, CompressionMode::Blocks) => s.zstd_block_bytes,
                        (Compression::Lz4, CompressionMode::Whole) => s.lz4_whole_bytes,
                        (Compression::Zstd, CompressionMode::Whole) => s.zstd_whole_bytes,
                        (Compression::Lz4, CompressionMode::Dict) => s.lz4_dict_effective_bytes,
                        (Compression::Zstd, CompressionMode::Dict) => s.zstd_dict_effective_bytes,
                        _ => unreachable!(),
                    };
                    let dictionary = if mode == CompressionMode::Dict {
                        s.dict_bytes
                    } else {
                        0
                    };
                    let count = layout_timings(experiment, compression, mode).2;
                    cells.extend([
                        bytes_per_row(stored - dictionary, rows),
                        bytes_per_row(stored, rows),
                        compression_ratio(s.raw_bytes, stored),
                        count.to_string(),
                        bytes_per_row(dictionary, rows),
                    ]);
                    if mode == CompressionMode::Dict {
                        let raw_units = if compression == Compression::Lz4 {
                            s.lz4_raw_units
                        } else {
                            s.zstd_raw_units
                        };
                        cells.extend(
                            [
                                raw_units,
                                count - raw_units,
                                s.trained_archetypes,
                                s.untrained_archetypes,
                                s.empty_archetypes,
                            ]
                            .map(|v| v.to_string()),
                        );
                    } else {
                        cells.extend(std::iter::repeat_n("n/a".to_owned(), 5));
                    }
                    csv_row(output, &cells)?;
                }
            }
        }
        writeln!(output)?;
        if options.compression_modes.contains(&CompressionMode::Dict)
            && options.compression.iter().any(|c| *c != Compression::None)
        {
            writeln!(output, "# dictionary_training")?;
            writeln!(
                output,
                "{DIMENSIONS},compression,compression_mode,unit,dictionary_scope,rows,trained_raw_bytes_per_row,trained_baseline_bytes_per_row,dict_payload_bytes_per_row,dict_bytes_per_row"
            )?;
            let mut seen = HashSet::new();
            for experiment in &results.experiments {
                let s = &experiment.totals;
                let rows = results.rows_for(&experiment.setting);
                for &compression in &options.compression {
                    if compression == Compression::None {
                        continue;
                    }
                    let mut cells =
                        compression_cells(&experiment.setting, compression, CompressionMode::Dict);
                    if !seen.insert(cells.clone()) {
                        continue;
                    }
                    let (baseline, payload) = if compression == Compression::Lz4 {
                        (s.trained_lz4_block_bytes, s.lz4_dict_payload_bytes)
                    } else {
                        (s.trained_zstd_block_bytes, s.dict_payload_bytes)
                    };
                    cells.push(rows.to_string());
                    // The trained subset is still amortized across the full
                    // selection, just like dictionary cost in the size table.
                    cells.extend(
                        [s.trained_raw_bytes, baseline, payload, s.dict_bytes]
                            .map(|v| bytes_per_row(v, rows)),
                    );
                    csv_row(output, &cells)?;
                }
            }
            writeln!(output)?;
        }
    }
    if tables.summary {
        writeln!(output, "# summary")?;
        writeln!(
            output,
            "fields,target,archetypes,rows,strings,raw_bytes_per_row,unique_strings_per_archetype,unique_strings_global,duplicate_string_bytes_per_row,cross_archetype_shared_strings,cross_archetype_duplicate_bytes_per_row,string_fields"
        )?;
        for summary in &results.summaries {
            let stats = &summary.totals;
            let (shared_strings, duplicate_bytes) = stats
                .cross_archetype_uses
                .iter()
                .filter(|(_, uses)| **uses > 1)
                .fold((0, 0), |(strings, bytes), (value, uses)| {
                    (strings + 1, bytes + value.len() * (uses - 1))
                });
            let mut cells = vec![summary.group.name().to_owned(), summary.target.clone()];
            cells.extend([
                stats.archetypes.to_string(),
                stats.rows.to_string(),
                stats.strings.to_string(),
                bytes_per_row(stats.raw_bytes, stats.rows),
                stats.unique_strings.to_string(),
                stats.cross_archetype_uses.len().to_string(),
                bytes_per_row(stats.duplicate_bytes, stats.rows),
                shared_strings.to_string(),
                bytes_per_row(duplicate_bytes, stats.rows),
            ]);
            cells.push(
                stats
                    .field_names
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(";"),
            );
            csv_row(output, &cells)?;
        }
        writeln!(output)?;
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{}", cli::HELP);
        return Ok(());
    }
    let options = Options::parse(args)?;
    let input = BufReader::new(File::open(&options.path)?);
    if let Some(path) = &options.dump_strings {
        // Create a fresh export so an accidental input/output path collision
        // cannot truncate the source dump or overwrite an existing exploration.
        let file = File::options().write(true).create_new(true).open(path)?;
        let mut output = io::BufWriter::new(file);
        dump_strings(input, &options, &mut output)?;
        output.flush()?;
        return Ok(());
    }
    let mut output = io::BufWriter::new(io::stdout().lock());
    if options.list_targets {
        list_targets(input, &options, &mut output)?;
        output.flush()?;
        return Ok(());
    }
    let has_compressed_mode = options
        .compression_modes
        .iter()
        .any(|mode| *mode != CompressionMode::None);
    let use_zstd = options.compression.contains(&Compression::Zstd) && has_compressed_mode;
    let train_dict = (options.tables.sizes || options.tables.timing)
        && options.compression_modes.contains(&CompressionMode::Dict)
        && options.compression.contains(&Compression::Lz4);
    let mut zstd = if (options.tables.timing || options.tables.sizes) && (use_zstd || train_dict) {
        Some(Zstd::load()?)
    } else {
        None
    };
    let results = read_dump(input, &options, &mut zstd)?;
    print_tables(&results, &options, &mut output)?;
    output.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_mean_weights_blocks_by_uncompressed_bytes() {
        let mut first_archetype = BlockTiming::default();
        first_archetype.record(Duration::from_micros(2), 100);
        first_archetype.record(Duration::from_micros(10), 300);
        let mut second_archetype = BlockTiming::default();
        second_archetype.record(Duration::from_micros(4), 600);
        first_archetype.add(&second_archetype);
        // (2 * 100 + 10 * 300 + 4 * 600) / 1000 = 5.6 us.
        assert!((first_archetype.byte_weighted_us() - 5.6).abs() < 1e-12);
        assert_eq!(first_archetype.total, Duration::from_micros(16));
        assert_eq!(BlockTiming::default().byte_weighted_us(), 0.0);
        // Regression case: ten equally sized 2 us blocks must average 2 us.
        let mut equal_blocks = BlockTiming::default();
        for _ in 0..10 {
            equal_blocks.record(Duration::from_micros(2), 100);
        }
        assert_eq!(equal_blocks.byte_weighted_us(), 2.0);
    }

    fn record(pairs: &[(&str, Value)]) -> Value {
        Value::Map(
            pairs
                .iter()
                .map(|(key, value)| (Value::from(*key), value.clone()))
                .collect(),
        )
    }

    #[test]
    fn string_export_streams_original_messages_with_unique_ids_and_targets() {
        let records = [
            record(&[("type", "format".into()), ("version", 1.into())]),
            record(&[
                ("type", "callsite".into()),
                ("id", 7.into()),
                ("target", "alpha::core".into()),
                (
                    "fields",
                    Value::Array(vec!["message".into(), "label".into()]),
                ),
            ]),
            record(&[
                ("type", "archetype".into()),
                ("callsite", 7.into()),
                ("id", 0.into()),
                ("fields", Value::Array(vec!["Str".into(), "Str".into()])),
            ]),
            record(&[
                ("type", "row".into()),
                ("callsite", 7.into()),
                ("archetype", 0.into()),
                ("index", 1.into()),
                (
                    "fields",
                    Value::Array(vec!["same".into(), "ignored".into()]),
                ),
            ]),
            record(&[
                ("type", "row".into()),
                ("callsite", 7.into()),
                ("archetype", 0.into()),
                ("index", 2.into()),
                (
                    "fields",
                    Value::Array(vec!["same".into(), "ignored".into()]),
                ),
            ]),
            record(&[
                ("type", "archetype".into()),
                ("callsite", 7.into()),
                ("id", 1.into()),
                (
                    "fields",
                    Value::Array(vec!["PrefixStrIndex".into(), "Str".into()]),
                ),
            ]),
            record(&[
                ("type", "row".into()),
                ("callsite", 7.into()),
                ("archetype", 1.into()),
                ("index", 1.into()),
                (
                    "fields",
                    Value::Array(vec!["héllo\t\n\\n\r".into(), "ignored".into()]),
                ),
            ]),
            record(&[
                ("type", "callsite".into()),
                ("id", 8.into()),
                ("target", "beta".into()),
                (
                    "fields",
                    Value::Array(vec!["label".into(), "message".into()]),
                ),
            ]),
            record(&[
                ("type", "archetype".into()),
                ("callsite", 8.into()),
                ("id", 0.into()),
                ("fields", Value::Array(vec!["Str".into(), "Str".into()])),
            ]),
            record(&[
                ("type", "row".into()),
                ("callsite", 8.into()),
                ("archetype", 0.into()),
                ("index", 1.into()),
                ("fields", Value::Array(vec!["ignored".into(), "".into()])),
            ]),
            record(&[
                ("type", "archetype".into()),
                ("callsite", 8.into()),
                ("id", 1.into()),
                ("fields", Value::Array(vec!["Str".into(), "U64".into()])),
            ]),
            record(&[
                ("type", "row".into()),
                ("callsite", 8.into()),
                ("archetype", 1.into()),
                ("index", 1.into()),
                ("fields", Value::Array(vec!["ignored".into(), 42.into()])),
            ]),
        ];
        let mut encoded = Vec::new();
        for record in records {
            rmpv::encode::write_value(&mut encoded, &record).unwrap();
        }
        for (target, count, expected) in [
            (
                "*",
                4,
                "7:0\talpha::core\tsame\n7:0\talpha::core\tsame\n7:1\talpha::core\théllo\\t\\n\\\\n\\r\n8:0\tbeta\t\n",
            ),
            (
                "alpha*,alpha",
                3,
                "7:0\talpha::core\tsame\n7:0\talpha::core\tsame\n7:1\talpha::core\théllo\\t\\n\\\\n\\r\n",
            ),
            ("missing", 0, ""),
        ] {
            let options = Options::parse([
                "--dump_strings=unused.tsv".to_owned(),
                format!("--target={target}"),
                // Export always selects original messages; compression
                // dimensions do not restrict or transform the output.
                "--fields=other".to_owned(),
                "--prefix=on".to_owned(),
                "--dedup=plain".to_owned(),
                "dump.msgpack".to_owned(),
            ])
            .unwrap();
            assert!(options.settings.is_empty());
            let mut output = Vec::new();
            assert_eq!(
                dump_strings(
                    BufReader::with_capacity(7, encoded.as_slice()),
                    &options,
                    &mut output,
                )
                .unwrap(),
                count
            );
            assert_eq!(String::from_utf8(output).unwrap(), expected);
        }
    }

    #[test]
    fn uncompressed_baseline_collapses_irrelevant_settings_without_a_codec() {
        let options = Options::parse(
            [
                "--compression=none",
                "--compression_mode=blocks,whole,dict",
                "--block_size=2,4",
                "--zstd_level=1,3",
                "--dict_size=1024,2048",
                "--sample_size=256,512",
                "--timing",
                "--sizes",
                "dump.msgpack",
            ]
            .map(str::to_owned),
        )
        .unwrap();
        let values = [(1, "aa"), (2, "aa"), (3, "bb"), (4, "aa")];
        let mut results = Results::new(&options);
        // This test builds experiments directly instead of streaming rows.
        results.summaries[0].totals.rows = values.len();
        let mut zstd = None;
        for experiment in &mut results.experiments {
            let setting = &experiment.setting;
            let data = build_mode(&values, setting.block_rows, setting.mode, setting.prefix);
            let stats = compare(&data, &mut zstd, setting, &options).unwrap();
            experiment.totals.add(&stats);
        }
        let mut output = Vec::new();
        print_tables(&results, &options, &mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains("# dictionary_training"));
        let rows: Vec<_> = output
            .split("# sizes\n")
            .nth(1)
            .unwrap()
            .lines()
            .skip(1)
            .filter(|line| !line.is_empty())
            .map(|line| line.split(',').collect::<Vec<_>>())
            .collect();
        // Plain, archetype, and global each have one baseline; block dedup has one for
        // each block size because its retained string bytes actually differ.
        assert_eq!(rows.len(), 5);
        for (row, raw_per_row) in rows
            .iter()
            .zip(["2.000000", "1.500000", "1.000000", "1.000000", "1.000000"])
        {
            assert_eq!(&row[8..10], &["none", "none"]);
            assert_eq!(row[12], "4");
            assert_eq!(row[13], raw_per_row);
            assert_eq!(row[14], raw_per_row);
            assert_eq!(row[15], raw_per_row);
            assert_eq!(row[16], "1.000");
        }
        let timing_rows = output
            .split("# timing\n")
            .nth(1)
            .unwrap()
            .split("\n\n")
            .next()
            .unwrap()
            .lines()
            .skip(1)
            .count();
        assert_eq!(timing_rows, 5);
        assert!(zstd.is_none());
        assert!(
            Options::parse(
                [
                    "--compression=none",
                    "--compression_mode=none",
                    "dump.msgpack"
                ]
                .map(str::to_owned)
            )
            .is_ok()
        );
    }

    #[test]
    fn each_mode_deduplicates_training_strings_and_blocks() {
        let values = [(1, "AA"), (2, "AA"), (4, "BB"), (6, "AA"), (7, "AA")];
        let plain = build_mode(&values, 3, DedupMode::Plain, false);
        let block = build_mode(&values, 3, DedupMode::Block, false);
        let archetype = build_mode(&values, 3, DedupMode::Archetype, false);
        assert_eq!(
            plain.blocks,
            vec![b"AAAA".to_vec(), b"BBAA".to_vec(), b"AA".to_vec()]
        );
        assert_eq!(
            block.blocks,
            vec![b"AA".to_vec(), b"BBAA".to_vec(), b"AA".to_vec()]
        );
        assert_eq!(archetype.blocks, vec![b"AA".to_vec(), b"BB".to_vec()]);
        assert_eq!(plain.samples, vec![b"AA", b"AA", b"BB", b"AA", b"AA"]);
        assert_eq!(block.samples, vec![b"AA", b"BB", b"AA", b"AA"]);
        assert_eq!(archetype.samples, vec![b"AA", b"BB"]);
    }

    #[test]
    fn prefix_splitting_respects_original_blocks_and_utf8_boundaries() {
        let values = [
            (1, "load abc"),
            (2, "load def"),
            (4, "é one"),
            (5, "ê two"),
            (7, "solo"),
        ];
        // Missing rows do not shift boundaries: blocks are [1..3], [4..6], [7..9].
        let parts = string_parts(&values, 3, true);
        assert_eq!(
            parts,
            [
                (1, StringPart::Prefix, "load "),
                (1, StringPart::Suffix, "abc"),
                (2, StringPart::Suffix, "def"),
                (4, StringPart::Suffix, "é one"),
                (5, StringPart::Suffix, "ê two"),
                (7, StringPart::Prefix, "solo"),
                (7, StringPart::Suffix, ""),
            ]
        );
        let mut prefix = "";
        let mut block = None;
        let mut restored = Vec::new();
        for (row, kind, value) in parts {
            let next_block = (row - 1) / 3;
            if block != Some(next_block) {
                prefix = "";
                block = Some(next_block);
            }
            match kind {
                StringPart::Prefix => prefix = value,
                StringPart::Suffix => restored.push((row, format!("{prefix}{value}"))),
            }
        }
        assert_eq!(restored, values.map(|(row, value)| (row, value.to_owned())));
        assert_eq!(
            string_parts(&[(1, "same"), (2, "same")], 2, true),
            [
                (1, StringPart::Prefix, "same"),
                (1, StringPart::Suffix, ""),
                (2, StringPart::Suffix, ""),
            ]
        );
        assert_eq!(
            string_parts(&[(1, ""), (2, "abc")], 2, true),
            [(1, StringPart::Suffix, ""), (2, StringPart::Suffix, "abc")]
        );
    }

    #[test]
    fn prefixes_and_suffixes_deduplicate_separately_in_the_selected_scope() {
        let values = [(1, "load A"), (2, "load B"), (3, "load A"), (4, "load B")];
        let block = build_mode(&values, 2, DedupMode::Block, true);
        let arch = build_mode(&values, 2, DedupMode::Archetype, true);
        assert_eq!(block.blocks, [b"load AB".to_vec(), b"load AB".to_vec()]);
        assert_eq!(
            block.samples,
            [b"load ".as_slice(), b"A", b"B", b"load ", b"A", b"B"]
        );
        assert_eq!(arch.samples, [b"load ".as_slice(), b"A", b"B"]);
        // A prefix and suffix with identical bytes still occupy distinct pools.
        let collision = [(1, "ab"), (2, "ac"), (3, "a"), (4, "z")];
        assert_eq!(
            build_mode(&collision, 2, DedupMode::Archetype, true).samples,
            [b"a", b"b", b"c", b"a", b"z"]
        );
    }

    #[test]
    fn global_prefix_pools_span_archetypes_and_are_distinct_per_block_size() {
        let options = Options::parse(
            [
                "--dedup=block,archetype,global",
                "--prefix=off,on",
                "--block_size=2,4",
                "--compression=none",
                "--sizes",
                "dump.msgpack",
            ]
            .map(str::to_owned),
        )
        .unwrap();
        let mut encoded = Vec::new();
        let mut write_record =
            |value: Value| rmpv::encode::write_value(&mut encoded, &value).unwrap();
        write_record(record(&[("type", "format".into()), ("version", 1.into())]));
        write_record(record(&[
            ("type", "callsite".into()),
            ("id", 7.into()),
            ("target", "target".into()),
            ("name", "event".into()),
            ("fields", Value::Array(vec!["message".into()])),
        ]));
        for (id, messages) in [
            ["red A", "red B", "blue A", "blue B"],
            ["red A", "red B", "green A", "green B"],
        ]
        .iter()
        .enumerate()
        {
            write_record(record(&[
                ("type", "archetype".into()),
                ("callsite", 7.into()),
                ("id", (id as u64).into()),
                ("fields", Value::Array(vec!["Str".into()])),
            ]));
            for (row, message) in messages.iter().enumerate() {
                write_record(record(&[
                    ("type", "row".into()),
                    ("callsite", 7.into()),
                    ("archetype", (id as u64).into()),
                    ("index", (row as u64 + 1).into()),
                    ("fields", Value::Array(vec![(*message).into()])),
                ]));
            }
        }
        let results = read_dump(BufReader::new(encoded.as_slice()), &options, &mut None).unwrap();
        assert_eq!(results.summaries[0].global_pools.len(), 3);
        for experiment in &results.experiments {
            let setting = &experiment.setting;
            let expected = match (setting.mode, setting.prefix, setting.block_rows) {
                (DedupMode::Block, true, 2) => 27,
                (DedupMode::Archetype, true, 2) => 23,
                (DedupMode::Global, true, 2) => 17,
                (DedupMode::Global, _, _) => 36,
                _ => 46,
            };
            assert_eq!(experiment.totals.raw_bytes, expected, "{setting:?}");
            assert_eq!(results.rows_for(setting), 8);
            if setting.mode.uses_message_dictionary() {
                assert_eq!(experiment.totals.block_count, 0);
                assert_eq!(experiment.totals.lz4_block_bytes, 0);
            }
        }
        let mut output = Vec::new();
        print_tables(&results, &options, &mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        // Both block sizes must survive normalization for prefix-on pools.
        assert!(output.contains("message,*,global,2,n/a,n/a,n/a,on,none,none,none,n/a,8,2.125000"));
        assert!(output.contains("message,*,global,4,n/a,n/a,n/a,on,none,none,none,n/a,8,4.500000"));
    }

    #[test]
    fn dictionary_uses_the_selected_modes_blocks() {
        let mut zstd = Some(Zstd::load().unwrap());
        let options = Options::parse(
            ["--block_size=10", "--dict_size=1024", "dump.msgpack"].map(str::to_owned),
        )
        .unwrap();
        let setting = &options.settings[0];
        let strings: Vec<String> = (0..600)
            .map(|index| {
                format!(
                    "variant {:03} / a repeatable message for dictionary training",
                    index % 300
                )
            })
            .collect();
        let values: Vec<(u64, &str)> = strings
            .iter()
            .enumerate()
            .map(|(index, value)| (index as u64 + 1, value.as_str()))
            .collect();
        let plain = build_mode(&values, 10, DedupMode::Plain, false);
        let arch = build_mode(&values, 10, DedupMode::Archetype, false);
        assert!(
            plain.blocks.iter().map(Vec::len).sum::<usize>()
                > arch.blocks.iter().map(Vec::len).sum::<usize>()
        );
        let plain_stats = compare(&plain, &mut zstd, setting, &options).unwrap();
        let arch_setting = Setting {
            mode: DedupMode::Archetype,
            ..setting.clone()
        };
        let arch_stats = compare(&arch, &mut zstd, &arch_setting, &options).unwrap();
        assert_eq!(
            plain_stats.raw_bytes,
            plain.blocks.iter().map(Vec::len).sum()
        );
        assert_eq!(arch_stats.raw_bytes, arch.blocks.iter().map(Vec::len).sum());
        assert_eq!(
            arch_stats.lz4_whole_bytes,
            lz4_compress(&arch.blocks.concat()).len()
        );
        assert_eq!(
            arch_stats.zstd_whole_bytes,
            zstd.as_mut()
                .unwrap()
                .compress(&arch.blocks.concat(), None, setting.zstd_level)
                .unwrap()
        );
        assert!(plain_stats.dict_payload_bytes.is_some());
        assert!(arch_stats.dict_payload_bytes.is_some());
        assert!(arch_stats.dict_payload_bytes.unwrap() <= arch_stats.raw_bytes);
        assert!(arch_stats.lz4_dict_payload_bytes.unwrap() <= arch_stats.raw_bytes);
        assert_eq!(arch_stats.dict_units, 300);
        assert_eq!(
            arch_stats.lz4_dict_timing.decompress.bytes,
            arch_stats.raw_bytes
        );
        assert_eq!(
            arch_stats.zstd_dict_timing.decompress.bytes,
            arch_stats.raw_bytes
        );
        // Independently sum min(raw, compressed) using individual samples.
        let dictionary = zstd
            .as_ref()
            .unwrap()
            .train(&arch.samples, setting.dict_max_bytes)
            .unwrap();
        let mut expected_zstd = 0;
        let mut expected_lz4 = 0;
        for message in &arch.samples {
            expected_zstd += message.len().min(
                zstd.as_mut()
                    .unwrap()
                    .compress(message, Some(&dictionary), setting.zstd_level)
                    .unwrap(),
            );
            expected_lz4 += message
                .len()
                .min(lz4_compress_with_dict(message, &dictionary).len());
        }
        assert_eq!(arch_stats.dict_payload_bytes, Some(expected_zstd));
        assert_eq!(arch_stats.lz4_dict_payload_bytes, Some(expected_lz4));
        assert_eq!(arch_stats.block_count, 0);
        assert_eq!(arch_stats.lz4_compress.bytes, 0);
        assert_eq!(arch_stats.zstd_compress.bytes, 0);
        assert!(plain_stats.lz4_dict_payload_bytes.is_some());
        assert!(arch_stats.lz4_dict_payload_bytes.is_some());
        let mut totals = ModeTotals::default();
        totals.add(&plain_stats);
        assert_eq!(
            totals.lz4_dict_effective_bytes,
            plain_stats.lz4_dict_payload_bytes.unwrap() + plain_stats.dict_bytes
        );
        let block = build_mode(&values, 10, DedupMode::Block, false);
        for data in [&plain, &block] {
            // LZ4 must be able to restore each block with the Zstd-trained bytes.
            let samples = training_samples(&data.samples, setting.sample_bytes);
            let sample_refs: Vec<_> = samples.iter().map(Vec::as_slice).collect();
            let zstd = zstd.as_mut().unwrap();
            let dictionary = zstd.train(&sample_refs, setting.dict_max_bytes).unwrap();
            for block in &data.blocks {
                let compressed = lz4_compress_with_dict(block, &dictionary);
                let restored =
                    lz4_flex::block::decompress_with_dict(&compressed, block.len(), &dictionary)
                        .unwrap();
                assert_eq!(&restored, block);
                let zstd_compressed = zstd
                    .compress_bytes(block, Some(&dictionary), setting.zstd_level)
                    .unwrap();
                let zstd_restored = zstd
                    .decompress_with_dict(&zstd_compressed, block.len(), &dictionary)
                    .unwrap();
                assert_eq!(&zstd_restored, block);
            }
        }
    }

    #[test]
    fn field_flag_selects_one_group_across_archetype_boundaries() {
        let records = [
            record(&[("type", "format".into()), ("version", 1.into())]),
            record(&[
                ("type", "callsite".into()),
                ("id", 7.into()),
                ("target", "target".into()),
                ("name", "event".into()),
                (
                    "fields",
                    Value::Array(vec!["message".into(), "label".into()]),
                ),
            ]),
            record(&[
                ("type", "archetype".into()),
                ("callsite", 7.into()),
                ("id", 0.into()),
                ("fields", Value::Array(vec!["Str".into(), "Str".into()])),
            ]),
            record(&[
                ("type", "row".into()),
                ("callsite", 7.into()),
                ("archetype", 0.into()),
                ("index", 1.into()),
                ("fields", Value::Array(vec!["same".into(), "red".into()])),
            ]),
            record(&[
                ("type", "row".into()),
                ("callsite", 7.into()),
                ("archetype", 0.into()),
                ("index", 2.into()),
                ("fields", Value::Array(vec!["same".into(), "blue".into()])),
            ]),
            record(&[
                ("type", "archetype".into()),
                ("callsite", 7.into()),
                ("id", 1.into()),
                ("fields", Value::Array(vec!["Str".into(), "Str".into()])),
            ]),
            record(&[
                ("type", "row".into()),
                ("callsite", 7.into()),
                ("archetype", 1.into()),
                ("index", 1.into()),
                ("fields", Value::Array(vec!["same".into(), "red".into()])),
            ]),
        ];
        let mut encoded = Vec::new();
        for record in records {
            rmpv::encode::write_value(&mut encoded, &record).unwrap();
        }
        let options = Options::parse(
            [
                "--fields=message,other",
                "--block_size=2,3",
                "--dict_size=512",
                "--target=target,missing",
                "dump.msgpack",
            ]
            .map(str::to_owned),
        )
        .unwrap();
        let results = read_dump(
            BufReader::new(encoded.as_slice()),
            &options,
            &mut Some(Zstd::load().unwrap()),
        )
        .unwrap();
        assert_eq!(results.experiments.len(), 24);
        for summary in &results.summaries {
            if summary.target == "missing" {
                assert_eq!(summary.totals.rows, 0);
                continue;
            }
            let totals = &summary.totals;
            assert_eq!(totals.archetypes, 2);
            assert_eq!(totals.rows, 3);
            assert_eq!(
                totals
                    .cross_archetype_uses
                    .values()
                    .filter(|&&v| v > 1)
                    .count(),
                1
            );
            let duplicate_bytes: usize = totals
                .cross_archetype_uses
                .iter()
                .map(|(s, count)| s.len() * (count - 1))
                .sum();
            assert_eq!(
                duplicate_bytes,
                if summary.group == FieldGroup::Message {
                    4
                } else {
                    3
                }
            );
        }
        for experiment in &results.experiments {
            let totals = &experiment.totals;
            if experiment.setting.mode.supports_blocks() {
                assert_eq!(totals.lz4_dict_effective_bytes, totals.lz4_block_bytes);
                assert_eq!(totals.zstd_dict_effective_bytes, totals.zstd_block_bytes);
                assert_eq!(totals.lz4_compress.bytes, totals.raw_bytes);
                assert_eq!(totals.zstd_compress.bytes, totals.raw_bytes);
                assert_eq!(totals.lz4_decompress.bytes, totals.raw_bytes);
                assert_eq!(totals.zstd_decompress.bytes, totals.raw_bytes);
            } else {
                assert_eq!(totals.block_count, 0);
                assert_eq!(totals.lz4_dict_effective_bytes, totals.raw_bytes);
                assert_eq!(totals.zstd_dict_effective_bytes, totals.raw_bytes);
                if experiment.setting.target == "target" {
                    let pools = if experiment.setting.mode == DedupMode::Global {
                        1
                    } else {
                        2
                    };
                    assert_eq!(totals.untrained_archetypes, pools);
                    if experiment.setting.mode == DedupMode::Global {
                        assert_eq!(
                            totals.raw_bytes,
                            if experiment.setting.group == FieldGroup::Message {
                                4
                            } else {
                                7
                            }
                        );
                    }
                }
            }
        }
        let mut output = Vec::new();
        print_tables(&results, &options, &mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains("MODE="));
        assert!(output.contains("# timing"));
        assert!(output.contains("# sizes"));
        assert!(output.contains("# summary"));
        assert!(output.contains(
            "compress_total_ms,decompress_total_ms,compress_unit_avg_us,decompress_unit_avg_us"
        ));
        let mut output = Vec::new();
        list_targets(BufReader::new(encoded.as_slice()), &options, &mut output).unwrap();
        assert_eq!(output, b"target\n");

        // Regression: size-only output must still count the original matched
        // rows, use that same divisor for global dedup, and handle empty filters.
        let size_options = Options::parse(
            [
                "--fields=message,other",
                "--target=target,missing",
                "--dedup=plain,archetype,global",
                "--compression=none",
                "--sizes",
                "dump.msgpack",
            ]
            .map(str::to_owned),
        )
        .unwrap();
        let size_results =
            read_dump(BufReader::new(encoded.as_slice()), &size_options, &mut None).unwrap();
        let mut output = Vec::new();
        print_tables(&size_results, &size_options, &mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        for line in output.lines().skip(2).filter(|line| !line.is_empty()) {
            let cells: Vec<_> = line.split(',').collect();
            let (rows, bytes) = match (cells[0], cells[1], cells[2]) {
                (_, "missing", _) => ("0", "n/a"),
                ("message", "target", "plain") => ("3", "4.000000"),
                ("message", "target", "archetype") => ("3", "2.666667"),
                ("message", "target", "global") => ("3", "1.333333"),
                ("other", "target", "plain" | "archetype") => ("3", "3.333333"),
                ("other", "target", "global") => ("3", "2.333333"),
                _ => panic!("unexpected size-only report row: {line}"),
            };
            assert_eq!(cells[12], rows);
            assert_eq!(&cells[13..16], &[bytes; 3]);
        }
    }

    #[test]
    fn samples_group_whole_messages_after_deduplication() {
        let values = [(1, "aa"), (2, "aa"), (3, "bb"), (4, "abcdefgh")];
        let plain = build_mode(&values, 2, DedupMode::Plain, false);
        let arch = build_mode(&values, 2, DedupMode::Archetype, false);
        assert_eq!(
            training_samples(&plain.samples, 4),
            vec![b"aaaa".to_vec(), b"bbabcdefgh".to_vec()]
        );
        assert_eq!(
            training_samples(&arch.samples, 4),
            vec![b"aabb".to_vec(), b"abcdefgh".to_vec()]
        );
        // Grouping changes sample boundaries but never changes training bytes.
        for data in [plain, arch] {
            assert_eq!(
                training_samples(&data.samples, 4).concat(),
                data.blocks.concat()
            );
        }
    }

    #[test]
    fn global_dictionary_trains_once_on_unique_messages_across_archetypes() {
        let options = Options::parse(
            [
                "--dedup=archetype,global",
                "--compression=lz4,zstd",
                "--compression_mode=dict",
                "--block_size=100,1000",
                "--sample_size=64,4096",
                "--dict_size=1024",
                "--timing",
                "--sizes",
                "dump.msgpack",
            ]
            .map(str::to_owned),
        )
        .unwrap();
        assert_eq!(options.settings.len(), 2); // Block/sample targets are irrelevant here.
        let messages: Vec<_> = (0..500).map(|i| format!("message {i:04}: shared wording for individual dictionary samples and compression")).collect();
        let mut encoded = Vec::new();
        let mut write_record =
            |value: Value| rmpv::encode::write_value(&mut encoded, &value).unwrap();
        write_record(record(&[("type", "format".into()), ("version", 1.into())]));
        write_record(record(&[
            ("type", "callsite".into()),
            ("id", 7.into()),
            ("target", "target".into()),
            ("name", "event".into()),
            ("fields", Value::Array(vec!["message".into()])),
        ]));
        for (archetype, start) in [(0, 0), (1, 200)] {
            write_record(record(&[
                ("type", "archetype".into()),
                ("callsite", 7.into()),
                ("id", archetype.into()),
                ("fields", Value::Array(vec!["Str".into()])),
            ]));
            for (index, message) in messages[start..start + 300].iter().enumerate() {
                write_record(record(&[
                    ("type", "row".into()),
                    ("callsite", 7.into()),
                    ("archetype", archetype.into()),
                    ("index", (index as u64 + 1).into()),
                    ("fields", Value::Array(vec![message.as_str().into()])),
                ]));
            }
        }
        let mut zstd = Some(Zstd::load().unwrap());
        let results = read_dump(BufReader::new(encoded.as_slice()), &options, &mut zstd).unwrap();
        let arch = &results.experiments[0].totals;
        let global = &results.experiments[1].totals;
        assert_eq!(arch.trained_archetypes, 2);
        assert_eq!(arch.dict_units, 600);
        assert_eq!(global.trained_archetypes, 1);
        assert_eq!(global.dict_units, 500);
        assert_eq!(global.raw_bytes, messages.iter().map(String::len).sum());
        assert_eq!(global.lz4_dict_timing.decompress.bytes, global.raw_bytes);
        assert_eq!(global.zstd_dict_timing.decompress.bytes, global.raw_bytes);
        let samples: Vec<_> = messages.iter().map(|m| m.as_bytes()).collect();
        let dictionary = zstd.as_ref().unwrap().train(&samples, 1024).unwrap();
        assert_eq!(global.dict_bytes, dictionary.len());
        let expected_lz4: usize = samples
            .iter()
            .map(|m| m.len().min(lz4_compress_with_dict(m, &dictionary).len()))
            .sum();
        let expected_zstd: usize = samples
            .iter()
            .map(|m| {
                m.len().min(
                    zstd.as_mut()
                        .unwrap()
                        .compress(m, Some(&dictionary), 3)
                        .unwrap(),
                )
            })
            .sum();
        assert_eq!(
            global.lz4_dict_effective_bytes,
            expected_lz4 + dictionary.len()
        );
        assert_eq!(
            global.zstd_dict_effective_bytes,
            expected_zstd + dictionary.len()
        );

        // Discouraged but supported: train/decode both pools as individual
        // parts, without ever introducing compressed archetype/global blocks.
        let prefix_options = Options::parse(
            [
                "--dedup=archetype,global",
                "--prefix=on",
                "--compression=lz4,zstd",
                "--compression_mode=dict",
                "--block_size=100",
                "--dict_size=1024",
                "--timing",
                "dump.msgpack",
            ]
            .map(str::to_owned),
        )
        .unwrap();
        let prefix_results = read_dump(
            BufReader::new(encoded.as_slice()),
            &prefix_options,
            &mut zstd,
        )
        .unwrap();
        let global = &prefix_results.experiments[1].totals;
        // Five prefixes (message 00..04) share 100 distinct numeric suffixes.
        assert_eq!(global.dict_units, 105);
        assert_eq!(global.trained_archetypes, 1);
        for experiment in &prefix_results.experiments {
            let stats = &experiment.totals;
            assert_eq!(stats.block_count, 0);
            assert_eq!(stats.lz4_block_bytes, 0);
            assert_eq!(stats.zstd_block_bytes, 0);
            assert_eq!(stats.lz4_dict_timing.decompress.bytes, stats.raw_bytes);
            assert_eq!(stats.zstd_dict_timing.decompress.bytes, stats.raw_bytes);
            assert!(stats.lz4_dict_effective_bytes - stats.dict_bytes <= stats.raw_bytes);
            assert!(stats.zstd_dict_effective_bytes - stats.dict_bytes <= stats.raw_bytes);
        }
    }

    #[test]
    fn short_dictionary_messages_use_raw_storage_and_zero_decode_work() {
        let mut zstd = Zstd::load().unwrap();
        for compression in [Compression::Lz4, Compression::Zstd] {
            let (bytes, raw, timing) = measure_dictionary_units(
                &[b"x"],
                compression,
                &mut zstd,
                b"a raw dictionary",
                3,
                true,
                true,
            )
            .unwrap();
            assert_eq!((bytes, raw), (1, 1));
            assert_eq!(timing.decompress.total, Duration::ZERO);
            assert_eq!(timing.decompress.bytes, 1);
        }
    }
}
