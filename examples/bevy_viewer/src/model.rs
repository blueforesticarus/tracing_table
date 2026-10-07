//! Example-owned compression cache and presentation/query policy. This module
//! deliberately has no Bevy dependency so root tests can exercise it cheaply.
use crate::inspection::{
    ArchetypeId, ArchetypeSnapshot, FrameSizes, InspectRecording, RecordedRow,
};
use std::collections::HashMap;
use tracing_table::data::{dataframe::CompressionStats, dyntuple::ValueData};
use tracing_table::subscriber::RecordingLayer;

pub struct CachedArchetype {
    pub snapshot: ArchetypeSnapshot,
    pub ring: CompressionStats,
    pub total: CompressionStats,
    pub fields: Vec<FieldCompression>,
    pub components: Vec<FieldCompression>,
    // Retain completed-block totals so a refresh does not rescan old row headers.
    pub block_duration_sum_ns: u128,
    pub timed_blocks: usize,
}

impl CachedArchetype {
    pub fn average_block_duration_ns(&self) -> Option<f64> {
        (self.timed_blocks > 0)
            .then(|| self.block_duration_sum_ns as f64 / self.timed_blocks as f64)
    }
}

/// Keep timestamp durations legible without losing sub-millisecond resolution.
pub fn duration_text(ns: Option<f64>) -> String {
    let Some(ns) = ns else {
        return "—".into();
    };
    let (value, unit) = if ns >= 1e9 {
        (ns / 1e9, "s")
    } else if ns >= 1e6 {
        (ns / 1e6, "ms")
    } else if ns >= 1e3 {
        (ns / 1e3, "µs")
    } else {
        (ns, "ns")
    };
    format!("{} {unit}", significant_text(value))
}

#[derive(Default)]
pub struct StatsCache {
    pub archetypes: HashMap<ArchetypeId, CachedArchetype>,
    pub total: CompressionStats,
    pub frame_sizes: FrameSizes,
    frames: HashMap<(ArchetypeId, usize), CachedFrame>,
    /// Visible diagnostic: only changed completed frames increment this counter.
    pub completed_recomputations: usize,
}

/// Byte accounting for a field's portion of completed blocks. A block stores
/// one full first row plus only varying byte positions in its remaining rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FieldCompression {
    pub raw: usize,
    pub packed: usize,
    pub constant_bytes: usize,
    pub blocks: usize,
    pub rows: usize,
}
struct CachedFrame {
    stats: CompressionStats,
    duration_ns: Option<u64>,
    fields: Vec<FieldCompression>,
    components: Vec<FieldCompression>,
}

fn field_contribution(
    range: Option<&std::ops::Range<usize>>,
    rows: usize,
    mask: &[u8],
) -> FieldCompression {
    let Some(range) = range else {
        return FieldCompression::default();
    };
    let width = range.len();
    let constant = mask[range.clone()]
        .iter()
        .filter(|byte| **byte == 0)
        .count();
    FieldCompression {
        raw: width * rows,
        packed: width + (rows - 1) * (width - constant),
        constant_bytes: constant,
        blocks: 1,
        rows,
    }
}
fn replace_fields(
    total: &mut [FieldCompression],
    previous: &[FieldCompression],
    next: &[FieldCompression],
) {
    for ((total, previous), next) in total.iter_mut().zip(previous).zip(next) {
        total.raw = total.raw - previous.raw + next.raw;
        total.packed = total.packed - previous.packed + next.packed;
        total.constant_bytes = total.constant_bytes - previous.constant_bytes + next.constant_bytes;
        total.blocks = total.blocks - previous.blocks + next.blocks;
        total.rows = total.rows - previous.rows + next.rows;
    }
}

fn subtract(a: CompressionStats, b: CompressionStats) -> CompressionStats {
    CompressionStats {
        data: a.data - b.data,
        compressed: a.compressed - b.compressed,
        overhead: a.overhead - b.overhead,
        overalloc: a.overalloc - b.overalloc,
    }
}

