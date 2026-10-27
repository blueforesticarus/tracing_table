//! Borrowed exception tables and their fitter. Column remains nongeneric; this
//! struct owns the codec details for a future Encoding interface to reuse.
use smallvec::SmallVec;

use super::{Candidate, Cost, MAX_RECORD_BYTES, check_record_size, unpack, varying_bits};

pub(super) const TAG: u8 = 9;
/// Code policy, not a per-column field or serialized flag. Duration zero is
/// always open; other callers can use the methods with false for ordinary zero.
pub(super) const ZERO_SPECIAL: bool = true;

/// Temporary view over one codec record; no expanded row array is retained.
#[derive(Clone, Copy)]
pub struct ExceptionEncoding<'a> {
    minimum: u128,
    high_minimum: u128,
    split: usize,
    patch_bits: usize,
    count: usize,
    position_bits: usize,
    bitmap: bool,
    positions: &'a [u8],
    patches: &'a [u8],
}

impl<'a> ExceptionEncoding<'a> {
    /// Four-byte header: tag; bitmap:1/split:7; count:8; high-minimum:1/patch:7.
    /// It is followed by the full-width minimum, optional high minimum, and
    /// separately byte-rounded positions and offsets. Row count comes from frame.
    pub fn new(record: &'a [u8], width: usize, rows: usize) -> Self {
        assert!(rows > 0 && record.len() >= 4 + width && record[0] == TAG);
        let split = usize::from(record[1] & 127);
        let patch_bits = usize::from(record[3] & 127);
        assert!(split <= width * 8 && patch_bits <= width * 8 - split);
        let count = usize::from(record[2]);
        assert!(count <= rows);
        let bitmap = record[1] & 128 != 0;
        let minimum = read_le(&record[4..4 + width]);
        let mut offset = 4 + width;
        let mut high_minimum = 0;
        if record[3] & 128 != 0 {
            let bytes = (width * 8 - split).div_ceil(8);
            high_minimum = read_le(&record[offset..offset + bytes]);
            offset += bytes;
        }
        let position_bits = (usize::BITS - (rows - 1).leading_zeros()) as usize;
        let bytes = if bitmap { rows } else { count * position_bits }.div_ceil(8);
        let positions = &record[offset..offset + bytes];
        offset += bytes;
        let end = offset + (count * patch_bits).div_ceil(8);
        check_record_size(record, end);
        Self {
            minimum,
            high_minimum,
            split,
            patch_bits,
            count,
            position_bits,
            bitmap,
            positions,
            patches: &record[offset..end],
        }
    }

    fn low_mask(self) -> u128 {
        // Split is at most 127 because it shares a byte with position mode.
        (1u128 << self.split) - 1
    }

