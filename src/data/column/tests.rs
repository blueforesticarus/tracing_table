//! Production-path tests cover codec selection as well as exact reconstruction.
use super::*;
use crate::data::blockbuffer::{BlockCache, CallsiteArchetype, ParentOffsets};
use streaming_iterator::StreamingIterator;

fn archive(values: &[u128], width: usize, strings: bool) -> (BlockCache, [Layout; 1]) {
    let layout = [Layout::new(1, width, strings)];
    let mut cache = BlockCache::new(width + 2, values.len(), values.len());
    for &value in values {
        let mut row = vec![23; width + 2];
        write_word(&mut row[1..1 + width], value);
        cache.push(&row, 0);
    }
    cache.cycle_if_needed_with_columns(&layout);
    (cache, layout)
}

fn check(cache: &BlockCache, layout: &[Layout], expected: &[u128]) {
    let block = &cache.ring[0].data;
    let (spec, record) = block.column_metadata(layout).entries().next().unwrap();
    let column = Column::new(&block.frame, spec, record);
    if let Codec::Predicted(p) = &column.codec {
        if !p.row && p.stride == 0 {
            assert_eq!(
                record.len(),
                1 + usize::from(spec.width),
                "flat fits must not retain cycle metadata"
            );
        }
    }
    let mut rows = cache.iter(layout);
    for (index, &value) in expected.iter().enumerate() {
        let row = rows.next().unwrap();
        assert_eq!(read_word(&row[spec.range()]), value, "row {index}");
        assert_eq!(column.read(index), value, "selective read {index}");
        assert_eq!(row[0], 23);
        assert_eq!(*row.last().unwrap(), 23);
    }
    assert!(rows.next().is_none());
}

fn tag(cache: &BlockCache, layout: &[Layout]) -> u8 {
    cache.ring[0]
        .data
        .column_metadata(layout)
        .entries()
        .next()
        .unwrap()
        .1
        .first()
        .copied()
        .unwrap_or(0)
}

fn irregular(count: usize, rows: usize, base: u128, step: u128) -> Vec<u128> {
    // Discover all entries before an irregular tail; dictionary IDs follow first occurrence.
    let mut state = 7u32;
    (0..rows)
        .map(|row| {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            let id = if row < count {
                (row + 7) % count
            } else {
                (state >> 16) as usize % count
            };
            base + id as u128 * step
        })
        .collect()
}

#[test]
fn dictionary_economics_distinguish_arbitrary_bits_from_leading_zeros() {
    // After removing 1000, entries are {0, 8}: one varying frame bit can
    // require a four-bit dictionary entry. Offset subtraction must precede width.
    let values = [1000u128, 1008];
    let mut frame = Frame::new(2, 2);
    frame.push(&1000u16.to_ne_bytes());
    frame.push(&1008u16.to_ne_bytes());
    let cost = Cost {
        frame: &frame,
        total_bits: 1,
        original_bits: 1,
        footer: 1,
        width: 2,
    };
    let (_, entry_bits) = dictionary_cost(&values, &cost);
    assert_eq!(entry_bits, 4);
    let sparse: Vec<_> = (0..512)
        .map(|row| if row % 2 == 0 { 0 } else { 8 })
        .collect();
    let (cache, layout) = archive(&sparse, 2, false);
    // Other predictors may improve this periodic input, but a direct dictionary cannot.
    assert_ne!(tag(&cache, &layout), DICTIONARY);
    check(&cache, &layout, &sparse);
}

#[test]
fn constant_columns_pay_only_their_size_byte() {
    for width in [1, 2, 4, 8, 16] {
        let values = vec![word_mask(width); 128];
        let (cache, layout) = archive(&values, width, false);
        assert_eq!(cache.ring[0].data.frame.trailer(), &[0]);
        check(&cache, &layout, &values);
    }
}

