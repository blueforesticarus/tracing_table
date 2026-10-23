//! Fit timestamp predictors once, keeping active writes as unsigned offsets.
use super::{CollocatedMetadata, Descriptor};
use crate::data::{
    dataframe::{AnyFrame, Frame, Row},
    float_u16::FloatU16,
};

impl CollocatedMetadata {
    /// The frame owns its row count; coefficients never duplicate that count.
    pub fn decode_timestamp(
        &self,
        descriptor: &Descriptor,
        encoded: u64,
        row: usize,
        length: usize,
    ) -> u64 {
        let offset = if self.timestamp_stride.to_u64() == 0 {
            encoded
        } else {
            // Modular ZigZag handles residuals throughout the u64 domain.
            self.predict_timestamp(row, length)
                .wrapping_add((encoded >> 1) ^ 0u64.wrapping_sub(encoded & 1))
        };
        descriptor.decode_timestamp(offset)
    }

    /// Predict in shifted ticks, preserving unit precision for coarse clocks.
    fn predict_timestamp(&self, row: usize, length: usize) -> u64 {
        assert!(row < length);
        let length = length as u128;
        ((u128::from(self.timestamp_stride.to_u64()) * row as u128 + length / 2) / length) as u64
    }

    /// Initial compaction only: archived edits must keep both coefficients fixed.
    pub(super) fn fit_timestamp_stride(&mut self, frame: &mut Frame) {
        let length = frame.length;
        assert_eq!(self.timestamp_stride.to_u64(), 0);
        if length < 2 {
            return;
        }
        // The immutable origin is the first timestamp. Fit the last start time,
        // never the mutable range end, which can include late span closes.
        let last = length * frame.unit;
        let end = u64::from_ne_bytes(frame.buffer[last..last + 8].try_into().unwrap());
        let stride = u128::from(end) * length as u128 / (length - 1) as u128;
        if stride > u128::from(FloatU16::MAX) {
            return; // Disable instead of saturating an out-of-range timestamp stride.
        }
        self.timestamp_stride = FloatU16::from_u64_lossy(stride as u64);
        if self.timestamp_stride.to_u64() == 0 {
            return; // Flat blocks retain the original unsigned offsets.
        }
        let mut first = 0;
        let mut mask = 0u64;
        for (r, row) in frame.buffer[frame.unit..(length + 1) * frame.unit]
            .chunks_exact_mut(frame.unit)
            .enumerate()
        {
            let offset = u64::from_ne_bytes(row[..8].try_into().unwrap());
            // Quantized coefficients must be decoded before forming residuals.
            let delta = offset.wrapping_sub(self.predict_timestamp(r, length));
            let encoded = (delta << 1) ^ ((delta as i64 >> 63) as u64);
            row[..8].copy_from_slice(&encoded.to_ne_bytes());
            if r == 0 {
                first = encoded;
            }
            mask |= encoded ^ first;
        }
        // Replace only timestamp bits; late span-end edits and other locks survive.
        frame.buffer[..8].copy_from_slice(&mask.to_ne_bytes());
    }
}

