//! Promotion must preserve prefixes, archived bytes, IDs, and monotonic learning.
use super::*;
use crate::data::blockbuffer::BlockCache;
use crate::data::dataframe::Frame;
use streaming_iterator::StreamingIterator;

fn layout(count: usize, signed: bool) -> PhysicalLayout {
    let fields = (0..count)
        .map(|i| {
            let logical = Layout::new(1 + i * 8, 8, false);
            Field {
                logical,
                stored: logical,
                source: i.try_into().unwrap(),
                family: 1,
                signed,
                integer: true,
                float64: false,
            }
        })
        .collect();
    PhysicalLayout::new(1, 1 + count * 8, false, fields, Default::default())
}
fn row(values: &[u64]) -> Vec<u8> {
    let mut row = vec![19];
    for value in values {
        row.extend_from_slice(&value.to_ne_bytes());
    }
    row
}
fn append(cache: &mut BlockCache, values: &[u64]) {
    let logical = row(values);
    cache.promote(&logical, 0);
    let p = cache.layouts.last().unwrap();
    let mut physical = vec![0; p.unit()];
    p.pack(&logical, &mut physical);
    cache.push(&physical, 0);
}

fn float_layout(count: usize, floats: bool) -> PhysicalLayout {
    let mut fields = layout(count, false).fields;
    for field in &mut fields {
        field.integer = false;
        field.float64 = true;
        field.stored = field.logical;
    }
    PhysicalLayout::new(
        1,
        1 + count * 8,
        false,
        fields.into_iter().collect(),
        LayoutPolicy {
            floats,
            ..Default::default()
        },
    )
}

#[test]
fn float_width_promotion_preserves_aliases_prefixes_and_archived_history() {
    let p = float_layout(2, true);
    let mut cache = BlockCache::new(p.unit(), 64, 64);
    cache.configure_layout(p, 64);
    let expected = [
        [1.5f64.to_bits(), 1.5f64.to_bits()],
        [0.0f64.to_bits(), (-0.0f64).to_bits()],
        [2.5f64.to_bits(), 3.5f64.to_bits()],
        [0.1f64.to_bits(), 4.5f64.to_bits()],
        [1.0f64.to_bits(), 5.5f64.to_bits()],
    ];
    append(&mut cache, &expected[0]);
    assert_eq!(cache.active.unit, 5); // two equal f64s share one f32 word
    assert_eq!(cache.layouts[0].fields[1].source, 0);
    append(&mut cache, &expected[1]);
    assert_eq!(cache.active.unit, 9); // signed zero splits the shared column
    cache.cycle_with_columns(&[]);
    let archived = &cache.ring[0].data.frame;
    let mut before = Frame::new(archived.unit(), archived.len());
    archived.uncompact(&mut before);
    let trailer = archived.trailer().to_vec();
    append(&mut cache, &expected[2]);
    append(&mut cache, &expected[3]);
    assert_eq!(cache.active.unit, 13); // only the inexact column promotes
    assert_eq!(cache.layouts.len(), 2);
    assert_eq!(cache.layouts[1].block_index, 1);
    append(&mut cache, &expected[4]);
    assert_eq!(cache.active.unit, 13); // promotion never narrows again
    // Compare archived decoded rows and trailer: promotion only rewrites active data.
    let current = &cache.ring[0].data.frame;
    let mut after = Frame::new(current.unit(), current.len());
    current.uncompact(&mut after);
    assert_eq!(before.buffer, after.buffer);
    assert_eq!(trailer, current.trailer());
    cache.cycle_with_columns(&[]);
    let mut rows = cache.iter(&[]);
    for values in expected {
        assert_eq!(rows.next().unwrap(), row(&values));
    }
    assert!(rows.next().is_none());
}

