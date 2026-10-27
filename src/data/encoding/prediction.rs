//! Flat origin/line/cycle encodings. Static unsigned mode preserves a permanent
//! origin and unsigned offsets when prediction is disabled; no codec is nested.
use super::common::*;
use super::{Context, Encoding, Footer, Layout};
use crate::data::{
    column::{read_word, rewrite, write_word},
    dataframe::Frame,
    util::float_u16::FloatU16,
};
use smallvec::SmallVec;
type Prediction = PredictionEncoding<false>;

// TODO: Make this encoding generic over 4-, 8-, and 16-byte words. For now,
// coefficients and row arithmetic use u64, and fitting skips 16-byte columns.
#[derive(Clone, Copy, Debug, Default)]
pub struct PredictionEncoding<const UNSIGNED: bool = false> {
    pub(crate) origin: u64,
    pub(crate) stride: u64,
    pub(crate) period: u8,
    pub(crate) phase: u8,
    pub(crate) row: bool,
    pub(crate) descending: bool,
}

impl<const UNSIGNED: bool> PredictionEncoding<UNSIGNED> {
    pub fn origin(&self) -> u64 {
        self.origin
    }
    pub fn stride_float(&self) -> FloatU16 {
        FloatU16::from_u64_lossy(self.stride)
    }
    pub(crate) fn at(self, row: usize, length: usize) -> u64 {
        let (row, length) = if self.period == 0 {
            (row, length)
        } else {
            (
                (row % usize::from(self.period) + usize::from(self.phase))
                    % usize::from(self.period),
                usize::from(self.period),
            )
        };
        let increase = if self.row {
            row as u64
        } else {
            advance(self.stride, row, length)
        };
        if self.descending {
            self.origin.wrapping_sub(increase)
        } else {
            self.origin.wrapping_add(increase)
        }
    }
    pub(crate) fn record(self, width: usize) -> SmallVec<[u8; 24]> {
        assert!(
            matches!(width, 1 | 2 | 4 | 8),
            "prediction codec supports up to u64"
        );
        let tag = match (self.row, self.period, self.stride) {
            (true, 0, _) => ROW,
            (true, _, _) => ROW_CYCLE,
            (false, _, 0) => OFFSET,
            (false, 0, _) => LINEAR,
            _ => CYCLE,
        };
        let mut bytes = SmallVec::new();
        bytes.push(tag | if self.descending { 0x80 } else { 0 });
        bytes.extend_from_slice(&self.origin.to_le_bytes()[..width]);
        if matches!(tag, LINEAR | CYCLE) {
            bytes.extend_from_slice(
                &FloatU16::from_u64_lossy(self.stride)
                    .to_bits()
                    .to_le_bytes(),
            );
        }
        if matches!(tag, CYCLE | ROW_CYCLE) {
            bytes.extend_from_slice(&[self.period, self.phase]);
        }
        bytes
    }
    pub(crate) fn parse(bytes: &[u8], width: usize) -> Self {
        assert!(
            matches!(width, 1 | 2 | 4 | 8),
            "prediction codec supports up to u64"
        );
        let tag = bytes[0] & 0x7f;
        assert!(
            matches!(tag, OFFSET | LINEAR | ROW | CYCLE | ROW_CYCLE),
            "unknown column prediction"
        );
        let mut origin = [0; 8];
        origin[..width].copy_from_slice(&bytes[1..1 + width]);
        let mut prediction = Self {
            origin: u64::from_le_bytes(origin),
            row: matches!(tag, ROW | ROW_CYCLE),
            descending: bytes[0] & 0x80 != 0,
            ..Self::default()
        };
        let mut end = 1 + width;
        if matches!(tag, LINEAR | CYCLE) {
            prediction.stride =
                FloatU16::from_bits(u16::from_le_bytes(bytes[end..end + 2].try_into().unwrap()))
                    .to_u64();
            end += 2;
        }
        if matches!(tag, CYCLE | ROW_CYCLE) {
            prediction.period = bytes[end];
            prediction.phase = bytes[end + 1];
            end += 2;
            assert!(
                prediction.period != 0 && prediction.phase < prediction.period,
                "invalid column cycle"
            );
        }
        check_record_size(bytes, end);
        prediction
    }
}