#[test]
fn cycle_discovery_shortlists_evidence_instead_of_enumerating_periods() {
    // Linear fields and long runs should never enter a cyclic residual search.
    let line: Vec<_> = (0..1024u128).map(|row| row * 7919).collect();
    let runs: Vec<_> = (0..1024u128).map(|row| row / 64).collect();
    assert!(cycle_periods(&line).is_empty());
    assert!(cycle_periods(&runs).is_empty());
    // A rotated cycle longer than the dictionary limit must remain discoverable.
    let cyclic: Vec<_> = (0..1024u128).map(|row| (row + 37) % 127).collect();
    assert_eq!(cycle_periods(&cyclic).as_slice(), &[127]);
    let noisy = irregular(17, 1024, 0, 1);
    assert!(cycle_periods(&noisy).len() <= MAX_CYCLE_CANDIDATES);
}

#[test]
fn near_ties_keep_raw_but_larger_savings_pay_for_an_origin() {
    // Subtracting an origin reduces these values' variation, but a small block
    // cannot save enough bytes to justify adding a codec and its footer.
    // Byte compaction already stores these values in one byte, so an origin
    // cannot improve even the larger block in that build.
    let larger = if cfg!(feature = "bit-compression") {
        OFFSET
    } else {
        0
    };
    for (rows, expected) in [(24, 0), (256, larger)] {
        let values = irregular(17, rows, 120, 1);
        let (cache, layout) = archive(&values, 2, false);
        assert_eq!(tag(&cache, &layout), expected);
        check(&cache, &layout, &values);
    }
}

#[test]
fn small_column_savings_are_independent_of_unrelated_payload_and_footer() {
    // These are the physical ZigZag values for {-1, +1}. Offset fitting saves
    // one varying bit per tail row, enough to pay for its three-byte record.
    // Adding a large noisy neighbor or shared footer must not raise its bar.
    let values = irregular(2, 120, 1, 1);
    let column = Layout::new(0, 2, false);
    for noise_bytes in [0, 128] {
        let mut frame = Frame::new(2 + noise_bytes, values.len());
        let mut state = 7u64;
        for &value in &values {
            let mut row = vec![0; frame.unit];
            write_word(&mut row[..2], value);
            for byte in &mut row[2..] {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                *byte = (state >> 32) as u8;
            }
            frame.push(&row);
        }
        for footer in [1, 4097] {
            let selected = select(&values, column, &frame, footer);
            // Byte compaction cannot remove a single varying bit, so retains raw.
            let expected = if cfg!(feature = "bit-compression") {
                OFFSET
            } else {
                0
            };
            assert_eq!(selected.record.first().copied().unwrap_or(0), expected);
        }
    }
}

#[test]
fn large_column_still_requires_proportional_savings() {
    // Preserve the relative guard for an expensive column, even when the
    // absolute eight-byte minimum would accept the candidate allocation.
    let mut frame = Frame::new(8, 1024);
    for value in 0..1024u64 {
        frame.push(&value.to_ne_bytes());
    }
    let cost = Cost {
        frame: &frame,
        total_bits: frame_bits(&frame),
        original_bits: frame_bits(&frame),
        footer: 1,
        width: 8,
    };
    let candidate = Candidate {
        record: SmallVec::new(),
        encoded: SmallVec::new(),
        mask: 1023,
        size: cost.size(cost.original_bits, 0),
    };
    assert!(!candidate.worthwhile(candidate.size - MIN_ENCODING_SAVINGS, &cost));
    assert!(candidate.worthwhile(candidate.size - 64, &cost));
}

