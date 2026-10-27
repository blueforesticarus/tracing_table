//! Identity encoding has no footer and no per-column state.
use super::{Context, Encoding, Footer};
#[derive(Clone, Copy, Debug, Default)]
pub struct RawEncoding;
impl<'a> Encoding<'a> for RawEncoding {
    fn parse(record: &'a [u8], _layout: super::Layout, _rows: usize) -> Self {
        assert!(record.is_empty());
        Self
    }
    fn write_footer(&self, _width: usize, _output: &mut Footer) {}
    #[inline]
    fn encode(&self, value: u128, _row: usize, _context: Context) -> Option<u128> {
        Some(value)
    }
    #[inline]
    fn decode(&self, encoded: u128, _row: usize, _context: Context) -> u128 {
        encoded
    }
}