    fn patch_index(self, row: usize) -> Option<usize> {
        if self.bitmap {
            let byte = row / 8;
            let bit = row % 8;
            if self.positions[byte] & (1 << bit) == 0 {
                return None;
            }
            let preceding: usize = self.positions[..byte]
                .iter()
                .map(|b| b.count_ones() as usize)
                .sum();
            return Some(
                preceding + (self.positions[byte] & ((1 << bit) - 1)).count_ones() as usize,
            );
        }
        // Sparse positions permit random lookup without scanning prior rows.
        let (mut low, mut high) = (0, self.count);
        while low < high {
            let middle = (low + high) / 2;
            let position = unpack(self.positions, middle, self.position_bits) as usize;
            if position < row {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        (low < self.count && unpack(self.positions, low, self.position_bits) == row as u128)
            .then_some(low)
    }

    fn high(self, row: usize) -> u128 {
        self.patch_index(row).map_or(0, |index| {
            self.high_minimum
                .checked_add(unpack(self.patches, index, self.patch_bits))
                .expect("exception high value overflow")
        })
    }

    pub fn decode(self, encoded: u128, row: usize, zero_special: bool) -> u128 {
        let offset = (self.high(row) << self.split) | (encoded & self.low_mask());
        if zero_special && offset == 0 {
            return 0;
        }
        self.minimum
            .checked_add(offset - u128::from(zero_special))
            .expect("exception minimum overflow")
    }

    /// Side tables are immutable. A changed high value or a value below the
    /// minimum requests the existing archived-edit reconstruction path.
    pub fn encode(self, value: u128, row: usize, first: u128, zero_special: bool) -> Option<u128> {
        let offset = normalize(value, self.minimum, zero_special)?;
        if offset >> self.split != self.high(row) {
            return None;
        }
        // Retain row zero's full unsigned offset in the first slot. Its high
        // bits are constant physical frame bits, ignored for other logical rows.
        Some((first & !self.low_mask()) | (offset & self.low_mask()))
    }

    pub(super) fn fit(best: &mut Candidate, values: &[u128], cost: &Cost<'_>, zero_special: bool) {
        if !best.worthwhile(cost.size(0, 4 + cost.width), cost) {
            return;
        }
        let Some(minimum) = values
            .iter()
            .copied()
            .filter(|&v| !zero_special || v != 0)
            .min()
        else {
            return;
        };
        let offsets: SmallVec<[u128; 256]> = values
            .iter()
            .map(|&value| normalize(value, minimum, zero_special).unwrap())
            .collect();
        let (variation, maximum) = offsets.iter().fold((0, 0), |(mask, maximum), &v| {
            (mask | (v ^ offsets[0]), maximum.max(v))
        });
        let mut selected: Option<(usize, usize, SmallVec<[u8; 24]>)> = None;
        let fixed_size = 4 + cost.width;
        let position_bits = (usize::BITS - (values.len() - 1).leading_zeros()) as usize;
        let bitmap_size = values.len().div_ceil(8);
        // Wider low splits than the maximum's bit length are identical, with
        // no exceptions. Search downward so 256 exceptions rule out all later
        // splits. Keep the original ascending search's smaller-split tie break.
        let max_split = (128 - maximum.leading_zeros()) as usize;
        // Split 128 cannot fit the header; ordinary origin encoding remains available.
        for split in (0..=max_split.min(cost.width * 8).min(127)).rev() {
            let low_mask = (1u128 << split) - 1;
            let row_bits = varying_bits(variation & low_mask, cost.width);
            // Even a free position/patch array cannot beat this lower bound.
            // Lower splits may still win by removing more varying low bits.
            let lower_bound = cost.size(row_bits, fixed_size);
            if selected
                .as_ref()
                .is_some_and(|p| (p.0, p.1) <= (lower_bound, split))
                || !best.worthwhile(lower_bound, cost)
            {
                continue;
            }
            // Future work: bit-length counts and suffix minima could provide
            // exact count/min/max statistics for all splits in one pass, then
            // collect positions only for the winner. Partial-scan bounds could
            // also bail once neither patch-base mode can possibly save space.
            let positions: SmallVec<[usize; 32]> = offsets
                .iter()
                .enumerate()
                .filter_map(|(row, &value)| (value >> split != 0).then_some(row))
                .take(256) // More positions cannot fit the eight-bit count.
                .collect();
            if positions.len() > u8::MAX as usize {
                break; // More high bits cannot reduce the exception count.
            }
            let count = positions.len();
            let list_size = (count * position_bits).div_ceil(8);
            let minimum_record_size = fixed_size + list_size.min(bitmap_size);
            if minimum_record_size > MAX_RECORD_BYTES {
                // Position count only grows as the low split shrinks.
                break;
            }
            let lower_bound = cost.size(row_bits, minimum_record_size);
            if selected
                .as_ref()
                .is_some_and(|p| (p.0, p.1) <= (lower_bound, split))
                || !best.worthwhile(lower_bound, cost)
            {
                continue;
            }
            let min_high = positions
                .iter()
                .map(|&i| offsets[i] >> split)
                .min()
                .unwrap_or(0);
            let max_high = positions
                .iter()
                .map(|&i| offsets[i] >> split)
                .max()
                .unwrap_or(0);
            for has_minimum in [false, true] {
                if has_minimum && count == 0 {
                    continue;
                }
                let high_minimum = if has_minimum { min_high } else { 0 };
                let patch_bits = (128 - (max_high - high_minimum).leading_zeros()) as usize;
                if patch_bits > 127 {
                    continue;
                }
                let base_size = fixed_size
                    + if has_minimum {
                        (cost.width * 8 - split).div_ceil(8)
                    } else {
                        0
                    }
                    + (count * patch_bits).div_ceil(8);
                // Only the cheaper position representation matters. Compare
                // allocated sizes so alignment ties still prefer the list,
                // as before; an oversized list must not hide a legal bitmap.
                let bitmap = base_size + list_size > MAX_RECORD_BYTES
                    || cost.size(row_bits, base_size + bitmap_size)
                        < cost.size(row_bits, base_size + list_size);
                let positions_size = if bitmap { bitmap_size } else { list_size };
                let record_size = base_size + positions_size;
                if record_size > MAX_RECORD_BYTES {
                    continue;
                }
                let size = cost.size(row_bits, record_size);
                if selected
                    .as_ref()
                    .is_some_and(|p| (p.0, p.1) <= (size, split))
                    || !best.worthwhile(size, cost)
                {
                    continue;
                }
                let mut record = SmallVec::<[u8; 24]>::new();
                record.extend_from_slice(&[
                    TAG,
                    split as u8 | ((bitmap as u8) << 7),
                    count as u8,
                    patch_bits as u8 | ((has_minimum as u8) << 7),
                ]);
                // Full-width minimum storage is inefficient. It could become
                // optional (especially for zero) or use fewer bytes in the
                // future; for now account for every byte of this exact value.
                record.extend_from_slice(&minimum.to_le_bytes()[..cost.width]);
                if has_minimum {
                    record.extend_from_slice(
                        &high_minimum.to_le_bytes()[..(cost.width * 8 - split).div_ceil(8)],
                    );
                }
                if bitmap {
                    let start = record.len();
                    record.resize(start + positions_size, 0);
                    for &row in &positions {
                        record[start + row / 8] |= 1 << (row % 8);
                    }
                } else {
                    append_packed(
                        &mut record,
                        positions.iter().map(|&row| row as u128),
                        count,
                        position_bits,
                    );
                }
                append_packed(
                    &mut record,
                    positions
                        .iter()
                        .map(|&row| (offsets[row] >> split) - high_minimum),
                    count,
                    patch_bits,
                );
                debug_assert_eq!(record.len(), record_size);
                selected = Some((size, split, record));
            }
        }
        if let Some((_, split, record)) = selected {
            let low_mask = (1u128 << split) - 1;
            let first = offsets[0];
            best.consider(&record, values, cost, |value, _| {
                (first & !low_mask) | (normalize(value, minimum, zero_special).unwrap() & low_mask)
            });
        }
    }
}

fn normalize(value: u128, minimum: u128, zero_special: bool) -> Option<u128> {
    if zero_special && value == 0 {
        Some(0)
    } else {
        // Closed durations get +1 so the minimum closed value cannot alias open.
        value
            .checked_sub(minimum)?
            .checked_add(u128::from(zero_special))
    }
}

fn read_le(bytes: &[u8]) -> u128 {
    let mut word = [0; 16];
    word[..bytes.len()].copy_from_slice(bytes);
    u128::from_le_bytes(word)
}

fn append_packed(
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

#[cfg(test)]
mod tests;
