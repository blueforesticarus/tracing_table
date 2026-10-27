//! Fit timestamp predictors once, keeping active writes as unsigned offsets.
use super::{ArchivedBlock, BlockCache, CollocatedMetadata, Descriptor, parent_offsets};
use crate::data::column;
use crate::data::dataframe::{AnyFrame, Frame, Row};
use crate::data::util::smallbox::SmallBox;

impl CollocatedMetadata {
    /// The frame owns its row count; coefficients never duplicate that count.
    pub fn decode_timestamp(
        &self,
        descriptor: &Descriptor,
        encoded: u64,
        row: usize,
        length: usize,
    ) -> u64 {
        assert!(
            !self.timestamp_parent_relative,
            "parent-relative timestamps require the parent start"
        );
        let offset = if self.timestamp_stride.to_u64() == 0 {
            encoded
        } else {
            column::restore(
                u128::from(encoded),
                u128::from(column::advance(self.timestamp_stride.to_u64(), row, length)),
                8,
            ) as u64
        };
        descriptor.decode_timestamp(offset)
    }

    /// Preserve the timestamp policy: fixed zero origin and optional FloatU16 line.
    pub(super) fn fit_timestamp_stride(&mut self, frame: &mut Frame) {
        self.timestamp_stride = column::fit_timestamp(frame);
    }
}

impl BlockCache {
    /// Strip active-only parent starts before fitting either complete archive.
    /// Compare real aligned allocations, retaining the old scheme on ties.
    pub(super) fn parent_timestamp_archive(
        &self,
        columns: &[column::Layout],
    ) -> SmallBox<ArchivedBlock> {
        let source = || {
            let mut frame = Frame::new(self.unit, self.active.length);
            for row in self.active.iter() {
                frame.push(&row[..self.unit]);
            }
            // Preserve editable end slots and other structural mask locks.
            frame.buffer[..self.unit].copy_from_slice(&self.active.mask()[..self.unit]);
            frame
        };
        let finish = |mut frame: Box<Frame>, relative: bool| {
            let mut metadata = CollocatedMetadata::default();
            metadata.timestamp_parent_relative = relative;
            if !relative && self.active_timestamp_header {
                metadata.fit_timestamp_stride(&mut frame);
            }
            let mut trailer = self.active_parent_offsets.clone();
            if !trailer.is_empty() {
                parent_offsets::fit_strides(
                    &mut frame,
                    &mut trailer,
                    &self.active_parent_row_offsets,
                );
            }
            column::fit(&mut frame, &mut trailer, columns);
            ArchivedBlock::new_with_trailer(&frame, metadata, &trailer)
        };
        let absolute = finish(source(), false);
        if !self.active_parent_timestamps_valid || !self.active_timestamp_header {
            return absolute;
        }
        let mut relative = source();
        let mut elapsed = Vec::with_capacity(self.active.length);
        for row in self.active.iter() {
            let offset = u64::from_ne_bytes(row[..8].try_into().unwrap());
            let parent = u64::from_ne_bytes(row[self.unit..self.unit + 8].try_into().unwrap());
            let Some(value) = self
                .active_descriptor
                .decode_timestamp(offset)
                .checked_sub(parent)
            else {
                return absolute;
            };
            elapsed.push(value);
        }
        column::rewrite(
            &mut relative,
            column::Layout::new(0, 8, false),
            |_, row, _| u128::from(elapsed[row]),
        );
        let relative = finish(relative, true);
        if relative.allocation_size() < absolute.allocation_size() {
            relative
        } else {
            absolute
        }
    }
}

