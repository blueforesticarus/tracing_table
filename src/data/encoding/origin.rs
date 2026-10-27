//! Tagged origins reserve the high nibble for representation, keeping coefficients
//! in the existing word. Relative rows pass through; their origin remains useful
//! to callers that need a block start without resolving external references.
use super::common::{advance, residual_u64, restore_u64};
use super::{Context, Encoding, Footer, Layout, PredictionEncoding};
use crate::data::util::float_u16::FloatU16;

#[derive(Clone, Copy, Debug)]
pub struct OriginEncoding {
    origin: u64,
    stride: u64,
    relative: bool,
}

impl OriginEncoding {
    pub const TAG_BITS: u32 = 4;
    pub const ORIGIN_BITS: u32 = u64::BITS - Self::TAG_BITS;
    pub const MAX_ORIGIN: u64 = (1 << Self::ORIGIN_BITS) - 1;
    const ABSOLUTE: u8 = 1;
    const RELATIVE: u8 = 2;

    pub fn absolute(prediction: PredictionEncoding<true>) -> Self {
        Self::check_origin(prediction.origin());
        Self {
            origin: prediction.origin(),
            stride: prediction.stride_float().to_u64(),
            relative: false,
        }
    }
    pub fn relative(origin: u64) -> Self {
        Self::check_origin(origin);
        Self {
            origin,
            stride: 0,
            relative: true,
        }
    }
    pub fn check_origin(origin: u64) {
        assert!(origin <= Self::MAX_ORIGIN, "tagged origin exceeds 60 bits");
    }
    pub fn origin(self) -> u64 {
        self.origin
    }
    pub fn is_relative(self) -> bool {
        self.relative
    }
    pub fn stride_float(self) -> FloatU16 {
        FloatU16::from_u64_lossy(self.stride)
    }
}

impl<'a> Encoding<'a> for OriginEncoding {
    const WIDTH: Option<usize> = Some(8);
    fn parse(record: &'a [u8], layout: Layout, _rows: usize) -> Self {
        assert_eq!(layout.width, 8);
        assert!(record.len() >= 8, "tagged origin missing");
        let packed = u64::from_le_bytes(record[..8].try_into().unwrap());
        let tag = (packed >> Self::ORIGIN_BITS) as u8;
        let origin = packed & Self::MAX_ORIGIN;
        match tag {
            Self::ABSOLUTE => {
                assert_eq!(record.len(), 10);
                Self {
                    origin,
                    stride: FloatU16::from_bits(u16::from_le_bytes(
                        record[8..].try_into().unwrap(),
                    ))
                    .to_u64(),
                    relative: false,
                }
            }
            Self::RELATIVE => {
                assert_eq!(record.len(), 8);
                Self::relative(origin)
            }
            _ => panic!("unknown tagged origin representation"),
        }
    }
    fn write_footer(&self, width: usize, output: &mut Footer) {
        assert_eq!(width, 8);
        Self::check_origin(self.origin);
        let tag = if self.relative {
            Self::RELATIVE
        } else {
            Self::ABSOLUTE
        };
        output.extend_from_slice(
            &(self.origin | (u64::from(tag) << Self::ORIGIN_BITS)).to_le_bytes(),
        );
        if !self.relative {
            output.extend_from_slice(&self.stride_float().to_bits().to_le_bytes());
        }
    }
    #[inline]
    fn encode(&self, value: u128, row: usize, context: Context) -> Option<u128> {
        let value = u64::try_from(value).ok()?;
        if self.relative {
            return Some(u128::from(value));
        }
        let offset = value.checked_sub(self.origin)?;
        Some(u128::from(if self.stride == 0 {
            offset
        } else {
            residual_u64(offset, advance(self.stride, row, context.rows()), 8)
        }))
    }
    #[inline]
    fn decode(&self, encoded: u128, row: usize, context: Context) -> u128 {
        let encoded = u64::try_from(encoded).expect("origin value exceeds u64");
        if self.relative {
            return u128::from(encoded);
        }
        let offset = if self.stride == 0 {
            encoded
        } else {
            restore_u64(encoded, advance(self.stride, row, context.rows()), 8)
        };
        u128::from(
            self.origin
                .checked_add(offset)
                .expect("unsigned origin overflow"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{blockbuffer::ArchivedFrame, dataframe::Frame};

    #[test]
    fn packed_nibble_preserves_all_sixty_origin_bits_and_exact_values() {
        let layout = Layout::tagged_origin(0);
        let mut frame = Frame::new(8, 2);
        frame.push(&[0; 8]);
        frame.push(&[0; 8]);
        let frame = ArchivedFrame::new_from(&frame);
        let context = Context {
            frame: &frame,
            layout,
        };
        // A maximum origin exercises the low nibble of the final byte; the
        // representation tag must never overwrite those four timestamp bits.
        for origin in [0, 1000, OriginEncoding::MAX_ORIGIN] {
            for relative in [false, true] {
                let encoding = if relative {
                    OriginEncoding::relative(origin)
                } else {
                    OriginEncoding {
                        origin,
                        stride: 100,
                        relative: false,
                    }
                };
                let mut footer = Footer::new();
                encoding.write_footer(8, &mut footer);
                assert_eq!(footer.len(), if relative { 8 } else { 10 });
                assert_eq!(footer[7] >> 4, if relative { 2 } else { 1 });
                assert_eq!(footer[7] & 15, ((origin >> 56) & 15) as u8);
                let parsed = OriginEncoding::parse(&footer, layout, 2);
                assert_eq!(parsed.origin(), origin);
                assert_eq!(parsed.is_relative(), relative);
                for (row, value) in [origin, origin.saturating_add(51)].into_iter().enumerate() {
                    let encoded = parsed.encode(u128::from(value), row, context).unwrap();
                    assert_eq!(parsed.decode(encoded, row, context), u128::from(value));
                }
            }
        }
        let mut footer = Footer::new();
        OriginEncoding {
            origin: 1000,
            stride: 100,
            relative: false,
        }
        .write_footer(8, &mut footer);
        assert_eq!(
            footer.as_slice(),
            &[0xe8, 0x03, 0, 0, 0, 0, 0, 0x10, 0x64, 0]
        );
    }

    #[test]
    #[should_panic(expected = "unknown tagged origin representation")]
    fn reserved_tags_are_rejected() {
        // Future tags must be explicitly implemented rather than read as raw.
        OriginEncoding::parse(&[0, 0, 0, 0, 0, 0, 0, 0xf0], Layout::tagged_origin(0), 1);
    }
}