#[test]
fn calculated_dictionary_size_matches_physical_key_packing() {
    for count in [2, 5, 16, 17, 32, 33, 64] {
        let values = irregular(count, 257, 1000, 900);
        let mut frame = Frame::new(8, values.len());
        let mut keys = Frame::new(8, values.len());
        let mut unique = Vec::new();
        for value in values {
            let id = match unique.iter().position(|&entry| entry == value) {
                Some(id) => id,
                None => {
                    unique.push(value);
                    unique.len() - 1
                }
            };
            frame.push(&(value as u64).to_ne_bytes());
            keys.push(&(id as u64).to_ne_bytes());
        }
        let cost = Cost {
            frame: &frame,
            total_bits: frame_bits(&frame),
            original_bits: frame_bits(&frame),
            footer: 1,
            width: 8,
        };
        let (calculated, bits) = dictionary_cost(&unique, &cost);
        let compact = ArchivedFrame::new_from(&keys);
        // Compare the algebraic cost against the actual compact-frame payload,
        // including its mask, dictionary record, directory and alignment.
        let physical = compact.compression_ratio().compressed
            + compact.unit()
            + 3
            + (count * bits).div_ceil(8)
            + 1;
        assert_eq!(calculated, physical.div_ceil(2) * 2, "count {count}");
    }
}

#[test]
fn strings_keep_row_and_rotated_cyclic_row_encodings() {
    for (values, expected) in [
        (
            (0..1024)
                .map(|row| (65500u128 + row) & 65535)
                .collect::<Vec<_>>(),
            ROW,
        ),
        (
            (0..1024).map(|row| 2000 + (row + 3) % 7).collect(),
            ROW_CYCLE,
        ),
    ] {
        let (cache, layout) = archive(&values, 2, true);
        assert_eq!(tag(&cache, &layout), expected);
        check(&cache, &layout, &values);
        let (_, record) = cache.ring[0]
            .data
            .column_metadata(&layout)
            .entries()
            .next()
            .unwrap();
        // Row-only metadata has no FloatU16 stride; cycles add only period/phase.
        assert_eq!(record.len(), if expected == ROW { 3 } else { 5 });
    }
}

#[test]
fn general_fields_choose_exact_offset_or_custom_float_line() {
    let offset = irregular(97, 1024, (1u128 << 50) - 40, 1);
    let line: Vec<_> = (0..1024)
        .map(|row| (1u128 << 110) + row * 100000003 + row % 2)
        .collect();
    for (values, width, expected) in [(offset, 8, OFFSET), (line, 16, LINEAR)] {
        let (cache, layout) = archive(&values, width, false);
        if expected == LINEAR {
            assert_eq!(tag(&cache, &layout) & 0x7f, LINEAR);
        } else {
            assert!(matches!(
                tag(&cache, &layout) & 0x7f,
                OFFSET | LINEAR | CYCLE
            ));
        }
        check(&cache, &layout, &values);
        let (_, record) = cache.ring[0]
            .data
            .column_metadata(&layout)
            .entries()
            .next()
            .unwrap();
        let p = Prediction::parse(record, width);
        if expected == LINEAR {
            assert_eq!(p.origin, values[0]);
            assert_eq!(record.len(), 1 + width + 2);
        }
    }
}

#[test]
fn custom_stride_and_cycles_compose_for_ascending_and_descending_fields() {
    for descending in [false, true] {
        let values: Vec<_> = (0..1024)
            .map(|row| {
                let delta = ((row + 37) % 127) * 123;
                if descending {
                    (1u128 << 48) - delta
                } else {
                    (1u128 << 48) + delta
                }
            })
            .collect();
        let (cache, layout) = archive(&values, 8, false);
        assert_eq!(tag(&cache, &layout) & 0x7f, CYCLE);
        let (_, record) = cache.ring[0]
            .data
            .column_metadata(&layout)
            .entries()
            .next()
            .unwrap();
        let p = Prediction::parse(record, 8);
        assert!(p.stride > 1);
        assert!(p.period != 0);
        assert_eq!(p.descending, descending);
        check(&cache, &layout, &values);
    }
}