impl StatsCache {
    pub fn refresh(&mut self, recorder: &RecordingLayer) {
        for mut snapshot in recorder.inspect(&self.frame_sizes) {
            let old = self.archetypes.remove(&snapshot.id);
            let mut ring = old
                .as_ref()
                .map_or(CompressionStats::default(), |old| old.ring);
            let mut fields = old.as_ref().map_or_else(
                || vec![FieldCompression::default(); snapshot.fields.len()],
                |old| old.fields.clone(),
            );
            let mut components = old.as_ref().map_or_else(
                || vec![FieldCompression::default(); snapshot.components.len()],
                |old| old.components.clone(),
            );
            let mut block_duration_sum_ns = old.as_ref().map_or(0, |old| old.block_duration_sum_ns);
            let mut timed_blocks = old.as_ref().map_or(0, |old| old.timed_blocks);
            for frame in snapshot.completed_updates.drain(..) {
                let key = (snapshot.id, frame.id);
                let next: Vec<_> = snapshot
                    .field_ranges
                    .iter()
                    .map(|range| field_contribution(range.as_ref(), frame.rows, &frame.mask))
                    .collect();
                let next_components: Vec<_> = snapshot
                    .components
                    .iter()
                    .map(|component| {
                        field_contribution(Some(&component.range), frame.rows, &frame.mask)
                    })
                    .collect();
                let previous = self.frames.insert(
                    key,
                    CachedFrame {
                        stats: frame.stats,
                        duration_ns: frame.duration_ns,
                        fields: next.clone(),
                        components: next_components.clone(),
                    },
                );
                if let Some(previous) = &previous {
                    ring = subtract(ring, previous.stats);
                    if let Some(duration) = previous.duration_ns {
                        block_duration_sum_ns -= duration as u128;
                        timed_blocks -= 1;
                    }
                }
                let empty = vec![FieldCompression::default(); fields.len()];
                replace_fields(
                    &mut fields,
                    previous
                        .as_ref()
                        .map_or(&empty[..], |previous| &previous.fields),
                    &next,
                );
                let empty_components = vec![FieldCompression::default(); components.len()];
                replace_fields(
                    &mut components,
                    previous
                        .as_ref()
                        .map_or(&empty_components[..], |previous| &previous.components),
                    &next_components,
                );
                if let Some(duration) = frame.duration_ns {
                    block_duration_sum_ns += duration as u128;
                    timed_blocks += 1;
                }
                ring = ring + frame.stats;
                self.frame_sizes.insert(key, frame.packed_len);
                self.completed_recomputations += 1;
            }
            let total = ring + snapshot.active;
            if let Some(old) = old {
                self.total = subtract(self.total, old.total);
            }
            self.total = self.total + total;
            self.archetypes.insert(
                snapshot.id,
                CachedArchetype {
                    snapshot,
                    ring,
                    total,
                    fields,
                    components,
                    block_duration_sum_ns,
                    timed_blocks,
                },
            );
        }
    }
}

