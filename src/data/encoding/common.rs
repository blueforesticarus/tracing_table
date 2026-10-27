//! Codec-independent arithmetic, packed footer arrays, and fitting economics.
//! Keep this module independent of concrete encodings so families stay decoupled.
use crate::data::{dataframe::Frame, util::float_u16::FloatU16};
use smallvec::SmallVec;

/// Tuning bounds fitting work and dictionary metadata; neither is a wire capacity.
pub const MAX_DICTIONARY_VALUES: usize = 64;
pub const MAX_CYCLE: u8 = 255;
// Tuning: favor predictable fitting work and simpler encodings over tiny savings.
pub const MAX_CYCLE_CANDIDATES: usize = 4;
pub const MIN_ENCODING_SAVINGS: usize = 8;
/// Directory bytes count two-byte units; zero still means no encoding.
pub const MAX_RECORD_BYTES: usize = u8::MAX as usize * 2;
pub(crate) const CYCLE_ANCHORS: usize = 8;

/// Tuning: minimize actual packed unsigned mask cost with a bounded search.
/// Disable both search flags to restore the original endpoint-only fit.
pub const UNSIGNED_MASK_STRIDE: bool = true;
/// Maximum mask-scoring passes per block (at least one for the endpoint baseline).
pub const UNSIGNED_MASK_CANDIDATES: usize = 64;
/// Seed mask search with minimax; also selects minimax when mask search is off.
/// Computing this seed adds about 18 scans, only when archiving.
pub const UNSIGNED_MINIMAX_STRIDE: bool = true;

pub(crate) fn record_storage_bytes(bytes: usize) -> usize {
    bytes.div_ceil(2) * 2
}

/// Standalone fitting records may be unpadded; archived records have at most
/// one trailing zero byte. Never interpret that byte as codec data.
pub(crate) fn check_record_size(record: &[u8], payload: usize) {
    assert!(
        record.len() == payload
            || (record.len() == record_storage_bytes(payload)
                && record[payload..].iter().all(|&byte| byte == 0)),
        "column encoding size/padding mismatch"
    );
}

#[inline]
pub(crate) fn word_mask(width: usize) -> u128 {
    u128::MAX >> (128 - width * 8)
}

/// Modular ZigZag is shared by native-width fields and fixed structural columns.
#[inline]
pub(crate) fn residual(value: u128, prediction: u128, width: usize) -> u128 {
    let delta = value.wrapping_sub(prediction) & word_mask(width);
    ((delta << 1) ^ 0u128.wrapping_sub(delta >> (width * 8 - 1))) & word_mask(width)
}
#[inline]
pub(crate) fn restore(encoded: u128, prediction: u128, width: usize) -> u128 {
    prediction.wrapping_add((encoded >> 1) ^ 0u128.wrapping_sub(encoded & 1)) & word_mask(width)
}

// Native-width counterparts keep the u64-only codecs off u128 arithmetic in
// row encode/decode loops. Dictionary codecs still support full u128 values.
#[inline]
pub(crate) fn word_mask_u64(width: usize) -> u64 {
    u64::MAX >> (64 - width * 8)
}
#[inline]
pub(crate) fn residual_u64(value: u64, prediction: u64, width: usize) -> u64 {
    let delta = value.wrapping_sub(prediction) & word_mask_u64(width);
    ((delta << 1) ^ 0u64.wrapping_sub(delta >> (width * 8 - 1))) & word_mask_u64(width)
}
#[inline]
pub(crate) fn restore_u64(encoded: u64, prediction: u64, width: usize) -> u64 {
    prediction.wrapping_add((encoded >> 1) ^ 0u64.wrapping_sub(encoded & 1)) & word_mask_u64(width)
}

// Exception patches and positions need at most 64 bits. Keep their accumulator
// native-sized; the general unpacker below remains available for dictionaries.
pub(crate) fn unpack_u64(entries: &[u8], id: usize, bits: usize) -> u64 {
    let mut value = 0;
    let mut done = 0;
    while done < bits {
        let position = id * bits + done;
        let count = (8 - position % 8).min(bits - done);
        value |=
            u64::from((entries[position / 8] >> (position % 8)) & (u8::MAX >> (8 - count))) << done;
        done += count;
    }
    value
}

