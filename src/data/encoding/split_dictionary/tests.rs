//! Exercise actual serialized codecs and frame reads, not fitting-only formulas.
use super::*;
use crate::data::{
    blockbuffer::ArchivedFrame,
    column::{self, Column},
    dataframe::Frame,
};

fn fitted(values: &[u64], scope: Scope) -> (Footer, SmallVec<[u128; 256]>, u128) {
    let layout = if matches!(scope, Scope::Values) {
        Layout::new(1, 8, false)
    } else {
        Layout::tagged_origin(1)
    };
    let mut frame = Frame::new(10, values.len());
    for &value in values {
        let mut row = [0xa5; 10];
        row[1..9].copy_from_slice(&value.to_ne_bytes());
        frame.push(&row);
    }
    let cost = Cost {
        frame: &frame,
        total_bits: frame_bits(&frame),
        original_bits: varying_bits(column::read_word(&frame.mask()[1..9]), 8),
        footer: 1,
        width: 8,
    };
    let mut candidate = Candidate {
        record: Footer::new(),
        encoded: SmallVec::new(),
        mask: column::read_word(&frame.mask()[1..9]),
        size: cost.size(cost.original_bits, 0),
    };
    SplitDictionaryEncoding::fit(
        &mut candidate,
        &values.iter().map(|&v| u128::from(v)).collect::<Vec<_>>(),
        &cost,
        scope,
        1,
    );
    assert!(
        !candidate.record.is_empty(),
        "synthetic bursts should pay for their codec"
    );
    let record = candidate.record.clone();
    column::rewrite(&mut frame, layout, |_, row, _| candidate.encoded[row]);
    let archived = ArchivedFrame::new_from(&frame);
    let typed = Column::<SplitDictionaryEncoding>::parse(&archived, layout, &record);
    let generic = Column::new(&archived, layout, &record);
    let mut written = Footer::new();
    typed.encoding().write_footer(8, &mut written);
    assert_eq!(written, record);
    for row in (0..values.len()).rev() {
        assert_eq!(typed.read(row), u128::from(values[row]));
        assert_eq!(generic.read(row), u128::from(values[row]));
        assert_eq!(
            typed.encode(u128::from(values[row]), row),
            Some(candidate.encoded[row])
        );
        let mut bytes = [0; 10];
        archived.read_row(row, &mut bytes);
        assert_eq!((bytes[0], bytes[9]), (0xa5, 0xa5));
    }
    assert_eq!(
        typed.encoding().origin(),
        match scope {
            Scope::Values => typed.encoding().numeric_origin,
            Scope::Absolute(v) | Scope::Relative(v) => v,
        }
    );
    (record, candidate.encoded, candidate.mask)
}

#[test]
fn runs_local_trends_phases_and_entry_tables_round_trip_in_reverse() {
    let values: Vec<_> = (0..192u64)
        .map(|i| 1_000_000 + i / 24 * 200_000 + i % 24 * 113 + i % 7)
        .collect();
    let (record, _, _) = fitted(&values, Scope::Absolute(values[0]));
    let model = SplitDictionaryEncoding::parse(&record, Layout::tagged_origin(1), values.len());
    // The probe's bit-cost winner uses runs. Byte packing can keep key bits
    // inside a byte already paid for by the fine value and prefer a stride.
    if cfg!(feature = "bit-compression") {
        assert!(model.uses_runs());
    }
    assert!(
        model.source_stride > 0,
        "local trend is required for the expected burst savings"
    );
    assert_eq!(record[7] >> 4, ABSOLUTE_TAG);
}

#[test]
fn nonmonotone_parent_ages_preserve_a_separate_absolute_origin() {
    let values: Vec<_> = (0..192u64)
        .map(|i| {
            if i % 31 == 0 {
                300_000 + i * 3
            } else {
                50 + i % 19 * 17
            }
        })
        .collect();
    let (record, _, _) = fitted(&values, Scope::Relative(10_000_000));
    let model = SplitDictionaryEncoding::parse(&record, Layout::tagged_origin(1), values.len());
    assert!(model.is_relative());
    assert_eq!(model.origin(), 10_000_000);
    assert_eq!(record[7] >> 4, RELATIVE_TAG);
    assert_ne!(model.origin(), model.numeric_origin);
}