#[test]
fn dictionaries_preserve_all_counts_and_full_width_value_bits() {
    for count in [2, 5, 16, 17, 32, 33, 64] {
        let values = irregular(count, 1024, 1000, 900);
        let (cache, layout) = archive(&values, 2, true);
        assert_eq!(tag(&cache, &layout), DICTIONARY, "count {count}");
        let block = &cache.ring[0].data;
        assert_eq!(read_word(&block.frame.first()[layout[0].range()]), 1000);
        let (_, record) = block.column_metadata(&layout).entries().next().unwrap();
        assert_eq!(record[1], count as u8);
        check(&cache, &layout, &values);
    }
    // Dictionary packing must also handle unaligned 127/128-bit entries exactly.
    for maximum in [u128::MAX >> 1, u128::MAX] {
        let values: Vec<_> = irregular(3, 1024, 0, 1)
            .into_iter()
            .map(|id| [maximum, 0, maximum / 2][id as usize])
            .collect();
        let (cache, layout) = archive(&values, 16, false);
        assert!(matches!(
            tag(&cache, &layout),
            DICTIONARY | DICTIONARY_CYCLE
        ));
        check(&cache, &layout, &values);
    }
}

#[test]
fn dictionary_keys_keep_the_experimental_cycle_support() {
    let distinct = [1000, 30000, 700, 15000, 60000, 999, 101];
    let values: Vec<_> = (0..1024)
        .map(|row| distinct[row % distinct.len()])
        .collect();
    let (cache, layout) = archive(&values, 2, true);
    assert_eq!(tag(&cache, &layout), DICTIONARY_CYCLE);
    check(&cache, &layout, &values);
}

#[test]
fn dictionary_stride_compresses_slow_runs_and_survives_archived_edits() {
    // Irregular values defeat a numeric line; discovery-order keys advance once
    // every 64 rows, so a fractional non-cyclic key stride is much cheaper.
    let mut state = 7u64;
    let entries: Vec<_> = (0..17)
        .map(|_| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            u128::from(state)
        })
        .collect();
    let mut values: Vec<_> = (0..1024).map(|row| entries[(row + 32) / 64]).collect();
    let (mut cache, layout) = archive(&values, 8, false);
    assert_eq!(tag(&cache, &layout), DICTIONARY_STRIDE);
    check(&cache, &layout, &values);
    let footer = cache.ring[0].data.frame.trailer().to_vec();
    values[100] = entries[9];
    write_word(
        &mut cache.get_mut(101, &layout).unwrap().1.get_mut()[1..9],
        values[100],
    );
    assert_eq!(cache.ring[0].data.frame.trailer(), footer);
    check(&cache, &layout, &values);
    // New entries must remove this dictionary variant too, preserving all rows.
    values[101] = u128::from(u64::MAX);
    write_word(
        &mut cache.get_mut(102, &layout).unwrap().1.get_mut()[1..9],
        values[101],
    );
    assert_eq!(tag(&cache, &layout), 0);
    check(&cache, &layout, &values);
}

#[test]
fn size_directory_supports_more_than_eight_columns_and_large_combined_metadata() {
    let columns: Vec<_> = (0..11)
        .map(|column| Layout::new(300 + column * 4, 4, false))
        .collect();
    let mut cache = BlockCache::new(344, 512, 512);
    let mut state = 7u32;
    let distinct: Vec<_> = (0..32)
        .map(|_| {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            state
        })
        .collect();
    let values = irregular(32, 512, 0, 1);
    for &id in &values {
        let mut row = vec![0; 344];
        for &column in &columns {
            write_word(&mut row[column.range()], u128::from(distinct[id as usize]));
        }
        cache.push(&row, 0);
    }
    cache.cycle_if_needed_with_columns(&columns);
    let block = &cache.ring[0].data;
    assert!(block.frame.trailer().len() > 510);
    assert_eq!(block.parent_offsets(&columns).len(), 0);
    let mut row = vec![0; 344];
    for (index, &id) in values.iter().enumerate() {
        block.read_row(index, &mut row, &columns);
        for &column in &columns {
            assert_eq!(
                read_word(&row[column.range()]),
                u128::from(distinct[id as usize])
            );
        }
    }
    assert_eq!(block.column_metadata(&columns).entries().count(), 11);
}

