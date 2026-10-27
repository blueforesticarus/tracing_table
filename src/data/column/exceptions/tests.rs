//! Verify the exact wire layout and the code-only zero policy independently of
//! the general selector, so cheaper competing codecs cannot hide coverage.
use super::*;
use crate::data::{
    blockbuffer::ArchivedFrame,
    column::{frame_bits, read_word, rewrite, write_word},
    dataframe::Frame,
};

fn fitted(
    values: &[u128],
    width: usize,
    zero_special: bool,
) -> (Box<ArchivedFrame>, SmallVec<[u8; 24]>) {
    let mut frame = Frame::new(width, values.len());
    for &value in values {
        let mut row = [0; 16];
        write_word(&mut row[..width], value);
        frame.push(&row[..width]);
    }
    let original = read_word(frame.mask());
    let cost = Cost {
        frame: &frame,
        total_bits: frame_bits(&frame),
        original_bits: varying_bits(original, width),
        footer: 1,
        width,
    };
    let mut candidate = Candidate {
        record: SmallVec::new(),
        encoded: SmallVec::new(),
        mask: original,
        size: cost.size(cost.original_bits, 0),
    };
    ExceptionEncoding::fit(&mut candidate, values, &cost, zero_special);
    assert_eq!(candidate.record.first(), Some(&TAG));
    rewrite(
        &mut frame,
        crate::data::column::Layout::new(0, width, false),
        |_, row, _| candidate.encoded[row],
    );
    (ArchivedFrame::new_from(&frame), candidate.record)
}

#[test]
fn fitted_exceptions_preserve_every_native_width_and_zero_policy() {
    for width in [1, 2, 4, 8, 16] {
        if width == 1 && !cfg!(feature = "bit-compression") {
            // A varying one-byte low column cannot get narrower in byte mode.
            continue;
        }
        for zero_special in [false, true] {
            let base = 1u128 << (width * 8 - 2);
            let outlier = 1u128 << (width * 8 - 3);
            let mut state = 7u64;
            let mut values: Vec<_> = (0..1024)
                .map(|_| {
                    state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                    base + u128::from((state >> 32) % 31)
                })
                .collect();
            for row in [0, 55, 203] {
                values[row] = base + outlier + 3;
            }
            if zero_special {
                values[1] = 0;
            }
            let (frame, record) = fitted(&values, width, zero_special);
            let codec = ExceptionEncoding::new(&record, width, values.len());
            let minimum = *values
                .iter()
                .filter(|&&v| !zero_special || v != 0)
                .min()
                .unwrap();
            assert_eq!(read_le(&record[4..4 + width]), minimum);
            let first = read_word(frame.first());
            assert_eq!(first, normalize(values[0], minimum, zero_special).unwrap());
            for row in (0..values.len()).rev() {
                let mut physical = [0; 16];
                frame.read_row(row, &mut physical[..width]);
                let encoded = read_word(&physical[..width]);
                assert_eq!(codec.decode(encoded, row, zero_special), values[row]);
                assert_eq!(
                    codec.encode(values[row], row, first, zero_special),
                    Some(encoded)
                );
            }
        }
    }
}

#[test]
fn header_flags_and_zero_width_patches_have_no_zero_policy_byte() {
    for bitmap in [false, true] {
        // Width two, S=4, high base=7, row positions {0,3}, no patch bits.
        let mut record = SmallVec::<[u8; 24]>::from_slice(&[
            TAG,
            4 | ((bitmap as u8) << 7),
            2,
            128,
            100,
            0,
            7,
            0,
        ]);
        if bitmap {
            record.push(0b1001);
        } else {
            record.push(0b1100);
        }
        let codec = ExceptionEncoding::new(&record, 2, 4);
        assert_eq!(record.len(), 9);
        assert_eq!(codec.decode(3, 0, false), 100 + (7 << 4) + 3);
        assert_eq!(codec.decode(0, 1, false), 100);
        assert_eq!(codec.decode(0, 1, true), 0);
        assert_eq!(codec.decode(1, 1, true), 100); // Minimum closed value.
        assert_eq!(codec.decode(3, 3, true), 100 + (7 << 4) + 2);
        assert_eq!(codec.encode(0, 1, 0, true), Some(0));
        assert_eq!(codec.encode(0, 1, 0, false), None);
    }
}

#[test]
fn bitmap_count_limit_lists_and_native_width_patches() {
    for (bitmap, count) in [(true, 255), (false, 127)] {
        let mut record = SmallVec::from_slice(&[TAG, (bitmap as u8) << 7, count as u8, 128, 1, 1]);
        if bitmap {
            record.extend_from_slice(&[255; 32]);
            record[37] = 127;
        } else {
            append_packed(&mut record, 0..count as u128, count, 8);
        }
        assert!(record.len() <= 255);
        let codec = ExceptionEncoding::new(&record, 1, 256);
        assert_eq!(codec.decode(0, count - 1, false), 2);
        assert_eq!(codec.decode(0, count, false), 1);
        assert_eq!(codec.patch_index(count - 1), Some(count - 1));
        assert_eq!(codec.patch_index(count), None);
    }
    let mut record = SmallVec::<[u8; 24]>::from_slice(&[TAG, 0, 1, 64]);
    record.extend_from_slice(&0u64.to_le_bytes());
    record.push(0); // Row zero in a two-row frame, padded to a byte.
    record.extend_from_slice(&u64::MAX.to_le_bytes());
    let codec = ExceptionEncoding::new(&record, 8, 2);
    assert_eq!(codec.decode(0, 0, false), u64::MAX as u128);
    assert_eq!(codec.decode(0, 1, false), 0);
}