impl<'a, const UNSIGNED: bool> Encoding<'a> for PredictionEncoding<UNSIGNED> {
    const WIDTH: Option<usize> = if UNSIGNED { Some(8) } else { None };
    #[inline]
    fn parse(record: &'a [u8], layout: Layout, _rows: usize) -> Self {
        let width = usize::from(layout.width);
        if UNSIGNED {
            assert_eq!(width, 8, "unsigned-origin codec requires u64 words");
            assert_eq!(record.len(), width + 2);
            let mut origin = [0; 8];
            origin[..width].copy_from_slice(&record[..width]);
            Self {
                origin: u64::from_le_bytes(origin),
                stride: FloatU16::from_bits(u16::from_le_bytes(
                    record[width..].try_into().unwrap(),
                ))
                .to_u64(),
                ..Default::default()
            }
        } else {
            Self::parse(record, width)
        }
    }
    fn write_footer(&self, width: usize, output: &mut Footer) {
        if UNSIGNED {
            output.extend_from_slice(&self.origin.to_le_bytes()[..width]);
            output.extend_from_slice(&self.stride_float().to_bits().to_le_bytes());
        } else {
            output.extend_from_slice(&self.record(width));
        }
    }
    #[inline]
    fn encode(&self, value: u128, row: usize, context: Context) -> Option<u128> {
        // u128 belongs to the shared column interface; arithmetic stays native.
        let value = u64::try_from(value).ok()?;
        let encoded = if UNSIGNED {
            let offset = value.checked_sub(self.origin)?;
            if self.stride == 0 {
                offset
            } else {
                residual_u64(offset, advance(self.stride, row, context.rows()), 8)
            }
        } else {
            residual_u64(value, self.at(row, context.rows()), context.width())
        };
        Some(u128::from(encoded))
    }
    #[inline]
    fn decode(&self, encoded: u128, row: usize, context: Context) -> u128 {
        let encoded = u64::try_from(encoded).expect("prediction value exceeds u64");
        let value = if UNSIGNED {
            let offset = if self.stride == 0 {
                encoded
            } else {
                restore_u64(encoded, advance(self.stride, row, context.rows()), 8)
            };
            self.origin
                .checked_add(offset)
                .expect("unsigned origin overflow")
        } else {
            restore_u64(encoded, self.at(row, context.rows()), context.width())
        };
        u128::from(value)
    }
}

/// Fit either a line or a line evaluated at cyclic row positions. The exact
/// origin compensates for FloatU16 quantization; residuals retain every value bit.
pub(crate) fn prediction(values: &[u128], layout: Layout, period: u8, phase: u8) -> Prediction {
    assert!(layout.width <= 8, "prediction codec supports up to u64");
    let length = if period == 0 {
        values.len()
    } else {
        usize::from(period)
    };
    let position = |row: usize| {
        if period == 0 {
            row
        } else {
            (row + usize::from(phase)) % length
        }
    };
    let mut p = Prediction {
        origin: values[0] as u64,
        period,
        phase,
        row: layout.is_strings(),
        ..Prediction::default()
    };
    if layout.is_strings() {
        p.origin = p.origin.wrapping_sub(u64::from(phase));
        return p;
    }
    let (low, high) = if period == 0 {
        (0, values.len() - 1)
    } else {
        // A complete cycle supplies both endpoints directly, avoiding another scan.
        (
            (length - usize::from(phase)) % length,
            (2 * length - 1 - usize::from(phase)) % length,
        )
    };
    let run = position(high) - position(low);
    if run == 0 {
        return Prediction {
            origin: p.origin,
            ..Prediction::default()
        };
    }
    let mask = word_mask_u64(usize::from(layout.width));
    let delta = (values[high] as u64).wrapping_sub(values[low] as u64) & mask;
    p.descending = delta >> (usize::from(layout.width) * 8 - 1) != 0;
    let rise = if p.descending {
        (values[low] as u64).wrapping_sub(values[high] as u64) & mask
    } else {
        delta
    };
    p.stride = fit_line(0, 0, run, u128::from(rise), length, false)
        .1
        .to_u64();
    let increase = advance(p.stride, position(low), length);
    p.origin = if p.descending {
        (values[low] as u64).wrapping_add(increase)
    } else {
        (values[low] as u64).wrapping_sub(increase)
    } & mask;
    // A flat fit needs neither cyclic row arithmetic nor stored stride/phase bytes.
    if p.stride == 0 {
        p.period = 0;
        p.phase = 0;
        p.descending = false;
    }
    p
}