#[test]
fn dictionary_edits_preserve_other_predictions_and_structural_metadata() {
    let columns = [Layout::new(18, 2, true), Layout::new(20, 2, true)];
    let mut cache = BlockCache::new(22, 1024, 1024);
    let mut expected = irregular(32, 1024, 1000, 900);
    for (row, &value) in expected.iter().enumerate() {
        let (parent, group) =
            cache.encode_parent(500 + row as u64 * 3, CallsiteArchetype::new(7, 3), 8);
        let (bytes, _, ts) = cache.allocate_timestamp(100 + row as u64 * 2);
        bytes[..8].copy_from_slice(&ts.to_ne_bytes());
        bytes[8..16].copy_from_slice(&parent.to_ne_bytes());
        bytes[16..18].copy_from_slice(&group.to_ne_bytes());
        write_word(&mut bytes[18..20], value);
        write_word(&mut bytes[20..22], 2000 + row as u128);
        cache.update_mask_after_append();
    }
    cache.cycle_if_needed_with_columns(&columns);
    let footer = cache.ring[0].data.frame.trailer().to_vec();
    assert_eq!(
        footer.len(),
        ParentOffsets::ENTRY_BYTES + cache.ring[0].data.column_footer_bytes(&columns)
    );
    // A known dictionary value can be edited without replacing coefficients.
    expected[17] = expected[31];
    write_word(
        &mut cache.get_mut(18, &columns).unwrap().1.get_mut()[18..20],
        expected[17],
    );
    assert_eq!(cache.ring[0].data.frame.trailer(), footer);
    // A new value below the minimum drops dictionaries, retaining the row predictor.
    expected[4] = 7;
    write_word(
        &mut cache.get_mut(5, &columns).unwrap().1.get_mut()[18..20],
        7,
    );
    let block = &cache.ring[0].data;
    let records: Vec<_> = block
        .column_metadata(&columns)
        .entries()
        .map(|(_, r)| r)
        .collect();
    assert!(records[0].is_empty());
    assert_eq!(records[1][0], ROW);
    let mut bytes = [0; 22];
    for (row, &value) in expected.iter().enumerate() {
        block.read_row(row, &mut bytes, &columns);
        assert_eq!(read_word(&bytes[18..20]), value);
        assert_eq!(read_word(&bytes[20..22]), 2000 + row as u128);
        assert_eq!(
            block.metadata.decode_timestamp(
                &cache.ring[0].descriptor,
                read_word(&bytes[..8]) as u64,
                row,
                1024
            ),
            100 + row as u64 * 2
        );
        assert_eq!(
            block.parent_offsets(&columns).entry(0).decode(
                read_word(&bytes[8..16]) as u64,
                row,
                1024
            ),
            500 + row as u64 * 3
        );
    }
}

#[test]
fn changing_dictionary_row_zero_rebuilds_without_changing_other_rows() {
    let mut expected = irregular(32, 1024, 1000, 900);
    let (mut cache, layout) = archive(&expected, 2, true);
    expected[0] = expected[1];
    write_word(
        &mut cache.get_mut(1, &layout).unwrap().1.get_mut()[1..3],
        expected[0],
    );
    assert_eq!(tag(&cache, &layout), 0);
    check(&cache, &layout, &expected);
}

#[test]
fn oversized_dictionary_records_fall_back_and_malformed_sizes_are_rejected() {
    let mut state = 7u64;
    let values: Vec<_> = (0..64)
        .map(|_| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            u128::from(state)
        })
        .collect();
    let values: Vec<_> = (0..1024).map(|row| values[row % 64]).collect();
    let (cache, layout) = archive(&values, 8, false);
    let (_, record) = cache.ring[0]
        .data
        .column_metadata(&layout)
        .entries()
        .next()
        .unwrap();
    assert!(record.len() <= 255);
    check(&cache, &layout, &values);
    assert!(std::panic::catch_unwind(|| Metadata::split(&[255], &layout)).is_err());
}

