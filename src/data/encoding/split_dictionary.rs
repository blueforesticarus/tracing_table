//! Lossless high-word dictionaries plus exact low bits in one physical u64.
//! Fitting mirrors util/timestamp_codec_probe.rs: local quantile trends, bucket
//! phases, entry prediction, key stride prediction, and monotone key runs.
//! Tagged timestamp records retain the absolute block origin independently of
//! the numeric origin used to center parent-relative values.
use super::common::*;
use super::{Context, Encoding, Footer, Layout, OriginEncoding};
use crate::data::util::float_u16::FloatU16;
use smallvec::SmallVec;

pub const TAG: u8 = 10;
pub const ABSOLUTE_TAG: u8 = 3;
pub const RELATIVE_TAG: u8 = 4;
// Tuning: keep the same search coverage as the offline experiments. This switch
// permits controlled A/B replays without changing other encodings or schemas.
pub const ENABLED: bool = true;
pub const MAX_LOW_BITS: usize = 32;
pub const MAX_PHASES: usize = 4;
/// Read once per process. Disabling fitting never disables decoding existing
/// records, which makes paired capture audits safe and reproducible.
pub fn enabled() -> bool {
    static ENABLED_BY_ENV: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        !std::env::var("SPLIT_DICTIONARY")
            .is_ok_and(|v| matches!(v.as_str(), "0" | "off" | "false"))
    });
    ENABLED && *ENABLED_BY_ENV
}
const RAW: u8 = 0;
const LOCAL: u8 = 1;
const LEGACY: u8 = 2;
const MINIMUM: u8 = 3;
const KEY_RAW: u8 = 0;
const KEY_STRIDE: u8 = 1;
const KEY_RUNS: u8 = 2;
const ENTRY_RAW: u8 = 0;
const ENTRY_SIGNED: u8 = 1;
const ENTRY_LINEAR: u8 = 2;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Scope {
    Values,
    Absolute(u64),
    Relative(u64),
}

#[derive(Clone, Copy, Debug)]
pub struct SplitDictionaryEncoding<'a> {
    scope: Scope,
    numeric_origin: u64,
    source: u8,
    low: usize,
    count: usize,
    entry_bits: usize,
    source_stride: u64,
    key_mode: u8,
    key_stride: u64,
    position_bits: usize,
    positions: &'a [u8],
    entry_mode: u8,
    entry_stride: u64,
    entries: &'a [u8],
    payload_bytes: usize,
}

fn bits(value: u64) -> usize {
    (64 - value.leading_zeros()) as usize
}
fn zz(value: i64) -> u64 {
    ((value as u64) << 1) ^ ((value >> 63) as u64)
}
fn unzz(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}
fn push_unique<T: PartialEq>(values: &mut SmallVec<[T; 8]>, value: T) {
    if !values.contains(&value) {
        values.push(value);
    }
}
fn float(value: u64) -> u64 {
    FloatU16::from_u64_lossy(value).to_u64()
}
fn coefficient(record: &[u8], cursor: &mut usize) -> u64 {
    let value = FloatU16::from_bits(u16::from_le_bytes(
        record[*cursor..*cursor + 2].try_into().unwrap(),
    ))
    .to_u64();
    *cursor += 2;
    value
}
fn append_coefficient(out: &mut Footer, value: u64) {
    out.extend_from_slice(&FloatU16::from_u64_lossy(value).to_bits().to_le_bytes());
}

impl<'a> SplitDictionaryEncoding<'a> {
    pub fn is_tagged(record: &[u8]) -> bool {
        record.len() >= 8 && matches!(record[7] >> 4, ABSOLUTE_TAG | RELATIVE_TAG)
    }
    pub fn origin(self) -> u64 {
        match self.scope {
            Scope::Absolute(v) | Scope::Relative(v) => v,
            Scope::Values => self.numeric_origin,
        }
    }
    pub fn is_relative(self) -> bool {
        matches!(self.scope, Scope::Relative(_))
    }
    pub fn stride_float(self) -> FloatU16 {
        FloatU16::from_u64_lossy(self.source_stride)
    }
    pub fn payload_bytes(self) -> usize {
        self.payload_bytes
    }
    pub fn low_bits(self) -> usize {
        self.low
    }
    pub fn entry_count(self) -> usize {
        self.count
    }
    pub fn uses_runs(self) -> bool {
        self.key_mode == KEY_RUNS
    }

