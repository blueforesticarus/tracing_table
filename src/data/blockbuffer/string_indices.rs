//! Archive-only string-index prediction. Sparse (column, start) footer entries
//! keep schema pointers, mode flags, and extra padding out of retained headers.
use smallvec::SmallVec;

use super::ParentOffsets;
use crate::data::dataframe::Frame;

/// Each selected column has a u16 byte position and u16 start, both little-endian.
#[derive(Clone, Copy, Default)]
pub struct StringIndexStarts<'a> {
    bytes: &'a [u8],
}

impl<'a> StringIndexStarts<'a> {
    pub const ENTRY_BYTES: usize = 4;
    pub const COUNT_BYTES: usize = 1;
    pub const MAX_ENTRIES: usize = u8::MAX as usize;

    pub fn len(self) -> usize {
        self.bytes.len() / Self::ENTRY_BYTES
    }
    pub fn is_empty(self) -> bool {
        self.bytes.is_empty()
    }

    pub fn entries(self) -> impl Iterator<Item = (usize, u16)> + 'a {
        self.bytes.chunks_exact(Self::ENTRY_BYTES).map(|entry| {
            (
                usize::from(u16::from_le_bytes(entry[..2].try_into().unwrap())),
                u16::from_le_bytes(entry[2..].try_into().unwrap()),
            )
        })
    }

    /// Callers see ordinary indices; the physical frame retains ZigZag residuals.
    pub(crate) fn decode(self, row: &mut [u8], index: usize) {
        for (offset, start) in self.entries() {
            let encoded = read(row, offset);
            let delta = (encoded >> 1) ^ 0u16.wrapping_sub(encoded & 1);
            write(
                row,
                offset,
                start.wrapping_add(index as u16).wrapping_add(delta),
            );
        }
    }

    pub(crate) fn encode(self, row: &mut [u8], index: usize) {
        for (offset, start) in self.entries() {
            write(row, offset, residual(read(row, offset), start, index));
        }
    }
}

/// Parent trailers are multiples of eight bytes. A nonempty string suffix is
/// 4*N+1 bytes, hence never a multiple of eight: its presence needs no header flag.
/// Blocks with no selected columns retain their original trailer byte-for-byte.
pub(super) fn split(trailer: &[u8]) -> (&[u8], StringIndexStarts<'_>) {
    if trailer.len() % ParentOffsets::ENTRY_BYTES == 0 {
        return (trailer, StringIndexStarts::default());
    }
    let count = usize::from(*trailer.last().expect("missing string-index footer count"));
    assert!(count > 0, "empty string-index footer");
    let size = count * StringIndexStarts::ENTRY_BYTES + StringIndexStarts::COUNT_BYTES;
    let start = trailer
        .len()
        .checked_sub(size)
        .expect("string-index footer exceeds trailer");
    assert_eq!(
        start % ParentOffsets::ENTRY_BYTES,
        0,
        "parent trailer size mismatch"
    );
    (
        &trailer[..start],
        StringIndexStarts {
            bytes: &trailer[start..trailer.len() - StringIndexStarts::COUNT_BYTES],
        },
    )
}

fn read(row: &[u8], offset: usize) -> u16 {
    u16::from_ne_bytes(row[offset..offset + 2].try_into().unwrap())
}
fn write(row: &mut [u8], offset: usize, value: u16) {
    row[offset..offset + 2].copy_from_slice(&value.to_ne_bytes());
}
fn residual(value: u16, start: u16, row: usize) -> u16 {
    let delta = value.wrapping_sub(start.wrapping_add(row as u16));
    (delta << 1) ^ ((delta as i16 >> 15) as u16)
}

// Score the archive's actual layout, including byte-mode builds and the optional
// byte-aligned bit layout. A small residual magnitude alone is not a space saving.
fn varying_bits(mask: &[u8]) -> usize {
    if cfg!(feature = "bit-compression") {
        mask.iter().map(|byte| byte.count_ones() as usize).sum()
    } else {
        mask.iter().filter(|&&byte| byte != 0).count() * 8
    }
}
fn payload_size(frame: &Frame, bits: usize) -> usize {
    let stride =
        if cfg!(feature = "bit-compression") && crate::data::dataframe::CONTINUOUS_BIT_PACKING {
            bits
        } else {
            bits.div_ceil(8) * 8
        };
    2 * frame.unit + ((frame.length - 1) * stride).div_ceil(8)
}

/// Fit once at rollover. Preserve unrelated mask bits and never refit archived
/// edits. Scratch is temporary/inline for ordinary schemas; cache headers do not grow.
pub(super) fn fit(frame: &mut Frame, trailer: &mut Vec<u8>, columns: &[u16]) {
    if frame.length < 2 || columns.is_empty() {
        return;
    }
    ParentOffsets::new(trailer);
    // offset, start, replacement mask: no retained allocation for candidate state.
    let mut candidates = SmallVec::<[(u16, u16, [u8; 2]); 4]>::new();
    let mut saved_bits = 0;
    for &column in columns {
        // Keep the first 255 profitable columns in schema order. Any remaining
        // columns stay raw, so arbitrarily wide schemas cannot truncate the count.
        if candidates.len() == StringIndexStarts::MAX_ENTRIES {
            break;
        }
        let offset = usize::from(column);
        assert!(offset + 2 <= frame.unit, "string-index column exceeds row");
        assert!(
            candidates.last().is_none_or(|entry| entry.0 < column),
            "columns must be ordered"
        );
        let original = varying_bits(&frame.mask()[offset..offset + 2]);
        if original == 0 {
            continue;
        } // Constants already cost zero varying bits.
        let start = read(&frame.buffer[frame.unit..2 * frame.unit], offset);
        let mut mask = 0u16;
        for (index, row) in frame.iter().enumerate() {
            mask |= residual(read(row, offset), start, index); // First residual is zero.
        }
        let bytes = mask.to_ne_bytes();
        let predicted = varying_bits(&bytes);
        if original > predicted
            && (original - predicted) * (frame.length - 1) > StringIndexStarts::ENTRY_BYTES * 8
        {
            saved_bits += original - predicted;
            candidates.push((column, start, bytes));
        }
    }
    if candidates.is_empty() {
        return;
    }
    let bits = varying_bits(frame.mask());
    let old_size = payload_size(frame, bits);
    let new_size = payload_size(frame, bits - saved_bits);
    let footer_size =
        candidates.len() * StringIndexStarts::ENTRY_BYTES + StringIndexStarts::COUNT_BYTES;
    // Existing frame/header/prefix/parent lengths are even. Compare final two-byte
    // allocation sizes, charging the shared count and retaining only a strict win.
    if (new_size + footer_size).div_ceil(2) >= old_size.div_ceil(2) {
        return;
    }
    // SmallBox's u16 DST length includes both row payload and the complete trailer.
    if new_size + trailer.len() + footer_size > u16::MAX as usize {
        return;
    }
    for &(column, start, mask) in &candidates {
        let offset = usize::from(column);
        for (index, row) in frame.buffer[frame.unit..(frame.length + 1) * frame.unit]
            .chunks_exact_mut(frame.unit)
            .enumerate()
        {
            write(row, offset, residual(read(row, offset), start, index));
        }
        frame.buffer[offset..offset + 2].copy_from_slice(&mask);
        trailer.extend_from_slice(&column.to_le_bytes());
        trailer.extend_from_slice(&start.to_le_bytes());
    }
    trailer.push(u8::try_from(candidates.len()).expect("string-index count exceeds u8"));
}
