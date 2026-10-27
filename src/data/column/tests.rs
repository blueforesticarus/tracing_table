//! Production-path tests cover codec selection as well as exact reconstruction.
use super::*;
type Prediction = PredictionEncoding<false>;
// Exercise fitting helpers directly as well as the column storage boundary.
use crate::data::blockbuffer::BlockStore;
use crate::data::encoding::dictionary::dictionary_cost;
use crate::data::encoding::prediction::*;
use crate::data::util::float_u16::FloatU16;
type BlockCache = BlockStore<()>;
use streaming_iterator::StreamingIterator;

fn archive(values: &[u128], width: usize, strings: bool) -> (BlockCache, [Layout; 1]) {
    let layout = [Layout::new(1, width, strings)];
    let mut cache = BlockCache::new(width + 2, values.len(), values.len());
    for &value in values {
        let mut row = vec![23; width + 2];
        write_word(&mut row[1..1 + width], value);
        cache.allocate_row().0.copy_from_slice(&row);
        cache.update_mask_after_append();
    }
    cache.archive_active(&layout);
    (cache, layout)
}

fn check(cache: &BlockCache, layout: &[Layout], expected: &[u128]) {
    let block = &cache.ring[0].data;
    let (spec, record) = block.column_metadata(layout).entries().next().unwrap();
    let column = Column::new(&block.frame, spec, record);
    // Every selected codec must own a lossless footer parser/writer, and its
    // statically typed column must agree with the heterogeneous archive view.
    let mut footer = Footer::new();
    column
        .encoding()
        .write_footer(usize::from(spec.width), &mut footer);
    footer.resize(record_storage_bytes(footer.len()), 0);
    assert_eq!(footer.as_slice(), record);
    macro_rules! typed {
        ($encoding:expr) => {{
            let typed = Column::with_encoding(&block.frame, spec, *$encoding);
            for (row, &value) in expected.iter().enumerate() {
                assert_eq!(typed.read(row), value);
            }
        }};
    }
    match column.encoding() {
        AnyEncoding::Raw(e) => typed!(e),
        AnyEncoding::Prediction(e) => typed!(e),
        AnyEncoding::Unsigned(e) => typed!(e),
        AnyEncoding::Origin(e) => typed!(e),
        AnyEncoding::Dictionary(e) => typed!(e),
        AnyEncoding::Exceptions(e) => typed!(e),
        AnyEncoding::SplitDictionary(e) => typed!(e),
    }

    if let AnyEncoding::Prediction(p) = &column.encoding {
        if !p.row && p.stride == 0 {
            assert_eq!(
                record.len(),
                record_storage_bytes(1 + usize::from(spec.width)),
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

/// A rebuild must select exactly the codecs a fresh rollover would select on
/// the edited values, rather than retaining a stale table or raw fallback.
fn check_refit(cache: &BlockCache, expected: &[u128], width: usize, strings: bool) {
    let (fresh, layout) = archive(expected, width, strings);
    let actual = &cache.ring[0].data.frame;
    let rebuilt = &fresh.ring[0].data.frame;
    assert_eq!(actual.trailer(), rebuilt.trailer());
    assert_eq!(&actual.mask()[..], &rebuilt.mask()[..]);
    assert_eq!(actual.first(), rebuilt.first());
    check(cache, &layout, expected);
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
        assert_eq!(
            cache.ring[0]
                .data
                .column_metadata(&layout)
                .entries()
                .next()
                .unwrap()
                .1,
            &[] as &[u8]
        );
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
    for rows in [24, 256] {
        let values = irregular(17, rows, 120, 1);
        let (cache, layout) = archive(&values, 2, false);
        if rows == 256 && cfg!(feature = "bit-compression") {
            // Record padding may favor unsigned minimum/exception storage
            // over a signed origin; both must repay the same savings guard.
            assert!(matches!(tag(&cache, &layout), OFFSET | exceptions::TAG));
        } else {
            assert_eq!(tag(&cache, &layout), 0);
        }
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
            // Byte mode can now move these values into bit-packed exception
            // patches, even though its row payload cannot remove one bit.
            let expected = if cfg!(feature = "bit-compression") {
                OFFSET
            } else {
                exceptions::TAG
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
            + record_storage_bytes(3 + (count * bits).div_ceil(8))
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
        assert_eq!(record.len(), if expected == ROW { 4 } else { 6 });
    }
}

#[test]
fn general_fields_choose_exact_offset_or_custom_float_line() {
    let offset = irregular(97, 1024, (1u128 << 50) - 40, 1);
    let line: Vec<_> = (0..1024)
        .map(|row| (1u128 << 50) + row * 100000003 + row % 2)
        .collect();
    for (values, width, expected) in [(offset, 8, OFFSET), (line, 8, LINEAR)] {
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
            assert_eq!(u128::from(p.origin), values[0]);
            assert_eq!(record.len(), record_storage_bytes(1 + width + 2));
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
        check(&cache, &layout, &values);
        // Exception tables can now beat dictionaries on this sparse pattern.
        // Exercise dictionary fitting directly to retain 127/128-bit table
        // packing coverage independently of the overall selector's winner.
        let mut frame = Frame::new(16, values.len());
        for value in &values {
            frame.push(&value.to_ne_bytes());
        }
        let original = read_word(frame.mask());
        let cost = Cost {
            frame: &frame,
            total_bits: frame_bits(&frame),
            original_bits: varying_bits(original, 16),
            footer: 1,
            width: 16,
        };
        let mut selected = Candidate {
            record: SmallVec::new(),
            encoded: SmallVec::new(),
            mask: original,
            size: cost.size(cost.original_bits, 0),
        };
        select_dictionary(&mut selected, &values, &cost);
        assert!(matches!(selected.record[0], DICTIONARY | DICTIONARY_CYCLE));
        rewrite(&mut frame, Layout::new(0, 16, false), |_, row, _| {
            selected.encoded[row]
        });
        let frame = ArchivedFrame::new_from(&frame);
        let column = Column::new(&frame, Layout::new(0, 16, false), &selected.record);
        for (row, &value) in values.iter().enumerate() {
            assert_eq!(column.read(row), value);
        }
    }
}

#[test]
fn exceptions_preserve_open_rows_low_edits_and_rebuild_for_late_closes() {
    let base = 1u128 << 40;
    let mut state = 7u64;
    let mut expected: Vec<_> = (0..1024)
        .map(|_| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            base + u128::from((state >> 32) % 31)
        })
        .collect();
    for row in [0, 55, 203] {
        expected[row] = base + (1 << 30) + 17;
    }
    expected[1] = 0; // Open duration among already closed, clustered durations.
    let (mut cache, layout) = archive(&expected, 8, false);
    assert_eq!(tag(&cache, &layout), exceptions::TAG);
    check(&cache, &layout, &expected);
    let footer = cache.ring[0].data.frame.trailer().to_vec();
    // A low-only edit to row zero must preserve its full first-offset high bits.
    expected[0] += 1;
    write_word(
        &mut cache.get_mut(1, &layout).unwrap().1.get_mut()[1..9],
        expected[0],
    );
    assert_eq!(cache.ring[0].data.frame.trailer(), footer);
    check(&cache, &layout, &expected);
    // A late close with a previously absent high value must refit the table,
    // retaining every other row, including the exceptional first row.
    expected[1] = base + (1 << 35);
    {
        let block = &cache.ring[0].data;
        let (column, record) = block.column_metadata(&layout).entries().next().unwrap();
        assert!(
            Column::new(&block.frame, column, record)
                .encode(expected[1], 1)
                .is_none(),
            "record {record:?}"
        );
    }
    write_word(
        &mut cache.get_mut(2, &layout).unwrap().1.get_mut()[1..9],
        expected[1],
    );
    assert_eq!(tag(&cache, &layout), exceptions::TAG);
    check_refit(&cache, &expected, 8, false);
}

#[test]
fn plain_archived_duration_close_refits_like_rollover() {
    // Entirely open blocks have no column codec. The first late close changes
    // omitted constant bits and must enter the same fitter as a table miss.
    let mut expected = vec![0u128; 1024];
    let (mut cache, layout) = archive(&expected, 8, false);
    assert_eq!(tag(&cache, &layout), 0);
    expected[73] = (1 << 40) + 17;
    write_word(
        &mut cache.get_mut(74, &layout).unwrap().1.get_mut()[1..9],
        expected[73],
    );
    assert_ne!(tag(&cache, &layout), 0);
    check_refit(&cache, &expected, 8, false);
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
    // This known entry widens the predicted-key mask, so the rebuild refits
    // discovery-order keys just as a fresh rollover of the edited rows would.
    values[100] = entries[9];
    write_word(
        &mut cache.get_mut(101, &layout).unwrap().1.get_mut()[1..9],
        values[100],
    );
    check_refit(&cache, &values, 8, false);
    // New entries must refit this dictionary variant too, preserving all rows.
    values[101] = u128::from(u64::MAX);
    write_word(
        &mut cache.get_mut(102, &layout).unwrap().1.get_mut()[1..9],
        values[101],
    );
    check_refit(&cache, &values, 8, false);
}

#[test]
fn directory_follows_stored_fields_and_counts_back_from_its_boundary() {
    // Span-shaped rows keep timestamp, duration and payload in dyntuple order.
    // Different footer lengths catch readers that assume a fixed record stride.
    let columns = [
        Layout::unsigned(0, 8),
        Layout::new(8, 8, false),
        Layout::new(16, 2, true),
    ];
    let mut cache = BlockCache::new(18, 512, 512);
    for index in 0..512u128 {
        let mut row = [0; 18];
        write_word(&mut row[..8], 1000 + index * 3);
        write_word(&mut row[8..16], (1 << 40) + index * 17);
        write_word(&mut row[16..], 2000 + index);
        cache.allocate_row().0.copy_from_slice(&row);
        cache.update_mask_after_append();
    }
    cache.archive_active(&columns);
    let block = &cache.ring[0].data;
    let trailer = block.frame.trailer();
    let directory = trailer.len() - columns.len();
    let mut end = directory;
    for (index, (layout, record)) in block.column_metadata(&columns).entries().enumerate() {
        assert_eq!(layout, columns[index]);
        let start = end - usize::from(trailer[directory + index]) * 2;
        assert_eq!(record.as_ptr(), trailer[start..end].as_ptr());
        assert_eq!(record.len(), end - start);
        end = start;
    }
    assert_eq!(end, 0);
    assert_ne!(trailer[directory], trailer[directory + 2]);
    for index in 0..512 {
        let mut row = [0; 18];
        block.read_row(index, &mut row, &columns);
        assert_eq!(read_word(&row[..8]), 1000 + index as u128 * 3);
        assert_eq!(read_word(&row[8..16]), (1 << 40) + index as u128 * 17);
        assert_eq!(read_word(&row[16..]), 2000 + index as u128);
    }
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
        cache.allocate_row().0.copy_from_slice(&row);
        cache.update_mask_after_append();
    }
    cache.archive_active(&columns);
    let block = &cache.ring[0].data;
    assert!(block.frame.trailer().len() > 510);
    assert!(
        Metadata::split(block.frame.trailer(), &columns)
            .0
            .is_empty()
    );
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
fn changing_dictionary_row_zero_rebuilds_without_changing_other_rows() {
    let mut expected = irregular(32, 1024, 1000, 900);
    let (mut cache, layout) = archive(&expected, 2, true);
    expected[0] = expected[1];
    write_word(
        &mut cache.get_mut(1, &layout).unwrap().1.get_mut()[1..3],
        expected[0],
    );
    check_refit(&cache, &expected, 2, true);
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
    assert!(record.len() <= MAX_RECORD_BYTES);
    check(&cache, &layout, &values);
    assert!(std::panic::catch_unwind(|| Metadata::split(&[255], &layout)).is_err());
}

#[test]
fn word_sized_directory_round_trips_a_510_byte_record() {
    // 39 entries * 104 bits = 507 table bytes, plus the three-byte dictionary
    // header. This exactly fills size byte 255, well beyond the old limit.
    let mut distinct = vec![0u128];
    let mut state = 7u128;
    for _ in 1..39 {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        distinct.push((1u128 << 103) | (state & ((1u128 << 103) - 1)));
    }
    let values: Vec<_> = irregular(39, 1024, 0, 1)
        .into_iter()
        .map(|id| distinct[id as usize])
        .collect();
    let (cache, layout) = archive(&values, 16, false);
    let block = &cache.ring[0].data;
    let (_, record) = block.column_metadata(&layout).entries().next().unwrap();
    assert_eq!(record[0], DICTIONARY);
    assert_eq!(record.len(), 510);
    assert_eq!(block.frame.trailer().last(), Some(&255));
    check(&cache, &layout, &values);
}

#[test]
fn odd_codec_records_have_one_zero_padding_byte_and_word_directory_units() {
    let values: Vec<_> = (0..1024).map(|row| 2000 + row).collect();
    let (cache, layout) = archive(&values, 2, true);
    let block = &cache.ring[0].data;
    let (_, record) = block.column_metadata(&layout).entries().next().unwrap();
    assert_eq!(record, &[ROW, 0xd0, 7, 0]); // Three bytes of row-codec data.
    assert_eq!(block.frame.trailer().last(), Some(&2)); // Four bytes / two.
    check(&cache, &layout, &values);
    assert!(std::panic::catch_unwind(|| Prediction::parse(&[ROW, 0xd0, 7, 1], 2)).is_err());
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
            let (bytes, _) = cache.allocate_row();
            bytes.copy_from_slice(&value.to_ne_bytes());
            cache.update_mask_after_append();
        }
        let frame = &cache.active;
        let score = |stride| {
            let (under, over) = unsigned_errors(frame, Layout::unsigned(0, 8), stride);
            under.max(over)
        };
        let fitted = minimax_unsigned_stride(frame, Layout::unsigned(0, 8));
        let best = (0..=u16::MAX)
            .map(|bits| score(FloatU16::from_bits(bits)))
            .min()
            .unwrap();
        assert_eq!(score(fitted), best, "{values:?}");
        assert!(score(fitted) <= score(endpoint_unsigned_stride(frame, Layout::unsigned(0, 8))));
    }
}

#[test]
fn unsigned_mask_search_is_bounded_and_never_worse_than_endpoint() {
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
            let (bytes, _) = cache.allocate_row();
            bytes.copy_from_slice(&value.to_ne_bytes());
            cache.update_mask_after_append();
        }
        let frame = &cache.active;
        let endpoint = endpoint_unsigned_stride(frame, Layout::unsigned(0, 8));
        let baseline = unsigned_mask_cost(unsigned_mask(frame, Layout::unsigned(0, 8), endpoint));
        for budget in [0, 1, 4, 16, 64] {
            let selected = mask_unsigned_stride(frame, Layout::unsigned(0, 8), budget);
            let cost = unsigned_mask_cost(unsigned_mask(frame, Layout::unsigned(0, 8), selected));
            assert!(cost <= baseline, "{values:?}, budget={budget}");
            if budget <= 1 {
                assert_eq!(selected, endpoint);
            }
            improvements += usize::from(cost < baseline);
        }
        if UNSIGNED_MASK_STRIDE {
            let selected =
                mask_unsigned_stride(frame, Layout::unsigned(0, 8), UNSIGNED_MASK_CANDIDATES);
            let expected_mask = unsigned_mask(frame, Layout::unsigned(0, 8), selected);
            cache.archive_active(&[Layout::unsigned(0, 8)]);
            let block = &cache.ring[0];
            // Verify the scored mask is the actual archive mask, not a proxy.
            assert_eq!(
                u16::from_le_bytes(
                    block
                        .data
                        .column_metadata(&[Layout::unsigned(0, 8)])
                        .entries()
                        .next()
                        .unwrap()
                        .1[8..10]
                        .try_into()
                        .unwrap()
                ),
                selected.to_bits()
            );
            assert_eq!(&block.data.frame.mask()[..8], &expected_mask.to_ne_bytes());
        }
    }
    assert!(improvements > 0);
}