#[test]
fn float_narrowing_is_bit_exact_for_extremes_and_can_be_disabled() {
    // Fresh layouts distinguish immediate promotion from a prior wide column.
    for bits in [
        0.0f64.to_bits(),
        (-0.0f64).to_bits(),
        (f32::from_bits(1) as f64).to_bits(),
        (f32::MAX as f64).to_bits(),
        f64::INFINITY.to_bits(),
        f64::NEG_INFINITY.to_bits(),
        0.1f64.to_bits(),
        16_777_217f64.to_bits(),
        1,
        f64::MAX.to_bits(),
        0x7ff8000000001234,
        0x7ff0000000001234,
    ] {
        for floats in [false, true] {
            let p = float_layout(1, floats);
            let mut cache = BlockCache::new(p.unit(), 16, 16);
            cache.configure_layout(p, 16);
            append(&mut cache, &[bits]);
            let value = f64::from_bits(bits);
            let exact = ((value as f32) as f64).to_bits() == bits;
            assert_eq!(cache.active.unit, if floats && exact { 5 } else { 9 });
            // Force archive codecs to see both repeated and varying float words.
            append(&mut cache, &[bits]);
            append(&mut cache, &[(-0.0f64).to_bits()]);
            cache.cycle_with_columns(&[]);
            let mut rows = cache.iter(&[]);
            assert_eq!(rows.next().unwrap(), row(&[bits]));
            assert_eq!(rows.next().unwrap(), row(&[bits]));
            assert_eq!(rows.next().unwrap(), row(&[(-0.0f64).to_bits()]));
        }
    }
}

#[test]
fn history_coalesces_first_block_and_keeps_archives_unchanged() {
    let p = layout(3, false);
    let mut cache = BlockCache::new(p.unit(), 8, 8);
    cache.configure_layout(p, 8);
    append(&mut cache, &[1, 1, 1]);
    append(&mut cache, &[2, 2, 2]);
    assert_eq!(cache.active.unit, 3);
    assert_eq!(cache.layouts.len(), 1);
    assert!(!cache.layouts.spilled());
    cache.cycle_with_columns(&[]);
    let snapshot = |cache: &BlockCache| {
        let archived = &cache.ring[0].data.frame;
        let mut raw = Frame::new(archived.unit(), archived.len());
        archived.uncompact(&mut raw);
        (raw.buffer.to_vec(), archived.trailer().to_vec())
    };
    let archived = snapshot(&cache);

    append(&mut cache, &[3, 4, 4]);
    assert_eq!(cache.layouts.len(), 2);
    assert_eq!(cache.layouts[1].block_index, 1);
    assert_eq!(cache.layouts[1].fields[2].source, 1);
    append(&mut cache, &[70_000, 5, 6]);
    assert_eq!(cache.layouts.len(), 2); // all changes in block 1 share one entry
    assert_eq!(cache.active.unit, 9);
    assert_eq!(snapshot(&cache), archived);
    cache.cycle_with_columns(&[]);
    append(&mut cache, &[1, 1, 1]);
    assert_eq!(cache.layouts.len(), 2); // no narrowing or re-sharing in block 2
    assert_eq!(cache.active.unit, 9);
    let mut rows = cache.iter(&[]);
    for values in [[1, 1, 1], [2, 2, 2], [3, 4, 4], [70_000, 5, 6], [1, 1, 1]] {
        assert_eq!(rows.next().unwrap(), row(&values));
    }
    assert!(rows.next().is_none());
    drop(rows);
    cache.drop_chunk(1).unwrap();
    assert_eq!(cache.iter(&[]).next().unwrap(), row(&[3, 4, 4]));
}

#[test]
fn signed_values_and_u64_extremes_promote_without_losing_bits() {
    for signed in [true, false] {
        let p = layout(1, signed);
        let mut cache = BlockCache::new(p.unit(), 16, 16);
        cache.configure_layout(p, 16);
        let values = if signed {
            vec![
                -1i64 as u64,
                0,
                i16::MIN as u64,
                i16::MAX as u64,
                -32769i64 as u64,
                i32::MIN as u64,
                i64::MIN as u64,
                i64::MAX as u64,
            ]
        } else {
            vec![
                0,
                u16::MAX as u64,
                u16::MAX as u64 + 1,
                u32::MAX as u64,
                u32::MAX as u64 + 1,
                u64::MAX,
            ]
        };
        for &value in &values {
            append(&mut cache, &[value]);
        }
        assert_eq!(cache.layouts.len(), 1);
        assert_eq!(cache.active.unit, 9);
        cache.cycle_with_columns(&[]);
        let mut rows = cache.iter(&[]);
        for value in values {
            assert_eq!(rows.next().unwrap(), row(&[value]));
        }
    }
}