/// Decimal units, rounded to three significant figures; promote after rounding
/// so 999,500 becomes 1.00M rather than an awkward 1000K.
pub fn size_text(bytes: usize) -> String {
    if bytes == 0 {
        return "0".into();
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < 2 {
        value /= 1000.0;
        unit += 1;
    }
    let factor = 10f64.powf(2.0 - value.log10().floor());
    value = (value * factor).round() / factor;
    if value >= 1000.0 && unit < 2 {
        value /= 1000.0;
        unit += 1;
    }
    format!("{}{}", significant_text(value), ["", "K", "M"][unit])
}

/// Also format fractional byte averages without inventing extra precision.
pub fn significant_text(mut value: f64) -> String {
    if value == 0.0 {
        return "0".into();
    }
    let factor = 10f64.powf(2.0 - value.log10().floor());
    value = (value * factor).round() / factor;
    let decimals = (2.0 - value.log10().floor()).max(0.0) as usize;
    format!("{value:.decimals$}")
}
pub fn compression_percent(raw: usize, packed: usize) -> f64 {
    // Fields share the same ratio calculation as whole-archetype statistics.
    100.0
        * CompressionStats {
            data: raw,
            compressed: packed,
            ..Default::default()
        }
        .raw_ratio()
}
pub fn storage(stats: CompressionStats) -> usize {
    stats.compressed + stats.overhead + stats.overalloc
}
pub fn storage_percent(part: CompressionStats, total: CompressionStats) -> f64 {
    let total = storage(total);
    if total == 0 {
        0.0
    } else {
        100.0 * storage(part) as f64 / total as f64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortColumn {
    Id,
    Target,
    Source,
    Level,
    Kind,
    Rows,
    Total,
    Raw,
    Packed,
    Overhead,
    Unused,
    Compression,
    RawCompression,
    Storage,
    Blocks,
    BlockDuration,
}
impl SortColumn {
    pub const COLUMNS: [(Self, &'static str); 15] = [
        (Self::Id, "Callsite:arch"),
        (Self::Target, "Target / name"),
        (Self::Source, "Source"),
        (Self::Level, "Level"),
        (Self::Kind, "Kind"),
        (Self::Rows, "Rows"),
        (Self::Blocks, "Blocks"),
        (Self::BlockDuration, "Avg block duration"),
        (Self::Raw, "Data"),
        (Self::Total, "Total"),
        (Self::Packed, "Compressed"),
        (Self::Overhead, "Overhead"),
        (Self::Unused, "Overalloc"),
        (Self::Compression, "Ratio %"),
        (Self::RawCompression, "Raw ratio %"),
    ];
}
#[derive(Clone, Copy)]
pub struct SortOrder {
    pub column: SortColumn,
    pub descending: bool,
    /// Byte columns follow the displayed units when the UI shows bytes per row.
    pub per_row: bool,
}
impl Default for SortOrder {
    fn default() -> Self {
        Self {
            column: SortColumn::Packed,
            descending: true,
            per_row: false,
        }
    }
}
impl SortOrder {
    pub fn select(&mut self, column: SortColumn) {
        if self.column == column {
            self.descending = !self.descending;
        } else {
            self.column = column;
            self.descending = !matches!(
                column,
                SortColumn::Id
                    | SortColumn::Target
                    | SortColumn::Source
                    | SortColumn::Level
                    | SortColumn::Kind
            );
        }
    }
}
pub fn sorted_archetypes(cache: &StatsCache, filter: &str, sort: SortOrder) -> Vec<ArchetypeId> {
    let mut ids: Vec<_> = cache
        .archetypes
        .iter()
        .filter(|(_, arch)| matches_archetype(arch, filter))
        .map(|(id, _)| *id)
        .collect();
    ids.sort_by(|a, b| {
        let a_arch = &cache.archetypes[a];
        let b_arch = &cache.archetypes[b];
        let a_snap = &a_arch.snapshot;
        let b_snap = &b_arch.snapshot;
        let compare_bytes = |a_bytes: usize, b_bytes: usize| {
            if sort.per_row {
                (a_bytes as f64 / a_snap.rows.max(1) as f64)
                    .total_cmp(&(b_bytes as f64 / b_snap.rows.max(1) as f64))
            } else {
                a_bytes.cmp(&b_bytes)
            }
        };
        let order = match sort.column {
            SortColumn::Id => a.cmp(b),
            SortColumn::Target => (a_snap.metadata.target(), a_snap.metadata.name())
                .cmp(&(b_snap.metadata.target(), b_snap.metadata.name())),
            SortColumn::Source => (a_snap.metadata.file(), a_snap.metadata.line())
                .cmp(&(b_snap.metadata.file(), b_snap.metadata.line())),
            // Use tracing's severity ordering rather than sorting level names.
            SortColumn::Level => a_snap.metadata.level().cmp(b_snap.metadata.level()),
            SortColumn::Kind => format!("{:?}", a_snap.kind).cmp(&format!("{:?}", b_snap.kind)),
            SortColumn::Rows => a_snap.rows.cmp(&b_snap.rows),
            SortColumn::Total => compare_bytes(storage(a_arch.total), storage(b_arch.total)),
            SortColumn::Raw => compare_bytes(a_arch.total.data, b_arch.total.data),
            SortColumn::Packed => compare_bytes(a_arch.total.compressed, b_arch.total.compressed),
            SortColumn::Overhead => compare_bytes(a_arch.total.overhead, b_arch.total.overhead),
            SortColumn::Unused => compare_bytes(a_arch.total.overalloc, b_arch.total.overalloc),
            SortColumn::Blocks => a_snap.blocks.cmp(&b_snap.blocks),
            SortColumn::BlockDuration => a_arch
                .average_block_duration_ns()
                .unwrap_or(-1.0)
                .total_cmp(&b_arch.average_block_duration_ns().unwrap_or(-1.0)),
            SortColumn::Compression => a_arch.total.ratio().total_cmp(&b_arch.total.ratio()),
            SortColumn::RawCompression => a_arch
                .total
                .raw_ratio()
                .total_cmp(&b_arch.total.raw_ratio()),
            // All shares use the same denominator, so sort by storage bytes.
            SortColumn::Storage => storage(a_arch.total).cmp(&storage(b_arch.total)),
        };
        (if sort.descending {
            order.reverse()
        } else {
            order
        })
        .then_with(|| a.cmp(b))
    });
    ids
}

pub fn entity_choices(rows: &[RecordedRow]) -> Vec<String> {
    let mut entities: Vec<_> = rows
        .iter()
        .filter_map(|row| {
            row.fields
                .iter()
                .find(|(name, _)| name == "entity")
                .map(|(_, value)| field_text(value))
        })
        .collect();
    entities.sort();
    entities.dedup();
    entities
}

/// String/debug entity values have an explicit prefix so a numeric 123 cannot
/// accidentally match a string "123". Display labels are also exact filter keys.
pub fn field_text(value: &ValueData) -> String {
    match value {
        ValueData::U64(v) => v.to_string(),
        ValueData::I64(v) => v.to_string(),
        ValueData::U128(v) => v.to_string(),
        ValueData::I128(v) => v.to_string(),
        ValueData::F64(v) => format!("f64:{v}"),
        ValueData::Bool(v) => format!("bool:{v}"),
        ValueData::Str(v) => format!("str:{v}"),
        ValueData::PrefixStr(a, b) => format!("str:{a}{b}"),
        // Raw global indexed values use the same display and filter keys as Atoms.
        ValueData::StrIndex(v) => format!("str:{v}"),
        ValueData::PrefixStrIndex(a, b) => format!("str:{a}{b}"),
    }
}

pub fn message(row: &RecordedRow) -> String {
    row.fields
        .iter()
        .find(|(name, _)| name == "message")
        .map_or_else(String::new, |(_, value)| match value {
            ValueData::Str(v) => v.to_string(),
            ValueData::PrefixStr(a, b) => format!("{a}{b}"),
            ValueData::StrIndex(v) => v.to_string(),
            ValueData::PrefixStrIndex(a, b) => format!("{a}{b}"),
            _ => field_text(value),
        })
}

pub fn matches_entity(row: &RecordedRow, filter: &str) -> bool {
    let filter = filter.trim();
    filter.is_empty()
        || row
            .fields
            .iter()
            .any(|(name, value)| name == "entity" && field_text(value) == filter)
}

pub fn matches_archetype(arch: &CachedArchetype, filter: &str) -> bool {
    let snapshot = &arch.snapshot;
    let metadata = snapshot.metadata;
    let searchable = format!(
        "{}:{} {} {} {}:{} {:?} {:?}",
        snapshot.id.callsite,
        snapshot.id.archetype,
        metadata.target(),
        metadata.name(),
        metadata.file().unwrap_or("?"),
        metadata.line().unwrap_or(0),
        snapshot.kind,
        snapshot.fields
    );
    searchable
        .to_lowercase()
        .contains(&filter.trim().to_lowercase())
}

/// Filtering stays here, outside the library. Page until we have the newest
/// matching rows; filtering just the newest unfiltered page would miss history.
pub fn matching_rows(
    recorder: &RecordingLayer,
    id: ArchetypeId,
    entity: &str,
    limit: usize,
) -> Vec<RecordedRow> {
    let mut rows = Vec::new();
    let mut before = None;
    while rows.len() < limit {
        let page = recorder.rows(id, before, 256);
        if page.is_empty() {
            break;
        }
        // Every row of an archetype has the same recorded-field schema. Avoid
        // scanning a hot engine archetype whose records have no own entity field.
        if !entity.trim().is_empty() && !page[0].fields.iter().any(|(name, _)| name == "entity") {
            break;
        }
        before = page.last().map(|row| row.index);
        for row in page {
            if matches_entity(&row, entity) {
                rows.push(row);
            }
            if rows.len() == limit {
                break;
            }
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inspection::RecordKind;
    use std::sync::Arc;
    fn stats_values(s: CompressionStats) -> (usize, usize, usize, usize) {
        (s.data, s.compressed, s.overhead, s.overalloc)
    }

    #[test]
    fn indexed_strings_share_atom_labels_and_messages() {
        use tracing_table::data::{dyntuple::Atom, stringcache::AtomIndexed};
        // Indexed fields must share the ordinary string display and filter keys.
        let text = "long indexed message";
        assert_eq!(
            field_text(&ValueData::StrIndex(AtomIndexed::from(text))),
            field_text(&ValueData::Str(Atom::from(text))),
        );
        let row = RecordedRow {
            index: 0,
            timestamp_ns: None,
            end_timestamp_ns: None,
            span: None,
            parent: None,
            follows_from: None,
            fields: vec![
                (
                    "message".into(),
                    ValueData::PrefixStrIndex(
                        AtomIndexed::from("long indexed "),
                        AtomIndexed::from("message"),
                    ),
                ),
                (
                    "entity".into(),
                    ValueData::StrIndex(AtomIndexed::from(text)),
                ),
            ],
        };
        assert_eq!(message(&row), text);
        assert_eq!(field_text(&row.fields[0].1), format!("str:{text}"));
        assert!(matches_entity(&row, &format!("str:{text}")));
    }

    #[test]
    fn ring_totals_replace_changed_frames_without_recomputing_stable_frames() {
        let recorder = Arc::new(RecordingLayer::new());
        let _guard =
            tracing::subscriber::set_default(crate::inspection::subscriber(recorder.clone()));
        // Use append-only event fixtures: testing span-storage fixes is outside
        // this example's scope. Cache replacement itself is tested separately.
        let emit = || tracing::info!("cached event");
        for _ in 0..300 {
            emit();
        }
        let mut cache = StatsCache::default();
        cache.refresh(&recorder);
        assert_eq!(
            stats_values(cache.total),
            stats_values(recorder.compression_ratio())
        );
        let initial = cache.completed_recomputations;
        assert!(initial > 0);
        cache.refresh(&recorder);
        assert_eq!(cache.completed_recomputations, initial);
        for _ in 0..300 {
            emit();
        }
        cache.refresh(&recorder);
        assert!(cache.completed_recomputations > initial);
        assert_eq!(
            stats_values(cache.total),
            stats_values(recorder.compression_ratio())
        );
        let after_rollover = cache.completed_recomputations;
        cache.refresh(&recorder);
        assert_eq!(cache.completed_recomputations, after_rollover);
    }

    #[test]
    fn block_duration_average_matches_completed_timestamps_and_is_cached() {
        let recorder = Arc::new(RecordingLayer::new());
        let _guard =
            tracing::subscriber::set_default(crate::inspection::subscriber(recorder.clone()));
        for value in 0..600u64 {
            tracing::info!(value, "block duration fixture");
        }
        let snapshot = recorder.inspect(&Default::default()).remove(0);
        let durations: Vec<_> = snapshot
            .completed_updates
            .iter()
            .filter_map(|frame| frame.duration_ns)
            .collect();
        assert!(!durations.is_empty());
        let expected_sum: u128 = durations.iter().map(|&ns| ns as u128).sum();
        let mut cache = StatsCache::default();
        cache.refresh(&recorder);
        let arch = &cache.archetypes[&snapshot.id];
        assert_eq!(arch.timed_blocks, snapshot.blocks - 1);
        assert_eq!(arch.block_duration_sum_ns, expected_sum);
        assert_eq!(
            arch.average_block_duration_ns(),
            Some(expected_sum as f64 / durations.len() as f64)
        );
        let calculations = cache.completed_recomputations;
        cache.refresh(&recorder);
        assert_eq!(cache.completed_recomputations, calculations);
        assert_eq!(
            cache.archetypes[&snapshot.id].block_duration_sum_ns,
            expected_sum
        );
    }

    #[test]
    fn sizes_round_to_three_significant_figures_and_promote_units() {
        for (bytes, expected) in [
            (0, "0"),
            (1, "1.00"),
            (12, "12.0"),
            (123, "123"),
            (999, "999"),
            (1000, "1.00K"),
            (12345, "12.3K"),
            (999499, "999K"),
            (999500, "1.00M"),
            (1234567, "1.23M"),
            (123456789, "123M"),
        ] {
            assert_eq!(size_text(bytes), expected);
        }
        assert_eq!(significant_text(16.0), "16.0");
        assert_eq!(significant_text(1.23456), "1.23");
        assert_eq!(compression_percent(0, 0), 0.0);
        assert_eq!(storage_percent(Default::default(), Default::default()), 0.0);
    }

    #[test]
    fn sorting_defaults_to_largest_packed_size_and_reverses_on_click() {
        let recorder = Arc::new(RecordingLayer::new());
        let _guard =
            tracing::subscriber::set_default(crate::inspection::subscriber(recorder.clone()));
        for i in 0..100u64 {
            tracing::info!(target: "large", value = i);
        }
        for i in 0..2u64 {
            tracing::info!(target: "small", value = i);
        }
        let mut cache = StatsCache::default();
        cache.refresh(&recorder);
        let mut sort = SortOrder::default();
        let ids = sorted_archetypes(&cache, "", sort);
        assert_eq!(
            cache.archetypes[&ids[0]].snapshot.metadata.target(),
            "large"
        );
        sort.select(SortColumn::Packed);
        let ids = sorted_archetypes(&cache, "", sort);
        assert_eq!(
            cache.archetypes[&ids[0]].snapshot.metadata.target(),
            "small"
        );
        sort.select(SortColumn::Rows);
        assert!(sort.descending);
        let ids = sorted_archetypes(&cache, "large", sort);
        assert_eq!(ids.len(), 1);
        // Share denominator includes hidden archetypes and allocation overhead.
        let shares: f64 = cache
            .archetypes
            .values()
            .map(|a| storage_percent(a.total, cache.total))
            .sum();
        assert!((shares - 100.0).abs() < 1e-9);
    }

    #[test]
    fn field_statistics_use_completed_masks_and_exclude_active_rows() {
        let recorder = Arc::new(RecordingLayer::new());
        let _guard =
            tracing::subscriber::set_default(crate::inspection::subscriber(recorder.clone()));
        for i in 0..1000u64 {
            tracing::info!(
                marker = 57u64,
                value = i,
                absent = tracing::field::Empty,
                message = "repeated payload"
            );
        }
        let snapshots = recorder.inspect(&Default::default());
        let snapshot = &snapshots[0];
        assert!(!snapshot.completed_updates.is_empty());
        assert_eq!(snapshot.blocks, snapshot.completed_updates.len() + 1);
        let completed_rows: usize = snapshot
            .completed_updates
            .iter()
            .map(|block| block.rows)
            .sum();
        assert!(completed_rows < snapshot.rows);
        let mut cache = StatsCache::default();
        cache.refresh(&recorder);
        let arch = &cache.archetypes[&snapshot.id];
        for (index, (_, kind)) in snapshot.fields.iter().enumerate() {
            let field = arch.fields[index];
            if kind.is_none() {
                assert_eq!(field, FieldCompression::default());
                continue;
            }
            let range = snapshot.field_ranges[index].as_ref().unwrap();
            let expected_packed: usize = snapshot
                .completed_updates
                .iter()
                .map(|block| {
                    let varying = block.mask[range.clone()]
                        .iter()
                        .filter(|b| **b != 0)
                        .count();
                    range.len() + (block.rows - 1) * varying
                })
                .sum();
            assert_eq!(field.raw, range.len() * completed_rows);
            assert_eq!(field.packed, expected_packed);
            assert_eq!(field.rows, completed_rows);
        }
        let marker = snapshot
            .fields
            .iter()
            .position(|(name, _)| name == "marker")
            .unwrap();
        assert_eq!(snapshot.field_ranges[marker], Some(8..16));
        assert_eq!(
            arch.fields[marker].packed,
            8 * snapshot.completed_updates.len()
        );
        assert!(compression_percent(arch.fields[marker].raw, arch.fields[marker].packed) > 98.0);
        let previous = arch.fields.clone();
        cache.refresh(&recorder);
        assert_eq!(cache.archetypes[&snapshot.id].fields, previous);
    }

    #[test]
    fn revised_field_masks_replace_instead_of_accumulating_old_contributions() {
        // One previously constant byte becomes variable in a four-row block.
        let old = field_contribution(Some(&(0..8)), 4, &[0; 8]);
        let mut mask = [0; 8];
        mask[0] = 0xff;
        let next = field_contribution(Some(&(0..8)), 4, &mask);
        let mut total = [FieldCompression {
            raw: old.raw * 2,
            packed: old.packed * 2,
            constant_bytes: old.constant_bytes * 2,
            blocks: 2,
            rows: 8,
        }];
        replace_fields(&mut total, &[old], &[next]);
        assert_eq!(total[0].raw, 64);
        assert_eq!(total[0].packed, 19);
        assert_eq!(total[0].constant_bytes, 15);
        assert_eq!(total[0].blocks, 2);
    }

    #[test]
    fn filters_page_history_and_preserve_entity_types_and_messages() {
        let recorder = Arc::new(RecordingLayer::new());
        let _guard =
            tracing::subscriber::set_default(crate::inspection::subscriber(recorder.clone()));
        for i in 0..600u64 {
            tracing::info!(entity = i % 600, message = "matching message suffix");
        }
        tracing::info!(entity = "123", message = "debug entity");
        tracing::info!(message = "no entity");
        let mut cache = StatsCache::default();
        cache.refresh(&recorder);
        let numeric = cache
            .archetypes
            .values()
            .find(|a| a.snapshot.rows == 600)
            .unwrap();
        assert!(matches_archetype(numeric, "entity"));
        assert!(!matches_archetype(numeric, "nonexistent target"));
        let rows = matching_rows(&recorder, numeric.snapshot.id, "123", 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(message(&rows[0]), "matching message suffix");
        assert!(!matches_entity(&rows[0], "str:123"));
        let mut choices = vec![rows[0].clone(), rows[0].clone()];
        let all = matching_rows(&recorder, numeric.snapshot.id, "", 3);
        assert_eq!(
            all.iter().map(|r| r.index).collect::<Vec<_>>(),
            [600, 599, 598]
        );
        for arch in cache.archetypes.values().filter(|a| a.snapshot.rows == 1) {
            assert_eq!(arch.snapshot.kind, RecordKind::Event);
            let row = recorder.rows(arch.snapshot.id, None, 1).remove(0);
            assert!(!matches_entity(&row, "123"));
            assert!(matches_entity(&row, ""));
            if message(&row) == "debug entity" {
                assert!(matches_entity(&row, "str:123"));
                choices.push(row);
            }
        }
        assert_eq!(entity_choices(&choices), ["123", "str:123"]);
        // Cache existing packed lengths without library-side mutation tracking.
        assert!(!cache.frame_sizes.is_empty());
    }
}