#[test]
fn unsigned_footer_owns_full_origin_and_stride_without_descriptor_time() {
    let values = [u64::MAX - 20, u64::MAX - 19, u64::MAX - 1];
    let layout = [Layout::unsigned(0, 8)];
    let mut store = BlockStore::<()>::new(8, values.len(), values.len());
    for value in values {
        store.allocate_row().0.copy_from_slice(&value.to_ne_bytes());
        store.update_mask_after_append();
    }
    store.archive_active(&layout);
    let block = &store.ring[0];
    let (_, record) = block
        .data
        .column_metadata(&layout)
        .entries()
        .next()
        .unwrap();
    assert_eq!(record.len(), 10);
    assert_eq!(&record[..8], &values[0].to_le_bytes());
    let typed = Column::<PredictionEncoding<true>>::parse(&block.data.frame, layout[0], record);
    let dynamic: Column = Column::parse(&block.data.frame, layout[0], record);
    let mut footer = Footer::new();
    typed.encoding().write_footer(8, &mut footer);
    assert_eq!(footer.as_slice(), record);
    for (row, value) in values.into_iter().enumerate() {
        assert_eq!(typed.read(row), value as u128);
        assert_eq!(dynamic.read(row), value as u128);
    }
    assert_eq!(
        size_of::<crate::data::blockbuffer::Descriptor>(),
        size_of::<usize>()
    );
}