#[test]
fn signed_zigzag_covers_every_width_and_extreme_with_or_without_narrowing() {
    // Verify the physical representation, including full-width i128 values,
    // independently of optional archive predictors and integer-width policy.
    for width in [1usize, 2, 4, 8, 16] {
        let bits = width * 8;
        let min = i128::MIN >> (128 - bits);
        let max = i128::MAX >> (128 - bits);
        let values = if width == 1 {
            (min..=max).collect::<Vec<_>>()
        } else {
            vec![min, min + 1, -1, 0, 1, max - 1, max]
        };
        for integers in [false, true] {
            let logical = Layout::new(1, width, false);
            let mut p = PhysicalLayout::new(
                1,
                1 + width,
                false,
                smallvec::smallvec![Field {
                    logical,
                    stored: logical,
                    source: 0,
                    family: 1,
                    signed: true,
                    integer: true,
                    float64: false,
                }],
                LayoutPolicy {
                    sharing: true,
                    integers,
                    floats: true,
                },
            );
            for &value in &values {
                let mut row = vec![19; 1 + width];
                write_word(&mut row[1..], value as u128);
                if let Some(next) = p.promoted(&row, false) {
                    p = next;
                }
                let mut packed = vec![0; p.unit()];
                p.pack(&row, &mut packed);
                // Magnitudes interleave: 0, -1, +1, -2, +2, ... .
                let expected = if value < 0 {
                    value.unsigned_abs().wrapping_mul(2).wrapping_sub(1)
                } else {
                    value as u128 * 2
                };
                assert_eq!(read_word(&packed[1..]), expected);
                let mut decoded = vec![0; row.len()];
                p.unpack(&packed, &mut decoded);
                assert_eq!(decoded, row, "width {width}, value {value}");
            }
        }
    }
}

#[test]
fn signed_zigzag_survives_sharing_active_and_archived_edits() {
    let p = layout(2, true);
    let mut cache = BlockCache::new(p.unit(), 256, 256);
    cache.configure_layout(p, 256);
    let mut expected = Vec::new();
    for i in 0..128 {
        // An irregular sign pattern exercises the ordinary raw ZigZag fallback.
        let value = if (i * 37 % 101) < 50 { -1i64 as u64 } else { 1 };
        append(&mut cache, &[value, value]);
        expected.push(row(&[value, value]));
    }
    assert_eq!(cache.layouts[0].fields[1].source, 0);
    // Keep aliases equal during edits; splitting/promotions have separate coverage.
    expected[1] = row(&[0, 0]);
    cache
        .get_mut(2, &[])
        .unwrap()
        .1
        .get_mut()
        .copy_from_slice(&expected[1]);
    cache.cycle_with_columns(&[]);
    expected[0] = row(&[-2i64 as u64, -2i64 as u64]);
    cache
        .get_mut(1, &[])
        .unwrap()
        .1
        .get_mut()
        .copy_from_slice(&expected[0]);
    let mut rows = cache.iter(&[]);
    for row in expected {
        assert_eq!(rows.next().unwrap(), row);
    }
    assert!(rows.next().is_none());
}

#[test]
fn layout_comparisons_are_bitwise_for_floats() {
    let mut p = layout(2, false);
    for f in &mut p.fields {
        f.integer = false;
        f.stored.width = 8;
    }
    p.positions();
    let nan = 0x7ff8000000001234;
    p = p.promoted(&row(&[nan, nan]), true).unwrap();
    assert_eq!(p.unit(), 9);
    let next = p.promoted(&row(&[0, (-0.0f64).to_bits()]), false).unwrap();
    assert_eq!(next.unit(), 17);
    let mut packed = vec![0; next.unit()];
    next.pack(&row(&[0, (-0.0f64).to_bits()]), &mut packed);
    let mut logical = vec![0; next.logical_unit()];
    next.unpack(&packed, &mut logical);
    assert_eq!(logical, row(&[0, (-0.0f64).to_bits()]));
    assert_eq!(size_of::<crate::data::blockbuffer::Data>(), 32);
    // Layout overhead is shared, but must stay small for one-off callsites too.
    assert!(size_of::<PhysicalLayout>() <= 64);
    assert!(size_of::<Field>() <= 16);
}
