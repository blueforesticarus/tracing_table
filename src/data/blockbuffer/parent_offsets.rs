//! Parent identity and predictor dictionaries appended to compacted frames.
use crate::data::column;
use crate::data::dataframe::Frame;
use crate::data::util::float_u16::FloatU16;
use smallvec::SmallVec;

/// Packed identity: callsite in the low 20 bits, archetype in the high 12 bits.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallsiteArchetype(u32);

impl CallsiteArchetype {
    pub const CALLSITE_BITS: u32 = 20;
    pub const ARCHETYPE_BITS: u32 = 12;
    pub const MAX_CALLSITE: u32 = (1 << Self::CALLSITE_BITS) - 1;
    pub const MAX_ARCHETYPE: u32 = (1 << Self::ARCHETYPE_BITS) - 1;

    /// Reject overflow instead of silently aliasing a different parent identity.
    pub const fn new(callsite: u32, archetype: u32) -> Self {
        assert!(callsite <= Self::MAX_CALLSITE, "callsite exceeds 20 bits");
        assert!(
            archetype <= Self::MAX_ARCHETYPE,
            "archetype exceeds 12 bits"
        );
        Self(callsite | (archetype << Self::CALLSITE_BITS))
    }

    pub const fn callsite(self) -> u32 {
        self.0 & Self::MAX_CALLSITE
    }

    pub const fn archetype(self) -> u32 {
        self.0 >> Self::CALLSITE_BITS
    }

    /// Every u32 bit pattern encodes a valid pair.
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    pub const fn to_bits(self) -> u32 {
        self.0
    }
}

/// Fields are decoded from bytes, so the trailer needs no alignment padding.
#[derive(Clone, Copy, Debug)]
pub struct ParentOffsetEntry {
    pub identity: CallsiteArchetype,
    pub offset: FloatU16,
    /// Predicted increase from row zero to row `frame_len`, not per row.
    pub stride: FloatU16,
}

impl ParentOffsetEntry {
    pub fn encode(self, actual: u64, row: usize, frame_len: usize) -> u64 {
        column::residual(
            u128::from(actual),
            u128::from(self.predict(row, frame_len)),
            8,
        ) as u64
    }

    pub fn decode(self, encoded: u64, row: usize, frame_len: usize) -> u64 {
        column::restore(
            u128::from(encoded),
            u128::from(self.predict(row, frame_len)),
            8,
        ) as u64
    }

    fn predict(self, row: usize, frame_len: usize) -> u64 {
        self.offset
            .to_u64()
            .wrapping_add(column::advance(self.stride.to_u64(), row, frame_len))
    }

    fn fit(mut self, first: usize, start: u64, last: usize, end: u64, length: usize) -> Self {
        let (offset, stride) = column::fit_line(
            first,
            u128::from(start),
            last,
            u128::from(end),
            length,
            true,
        );
        self.offset = FloatU16::from_u64_lossy(offset as u64);
        self.stride = stride;
        self
    }
}

/// Every entry is CallsiteArchetype + FloatU16 offset/stride (8 bytes),
/// including a singleton. Rows store a u16 dictionary index beside the residual.
#[derive(Clone, Copy, Default)]
pub struct ParentOffsets<'a> {
    bytes: &'a [u8],
}

impl<'a> ParentOffsets<'a> {
    pub const ENTRY_BYTES: usize = size_of::<CallsiteArchetype>() + 2 * size_of::<FloatU16>();
    pub const ROW_BYTES: usize = size_of::<u64>() + size_of::<u16>();

    pub fn new(bytes: &'a [u8]) -> Self {
        // The exact trailer/scratch length is the source of truth in both states.
        assert_eq!(
            bytes.len() % Self::ENTRY_BYTES,
            0,
            "parent offset trailer size mismatch"
        );
        u16::try_from(bytes.len() / Self::ENTRY_BYTES).expect("parent identity count exceeds u16");
        Self { bytes }
    }

    pub fn len(self) -> usize {
        self.bytes.len() / Self::ENTRY_BYTES
    }
    pub fn is_empty(self) -> bool {
        self.bytes.is_empty()
    }

    pub fn entry(self, index: u16) -> ParentOffsetEntry {
        assert!(
            usize::from(index) < self.len(),
            "parent table index out of range"
        );
        let start = usize::from(index) * Self::ENTRY_BYTES;
        let bytes = &self.bytes[start..start + Self::ENTRY_BYTES];
        ParentOffsetEntry {
            identity: CallsiteArchetype::from_bits(u32::from_le_bytes(
                bytes[..4].try_into().unwrap(),
            )),
            // The unit-scaled predictor rounds down; row deltas preserve all
            // low bits and also support IDs beyond the predictor's range.
            offset: FloatU16::from_bits(u16::from_le_bytes(bytes[4..6].try_into().unwrap())),
            stride: FloatU16::from_bits(u16::from_le_bytes(bytes[6..8].try_into().unwrap())),
        }
    }

