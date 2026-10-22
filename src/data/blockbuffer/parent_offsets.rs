//! Parent identity and predictor dictionaries appended to compacted frames.
use crate::data::float_u16::FloatU16;

/// Fields are decoded from bytes, so the trailer needs no alignment padding.
#[derive(Clone, Copy, Debug)]
pub struct ParentOffsetEntry {
    pub callsite: u32,
    pub archetype: u32,
    pub offset: FloatU16,
}

impl ParentOffsetEntry {
    pub fn encode(self, actual: u64) -> u64 {
        let delta = actual.wrapping_sub(self.offset.to_u64());
        (delta << 1) ^ ((delta as i64 >> 63) as u64)
    }

    pub fn decode(self, encoded: u64) -> u64 {
        // Modular signed ZigZag remains exact across the entire u64 domain.
        self.offset
            .to_u64()
            .wrapping_add((encoded >> 1) ^ 0u64.wrapping_sub(encoded & 1))
    }
}

/// Every entry is u32 callsite + u32 archetype + FloatU16 offset (10 bytes),
/// including a singleton. Rows store a u16 dictionary index beside the residual.
#[derive(Clone, Copy, Default)]
pub struct ParentOffsets<'a> {
    bytes: &'a [u8],
    count: u16,
}

impl<'a> ParentOffsets<'a> {
    pub const ENTRY_BYTES: usize = 2 * size_of::<u32>() + size_of::<FloatU16>();
    pub const ROW_BYTES: usize = size_of::<u64>() + size_of::<u16>();

    pub fn new(bytes: &'a [u8], count: u16) -> Self {
        assert_eq!(
            bytes.len(),
            usize::from(count) * Self::ENTRY_BYTES,
            "parent offset trailer size mismatch"
        );
        Self { bytes, count }
    }

    pub fn len(self) -> usize {
        usize::from(self.count)
    }
    pub fn is_empty(self) -> bool {
        self.count == 0
    }

    pub fn entry(self, index: u16) -> ParentOffsetEntry {
        assert!(index < self.count, "parent table index out of range");
        let start = usize::from(index) * Self::ENTRY_BYTES;
        let bytes = &self.bytes[start..start + Self::ENTRY_BYTES];
        ParentOffsetEntry {
            callsite: u32::from_le_bytes(bytes[..4].try_into().unwrap()),
            archetype: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            // The unit-scaled predictor rounds down; row deltas preserve all
            // low bits and also support IDs beyond the predictor's range.
            offset: FloatU16::from_bits(u16::from_le_bytes(bytes[8..].try_into().unwrap())),
        }
    }

    pub(super) fn find(self, callsite: u32, archetype: u32) -> Option<u16> {
        (0..self.count).find(|&i| {
            let entry = self.entry(i);
            (entry.callsite, entry.archetype) == (callsite, archetype)
        })
    }
}