    fn entry(self, id: usize) -> i64 {
        let value = unpack_u64(self.entries, id, self.entry_bits);
        match self.entry_mode {
            ENTRY_RAW => value as i64,
            ENTRY_SIGNED => unzz(value),
            ENTRY_LINEAR => unzz(value)
                .checked_add(advance(self.entry_stride, id, self.count) as i64)
                .expect("split entry overflow"),
            _ => unreachable!(),
        }
    }
    fn run(self, row: usize) -> usize {
        // Entry zero begins at row zero; subsequent boundaries are packed once.
        let (mut low, mut high) = (0, self.count - 1);
        while low < high {
            let middle = (low + high) / 2;
            if unpack_u64(self.positions, middle, self.position_bits) as usize <= row {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }
    fn phase(self, context: Context) -> u64 {
        if self.source == MINIMUM {
            0
        } else {
            context.first() as u64 & ((1u64 << self.low) - 1)
        }
    }
    fn transform(self, value: u64, row: usize, rows: usize) -> Option<i64> {
        let centered = i128::from(value) - i128::from(self.numeric_origin);
        let prediction = i128::from(advance(self.source_stride, row, rows));
        let value = match self.source {
            RAW | MINIMUM => centered,
            LOCAL => centered - prediction,
            LEGACY => {
                if self.source_stride == 0 {
                    centered
                } else {
                    i128::from(zz(i64::try_from(centered - prediction).ok()?))
                }
            }
            _ => unreachable!(),
        };
        i64::try_from(value).ok()
    }

    /// Select a complete flat record against an already fitted baseline. Search
    /// uses strict improvements; apply the eight-byte threshold once at the end,
    /// rather than losing small intermediate improvements to greedy hysteresis.
    pub(crate) fn fit(
        best: &mut Candidate,
        values: &[u128],
        cost: &Cost<'_>,
        scope: Scope,
        legacy_stride: u64,
    ) {
        if !enabled() || cost.width != 8 || values.len() < 2 {
            return;
        }
        let Some(original) = values
            .iter()
            .map(|&v| u64::try_from(v).ok())
            .collect::<Option<SmallVec<[u64; 256]>>>()
        else {
            return;
        };
        let first = original[0];
        let Some(_centered) = original
            .iter()
            .map(|&v| i64::try_from(i128::from(v) - i128::from(first)).ok())
            .collect::<Option<SmallVec<[i64; 256]>>>()
        else {
            return;
        };
        let n = values.len();
        let Some(gaps): Option<SmallVec<[i64; 256]>> = original
            .windows(2)
            .map(|w| i64::try_from(i128::from(w[1]) - i128::from(w[0])).ok())
            .collect()
        else {
            return;
        };
        let mut sorted = gaps.clone();
        sorted.sort_unstable();
        let mut trends = SmallVec::<[u64; 8]>::new();
        trends.push(0);
        for q in [10, 25, 50, 75] {
            // The offline probe rounds this positive quantile product before
            // FloatU16 quantization. Integer products are already exact here.
            let value = sorted[(sorted.len() - 1) * q / 100].max(0) as u64;
            push_unique(&mut trends, float(value.saturating_mul(n as u64)));
        }
        let mut largest: SmallVec<[usize; 256]> = (1..n).collect();
        largest.sort_unstable_by_key(|&i| std::cmp::Reverse(gaps[i - 1]));
        let baseline = best.size;
        let mut chosen: Option<(usize, Footer, SmallVec<[u128; 256]>, u128)> = None;
        // Minimum mode supplies the ordinary minimum-first dictionary fallback
        // from the parent-offset comparison, without any zero-sentinel policy.
        for choice in 0..trends.len() + 2 {
            let (source, stride, numeric_origin) = if choice < trends.len() {
                (
                    if trends[choice] == 0 { RAW } else { LOCAL },
                    trends[choice],
                    first,
                )
            } else if choice == trends.len() {
                if legacy_stride == 0 {
                    continue;
                }
                (LEGACY, legacy_stride, first)
            } else {
                if matches!(scope, Scope::Absolute(_)) {
                    continue;
                }
                (MINIMUM, 0, *original.iter().min().unwrap())
            };
            let source_values: Option<SmallVec<[i64; 256]>> = original
                .iter()
                .enumerate()
                .map(|(r, &v)| {
                    let d = i128::from(v) - i128::from(numeric_origin);
                    let p = i128::from(advance(stride, r, n));
                    match source {
                        RAW | MINIMUM => i64::try_from(d).ok(),
                        LOCAL => i64::try_from(d - p).ok(),
                        LEGACY => i64::try_from(d - p)
                            .ok()
                            .and_then(|d| i64::try_from(zz(d)).ok()),
                        _ => unreachable!(),
                    }
                })
                .collect();
            let Some(source_values) = source_values else {
                continue;
            };
            let minimum = *source_values.iter().min().unwrap();
            let maximum = *source_values.iter().max().unwrap();
            let prefix = match scope {
                Scope::Values => 9,
                Scope::Absolute(_) => 8,
                Scope::Relative(_) => 16,
            };
            let fixed_bytes = prefix + 4 + usize::from(matches!(source, LOCAL | LEGACY)) * 2;
            for low in 1..=MAX_LOW_BITS {
                let low_mask = (1u64 << low) - 1;
                let mut phases = SmallVec::<[u64; 8]>::new();
                phases.push(0);
                if source != MINIMUM {
                    push_unique(&mut phases, 1u64 << (low - 1));
                    for &i in largest.iter().take(MAX_PHASES.saturating_sub(2)) {
                        push_unique(
                            &mut phases,
                            source_values[i].wrapping_neg() as u64 & low_mask,
                        );
                    }
                }
                for phase in phases {
                    let (Some(minimum), Some(maximum)) = (
                        minimum.checked_add(phase as i64),
                        maximum.checked_add(phase as i64),
                    ) else {
                        continue;
                    };
                    if minimum >> low == maximum >> low {
                        continue;
                    }
                    // Exact lower bounds preserve the complete probe search.
                    // A candidate cannot win even with free keys/table when its
                    // retained fine mask already costs too much.
                    let first_fine = (source_values[0] + phase as i64) as u64 & low_mask;
                    let fine_mask = source_values.iter().fold(0, |mask, &v| {
                        mask | (((v + phase as i64) as u64 & low_mask) ^ first_fine)
                    });
                    if cost.size(varying_bits(u128::from(fine_mask), 8), fixed_bytes)
                        >= chosen.as_ref().map_or(baseline, |v| v.0)
                    {
                        continue;
                    }
                    let mut entries = SmallVec::<[i64; 64]>::new();
                    let mut ids = SmallVec::<[u64; 256]>::new();
                    let mut fine = SmallVec::<[u64; 256]>::new();
                    let mut previous = None;
                    for &v in &source_values {
                        let Some(v) = v.checked_add(phase as i64) else {
                            break;
                        };
                        let high = v >> low;
                        let id = if let Some((_, id)) = previous.filter(|(h, _)| *h == high) {
                            id
                        } else if entries.last() == Some(&high) {
                            entries.len() - 1
                        } else if let Some(id) = entries.iter().position(|&e| e == high) {
                            id
                        } else {
                            if entries.len() == MAX_DICTIONARY_VALUES {
                                break;
                            }
                            entries.push(high);
                            entries.len() - 1
                        };
                        previous = Some((high, id));
                        let value = v as u64 & low_mask;
                        fine.push(value);
                        ids.push(id as u64);
                    }
                    if ids.len() != n || entries.len() < 2 {
                        continue;
                    }
                    let entry_plan = EntryPlan::fit(&entries);
                    if cost.size(
                        varying_bits(u128::from(fine_mask), 8),
                        fixed_bytes + entry_plan.bytes,
                    ) >= chosen.as_ref().map_or(baseline, |v| v.0)
                    {
                        continue;
                    }
                    let mut modes = SmallVec::<[(u8, u64); 10]>::new();
                    modes.push((KEY_RAW, 0));
                    let max = *ids.iter().max().unwrap();
                    let endpoint = ids.iter().rposition(|&id| id == max).unwrap();
                    let center = max * n as u64 / endpoint as u64;
                    let mut key_slopes = SmallVec::<[u64; 8]>::new();
                    for s in [
                        center.saturating_sub(2),
                        center.saturating_sub(1),
                        center,
                        center + 1,
                        center + 2,
                        max,
                        max + 1,
                    ] {
                        let s = float(s);
                        if s > 0 {
                            push_unique(&mut key_slopes, s);
                        }
                    }
                    modes.extend(key_slopes.iter().map(|&s| (KEY_STRIDE, s)));
                    let mut positions = SmallVec::<[u64; 64]>::new();
                    let mut ordinal = 0;
                    let mut valid_runs = true;
                    for r in 1..n {
                        if ids[r] != ids[r - 1] {
                            ordinal += 1;
                            if ids[r] != ordinal {
                                valid_runs = false;
                                break;
                            }
                            positions.push(r as u64);
                        }
                    }
                    if valid_runs {
                        modes.push((KEY_RUNS, 0));
                    }
                    for (key_mode, key_stride) in modes {
                        let key_bytes = match key_mode {
                            KEY_RAW => 0,
                            KEY_STRIDE => 2,
                            _ => 2 + (positions.len() * bits((n - 1) as u64)).div_ceil(8),
                        };
                        let record_bytes = fixed_bytes + key_bytes + entry_plan.bytes;
                        if record_bytes > MAX_RECORD_BYTES {
                            continue;
                        }
                        let mut key_mask = 0;
                        let mut impossible = false;
                        if key_mode != KEY_RUNS {
                            for (r, &id) in ids.iter().enumerate() {
                                let previous_mask = key_mask;
                                key_mask |= if key_mode == KEY_RAW {
                                    id
                                } else {
                                    zz(id as i64 - advance(key_stride, r, n) as i64)
                                };
                                if key_mask != previous_mask
                                    && cost.size(
                                        varying_bits(u128::from(fine_mask | (key_mask << low)), 8),
                                        record_bytes,
                                    ) >= chosen.as_ref().map_or(baseline, |v| v.0)
                                {
                                    impossible = true;
                                    break;
                                }
                            }
                        }
                        if impossible || low + bits(key_mask) > 64 {
                            continue;
                        }
                        let row_mask = u128::from(fine_mask | (key_mask << low));
                        let size = cost.size(varying_bits(row_mask, 8), record_bytes);
                        if size >= chosen.as_ref().map_or(baseline, |v| v.0) {
                            continue;
                        }
                        let mut record = Footer::new();
                        match scope {
                            Scope::Values => {
                                record.push(TAG);
                                record.extend_from_slice(&numeric_origin.to_le_bytes());
                            }
                            Scope::Absolute(origin) | Scope::Relative(origin) => {
                                let tag = if matches!(scope, Scope::Relative(_)) {
                                    RELATIVE_TAG
                                } else {
                                    ABSOLUTE_TAG
                                };
                                record.extend_from_slice(
                                    &(origin | (u64::from(tag) << 60)).to_le_bytes(),
                                );
                                if matches!(scope, Scope::Relative(_)) {
                                    record.extend_from_slice(&numeric_origin.to_le_bytes());
                                }
                            }
                        }
                        record.extend_from_slice(&[
                            source | (key_mode << 2) | (entry_plan.mode << 4),
                            low as u8,
                            entries.len() as u8,
                            entry_plan.bits as u8,
                        ]);
                        if matches!(source, LOCAL | LEGACY) {
                            append_coefficient(&mut record, stride);
                        }
                        match key_mode {
                            KEY_STRIDE => append_coefficient(&mut record, key_stride),
                            KEY_RUNS => {
                                let width = bits((n - 1) as u64);
                                record.extend_from_slice(&[entries.len() as u8, width as u8]);
                                append_packed(
                                    &mut record,
                                    positions.iter().map(|&v| u128::from(v)),
                                    positions.len(),
                                    width,
                                );
                            }
                            _ => {}
                        }
                        entry_plan.write(&entries, &mut record);
                        debug_assert_eq!(record.len(), record_bytes);
                        let encoded = fine
                            .iter()
                            .zip(&ids)
                            .enumerate()
                            .map(|(r, (&fine, &id))| {
                                let key = match key_mode {
                                    KEY_RAW => id,
                                    KEY_STRIDE => zz(id as i64 - advance(key_stride, r, n) as i64),
                                    _ => 0,
                                };
                                u128::from(fine | (key << low))
                            })
                            .collect();
                        chosen = Some((size, record, encoded, row_mask));
                    }
                }
            }
        }
        if let Some((size, record, encoded, mask)) = chosen {
            if best.worthwhile(size, cost) {
                best.size = size;
                best.record = record;
                best.encoded = encoded;
                best.mask = mask;
            }
        }
    }
}

struct EntryPlan {
    mode: u8,
    bits: usize,
    stride: u64,
    bytes: usize,
}
impl EntryPlan {
    fn fit(entries: &[i64]) -> Self {
        let signed = entries.iter().any(|&v| v < 0);
        let width = bits(
            entries
                .iter()
                .map(|&v| if signed { zz(v) } else { v as u64 })
                .max()
                .unwrap(),
        );
        let mut best = Self {
            mode: if signed { ENTRY_SIGNED } else { ENTRY_RAW },
            bits: width,
            stride: 0,
            bytes: (entries.len() * width).div_ceil(8),
        };
        if entries[entries.len() - 1] > 0 {
            // Match the probe's rounded endpoint before lossy FloatU16 storage.
            let value = (entries[entries.len() - 1] as f64 * entries.len() as f64
                / (entries.len() - 1) as f64)
                .round() as u64;
            let stride = float(value);
            let width = bits(
                entries
                    .iter()
                    .enumerate()
                    .map(|(i, &v)| zz(v.wrapping_sub(advance(stride, i, entries.len()) as i64)))
                    .max()
                    .unwrap(),
            );
            let bytes = 3 + (entries.len() * width).div_ceil(8);
            if bytes < best.bytes {
                best = Self {
                    mode: ENTRY_LINEAR,
                    bits: width,
                    stride,
                    bytes,
                };
            }
        }
        best
    }
    fn write(&self, entries: &[i64], record: &mut Footer) {
        if self.mode == ENTRY_LINEAR {
            record.push(1);
            append_coefficient(record, self.stride);
        }
        append_packed(
            record,
            entries.iter().enumerate().map(|(i, &v)| {
                u128::from(match self.mode {
                    ENTRY_RAW => v as u64,
                    ENTRY_SIGNED => zz(v),
                    _ => zz(v.wrapping_sub(advance(self.stride, i, entries.len()) as i64)),
                })
            }),
            entries.len(),
            self.bits,
        );
    }
}

impl<'a> Encoding<'a> for SplitDictionaryEncoding<'a> {
    const WIDTH: Option<usize> = Some(8);
    fn parse(record: &'a [u8], layout: Layout, rows: usize) -> Self {
        assert_eq!(layout.width, 8);
        let (scope, numeric_origin, mut cursor) = if layout.is_tagged_origin() {
            let word = u64::from_le_bytes(record[..8].try_into().unwrap());
            let origin = word & OriginEncoding::MAX_ORIGIN;
            match (word >> 60) as u8 {
                ABSOLUTE_TAG => (Scope::Absolute(origin), origin, 8),
                RELATIVE_TAG => (
                    Scope::Relative(origin),
                    u64::from_le_bytes(record[8..16].try_into().unwrap()),
                    16,
                ),
                _ => panic!("invalid split timestamp tag"),
            }
        } else {
            assert_eq!(record[0], TAG);
            (
                Scope::Values,
                u64::from_le_bytes(record[1..9].try_into().unwrap()),
                9,
            )
        };
        let flags = record[cursor];
        let source = flags & 3;
        let key_mode = (flags >> 2) & 3;
        let entry_mode = (flags >> 4) & 3;
        let low = usize::from(record[cursor + 1]);
        let count = usize::from(record[cursor + 2]);
        let entry_bits = usize::from(record[cursor + 3]);
        assert!(flags & 192 == 0 && key_mode <= KEY_RUNS && entry_mode <= ENTRY_LINEAR);
        assert!(
            (1..=MAX_LOW_BITS).contains(&low)
                && (2..=MAX_DICTIONARY_VALUES).contains(&count)
                && entry_bits <= 64
        );
        cursor += 4;
        let source_stride = if matches!(source, LOCAL | LEGACY) {
            coefficient(record, &mut cursor)
        } else {
            0
        };
        let key_stride = if key_mode == KEY_STRIDE {
            coefficient(record, &mut cursor)
        } else {
            0
        };
        let mut position_bits = 0;
        let mut positions = &record[0..0];
        if key_mode == KEY_RUNS {
            assert_eq!(usize::from(record[cursor]), count);
            position_bits = usize::from(record[cursor + 1]);
            cursor += 2;
            assert_eq!(position_bits, bits((rows - 1) as u64));
            let end = cursor + ((count - 1) * position_bits).div_ceil(8);
            positions = &record[cursor..end];
            cursor = end;
        }
        let entry_stride = if entry_mode == ENTRY_LINEAR {
            assert_eq!(record[cursor], 1);
            cursor += 1;
            coefficient(record, &mut cursor)
        } else {
            0
        };
        let end = cursor + (count * entry_bits).div_ceil(8);
        let entries = &record[cursor..end];
        check_record_size(record, end);
        Self {
            scope,
            numeric_origin,
            source,
            low,
            count,
            entry_bits,
            source_stride,
            key_mode,
            key_stride,
            position_bits,
            positions,
            entry_mode,
            entry_stride,
            entries,
            payload_bytes: end,
        }
    }
    fn write_footer(&self, width: usize, output: &mut Footer) {
        assert_eq!(width, 8);
        match self.scope {
            Scope::Values => {
                output.push(TAG);
                output.extend_from_slice(&self.numeric_origin.to_le_bytes());
            }
            Scope::Absolute(origin) | Scope::Relative(origin) => {
                let tag = if self.is_relative() {
                    RELATIVE_TAG
                } else {
                    ABSOLUTE_TAG
                };
                output.extend_from_slice(&(origin | (u64::from(tag) << 60)).to_le_bytes());
                if self.is_relative() {
                    output.extend_from_slice(&self.numeric_origin.to_le_bytes());
                }
            }
        }
        output.extend_from_slice(&[
            self.source | (self.key_mode << 2) | (self.entry_mode << 4),
            self.low as u8,
            self.count as u8,
            self.entry_bits as u8,
        ]);
        if matches!(self.source, LOCAL | LEGACY) {
            append_coefficient(output, self.source_stride);
        }
        match self.key_mode {
            KEY_STRIDE => append_coefficient(output, self.key_stride),
            KEY_RUNS => {
                output.extend_from_slice(&[self.count as u8, self.position_bits as u8]);
                output.extend_from_slice(self.positions);
            }
            _ => {}
        }
        if self.entry_mode == ENTRY_LINEAR {
            output.push(1);
            append_coefficient(output, self.entry_stride);
        }
        output.extend_from_slice(self.entries);
    }
    #[inline]
    fn decode(&self, encoded: u128, row: usize, context: Context) -> u128 {
        let encoded = u64::try_from(encoded).expect("split word exceeds u64");
        let key = encoded >> self.low;
        let id = match self.key_mode {
            KEY_RAW => key as usize,
            KEY_STRIDE => {
                (unzz(key) + advance(self.key_stride, row, context.rows()) as i64) as usize
            }
            KEY_RUNS => self.run(row),
            _ => unreachable!(),
        };
        assert!(id < self.count, "split key exceeds dictionary");
        let value = (i128::from(self.entry(id)) << self.low)
            | i128::from(encoded & ((1u64 << self.low) - 1));
        let value = value - i128::from(self.phase(context));
        let relative = if self.source == LEGACY && self.source_stride != 0 {
            i128::from(unzz(
                u64::try_from(value).expect("negative split ZigZag word"),
            )) + i128::from(advance(self.source_stride, row, context.rows()))
        } else {
            value + i128::from(advance(self.source_stride, row, context.rows()))
        };
        u128::from(
            u64::try_from(i128::from(self.numeric_origin) + relative)
                .expect("split value exceeds u64"),
        )
    }
    fn encode(&self, value: u128, row: usize, context: Context) -> Option<u128> {
        let transformed = self
            .transform(u64::try_from(value).ok()?, row, context.rows())?
            .checked_add(self.phase(context) as i64)?;
        let high = transformed >> self.low;
        let id = (0..self.count).find(|&id| self.entry(id) == high)?;
        let key = match self.key_mode {
            KEY_RAW => id as u64,
            KEY_STRIDE => zz(id as i64 - advance(self.key_stride, row, context.rows()) as i64),
            KEY_RUNS => {
                if self.run(row) != id {
                    return None;
                }
                0
            }
            _ => unreachable!(),
        };
        if bits(key) + self.low > 64 {
            return None;
        }
        let encoded = (transformed as u64 & ((1u64 << self.low) - 1)) | (key << self.low);
        // The first physical word owns the phase; changing it requires refitting.
        if row == 0 && u128::from(encoded) != context.first() {
            return None;
        }
        Some(u128::from(encoded))
    }
}

#[cfg(test)]
mod tests;