/// Preserve the existing timestamp/parent rational rounding, without float math.
#[inline]
pub(crate) fn advance(stride: u64, row: usize, length: usize) -> u64 {
    if stride == 0 {
        return 0;
    }
    assert!(row < length);
    // Most fitted lines fit the native numerator. Keep exact u128 arithmetic
    // as the overflow fallback, while avoiding a software division per row.
    if let Some(numerator) = stride
        .checked_mul(row as u64)
        .and_then(|rise| rise.checked_add(length as u64 / 2))
    {
        numerator / length as u64
    } else {
        ((u128::from(stride) * row as u128 + length as u128 / 2) / length as u128) as u64
    }
}

/// Endpoint fitting is shared; only parents quantize their origin as well.
pub(crate) fn fit_line(
    first: usize,
    start: u128,
    last: usize,
    end: u128,
    length: usize,
    quantized_origin: bool,
) -> (u128, FloatU16) {
    let quantize = |value: u128| {
        if quantized_origin {
            u128::from(FloatU16::from_u64_lossy(value.min(u64::MAX as u128) as u64).to_u64())
        } else {
            value
        }
    };
    if first == last || end <= start {
        return (quantize(start), FloatU16::default());
    }
    let run = (last - first) as u128;
    let rise = end - start;
    let origin = start.saturating_mul(run);
    let projected = rise.saturating_mul(first as u128);
    let (offset, stride) = if origin < projected {
        (0, end.saturating_mul(length as u128) / last as u128)
    } else {
        (
            quantize((origin - projected) / run),
            rise.saturating_mul(length as u128) / run,
        )
    };
    (
        offset,
        FloatU16::from_u64_lossy(stride.min(u64::MAX as u128) as u64),
    )
}

// Only the selected form is serialized. Row mode is retained for experimental
// string-index behavior; general fields use the two-byte custom-float stride.
pub(crate) const OFFSET: u8 = 1;
pub(crate) const LINEAR: u8 = 2;
pub(crate) const ROW: u8 = 3;
pub(crate) const CYCLE: u8 = 4;
pub(crate) const ROW_CYCLE: u8 = 5;
pub(crate) const DICTIONARY: u8 = 6;
pub(crate) const DICTIONARY_CYCLE: u8 = 7;
// Predict slowly advancing dictionary keys without wrapping at a cycle boundary.
pub(crate) const DICTIONARY_STRIDE: u8 = 8;

pub(crate) fn varying_bits(value: u128, width: usize) -> usize {
    if cfg!(feature = "bit-compression") {
        value.count_ones() as usize
    } else {
        value.to_le_bytes()[..width]
            .iter()
            .filter(|&&v| v != 0)
            .count()
            * 8
    }
}
pub(crate) fn frame_bits(frame: &Frame) -> usize {
    frame
        .mask()
        .iter()
        .map(|&byte| varying_bits(u128::from(byte), 1))
        .sum()
}
pub(crate) fn payload_size(frame: &Frame, bits: usize) -> usize {
    let stride =
        if cfg!(feature = "bit-compression") && crate::data::dataframe::CONTINUOUS_BIT_PACKING {
            bits
        } else {
            bits.div_ceil(8) * 8
        };
    2 * frame.unit + ((frame.length - 1) * stride).div_ceil(8)
}

/// Frame packing is the baseline: it omits any constant bit, not just leading zeros.
pub(crate) struct Cost<'a> {
    pub(crate) frame: &'a Frame,
    pub(crate) total_bits: usize,
    pub(crate) original_bits: usize,
    pub(crate) footer: usize,
    pub(crate) width: usize,
}
impl Cost<'_> {
    pub(crate) fn size(&self, bits: usize, metadata: usize) -> usize {
        (payload_size(self.frame, self.total_bits - self.original_bits + bits)
            + self.footer
            + record_storage_bytes(metadata))
        .div_ceil(2)
            * 2
    }
}