#[test]
fn native_predictor_numerator_matches_full_precision_at_overflow_boundaries() {
    // Exercise both sides of the native numerator's overflow guard. A rounded
    // numerator can overflow even when the unrounded multiplication still fits.
    for stride in [0, 1, 2, FloatU16::MAX, u64::MAX / 2, u64::MAX] {
        for length in [1, 2, 65_535, usize::MAX] {
            for row in [0, length / 2, length - 1] {
                let exact = ((u128::from(stride) * row as u128 + length as u128 / 2)
                    / length as u128) as u64;
                assert_eq!(advance(stride, row, length), exact);
            }
        }
    }
}

#[test]
fn wide_columns_skip_u64_prediction_and_exceptions_without_truncation() {
    // More than 64 distinct values also rules out dictionary fallback here.
    let line: Vec<_> = (0..1024u128)
        .map(|row| (1u128 << 110) + row * 100000003 + row % 2)
        .collect();
    let mut outliers: Vec<_> = (0..1024u128).map(|row| (1u128 << 110) + row % 97).collect();
    outliers[55] += 1u128 << 90;
    for values in [line, outliers] {
        let (cache, layout) = archive(&values, 16, false);
        assert_eq!(tag(&cache, &layout), 0);
        check(&cache, &layout, &values);
    }
    // Explicit parsing must also refuse the unsupported width.
    let mut record = vec![OFFSET];
    record.extend_from_slice(&(1u128 << 110).to_le_bytes());
    assert!(std::panic::catch_unwind(|| Prediction::parse(&record, 16)).is_err());
}