pub(crate) fn consider_prediction(
    best: &mut Candidate,
    prediction: Prediction,
    values: &[u128],
    cost: &Cost<'_>,
) {
    best.consider(
        &prediction.record(cost.width),
        values,
        cost,
        |value, row| {
            u128::from(residual_u64(
                value as u64,
                prediction.at(row, values.len()),
                cost.width,
            ))
        },
    );
}

pub(crate) fn select_cycles(
    best: &mut Candidate,
    values: &[u128],
    layout: Layout,
    cost: &Cost<'_>,
) {
    let metadata_bytes = 1 + cost.width + if layout.is_strings() { 2 } else { 4 };
    if !best.worthwhile(cost.size(0, metadata_bytes), cost) {
        return;
    }
    let periods = cycle_periods(values);
    if periods.is_empty() {
        return;
    }
    // Locate rotation anchors only when a plausible cycle can afford its footer.
    let distance = |value: u128| {
        let delta = (value as u64).wrapping_sub(values[0] as u64) & word_mask_u64(cost.width);
        (delta << (64 - cost.width * 8)) as i64
    };
    let minimum = (0..values.len())
        .min_by_key(|&row| distance(values[row]))
        .unwrap();
    let maximum = (0..values.len())
        .max_by_key(|&row| distance(values[row]))
        .unwrap();
    let descending = values
        .iter()
        .skip(1)
        .find(|&&value| value != values[0])
        .is_some_and(|&value| distance(value) < 0);
    for period in periods {
        let rotate =
            |row| ((usize::from(period) - row % usize::from(period)) % usize::from(period)) as u8;
        // Start at the low value for ascending cycles and the high value for
        // descending ones, avoiding a poor rotated fit before the obvious one.
        let phases = if layout.is_strings() {
            [rotate(minimum), 0, 0]
        } else if descending {
            [rotate(maximum), rotate(minimum), 0]
        } else {
            [rotate(minimum), rotate(maximum), 0]
        };
        for (choice, phase) in phases.into_iter().enumerate() {
            if phases[..choice].contains(&phase) {
                continue;
            }
            let p = prediction(values, layout, period, phase);
            // Degenerate cycles are the offset case, not another candidate scan.
            if p.period == 0 {
                continue;
            }
            let previous = best.size;
            consider_prediction(best, p, values, cost);
            if best.size < previous {
                return;
            }
        }
    }
}

/// Measure ordinary absolute time errors, using the decoder's exact rounding.
/// Underprediction falls monotonically with stride; overprediction rises.
pub(crate) fn unsigned_errors(frame: &Frame, layout: Layout, stride: FloatU16) -> (u64, u64) {
    let mut under = 0;
    let mut over = 0;
    for (row, bytes) in frame.buffer[frame.unit..(frame.length + 1) * frame.unit]
        .chunks_exact(frame.unit)
        .enumerate()
    {
        let value = u64::from_ne_bytes(bytes[layout.range()].try_into().unwrap());
        let predicted = advance(stride.to_u64(), row, frame.length);
        under = under.max(value.saturating_sub(predicted));
        over = over.max(predicted.saturating_sub(value));
    }
    (under, over)
}