#[test]
fn ordinary_u64_encoding_handles_zero_and_rejects_unknown_dictionary_edits() {
    let values: Vec<_> = (0..192u64).map(|i| i / 24 * 100_000 + i % 24 * 4).collect();
    let (record, words, _) = fitted(&values, Scope::Values);
    assert_eq!(record[0], TAG);
    let layout = Layout::new(0, 8, false);
    let mut frame = Frame::new(8, values.len());
    for word in words {
        frame.push(&(word as u64).to_ne_bytes());
    }
    let frame = ArchivedFrame::new_from(&frame);
    let column = Column::<SplitDictionaryEncoding>::parse(&frame, layout, &record);
    assert_eq!(column.encode(u128::from(u64::MAX), 10), None);
    assert_eq!(column.encode(9, 0), None, "row zero owns the phase");
}

#[test]
fn extreme_ranges_skip_split_fitting_without_losing_unsigned_values() {
    let values = [0, u64::MAX, 1, 1 << 63];
    let mut frame = Frame::new(8, values.len());
    for value in values {
        frame.push(&value.to_ne_bytes());
    }
    let cost = Cost {
        frame: &frame,
        total_bits: frame_bits(&frame),
        original_bits: 64,
        footer: 0,
        width: 8,
    };
    let mut candidate = Candidate {
        record: Footer::new(),
        encoded: SmallVec::new(),
        mask: u128::from(u64::MAX),
        size: cost.size(64, 0),
    };
    SplitDictionaryEncoding::fit(
        &mut candidate,
        &values.iter().map(|&v| u128::from(v)).collect::<Vec<_>>(),
        &cost,
        Scope::Values,
        0,
    );
    assert!(candidate.record.is_empty());
    let frame = ArchivedFrame::new_from(&frame);
    let column = Column::new(&frame, Layout::new(0, 8, false), &[]);
    for row in (0..values.len()).rev() {
        assert_eq!(column.read(row), u128::from(values[row]));
    }
}

#[test]
fn timestamp_selection_uses_split_not_exceptions_for_sparse_parent_ages() {
    let columns = [Layout::tagged_origin(0)];
    // Vary outlier magnitudes too: a single repeated power of two already
    // costs only one raw mask bit and should correctly retain the raw codec.
    let values: Vec<u64> = (0..192)
        .map(|r| if r % 31 == 0 { (1 << 22) + r * 7919 } else { 0 })
        .collect();
    let mut frame = Frame::new(8, values.len());
    for value in &values {
        frame.push(&value.to_ne_bytes());
    }
    let mut trailer = Vec::new();
    column::fit_relative(&mut frame, &mut trailer, &columns, 123_000);
    let record = column::Metadata::split(&trailer, &columns)
        .1
        .entries()
        .next()
        .unwrap()
        .1;
    assert_eq!(record[7] >> 4, RELATIVE_TAG);
    let frame = ArchivedFrame::new_from(&frame);
    let codec = Column::new(&frame, columns[0], record);
    assert!(codec.encoding().is_split_dictionary());
    for r in (0..values.len()).rev() {
        assert_eq!(codec.read(r), u128::from(values[r]));
    }
}

#[test]
fn timestamp_footer_and_values_survive_physical_and_logical_payload_refits() {
    use crate::data::blockbuffer::BlockStore;
    let n = 192;
    let columns = [Layout::tagged_origin(0), Layout::new(8, 8, false)];
    let times: Vec<u64> = (0..n as u64)
        .map(|r| 1_000_000 + r / 24 * 200_000 + r % 24 * 113)
        .collect();
    let mut payload = vec![0u64; n];
    let mut store = BlockStore::<()>::new(16, n, n);
    for &ts in &times {
        let mut row = [0; 16];
        row[..8].copy_from_slice(&ts.to_ne_bytes());
        store.allocate_row().0.copy_from_slice(&row);
        store.update_mask_after_append();
    }
    store.archive_active(&columns);
    let footer = store.ring[0]
        .data
        .column_metadata(&columns)
        .entries()
        .next()
        .unwrap()
        .1
        .to_vec();
    assert!(SplitDictionaryEncoding::is_tagged(&footer));
    for (index, physical) in [(0, true), (77, false), (129, true)] {
        payload[index] = u64::MAX - index as u64;
        let mut row = if physical {
            store.get_mut_physical(index + 1, &columns).unwrap().1
        } else {
            store.get_mut(index + 1, &columns).unwrap().1
        };
        row.get_mut()[8..16].copy_from_slice(&payload[index].to_ne_bytes());
        drop(row);
        let block = &store.ring[0].data;
        assert_eq!(
            block.column_metadata(&columns).entries().next().unwrap().1,
            footer
        );
        for r in (0..n).rev() {
            let mut logical = [0; 16];
            block.read_row(r, &mut logical, &columns);
            assert_eq!(
                u64::from_ne_bytes(logical[..8].try_into().unwrap()),
                times[r]
            );
            assert_eq!(
                u64::from_ne_bytes(logical[8..].try_into().unwrap()),
                payload[r]
            );
        }
    }
}