impl Row<'_> {
    /// Span-close edits decode through the borrowed archive, without another lookup.
    pub(crate) fn decode_timestamp(&mut self, descriptor: &Descriptor) -> u64 {
        let encoded = u64::from_ne_bytes(self.get_mut()[..8].try_into().unwrap());
        match &self.frame {
            AnyFrame::Archived(block) => {
                block
                    .metadata
                    .decode_timestamp(descriptor, encoded, self.index, block.frame.len())
            }
            // Active/bare frames have unsigned offsets and no archived predictor.
            _ => descriptor.decode_timestamp(encoded),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::blockbuffer::{BlockCache, CallsiteArchetype};

    fn append(cache: &mut BlockCache, ts: u64) {
        // Match the subscriber's timestamp-header write and mask update order.
        let (row, _, offset) = cache.allocate_timestamp(ts);
        row.fill(0);
        row[..8].copy_from_slice(&offset.to_ne_bytes());
        cache.update_mask_after_append();
        cache.update_ts_range(ts as usize, None);
    }

    fn check_archived(cache: &BlockCache, times: &[u64]) {
        let block = cache.ring.back().unwrap();
        assert_eq!(block.data.frame.len(), times.len());
        let mut row = vec![0; block.data.frame.unit()];
        let mut mask = 0u64;
        let mut first = 0;
        for (r, &ts) in times.iter().enumerate() {
            block.data.frame.read_row(r, &mut row);
            let encoded = u64::from_ne_bytes(row[..8].try_into().unwrap());
            assert_eq!(
                block.data.metadata.decode_timestamp(
                    &block.descriptor,
                    encoded,
                    r,
                    block.data.frame.len()
                ),
                ts
            );
            // Random access must use the same block metadata and local row origin.
            assert_eq!(
                cache.decode_timestamp(encoded, block.descriptor.index + r),
                Some(ts)
            );
            if r == 0 {
                first = encoded;
            }
            mask |= encoded ^ first;
        }
        // Fitting must clear stale timestamp mask bits to realize the savings.
        assert_eq!(&block.data.frame.mask()[..8], mask.to_ne_bytes());
    }

    #[test]
    fn coarse_ticks_keep_small_strides_exact_and_remove_linear_variation() {
        for shift in [0, 10, 20, 40] {
            let mut cache = BlockCache::new(8, 50, 50);
            let times: Vec<_> = (0..50)
                .map(|r| (1_000_000 + (r << shift)) >> shift)
                .collect();
            for &ts in &times {
                append(&mut cache, ts);
                assert_eq!(cache.active_metadata.timestamp_stride.to_u64(), 0);
            }
            // Fifty roughly millisecond ticks at shift 20 need a stride of 50.
            cache.cycle();
            assert_eq!(cache.ring[0].data.metadata.timestamp_stride.to_u64(), 50);
            check_archived(&cache, &times);
            assert_eq!(&cache.ring[0].data.frame.mask()[..8], &[0; 8]);
        }
    }

    #[test]
    fn stride_limit_disables_instead_of_saturating_and_flat_blocks_stay_unsigned() {
        for (times, stride) in [
            (vec![0, FloatU16::MAX / 2], FloatU16::MAX),
            (vec![0, FloatU16::MAX / 2 + 1], 0),
            (vec![0, u64::MAX], 0),
            (vec![u64::MAX], 0),
            (vec![7, 7, 7], 0),
            (vec![0, 9, 0], 0),
        ] {
            let mut cache = BlockCache::new(8, times.len(), times.len());
            for &ts in &times {
                append(&mut cache, ts);
            }
            cache.cycle();
            assert_eq!(
                cache.ring[0].data.metadata.timestamp_stride.to_u64(),
                stride
            );
            check_archived(&cache, &times);
        }
    }

    #[test]
    fn irregular_full_range_times_and_quantized_strides_round_trip() {
        for times in [
            vec![0, u64::MAX, 2],
            vec![1 << 63, (1 << 63) + 5_000, (1 << 63) + 2, (1 << 63) + 20],
            vec![0, 0, 5_003], // A fractional fit also loses a FloatU16 precision bit.
            vec![u64::MAX - 2, u64::MAX - 1, u64::MAX],
        ] {
            let mut cache = BlockCache::new(8, times.len(), times.len());
            for &ts in &times {
                append(&mut cache, ts);
            }
            cache.cycle();
            assert!(cache.ring[0].data.metadata.timestamp_stride.to_u64() > 0);
            check_archived(&cache, &times);
        }
    }

    #[test]
    fn early_archive_edits_late_closes_and_eviction_preserve_predictions() {
        let mut cache = BlockCache::new(10, 32, 32);
        let times = [100, 102, 103, 106];
        for ts in times {
            append(&mut cache, ts);
        }
        cache.active.lock_editable(8, 9);
        // A close before archival must not influence the start-time fit.
        cache.update_ts_range(1_000_000, Some(1));
        cache.cycle();
        assert_eq!(cache.ring[0].data.metadata.timestamp_stride.to_u64(), 8);
        assert_eq!(cache.ring[0].data.frame.mask()[8], 0xff);
        check_archived(&cache, &times);
        // Changing a constant byte rebuilds the archive without refitting times.
        cache.get_mut(1).unwrap().1.get_mut()[9] = 42;
        cache.update_ts_range(2_000_000, Some(2));
        check_archived(&cache, &times);
        for ts in [500, 501] {
            append(&mut cache, ts);
        }
        cache.cycle();
        cache.drop_chunk(1).unwrap();
        check_archived(&cache, &[500, 501]);
        assert_eq!(cache.ring[0].descriptor.index, 5);
    }

    #[test]
    fn timestamp_and_parent_strides_fit_independently_in_the_same_frame() {
        let mut cache = BlockCache::new(18, 4, 4);
        for r in 0..4 {
            let (parent, group) = cache.encode_parent(200 + r * 3, CallsiteArchetype::new(7, 3), 8);
            let (row, _, offset) = cache.allocate_timestamp(100 + r * 2);
            row[..8].copy_from_slice(&offset.to_ne_bytes());
            row[8..16].copy_from_slice(&parent.to_ne_bytes());
            row[16..18].copy_from_slice(&group.to_ne_bytes());
            cache.update_mask_after_append();
        }
        cache.cycle();
        check_archived(&cache, &[100, 102, 104, 106]);
        let block = &cache.ring[0];
        let entry = block.data.parent_offsets().entry(0);
        assert_eq!(entry.stride.to_u64(), 12);
        let mut row = [0; 18];
        for r in 0..4 {
            block.data.frame.read_row(r, &mut row);
            assert_eq!(
                entry.decode(u64::from_ne_bytes(row[8..16].try_into().unwrap()), r, 4),
                200 + r as u64 * 3
            );
        }
    }
}
