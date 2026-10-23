//! Compact unsigned integer predictors; exact values belong in their residuals.

/// An unsigned 16-bit floating integer: 5 exponent bits and 11 mantissa bits.
///
/// Exponent zero decodes as `mantissa * UNIT`. Other exponents restore an implicit
/// leading one and decode as `((2048 + mantissa) << (exponent - 1)) * UNIT`.
/// Multiples of UNIT through 4,095 units are exact; larger values round down and
/// retain twelve significant bits. Exact row residuals retain discarded bits. Inputs above
/// [`Self::MAX`] saturate; callers storing exact residuals still lose no data.
/// The exponent occupies the high five bits of the stored u16.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct FloatU16(u16);

impl FloatU16 {
    pub const MANTISSA_BITS: u32 = 11;
    pub const EXPONENT_BITS: u32 = 5;
    /// Tuning: change this one constant and rebuild to set the predictor's unit.
    /// Any positive unit is allowed if the maximum decoded value fits in u64.
    /// Encoder, decoder, and measurement labels all use this same constant.
    pub const UNIT: u64 = 1;
    const MANTISSA_MASK: u16 = (1 << Self::MANTISSA_BITS) - 1;
    const MAX_EXPONENT: u32 = (1 << Self::EXPONENT_BITS) - 1;
    pub const MAX: u64 = (((2 << Self::MANTISSA_BITS) - 1u64) << (Self::MAX_EXPONENT - 1))
        .checked_mul(Self::UNIT)
        .expect("FloatU16 unit exceeds u64 range");

    /// Quantize a predictor downward; reconstruct residuals from `to_u64()`,
    /// never from the original input, so discarded bits remain in the residual.
    pub const fn from_u64_lossy(value: u64) -> Self {
        // Clamp without a conditional; saturation keeps the exponent in five bits.
        let value = value - value.saturating_sub(Self::MAX);
        let value = value / Self::UNIT;
        let width = u64::BITS - value.leading_zeros();
        let exponent = width.saturating_sub(Self::MANTISSA_BITS);
        // Exponents zero and one share a shift, keeping small values contiguous.
        let shift = exponent.saturating_sub(1);
        let mantissa = ((value >> shift) as u16) & Self::MANTISSA_MASK;
        Self(((exponent as u16) << Self::MANTISSA_BITS) | mantissa)
    }

    /// All 65,536 bit patterns are valid and canonical.
    pub const fn from_bits(bits: u16) -> Self {
        Self(bits)
    }

    pub const fn to_bits(self) -> u16 {
        self.0
    }

    pub const fn to_u64(self) -> u64 {
        let exponent = (self.0 >> Self::MANTISSA_BITS) as u32;
        // A comparison cast restores the hidden bit without branching for exp=0.
        let leading = ((exponent != 0) as u64) << Self::MANTISSA_BITS;
        let mantissa = (self.0 & Self::MANTISSA_MASK) as u64 | leading;
        (mantissa << exponent.saturating_sub(1)) * Self::UNIT
    }
}

// Reject an invalid tuning value at compile time rather than during recording.
const _: () = assert!(FloatU16::UNIT > 0);

#[cfg(test)]
mod tests {
    use super::FloatU16;

    #[test]
    fn layout_and_all_representable_values_round_trip() {
        assert_eq!(size_of::<FloatU16>(), 2);
        for bits in 0..=u16::MAX {
            let encoded = FloatU16::from_bits(bits);
            assert_eq!(encoded.to_bits(), bits);
            // Each encoding now represents a distinct value, in increasing order.
            assert_eq!(FloatU16::from_u64_lossy(encoded.to_u64()).to_bits(), bits);
            if bits > 0 {
                assert!(encoded.to_u64() > FloatU16::from_bits(bits - 1).to_u64());
            }
        }
    }

    #[test]
    fn precision_at_each_exponent_boundary_and_saturation() {
        // Keep coverage bounded even when UNIT is tuned to a large value.
        for units in 0..4096 {
            for remainder in [0, 1.min(FloatU16::UNIT - 1), FloatU16::UNIT - 1] {
                let value = units * FloatU16::UNIT + remainder;
                assert_eq!(
                    FloatU16::from_u64_lossy(value).to_u64(),
                    units * FloatU16::UNIT
                );
            }
        }
        for exponent in 1..=31 {
            let boundary = (2048u64 << (exponent - 1)) * FloatU16::UNIT;
            for value in [
                boundary - 1,
                boundary,
                boundary + 1,
                (4095u64 << (exponent - 1)) * FloatU16::UNIT - 1,
            ] {
                let encoded = FloatU16::from_u64_lossy(value);
                let decoded = encoded.to_u64();
                assert!(decoded <= value);
                let shift = (encoded.to_bits() >> FloatU16::MANTISSA_BITS).saturating_sub(1);
                let step = FloatU16::UNIT << shift;
                assert!(value - decoded < step);
            }
        }
        assert_eq!(FloatU16::from_bits(1).to_u64(), FloatU16::UNIT);
        // Pin the subnormal/normal transition and the first precision change.
        for (bits, units) in [
            (0x07ff, 2047),
            (0x0800, 2048),
            (0x0fff, 4095),
            (0x1000, 4096),
        ] {
            assert_eq!(FloatU16::from_bits(bits).to_u64(), units * FloatU16::UNIT);
        }
        for value in [FloatU16::MAX, FloatU16::MAX + 1, u64::MAX] {
            assert_eq!(FloatU16::from_u64_lossy(value).to_u64(), FloatU16::MAX);
        }
    }
}