#[test]
fn mixed_width_small_blocks_wrapping_and_irregular_values_round_trip() {
    // Exercise quantized fits and modular residuals independently of which codec wins.
    for width in [1, 2, 4, 8, 16] {
        for rows in [1, 2, 3, 31, 257] {
            let mut state = 7u128;
            let values: Vec<_> = (0..rows)
                .map(|row| {
                    state = state
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    let value = match row % 4 {
                        0 => state,
                        1 => u128::MAX.wrapping_sub(row as u128 * 123),
                        _ => ((row % 17) * 37) as u128,
                    };
                    value & word_mask(width)
                })
                .collect();
            let (cache, layout) = archive(&values, width, false);
            check(&cache, &layout, &values);
        }
    }
}

#[test]
fn timestamp_minimax_matches_exhaustive_representable_search() {
    // Include bursts, backward jumps, quantization boundaries, and unsigned limits.
    for values in [
        vec![0, 0, 0],
        vec![0, 2, 4, 6],
        vec![0, 90, 100, 101],
        vec![0, 9, 0],
        vec![0, 0, 5003],
        vec![0, u64::MAX, 2],
        vec![0, FloatU16::MAX / 2 + 1],
    ] {
        let mut cache = BlockCache::new(8, values.len(), values.len());
        for &value in &values {
            let (bytes, _, offset) = cache.allocate_timestamp(value);
            bytes.copy_from_slice(&offset.to_ne_bytes());
            cache.update_mask_after_append();
        }
        let frame = &cache.active;
        let score = |stride| {
            let (under, over) = timestamp_errors(frame, stride);
            under.max(over)
        };
        let fitted = minimax_timestamp_stride(frame);
        let best = (0..=u16::MAX)
            .map(|bits| score(FloatU16::from_bits(bits)))
            .min()
            .unwrap();
        assert_eq!(score(fitted), best, "{values:?}");
        assert!(score(fitted) <= score(endpoint_timestamp_stride(frame)));
    }
}

#[test]
fn timestamp_mask_search_is_bounded_and_never_worse_than_endpoint() {
    let mut cases = vec![
        vec![0, 0, 0],
        vec![0, 2, 3, 6], // Minimax has a larger mask despite smaller worst error.
        vec![0, 90, 100, 101],
        vec![0, 9, 0],
        vec![0, 0, 5003],
        vec![0, u64::MAX, 2],
        vec![0, FloatU16::MAX / 2 + 1],
    ];
    // Deterministic bursty and irregular blocks exercise the nonmonotonic objective.
    let mut state = 7u64;
    for length in 2..=40 {
        let mut values = vec![0];
        for _ in 1..length {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            values.push(state % 100_000);
        }
        cases.push(values);
    }
    let mut improvements = 0;
    for values in cases {
        let mut cache = BlockCache::new(8, values.len(), values.len());
        for &value in &values {
            let (bytes, _, offset) = cache.allocate_timestamp(value);
            bytes.copy_from_slice(&offset.to_ne_bytes());
            cache.update_mask_after_append();
        }
        let frame = &cache.active;
        let endpoint = endpoint_timestamp_stride(frame);
        let baseline = timestamp_mask_cost(timestamp_mask(frame, endpoint));
        for budget in [0, 1, 4, 16, 64] {
            let selected = mask_timestamp_stride(frame, budget);
            let cost = timestamp_mask_cost(timestamp_mask(frame, selected));
            assert!(cost <= baseline, "{values:?}, budget={budget}");
            if budget <= 1 {
                assert_eq!(selected, endpoint);
            }
            improvements += usize::from(cost < baseline);
        }
        if TIMESTAMP_MASK_STRIDE {
            let selected = mask_timestamp_stride(frame, TIMESTAMP_MASK_CANDIDATES);
            let expected_mask = timestamp_mask(frame, selected);
            cache.cycle();
            let block = &cache.ring[0];
            // Verify the scored mask is the actual archive mask, not a proxy.
            assert_eq!(block.data.metadata.timestamp_stride, selected);
            assert_eq!(&block.data.frame.mask()[..8], &expected_mask.to_ne_bytes());
        }
    }
    assert!(improvements > 0);
}