impl Row<'_> {
    /// Span-close edits decode through the borrowed archive, without another lookup.
    pub(crate) fn decode_timestamp(&mut self, descriptor: &Descriptor) -> u64 {
        let encoded = u64::from_ne_bytes(self.get_mut()[..8].try_into().unwrap());
        match &self.frame {
            AnyFrame::Archived(block, _) => {
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
    use crate::data::util::float_u16::FloatU16;

    fn append(cache: &mut BlockCache, ts: u64) {
        // Match the subscriber's timestamp-header write and mask update order.
        let (row, _, offset) = cache.allocate_timestamp(ts);
        row.fill(0);
        row[..8].copy_from_slice(&offset.to_ne_bytes());
        cache.update_mask_after_append();
        cache.update_ts_range(ts as usize, None);
    }

    #[test]
    fn active_parent_column_is_omitted_and_smaller_archives_win() {
        let mut cache = BlockCache::new(8, 32, 32);
        cache.cache_parent_timestamps(true);
        for r in 0..32u64 {
            let parent = 1_000_000 + r * r * 10_000;
            let (row, _, offset) = cache.allocate_timestamp_with_parent(parent + 7, Some(parent));
            row.copy_from_slice(&offset.to_ne_bytes());
            cache.update_mask_after_append();
        }
        assert_eq!(cache.active.unit, 16);
        assert_eq!(
            u64::from_ne_bytes(cache.active.iter().nth(7).unwrap()[8..].try_into().unwrap()),
            1_490_000
        );
        cache.cycle();
        let block = &cache.ring[0];
        assert!(block.data.metadata.timestamp_parent_relative);
        assert_eq!(block.data.frame.unit(), 8);
        assert_eq!(block.data.frame.mask()[..8], [0; 8]);
        assert_eq!(
            u64::from_ne_bytes(block.data.frame.first()[..8].try_into().unwrap()),
            7
        );
        assert_eq!(cache.active.unit, 16);
    }

    #[test]
    fn cache_keeps_absolute_prediction_when_relative_is_larger_or_unavailable() {
        for missing in [false, true] {
            let mut cache = BlockCache::new(8, 50, 50);
            cache.cache_parent_timestamps(true);
            for r in 0..50 {
                let (row, _, offset) = cache.allocate_timestamp_with_parent(
                    1_000 + r * 2,
                    if missing { None } else { Some(0) },
                );
                row.copy_from_slice(&offset.to_ne_bytes());
                cache.update_mask_after_append();
            }
            cache.cycle();
            assert!(!cache.ring[0].data.metadata.timestamp_parent_relative);
            assert_eq!(cache.ring[0].data.frame.unit(), 8);
            check_archived(&cache, &(0..50).map(|r| 1_000 + r * 2).collect::<Vec<_>>());
        }
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
    fn stride_limits_and_flat_blocks_round_trip() {
        for (times, stride) in [
            (vec![0, FloatU16::MAX / 2], FloatU16::MAX),
            (
                vec![0, FloatU16::MAX / 2 + 1],
                if column::TIMESTAMP_MINIMAX_STRIDE {
                    FloatU16::MAX
                } else {
                    0
                },
            ),
            (
                vec![0, u64::MAX],
                if column::TIMESTAMP_MINIMAX_STRIDE {
                    FloatU16::MAX
                } else {
                    0
                },
            ),
            (vec![u64::MAX], 0),
            (vec![7, 7, 7], 0),
            (
                vec![0, 9, 0],
                if column::TIMESTAMP_MINIMAX_STRIDE {
                    8
                } else {
                    0
                },
            ),
        ] {
            let mut cache = BlockCache::new(8, times.len(), times.len());
            for &ts in &times {
                append(&mut cache, ts);
            }
            cache.cycle();
            // Mask fitting may choose a very different coefficient with fewer bits.
            // Keep legacy coefficient checks when that optional policy is selected.
            if !column::TIMESTAMP_MASK_STRIDE {
                assert_eq!(
                    cache.ring[0].data.metadata.timestamp_stride.to_u64(),
                    stride
                );
            }
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
        assert_eq!(
            cache.ring[0].data.metadata.timestamp_stride.to_u64(),
            if column::TIMESTAMP_MASK_STRIDE {
                8 // Endpoint's residual mask is smaller than the minimax mask.
            } else if column::TIMESTAMP_MINIMAX_STRIDE {
                6
            } else {
                8
            }
        );
        assert_eq!(cache.ring[0].data.frame.mask()[8], 0xff);
        check_archived(&cache, &times);
        // Changing a constant byte rebuilds the archive without refitting times.
        cache.get_mut(1, &[]).unwrap().1.get_mut()[9] = 42;
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
        let entry = block.data.parent_offsets(&[]).entry(0);
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