    pub(super) fn find(self, identity: CallsiteArchetype) -> Option<u16> {
        (0..self.len() as u16).find(|&i| self.entry(i).identity == identity)
    }
}

/// Transform a completed active frame exactly once, before initial compaction.
/// Archived edits must retain these coefficients and must NOT call this again.
pub(super) fn fit_strides(frame: &mut Frame, bytes: &mut [u8], offsets: &[u16]) {
    let length = frame.length;
    assert!(length > 0 && length <= u16::MAX as usize);
    for &offset in offsets {
        assert!(usize::from(offset) + ParentOffsets::ROW_BYTES <= frame.unit);
    }
    let table = ParentOffsets::new(bytes);
    // Most blocks have few identities. Inline scratch keeps singleton rollover
    // at one allocation; spill storage for larger dictionaries is temporary.
    struct Endpoints {
        original: ParentOffsetEntry,
        first_row: usize,
        first_index: u64,
        last_row: usize,
        last_index: u64,
    }
    let mut endpoints: SmallVec<[Endpoints; 4]> = (0..table.len() as u16)
        .map(|i| Endpoints {
            original: table.entry(i),
            first_row: usize::MAX,
            first_index: 0,
            last_row: 0,
            last_index: 0,
        })
        .collect();
    let read = |row: &[u8], offset: usize| {
        (
            u64::from_ne_bytes(row[offset..offset + 8].try_into().unwrap()),
            usize::from(u16::from_ne_bytes(
                row[offset + 8..offset + 10].try_into().unwrap(),
            )),
        )
    };
    for (r, row) in frame.buffer[frame.unit..(length + 1) * frame.unit]
        .chunks_exact(frame.unit)
        .enumerate()
    {
        for &offset in offsets {
            let (residual, group) = read(row, usize::from(offset));
            let e = &mut endpoints[group];
            let actual = e.original.decode(residual, r, length);
            if e.first_row == usize::MAX {
                e.first_row = r;
                e.first_index = actual;
            }
            e.last_row = r;
            e.last_index = actual;
        }
    }
    for (g, e) in endpoints.iter().enumerate() {
        assert_ne!(e.first_row, usize::MAX, "unused active parent entry");
        let fitted = e
            .original
            .fit(e.first_row, e.first_index, e.last_row, e.last_index, length);
        let begin = g * ParentOffsets::ENTRY_BYTES;
        bytes[begin + 4..begin + 6].copy_from_slice(&fitted.offset.to_bits().to_le_bytes());
        bytes[begin + 6..begin + 8].copy_from_slice(&fitted.stride.to_bits().to_le_bytes());
    }
    let fitted = ParentOffsets::new(bytes);
    // Each reference column keeps its own mask even when identities are shared.
    for &offset in offsets {
        let offset = usize::from(offset);
        column::rewrite(
            frame,
            column::Layout::new(offset, 8, false),
            |residual, r, row| {
                let group = read(row, offset).1 as u16;
                let actual =
                    endpoints[usize::from(group)]
                        .original
                        .decode(residual as u64, r, length);
                u128::from(fitted.entry(group).encode(actual, r, length))
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_identity_preserves_both_fields_at_their_boundaries() {
        assert_eq!(size_of::<CallsiteArchetype>(), 4);
        assert_eq!(size_of::<ParentOffsetEntry>(), 8);
        assert_eq!(ParentOffsets::ENTRY_BYTES, 8);
        // Cross-product catches a shift or mask that leaks across either field.
        for callsite in [0, 1, CallsiteArchetype::MAX_CALLSITE] {
            for archetype in [0, 1, CallsiteArchetype::MAX_ARCHETYPE] {
                let identity = CallsiteArchetype::new(callsite, archetype);
                assert_eq!(identity.callsite(), callsite);
                assert_eq!(identity.archetype(), archetype);
                assert_eq!(CallsiteArchetype::from_bits(identity.to_bits()), identity);
            }
        }
        assert_eq!(
            CallsiteArchetype::from_bits(u32::MAX).callsite(),
            (1 << 20) - 1
        );
        assert_eq!(
            CallsiteArchetype::from_bits(u32::MAX).archetype(),
            (1 << 12) - 1
        );
    }

    #[test]
    #[should_panic(expected = "callsite exceeds 20 bits")]
    fn packed_identity_rejects_callsite_overflow() {
        CallsiteArchetype::new(1 << 20, 0);
    }

    #[test]
    #[should_panic(expected = "archetype exceeds 12 bits")]
    fn packed_identity_rejects_archetype_overflow() {
        CallsiteArchetype::new(0, 1 << 12);
    }

    fn entry() -> ParentOffsetEntry {
        ParentOffsetEntry {
            identity: CallsiteArchetype::new(7, 3),
            offset: FloatU16::default(),
            stride: FloatU16::default(),
        }
    }

    #[test]
    fn endpoint_fit_clamps_by_pivoting_on_the_last_point() {
        let unit = FloatU16::UNIT;
        let ordinary = entry().fit(2, 10 * unit, 6, 18 * unit, 8);
        assert_eq!(ordinary.offset.to_u64(), 6 * unit);
        assert_eq!(ordinary.stride.to_u64(), 16 * unit);
        assert_eq!(ordinary.predict(2, 8), 10 * unit);
        assert_eq!(ordinary.predict(6, 8), 18 * unit);

        // The original line has intercept -6. Clipping only that intercept
        // would predict 18 at row six; refitting must predict the actual 12.
        let clamped = entry().fit(2, 0, 6, 12 * unit, 8);
        assert_eq!(clamped.offset.to_u64(), 0);
        assert_eq!(clamped.stride.to_u64(), 16 * unit);
        assert_eq!(clamped.predict(6, 8), 12 * unit);
        assert_eq!(clamped.predict(2, 8), 4 * unit);

        // Extrapolate to L, not L-1, even for a block closed before capacity.
        let short = entry().fit(0, 0, 1, 2 * unit, 2);
        assert_eq!(short.stride.to_u64(), 4 * unit);
        for (first, start, last, end) in [(2, 10, 6, 4), (2, 10, 6, 10), (2, 10, 2, 10)] {
            let flat = entry().fit(first, start * unit, last, end * unit, 8);
            assert_eq!(flat.offset.to_u64(), 10 * unit);
            assert_eq!(flat.stride.to_u64(), 0);
        }
    }

    #[test]
    fn full_range_ids_round_trip_with_saturated_and_fractional_predictors() {
        let length = u16::MAX as usize;
        let saturated = entry().fit(1, 0, 2, u64::MAX, length);
        assert_eq!(saturated.offset.to_u64(), 0);
        assert_eq!(saturated.stride.to_u64(), FloatU16::MAX);
        let fractional = ParentOffsetEntry {
            offset: FloatU16::from_u64_lossy(3 * FloatU16::UNIT),
            stride: FloatU16::from_u64_lossy(5 * FloatU16::UNIT),
            ..entry()
        };
        let above_f64 = entry().fit(0, u64::MAX - 4, 3, u64::MAX, length);
        for predictor in [saturated, fractional, above_f64] {
            for row in [0, 1, 2, length / 2, length - 1] {
                for actual in [0, 1, 4095, 1 << 53, (1 << 53) + 1, u64::MAX - 1, u64::MAX] {
                    let residual = predictor.encode(actual, row, length);
                    assert_eq!(predictor.decode(residual, row, length), actual);
                }
            }
        }
        let half = ParentOffsetEntry {
            stride: FloatU16::from_u64_lossy(FloatU16::UNIT),
            ..entry()
        };
        assert_eq!(half.predict(1, 2), FloatU16::UNIT.div_ceil(2));
    }

    #[test]
    fn early_archive_uses_actual_length_and_removes_old_residual_mask_bits() {
        let original = entry();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&original.identity.to_bits().to_le_bytes());
        bytes.extend_from_slice(&original.offset.to_bits().to_le_bytes());
        bytes.extend_from_slice(&original.stride.to_bits().to_le_bytes());
        let mut frame = Frame::new(11, 32);
        for r in 0..4 {
            let mut row = [0; 11];
            let actual = r as u64 * FloatU16::UNIT;
            row[1..9].copy_from_slice(&original.encode(actual, r, 4).to_ne_bytes());
            frame.push(&row);
        }
        assert_ne!(&frame.buffer[1..9], &[0; 8]);
        frame.lock_editable(0, 1);
        fit_strides(&mut frame, &mut bytes, &[1]);
        let fitted = ParentOffsets::new(&bytes).entry(0);
        assert_eq!(fitted.stride.to_u64(), 4 * FloatU16::UNIT);
        assert_eq!(&frame.buffer[1..9], &[0; 8]);
        assert_eq!(frame.buffer[0], 0xff);
        for r in 0..4 {
            let begin = (r + 1) * frame.unit + 1;
            let residual = u64::from_ne_bytes(frame.buffer[begin..begin + 8].try_into().unwrap());
            assert_eq!(residual, 0);
            assert_eq!(fitted.decode(residual, r, 4), r as u64 * FloatU16::UNIT);
        }
    }
}