pub(crate) struct Candidate {
    pub(crate) record: SmallVec<[u8; 24]>,
    pub(crate) encoded: SmallVec<[u128; 256]>,
    pub(crate) mask: u128,
    pub(crate) size: usize,
}
impl Candidate {
    // Scale the threshold by this column's current first/mask slots, packed
    // values, directory byte, and codec record. Unrelated columns and shared
    // footer bytes must not make a useful encoding harder to select.
    pub(crate) fn worthwhile(&self, size: usize, cost: &Cost<'_>) -> bool {
        let column_bytes = 2 * cost.width
            + ((cost.frame.length - 1) * varying_bits(self.mask, cost.width)).div_ceil(8)
            + 1
            + record_storage_bytes(self.record.len());
        // Compare actual frame allocations to retain packing/alignment effects;
        // only the relative savings threshold is column-local.
        size.saturating_add(MIN_ENCODING_SAVINGS.max(column_bytes / 64)) <= self.size
    }

    /// Only shortlisted predictors reach this pass. Residual masks cannot be
    /// inferred from range alone because compaction omits arbitrary constant bits.
    pub(crate) fn consider(
        &mut self,
        record: &[u8],
        values: &[u128],
        cost: &Cost<'_>,
        encode: impl Fn(u128, usize) -> u128,
    ) {
        if record.len() > MAX_RECORD_BYTES || !self.worthwhile(cost.size(0, record.len()), cost) {
            return;
        }
        let first = encode(values[0], 0);
        let mut mask = 0;
        for (row, &value) in values.iter().enumerate().skip(1) {
            let previous = mask;
            mask |= encode(value, row) ^ first;
            if mask != previous
                && !self.worthwhile(
                    cost.size(varying_bits(mask, cost.width), record.len()),
                    cost,
                )
            {
                return;
            }
        }
        self.size = cost.size(varying_bits(mask, cost.width), record.len());
        self.mask = mask;
        self.record = SmallVec::from_slice(record);
        self.encoded.clear();
        self.encoded.extend(
            values
                .iter()
                .enumerate()
                .map(|(row, &value)| encode(value, row)),
        );
    }
}

/// Discover likely periods from repeated anchor values, rather than trying every
/// period. Eight anchors bound comparisons; vote counts rank at most four fits.
/// Exact and approximate codecs remain lossless even if this heuristic misses a cycle.
pub(crate) fn cycle_periods(values: &[u128]) -> SmallVec<[u8; MAX_CYCLE_CANDIDATES]> {
    let mut anchors = SmallVec::<[(u128, usize); CYCLE_ANCHORS]>::new();
    let mut votes = [0usize; 256];
    for (row, &value) in values.iter().enumerate() {
        if let Some((_, previous)) = anchors.iter_mut().find(|(anchor, _)| *anchor == value) {
            let distance = row - *previous;
            *previous = row;
            // Adjacent equal values are runs, not useful cyclic predictions.
            if (2..=usize::from(MAX_CYCLE)).contains(&distance) {
                votes[distance] += 1;
            }
        } else if anchors.len() < CYCLE_ANCHORS {
            anchors.push((value, row));
        }
    }
    let mut periods = SmallVec::new();
    for _ in 0..MAX_CYCLE_CANDIDATES {
        // Prefer a shorter period when the evidence ties.
        let Some(period) = (2..=usize::from(MAX_CYCLE))
            .filter(|&period| votes[period] != 0)
            .max_by_key(|&period| (votes[period], std::cmp::Reverse(period)))
        else {
            break;
        };
        periods.push(period as u8);
        votes[period] = 0;
    }
    periods
}

pub(crate) fn unpack(entries: &[u8], id: usize, bits: usize) -> u128 {
    // Consume byte fragments, including the seventeenth byte of an unaligned
    // 128-bit entry, without overreading the record or shifting by 128.
    let mut value = 0;
    let mut done = 0;
    while done < bits {
        let position = id * bits + done;
        let count = (8 - position % 8).min(bits - done);
        value |= u128::from((entries[position / 8] >> (position % 8)) & (u8::MAX >> (8 - count)))
            << done;
        done += count;
    }
    value
}

pub(crate) fn append_packed(
    record: &mut SmallVec<[u8; 24]>,
    values: impl Iterator<Item = u128>,
    count: usize,
    bits: usize,
) {
    let start = record.len();
    record.resize(start + (count * bits).div_ceil(8), 0);
    for (index, value) in values.enumerate() {
        for bit in 0..bits {
            let position = index * bits + bit;
            record[start + position / 8] |= (((value >> bit) & 1) as u8) << (position % 8);
        }
    }
}
