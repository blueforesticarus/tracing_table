//! Exception tables own their footer format. Zero handling is a static policy,
//! so duration sentinels need neither a duplicate codec nor a stored flag.
use smallvec::SmallVec;

use super::common::{
    Candidate, Cost, MAX_RECORD_BYTES, append_packed, check_record_size, unpack_u64, varying_bits,
};
use super::{Context, Encoding, Footer, Layout};

pub(crate) const TAG: u8 = 9;
/// Temporary view over one codec record; no expanded row array is retained.
// TODO: Make this encoding generic over 4-, 8-, and 16-byte words. For now,
// coefficients and row arithmetic use u64, and fitting skips 16-byte columns.
#[derive(Clone, Copy)]
pub struct ExceptionsEncoding<'a, const ZERO_SPECIAL: bool = true> {
    minimum: u64,
    high_minimum: u64,
    split: usize,
    patch_bits: usize,
    count: usize,
    position_bits: usize,
    bitmap: bool,
    positions: &'a [u8],
    patches: &'a [u8],
}

impl<'a, const ZERO_SPECIAL: bool> ExceptionsEncoding<'a, ZERO_SPECIAL> {
    /// Four-byte header: tag; bitmap:1/split:7; count:8; high-minimum:1/patch:7.
    /// It is followed by the full-width minimum, optional high minimum, and
    /// separately byte-rounded positions and offsets. Row count comes from frame.
    pub fn new(record: &'a [u8], width: usize, rows: usize) -> Self {
        assert!(
            matches!(width, 1 | 2 | 4 | 8),
            "exception codec supports up to u64"
        );
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

    fn low_mask(self) -> u64 {
        low_mask(self.split)
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
            let position = unpack_u64(self.positions, middle, self.position_bits) as usize;
            if position < row {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        (low < self.count && unpack_u64(self.positions, low, self.position_bits) == row as u64)
            .then_some(low)
    }

    fn high(self, row: usize) -> u64 {
        self.patch_index(row).map_or(0, |index| {
            self.high_minimum
                .checked_add(unpack_u64(self.patches, index, self.patch_bits))
                .expect("exception high value overflow")
        })
    }

    pub fn decode(self, encoded: u128, row: usize) -> u128 {
        // Preserve the shared column interface, narrowing at the codec boundary.
        let encoded = u64::try_from(encoded).expect("exception value exceeds u64");
        // A split of 64 keeps the entire value in the low word, with no patches.
        let offset = self.high(row).checked_shl(self.split as u32).unwrap_or(0)
            | (encoded & self.low_mask());
        if ZERO_SPECIAL && offset == 0 {
            return 0;
        }
        u128::from(
            self.minimum
                .checked_add(offset - u64::from(ZERO_SPECIAL))
                .expect("exception minimum overflow"),
        )
    }

    /// Side tables are immutable. A changed high value or a value below the
    /// minimum requests the existing archived-edit reconstruction path.
    pub fn encode(self, value: u128, row: usize, first: u128) -> Option<u128> {
        let value = u64::try_from(value).ok()?;
        let first = u64::try_from(first).ok()?;
        let offset = normalize(value, self.minimum, ZERO_SPECIAL)?;
        if offset.checked_shr(self.split as u32).unwrap_or(0) != self.high(row) {
            return None;
        }
        // Retain row zero's full unsigned offset in the first slot. Its high
        // bits are constant physical frame bits, ignored for other logical rows.
        Some(u128::from(
            (first & !self.low_mask()) | (offset & self.low_mask()),
        ))
    }

    pub(crate) fn fit(best: &mut Candidate, values: &[u128], cost: &Cost<'_>) {
        // Wider fields remain eligible for raw and dictionary encodings.
        if cost.width > 8 || !best.worthwhile(cost.size(0, 4 + cost.width), cost) {
            return;
        }
        let Some(minimum) = values
            .iter()
            .map(|&value| value as u64)
            .filter(|&v| !ZERO_SPECIAL || v != 0)
            .min()
        else {
            return;
        };
        let offsets: SmallVec<[u64; 256]> = values
            .iter()
            .map(|&value| normalize(value as u64, minimum, ZERO_SPECIAL).unwrap())
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
        let max_split = (64 - maximum.leading_zeros()) as usize;
        for split in (0..=max_split.min(cost.width * 8)).rev() {
            let low_mask = low_mask(split);
            let row_bits = varying_bits(u128::from(variation & low_mask), cost.width);
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
                .filter_map(|(row, &value)| {
                    (value.checked_shr(split as u32).unwrap_or(0) != 0).then_some(row)
                })
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
                .map(|&i| offsets[i].checked_shr(split as u32).unwrap_or(0))
                .min()
                .unwrap_or(0);
            let max_high = positions
                .iter()
                .map(|&i| offsets[i].checked_shr(split as u32).unwrap_or(0))
                .max()
                .unwrap_or(0);
            for has_minimum in [false, true] {
                if has_minimum && count == 0 {
                    continue;
                }
                let high_minimum = if has_minimum { min_high } else { 0 };
                let patch_bits = (64 - (max_high - high_minimum).leading_zeros()) as usize;
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
                    positions.iter().map(|&row| {
                        u128::from(
                            offsets[row].checked_shr(split as u32).unwrap_or(0) - high_minimum,
                        )
                    }),
                    count,
                    patch_bits,
                );
                debug_assert_eq!(record.len(), record_size);
                selected = Some((size, split, record));
            }
        }
        if let Some((_, split, record)) = selected {
            let low_mask = low_mask(split);
            let first = offsets[0];
            best.consider(&record, values, cost, |value, _| {
                u128::from(
                    (first & !low_mask)
                        | (normalize(value as u64, minimum, ZERO_SPECIAL).unwrap() & low_mask),
                )
            });
        }
    }
}

fn normalize(value: u64, minimum: u64, zero_special: bool) -> Option<u64> {
    if zero_special && value == 0 {
        Some(0)
    } else {
        // Closed durations get +1 so the minimum closed value cannot alias open.
        value
            .checked_sub(minimum)?
            .checked_add(u64::from(zero_special))
    }
}

// Checked shifts cover both an empty low part and a full 64-bit low word.
fn low_mask(split: usize) -> u64 {
    u64::MAX.checked_shr((64 - split) as u32).unwrap_or(0)
}

fn read_le(bytes: &[u8]) -> u64 {
    let mut word = [0; 8];
    word[..bytes.len()].copy_from_slice(bytes);
    u64::from_le_bytes(word)
}

#[cfg(test)]
mod tests;

impl<'a, const ZERO_SPECIAL: bool> Encoding<'a> for ExceptionsEncoding<'a, ZERO_SPECIAL> {
    fn parse(record: &'a [u8], layout: Layout, rows: usize) -> Self {
        Self::new(record, usize::from(layout.width), rows)
    }
    fn write_footer(&self, width: usize, output: &mut Footer) {
        assert!(
            matches!(width, 1 | 2 | 4 | 8),
            "exception codec supports up to u64"
        );
        output.extend_from_slice(&[
            TAG,
            self.split as u8 | ((self.bitmap as u8) << 7),
            self.count as u8,
            self.patch_bits as u8 | ((self.high_minimum != 0) as u8) << 7,
        ]);
        output.extend_from_slice(&self.minimum.to_le_bytes()[..width]);
        if self.high_minimum != 0 {
            output.extend_from_slice(
                &self.high_minimum.to_le_bytes()[..(width * 8 - self.split).div_ceil(8)],
            );
        }
        output.extend_from_slice(self.positions);
        output.extend_from_slice(self.patches);
    }
    #[inline]
    fn encode(&self, value: u128, row: usize, context: Context) -> Option<u128> {
        (*self).encode(value, row, context.first())
    }
    #[inline]
    fn decode(&self, encoded: u128, row: usize, _context: Context) -> u128 {
        (*self).decode(encoded, row)
    }
}