/// Search ordered FloatU16 codes directly so quantization is part of the fit.
/// The minimum of max(under, over) lies on either side of their crossing.
pub(crate) fn minimax_unsigned_stride(frame: &Frame, layout: Layout) -> FloatU16 {
    let mut low = 0u16;
    let mut high = u16::MAX;
    while low < high {
        let mid = low + (high - low) / 2;
        let (under, over) = unsigned_errors(frame, layout, FloatU16::from_bits(mid));
        if under > over {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    let upper = FloatU16::from_bits(low);
    let lower = FloatU16::from_bits(low.saturating_sub(1));
    let score = |stride| {
        let (under, over) = unsigned_errors(frame, layout, stride);
        under.max(over)
    };
    // Prefer the smaller coefficient when neighboring fits tie.
    if score(lower) <= score(upper) {
        lower
    } else {
        upper
    }
}

/// Match the archive's mask exactly, including zero's unsigned-offset encoding.
pub(crate) fn unsigned_mask(frame: &Frame, layout: Layout, stride: FloatU16) -> u64 {
    let mut first = 0;
    let mut mask = 0;
    for (row, bytes) in frame.buffer[frame.unit..(frame.length + 1) * frame.unit]
        .chunks_exact(frame.unit)
        .enumerate()
    {
        let value = u64::from_ne_bytes(bytes[layout.range()].try_into().unwrap());
        let encoded = if stride.to_u64() == 0 {
            value
        } else {
            residual_u64(value, advance(stride.to_u64(), row, frame.length), 8)
        };
        if row == 0 {
            first = encoded;
        }
        mask |= encoded ^ first;
    }
    mask
}

/// Bit packing stores set bits; byte packing stores whole nonzero mask bytes.
pub(crate) fn unsigned_mask_cost(mask: u64) -> u32 {
    if cfg!(feature = "bit-compression") {
        mask.count_ones()
    } else {
        mask.to_ne_bytes().iter().filter(|&&byte| byte != 0).count() as u32 * 8
    }
}

/// A bounded, deduplicated candidate list makes tuning independent of block size.
/// Strict improvement preserves the endpoint on ties and prevents regressions.
fn consider_unsigned_stride(
    frame: &Frame,
    layout: Layout,
    stride: FloatU16,
    best: &mut (FloatU16, u32),
    seen: &mut SmallVec<[u16; 64]>,
    budget: usize,
) {
    if seen.len() >= budget || best.1 == 0 || seen.contains(&stride.to_bits()) {
        return;
    }
    seen.push(stride.to_bits());
    let cost = unsigned_mask_cost(unsigned_mask(frame, layout, stride));
    if cost < best.1 {
        *best = (stride, cost);
    }
}

/// Heuristic only: mask cost is nonmonotonic, so sampled strides need not include
/// the global optimum. Always keep the endpoint as a no-regression baseline.
pub(crate) fn mask_unsigned_stride(frame: &Frame, layout: Layout, budget: usize) -> FloatU16 {
    let endpoint = endpoint_unsigned_stride(frame, layout);
    let mut best = (
        endpoint,
        unsigned_mask_cost(unsigned_mask(frame, layout, endpoint)),
    );
    let mut seen = SmallVec::<[u16; 64]>::new();
    seen.push(endpoint.to_bits());
    let budget = budget.max(1);
    if best.1 == 0 || budget == 1 {
        return best.0;
    }
    consider_unsigned_stride(
        frame,
        layout,
        FloatU16::default(),
        &mut best,
        &mut seen,
        budget,
    );
    let minimax = if UNSIGNED_MINIMAX_STRIDE && best.1 != 0 && seen.len() < budget {
        minimax_unsigned_stride(frame, layout)
    } else {
        endpoint
    };
    consider_unsigned_stride(frame, layout, minimax, &mut best, &mut seen, budget);

    // Sample the plausible numeric slope range before a sparse global code grid.
    let limit = endpoint
        .to_u64()
        .max(minimax.to_u64())
        .saturating_mul(2)
        .min(FloatU16::MAX);
    for part in 1..=8 {
        let stride = FloatU16::from_u64_lossy((u128::from(limit) * part / 8) as u64);
        consider_unsigned_stride(frame, layout, stride, &mut best, &mut seen, budget);
    }
    for part in 1..=8 {
        let stride = FloatU16::from_bits((u32::from(u16::MAX) * part / 8) as u16);
        consider_unsigned_stride(frame, layout, stride, &mut best, &mut seen, budget);
    }
    // Refine the current winner from coarse code steps down to adjacent codes.
    for shift in (0..=10).rev() {
        let center = best.0.to_bits();
        let step = 1u16 << shift;
        for bits in [center.saturating_sub(step), center.saturating_add(step)] {
            consider_unsigned_stride(
                frame,
                layout,
                FloatU16::from_bits(bits),
                &mut best,
                &mut seen,
                budget,
            );
        }
    }
    // Spend the remaining budget near the winner; freeze the center for coverage.
    let center = best.0.to_bits();
    for distance in 1..=budget.min(u16::MAX as usize) {
        if seen.len() >= budget || best.1 == 0 {
            break;
        }
        for bits in [
            center.saturating_sub(distance as u16),
            center.saturating_add(distance as u16),
        ] {
            consider_unsigned_stride(
                frame,
                layout,
                FloatU16::from_bits(bits),
                &mut best,
                &mut seen,
                budget,
            );
        }
    }
    best.0
}

/// Fit immutable starts, never late close/range-end values. Zero keeps unsigned
/// offsets; nonzero coefficients store exact modular ZigZag residuals.
pub(crate) fn fit_unsigned(frame: &mut Frame, layout: Layout) -> PredictionEncoding<true> {
    assert_eq!(
        layout.width, 8,
        "unsigned-origin fitting requires u64 words"
    );
    let origin = u64::from_ne_bytes(
        frame.buffer[frame.unit..2 * frame.unit][layout.range()]
            .try_into()
            .unwrap(),
    );
    // Normalize once before the bounded search. Repeating this subtraction in
    // every candidate scan would multiply origin work by the search budget.
    for row in
        frame.buffer[frame.unit..(frame.length + 1) * frame.unit].chunks_exact_mut(frame.unit)
    {
        let offset = (read_word(&row[layout.range()]) as u64)
            .checked_sub(origin)
            .expect("value precedes unsigned origin");
        write_word(&mut row[layout.range()], u128::from(offset));
    }
    let stride = if frame.length < 2 {
        FloatU16::default()
    } else if UNSIGNED_MASK_STRIDE {
        mask_unsigned_stride(frame, layout, UNSIGNED_MASK_CANDIDATES)
    } else if UNSIGNED_MINIMAX_STRIDE {
        minimax_unsigned_stride(frame, layout)
    } else {
        endpoint_unsigned_stride(frame, layout)
    };
    let encoding = PredictionEncoding {
        origin,
        stride: stride.to_u64(),
        ..Default::default()
    };
    let length = frame.length;
    let width = usize::from(layout.width);
    rewrite(frame, layout, |offset, row, _| {
        if encoding.stride == 0 {
            offset
        } else {
            u128::from(residual_u64(
                offset as u64,
                advance(encoding.stride, row, length),
                width,
            ))
        }
    });
    encoding
}

/// Preserve the original endpoint fit, including its out-of-range fallback.
pub(crate) fn endpoint_unsigned_stride(frame: &Frame, layout: Layout) -> FloatU16 {
    let end = u128::from(u64::from_ne_bytes(
        frame.buffer[frame.length * frame.unit + usize::from(layout.offset)
            ..frame.length * frame.unit + usize::from(layout.offset) + 8]
            .try_into()
            .unwrap(),
    ));
    let stride = end * frame.length as u128 / (frame.length - 1) as u128;
    if stride > u128::from(FloatU16::MAX) {
        return FloatU16::default();
    }
    FloatU16::from_u64_lossy(stride as u64)
}

/// Preserve origin, line, then cycle trial order for every ordinary field.
pub(crate) fn fit_candidates(
    best: &mut Candidate,
    values: &[u128],
    layout: Layout,
    cost: &Cost<'_>,
) {
    // Wider columns retain lossless raw/dictionary support without truncation.
    if layout.width > 8 {
        return;
    }
    let origin = Prediction {
        origin: values[0] as u64,
        ..Prediction::default()
    };
    consider_prediction(best, origin, values, cost);
    let line = prediction(values, layout, 0, 0);
    // A flat endpoint fit collapses to the exact origin already considered.
    if line.row || line.stride != 0 {
        consider_prediction(best, line, values, cost);
    }
    // A compact arithmetic cycle is simpler than an identity dictionary.
    select_cycles(best, values, layout, cost);
}
